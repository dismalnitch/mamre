//! The slow, obviously-correct version: a plain list CRDT with tombstones,
//! replaying the whole history and rebuilding each event's own version of
//! the document from scratch, by definition, before applying it.
//!
//! No retreating, advancing or effect positions: an item exists at a
//! version if its insert is in it, and is visible if every delete in it that
//! removed it is undone there too (some undelete of it is in the version).
//! The walk must agree with this on every history. They share only the
//! ordering rule for concurrent inserts.
//!
//! # Moves
//!
//! Records keep one order for good, the *home order*: a move reorders
//! nothing. It names an interval of the home order, from one record to
//! another, and puts a *portal* where it went, placed as an insert would
//! be. Drawing the document at a version walks the home order, skips each
//! active move's interval where it lies, and draws it at its portal
//! instead. Intervals and portals nest, so this recurses: a *frame* is the
//! root, or the inside of an active move's interval.
//!
//! Anything put inside a moved interval, at the same time or later, is in
//! it, so it goes wherever the interval goes: concurrent typing follows a
//! moved section, with no redirecting.
//!
//! Which moves are active at a version: its moves in one order (Lamport
//! time, then the order concurrent inserts use), each active unless its
//! interval partly overlaps an active one's (dropped), or equals one's (it
//! replaces it: the later move of a block wins), or its portal would be
//! drawn inside its own interval, a loop (dropped).
//!
//! A move is fixed when it's made, at its own version: its interval is the
//! elements of one frame that its range covers exactly. One that doesn't
//! (a range cutting into a moved block), or that moves nothing, does
//! nothing, ever.

use std::collections::HashMap;
use std::ops::Range;

use crate::oplog::{Lv, Op, OpLog};
use crate::order::{Placed, place};
use crate::walk::tie_break;

enum Rec<T> {
    Item {
        id: Lv,
        left: Option<Lv>,
        right: Option<Lv>,
        deleted_by: Vec<Lv>,
        value: T,
    },
    /// Where move `id`'s interval is drawn.
    Portal {
        id: Lv,
        left: Option<Lv>,
        right: Option<Lv>,
    },
}

impl<T> Placed for Rec<T> {
    fn id(&self) -> Lv {
        match self {
            Rec::Item { id, .. } | Rec::Portal { id, .. } => *id,
        }
    }
    fn left(&self) -> Option<Lv> {
        match self {
            Rec::Item { left, .. } | Rec::Portal { left, .. } => *left,
        }
    }
    fn right(&self) -> Option<Lv> {
        match self {
            Rec::Item { right, .. } | Rec::Portal { right, .. } => *right,
        }
    }
}

/// A move that does something: its interval's first and last record.
#[derive(Clone, Copy)]
struct Move {
    id: Lv,
    first: Lv,
    last: Lv,
}

/// The document at `version`, the slow way.
pub fn checkout<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> Vec<T> {
    let at = log.closure(version);
    let state = replay(log);
    let view = state.view(&at);
    view.entries
        .iter()
        .filter(|e| e.visible)
        .map(|e| match &state.list[e.at] {
            Rec::Item { value, .. } => value.clone(),
            Rec::Portal { .. } => unreachable!("entries are items"),
        })
        .collect()
}

/// Which insert each item visible at `version` came from, in order: the
/// identities a replica's [`Identities`](crate::Identities) must hold.
#[doc(hidden)]
pub fn identities<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> Vec<Lv> {
    visible_ids(log, &log.closure(version))
}

/// Which insert each item visible at `at` (a version, as a set) came
/// from, in order: their identities.
pub(crate) fn visible_ids<T: Clone>(log: &OpLog<T>, at: &[bool]) -> Vec<Lv> {
    let state = replay(log);
    let view = state.view(at);
    view.entries
        .iter()
        .filter(|e| e.visible)
        .map(|e| state.list[e.at].id())
        .collect()
}

/// The moved blocks the document shows at `at` (a version, as a set):
/// each active move with something visible in it, and the visible
/// positions it covers. They nest, and come in no particular order.
pub(crate) fn blocks<T: Clone>(log: &OpLog<T>, at: &[bool]) -> Vec<(Lv, Range<usize>)> {
    let state = replay(log);
    let view = state.view(at);
    let visible: Vec<&Entry> = view.entries.iter().filter(|e| e.visible).collect();
    view.active
        .iter()
        .filter_map(|(m, _, _)| {
            let mut inside = (0..visible.len()).filter(|&k| visible[k].path.contains(&m.id));
            let first = inside.next()?;
            let last = inside.next_back().unwrap_or(first);
            Some((m.id, first..last + 1))
        })
        .collect()
}

/// Every record, in home order, and what drawing them needs.
struct State<'a, T> {
    log: &'a OpLog<T>,
    list: Vec<Rec<T>>,
    /// Each delete's undeletes.
    undone_by: HashMap<Lv, Vec<Lv>>,
    /// The moves that do something, in the order activity is decided in.
    moves: Vec<Move>,
    lamport: Vec<u64>,
}

/// An item as drawn at a version.
#[derive(Clone)]
struct Entry {
    /// Its index in the home order.
    at: usize,
    visible: bool,
    /// The active moves it's drawn inside, outermost first.
    path: Vec<Lv>,
}

/// An active move, with its interval's home indices.
type Active = (Move, usize, usize);

/// The document drawn at a version.
struct View {
    /// Every item in the version, deleted or not, in drawn order.
    entries: Vec<Entry>,
    active: Vec<Active>,
    /// Whether each record is in the version.
    present: Vec<bool>,
}

/// The innermost of `active`'s intervals containing home index `i`.
fn innermost(active: &[Active], i: usize) -> Option<Lv> {
    active
        .iter()
        .filter(|(_, lo, hi)| (*lo..=*hi).contains(&i))
        .min_by_key(|(_, lo, hi)| hi - lo)
        .map(|(m, _, _)| m.id)
}

impl View {
    fn interval(&self, m: Lv) -> (usize, usize) {
        let &(_, lo, hi) = self.active.iter().find(|(a, _, _)| a.id == m).unwrap();
        (lo, hi)
    }

    /// The frame `f`'s interval lies in.
    fn parent(&self, f: Lv) -> Option<Lv> {
        let (lo, hi) = self.interval(f);
        self.active
            .iter()
            .filter(|(m, a, b)| m.id != f && *a <= lo && hi <= *b)
            .min_by_key(|(_, a, b)| b - a)
            .map(|(m, _, _)| m.id)
    }
}

impl<T: Clone> State<'_, T> {
    fn index_of(&self, id: Lv) -> usize {
        self.list.iter().position(|r| r.id() == id).unwrap()
    }

    fn visible(&self, deleted_by: &[Lv], at: &[bool]) -> bool {
        deleted_by.iter().all(|&d| {
            !at[d]
                || self
                    .undone_by
                    .get(&d)
                    .is_some_and(|us| us.iter().any(|&u| at[u]))
        })
    }

    /// Which moves are active at `at`, by the rules above.
    fn active(&self, at: &[bool]) -> Vec<Active> {
        let mut active: Vec<Active> = Vec::new();
        for &m in self.moves.iter().filter(|m| at[m.id]) {
            let (lo, hi) = (self.index_of(m.first), self.index_of(m.last));
            let overlaps = active
                .iter()
                .any(|&(_, a, b)| (a < lo && lo <= b && b < hi) || (lo < a && a <= hi && hi < b));
            if overlaps {
                continue;
            }
            let replaced = active.iter().position(|&(_, a, b)| (a, b) == (lo, hi));
            let replaced = replaced.map(|k| active.remove(k));
            active.push((m, lo, hi));
            if self.loops(&active, m.id) {
                active.pop();
                active.extend(replaced);
            }
        }
        active
    }

    /// Whether move `m`'s portal is drawn inside its own interval.
    fn loops(&self, active: &[Active], m: Lv) -> bool {
        let mut portal = m;
        for _ in 0..=active.len() {
            match innermost(active, self.index_of(portal)) {
                None => return false,
                Some(f) if f == m => return true,
                Some(f) => portal = f,
            }
        }
        unreachable!("the active moves had a loop before {m}")
    }

    /// The document drawn at `at`.
    fn view(&self, at: &[bool]) -> View {
        let mut view = View {
            entries: Vec::new(),
            active: self.active(at),
            present: self.list.iter().map(|r| at[r.id()]).collect(),
        };
        let mut entries = Vec::new();
        let mut path = Vec::new();
        self.draw(&view, None, 0..self.list.len(), &mut path, at, &mut entries);
        view.entries = entries;
        view
    }

    /// Draws frame `frame`'s records in `range` of the home order.
    fn draw(
        &self,
        view: &View,
        frame: Option<Lv>,
        range: Range<usize>,
        path: &mut Vec<Lv>,
        at: &[bool],
        out: &mut Vec<Entry>,
    ) {
        for i in range {
            if !view.present[i] || innermost(&view.active, i) != frame {
                continue;
            }
            match &self.list[i] {
                Rec::Item { deleted_by, .. } => out.push(Entry {
                    at: i,
                    visible: self.visible(deleted_by, at),
                    path: path.clone(),
                }),
                Rec::Portal { id, .. } => {
                    if view.active.iter().any(|(m, _, _)| m.id == *id) {
                        let (lo, hi) = view.interval(*id);
                        path.push(*id);
                        self.draw(view, Some(*id), lo..hi + 1, path, at, out);
                        path.pop();
                    }
                }
            }
        }
    }

    /// The home index of what draws `entry` in the frame at `depth` of its
    /// path: the item itself, or the portal of the block it's inside.
    fn lifted(&self, entry: &Entry, depth: usize) -> usize {
        match entry.path.get(depth) {
            None => entry.at,
            Some(&m) => self.index_of(m),
        }
    }

    /// The next element after home index `after` (or from the start) in
    /// frame `frame`'s own home order: a record of the frame, or the first
    /// record of an interval moved out of it.
    fn next_element(&self, view: &View, frame: Option<Lv>, after: Option<usize>) -> Option<Lv> {
        let (lo, end) = match frame {
            None => (0, self.list.len()),
            Some(f) => {
                let (lo, hi) = view.interval(f);
                (lo, hi + 1)
            }
        };
        let from = after.map_or(lo, |a| a + 1);
        (from..end).find_map(|i| {
            if !view.present[i] {
                return None;
            }
            // Its own record, or where a block moved out of it starts:
            // any block, not just the innermost here, as blocks nest and
            // can start at the same record.
            let ours = innermost(&view.active, i) == frame
                || view
                    .active
                    .iter()
                    .any(|(m, lo, _)| *lo == i && view.parent(m.id) == frame);
            ours.then(|| self.list[i].id())
        })
    }

    /// The origins for something put in at visible position `pos` of
    /// `view`: the elements either side of it in their lowest common frame.
    fn origins(&self, view: &View, pos: usize) -> (Option<Lv>, Option<Lv>) {
        let visible: Vec<usize> = (0..view.entries.len())
            .filter(|&k| view.entries[k].visible)
            .collect();
        assert!(pos <= visible.len(), "position {pos} is past the end");
        let left = pos.checked_sub(1).map(|p| visible[p]);
        // The frame they share is decided by what shows: the visible item
        // after, not a tombstone, so an edge is where it looks to be.
        let right = visible.get(pos).copied();
        let empty = Vec::new();
        let left_path = left.map_or(&empty, |k| &view.entries[k].path);
        let right_path = right.map_or(&empty, |k| &view.entries[k].path);
        let depth = left_path
            .iter()
            .zip(right_path)
            .take_while(|(a, b)| a == b)
            .count();
        let frame = depth.checked_sub(1).map(|d| left_path[d]);
        let lifted = left.map(|k| self.lifted(&view.entries[k], depth));
        let left_id = lifted.map(|i| self.list[i].id());
        (left_id, self.next_element(view, frame, lifted))
    }

    fn put(&mut self, lv: Lv, left: Option<Lv>, right: Option<Lv>, rec: Rec<T>) {
        let log = self.log;
        let dest = place(
            &self.list,
            left,
            right,
            |id| self.index_of(id),
            |other| tie_break(log, lv, other),
        );
        self.list.insert(dest, rec);
    }

    /// The interval a move of the `len` visible items at `pos` names, if
    /// its range is whole elements of one frame.
    fn interval(&self, view: &View, pos: usize, len: usize) -> Option<(Lv, Lv)> {
        let visible: Vec<&Entry> = view.entries.iter().filter(|e| e.visible).collect();
        if len == 0 || pos + len > visible.len() {
            return None;
        }
        let (f, l) = (visible[pos], visible[pos + len - 1]);
        let depth = f
            .path
            .iter()
            .zip(&l.path)
            .take_while(|(a, b)| a == b)
            .count();
        let prefix = &f.path[..depth];
        let (lo, hi) = (self.lifted(f, depth), self.lifted(l, depth));
        // What the elements from `lo` to `hi` draw must be the range, all
        // of it and nothing else.
        let covered: Vec<usize> = (0..visible.len())
            .filter(|&k| {
                let e = visible[k];
                e.path.starts_with(prefix) && (lo..=hi).contains(&self.lifted(e, depth))
            })
            .collect();
        let exact = covered.len() == len && covered.first() == Some(&pos);
        exact.then(|| (self.list[lo].id(), self.list[hi].id()))
    }
}

fn replay<T: Clone>(log: &OpLog<T>) -> State<'_, T> {
    let mut state = State {
        log,
        list: Vec::new(),
        undone_by: HashMap::new(),
        moves: Vec::new(),
        lamport: Vec::with_capacity(log.len()),
    };
    for lv in 0..log.len() {
        let event = log.event(lv);
        let lamport = 1 + event
            .parents
            .iter()
            .map(|&p| state.lamport[p])
            .max()
            .unwrap_or(0);
        state.lamport.push(lamport);
        let at = log.closure(&event.parents);
        let view = state.view(&at);
        match &event.op {
            Op::Insert { pos, item } => {
                let (left, right) = state.origins(&view, *pos);
                let rec = Rec::Item {
                    id: lv,
                    left,
                    right,
                    deleted_by: Vec::new(),
                    value: item.clone(),
                };
                state.put(lv, left, right, rec);
            }
            Op::Delete { pos } => {
                let k = view
                    .entries
                    .iter()
                    .filter(|e| e.visible)
                    .nth(*pos)
                    .expect("a delete is of an item its version shows")
                    .at;
                match &mut state.list[k] {
                    Rec::Item { deleted_by, .. } => deleted_by.push(lv),
                    Rec::Portal { .. } => unreachable!("entries are items"),
                }
            }
            Op::Undelete { target } => {
                assert!(at[*target], "undelete {lv} undoes a delete it includes");
                state.undone_by.entry(*target).or_default().push(lv);
            }
            &Op::Move { pos, len, to } => {
                if (pos..=pos + len).contains(&to) {
                    // To where it is, or into itself: nothing moves.
                    continue;
                }
                let Some((first, last)) = state.interval(&view, pos, len) else {
                    continue;
                };
                let (left, right) = state.origins(&view, to);
                state.put(
                    lv,
                    left,
                    right,
                    Rec::Portal {
                        id: lv,
                        left,
                        right,
                    },
                );
                // Kept in the order activity is decided in: Lamport time,
                // then as concurrent inserts are ordered.
                let key = |id: Lv| (state.lamport[id], log.order_key(id));
                let k = state.moves.partition_point(|m| key(m.id) < key(lv));
                state.moves.insert(
                    k,
                    Move {
                        id: lv,
                        first,
                        last,
                    },
                );
            }
        }
    }
    state
}
