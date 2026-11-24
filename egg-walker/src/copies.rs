//! The reference for moves as copies (#20): slow and obviously right, by
//! definition, replaying every event against its own version of the
//! document rebuilt from scratch.
//!
//! # Records
//!
//! An insert makes a record. A move makes a *copy* of every record it
//! covers, in a run at its destination, and *takes* the originals: they
//! don't show any more. A record has a place (its key: where it is) and a
//! root (the insert it descends from: who it is). Deletes, undeletes and
//! marks act on the root, so they follow an item wherever it's moved.
//!
//! A move is fixed when it's made, at its own version: it covers every
//! record drawn from the first visible item of its range to the last,
//! tombstones too, so an undo brings text back where it was moved to. Any
//! range moves; nothing nests. A move of nothing, or to where it is, is
//! inert.
//!
//! # At the same time
//!
//! Something done without seeing a move, whose visible neighbours (at its
//! own version) the move both took, goes with it: its origins are
//! redirected to their copies. So typing in a section someone is moving
//! goes where the section goes. At an edge (one neighbour taken, one not,
//! or the document's end) it stays: what decides is what its author saw
//! either side, not where ties among concurrent inserts happen to put it.
//! Of moves taking both, the innermost takes it, and so on outward.
//!
//! Moves made without seeing each other, in Lamport order then the order
//! concurrent inserts use, each against the active ones:
//! - the same records: the later replaces the earlier;
//! - records that cross (share some, neither holding the other): the later
//!   is dropped;
//! - one holding the other: both stand, and what's in both goes with the
//!   inner;
//! - moves whose copies would each go with the other's: the latest of
//!   them is dropped.
//!
//! An item's records make a tree: its insert, the copies moves made of
//! it, copies of those. Each record goes with the innermost active move
//! that took it, and the item shows only at the record that path ends on.
//! A copy off the path is laid out but never drawn, and so is any copy of
//! it: a later move of only such copies moves nothing.
//!
//! A move that doesn't stand makes no copies, so a copy of a copy it made
//! is no copy either: whatever's aimed at one is aimed at what it copied,
//! and a later move of only such copies moves nothing.
//!
//! Every move or mark discarded is a [`Loss`]: what was lost, to what, and
//! where its items are now.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use crate::oplog::{Lv, Op, OpLog};
use crate::order::{Placed, place};
use crate::walk::tie_break;

/// A record's key: an insert's own event, or past the log's events, a copy.
type Key = usize;

/// Where an insert, or a move's copies, go: its origins, and the visible
/// neighbours that decide what it goes with.
#[derive(Clone, Copy, Default)]
struct Spot {
    left: Option<Key>,
    right: Option<Key>,
    /// The visible items either side, at its version.
    before: Option<Key>,
    after: Option<Key>,
}

#[derive(Clone)]
struct Move {
    /// Every record it covers, in drawn order, tombstones too.
    sources: Vec<Key>,
    set: HashSet<Key>,
    /// Of those, the items that showed.
    shown: Vec<Key>,
    /// Its copies are `first_copy..first_copy + sources.len()`.
    first_copy: Key,
    to: Spot,
}

#[derive(Clone)]
enum Info {
    Insert(Spot),
    Delete {
        root: Lv,
    },
    Undelete {
        target: Lv,
    },
    Mark {
        root: Lv,
    },
    Move(Move),
    /// A move of nothing, or to where it is.
    Inert,
}

/// A move or mark discarded at a version: what lost, to what (none for a
/// move that was inert from the start), and where its items are now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loss {
    pub op: Lv,
    pub lost_to: Option<Lv>,
    pub ranges: Vec<Range<usize>>,
}

struct Rec {
    key: Key,
    left: Option<Key>,
    right: Option<Key>,
}

impl Placed for Rec {
    fn id(&self) -> Lv {
        self.key
    }
    fn left(&self) -> Option<Lv> {
        self.left
    }
    fn right(&self) -> Option<Lv> {
        self.right
    }
}

struct Replay<'a, T> {
    log: &'a OpLog<T>,
    /// Where copies' keys start: the log's length.
    base: Key,
    info: Vec<Info>,
    /// What each event saw: its parents' closure.
    past: Vec<Vec<bool>>,
    lamport: Vec<u64>,
    /// Each copy (by key less the log's length): its move, and its source.
    copies: Vec<(Lv, Key)>,
}

/// The document drawn at a version.
struct View {
    /// Every record laid out, in order, drawn or not.
    order: Vec<Key>,
    /// Every record that shows or is a tombstone where it lies, in order,
    /// and whether it shows.
    drawn: Vec<(Key, bool)>,
    losses: Vec<Loss>,
}

impl<T: Clone> Replay<'_, T> {
    fn events(&self) -> usize {
        self.info.len()
    }

    /// The event that made record `key`.
    fn by(&self, key: Key) -> Lv {
        if key < self.base {
            key
        } else {
            self.copies[key - self.base].0
        }
    }

    /// Who record `key` is: the insert it descends from.
    fn root(&self, mut key: Key) -> Lv {
        while key >= self.base {
            key = self.copies[key - self.base].1;
        }
        key
    }

    fn mv(&self, m: Lv) -> &Move {
        match &self.info[m] {
            Info::Move(mv) => mv,
            _ => unreachable!("{m} is a move"),
        }
    }

    fn concurrent(&self, a: Lv, b: Lv) -> bool {
        a != b && !self.past[a][b] && !self.past[b][a]
    }

    /// Whether record `key` is laid out among the `active` moves: an
    /// insert's always is; a copy is if its move stands and its source is.
    fn lives(&self, mut key: Key, active: &[Lv]) -> bool {
        while key >= self.base {
            let (m, source) = self.copies[key - self.base];
            if !active.contains(&m) {
                return false;
            }
            key = source;
        }
        true
    }

    /// `key`, or if it isn't laid out, the nearest thing it copied that is.
    fn live(&self, mut key: Key, active: &[Lv]) -> Key {
        while !self.lives(key, active) {
            key = self.copies[key - self.base].1;
        }
        key
    }

    fn spot(&self, x: Lv) -> Spot {
        match &self.info[x] {
            Info::Insert(spot) => *spot,
            Info::Move(mv) => mv.to,
            _ => unreachable!("{x} puts something somewhere"),
        }
    }

    /// Where `x` (an insert, or a move's copies) goes among the `active`
    /// moves: its origins and neighbours, redirected through each move it
    /// didn't see that took both neighbours, innermost first; and those
    /// moves.
    fn redirect(&self, x: Lv, active: &[Lv]) -> (Spot, Vec<Lv>) {
        let live = |k: Option<Key>| k.map(|k| self.live(k, active));
        let raw = self.spot(x);
        let mut spot = Spot {
            left: live(raw.left),
            right: live(raw.right),
            before: live(raw.before),
            after: live(raw.after),
        };
        let mut through: Vec<Lv> = Vec::new();
        loop {
            let holder = active
                .iter()
                .copied()
                .filter(|&m| m != x && self.concurrent(x, m) && !through.contains(&m))
                .filter(|&m| {
                    let set = &self.mv(m).set;
                    spot.before.is_some_and(|k| set.contains(&k))
                        && spot.after.is_some_and(|k| set.contains(&k))
                })
                .min_by_key(|&m| self.mv(m).sources.len());
            let Some(m) = holder else {
                return (spot, through);
            };
            let mv = self.mv(m);
            let map = |k: Option<Key>| {
                k.map(|k| match mv.sources.iter().position(|&s| s == k) {
                    Some(i) => self.live(mv.first_copy + i, active),
                    None => k,
                })
            };
            spot = Spot {
                left: map(spot.left),
                right: map(spot.right),
                before: map(spot.before),
                after: map(spot.after),
            };
            through.push(m);
        }
    }

    /// Which moves stand at `at`, by the rules above, leaving out
    /// `dropped` (moves that made a loop), and the losses.
    fn active(&self, at: &[bool], dropped: &HashSet<Lv>, losses: &mut Vec<Loss>) -> Vec<Lv> {
        let key = |m: Lv| (self.lamport[m], self.log.order_key(m));
        let mut moves: Vec<Lv> = (0..self.info.len())
            .filter(|&lv| at[lv] && !dropped.contains(&lv))
            .filter(|&lv| match &self.info[lv] {
                Info::Move(_) => true,
                Info::Inert => {
                    losses.push(Loss {
                        op: lv,
                        lost_to: None,
                        ranges: Vec::new(),
                    });
                    false
                }
                _ => false,
            })
            .collect();
        moves.sort_by_key(|&m| key(m));
        let mut active: Vec<Lv> = Vec::new();
        'next: for m in moves {
            let set = &self.mv(m).set;
            let mut replaced = None;
            for (i, &a) in active.iter().enumerate() {
                if !self.concurrent(a, m) {
                    continue;
                }
                let theirs = &self.mv(a).set;
                if theirs == set {
                    replaced = Some(i);
                } else if !theirs.is_disjoint(set)
                    && !theirs.is_subset(set)
                    && !set.is_subset(theirs)
                {
                    losses.push(Loss {
                        op: m,
                        lost_to: Some(a),
                        ranges: Vec::new(),
                    });
                    continue 'next;
                }
            }
            if let Some(i) = replaced {
                let a = active.remove(i);
                losses.push(Loss {
                    op: a,
                    lost_to: Some(m),
                    ranges: Vec::new(),
                });
            }
            active.push(m);
        }
        active
    }

    /// The roots deleted at `at`: by a delete there that isn't undone.
    fn deleted(&self, at: &[bool]) -> HashSet<Lv> {
        let undone: HashSet<Lv> = (0..self.events())
            .filter(|&u| at[u])
            .filter_map(|u| match self.info[u] {
                Info::Undelete { target } => Some(target),
                _ => None,
            })
            .collect();
        (0..self.events())
            .filter(|&d| at[d] && !undone.contains(&d))
            .filter_map(|d| match self.info[d] {
                Info::Delete { root } => Some(root),
                _ => None,
            })
            .collect()
    }

    fn view(&self, at: &[bool]) -> View {
        // Moves dropped for making a loop, and what each would have gone
        // with.
        let mut dropped: HashMap<Lv, Lv> = HashMap::new();
        let (active, order, mut losses) = loop {
            let mut losses = Vec::new();
            let skip: HashSet<Lv> = dropped.keys().copied().collect();
            let active = self.active(at, &skip, &mut losses);
            match self.build(at, &active) {
                Ok(order) => break (active, order, losses),
                Err(cycle) => {
                    let key = |m: &Lv| (self.lamport[*m], self.log.order_key(*m));
                    let (k, &latest) = cycle
                        .iter()
                        .enumerate()
                        .max_by_key(|(_, m)| key(m))
                        .unwrap();
                    dropped.insert(latest, cycle[(k + 1) % cycle.len()]);
                }
            }
        };
        for (&m, &with) in &dropped {
            losses.push(Loss {
                op: m,
                lost_to: Some(with),
                ranges: Vec::new(),
            });
        }
        let deleted = self.deleted(at);

        // What doesn't show where it lies: what an active move took, and
        // copies of what an inner move made at the same time took instead.
        let taken: HashSet<Key> = active
            .iter()
            .flat_map(|&m| self.mv(m).sources.iter().copied())
            .collect();
        let drawn: Vec<(Key, bool)> = order
            .iter()
            .copied()
            .filter(|&k| !taken.contains(&k) && self.on_path(k, &active))
            .map(|k| (k, !deleted.contains(&self.root(k))))
            .collect();
        let mut view = View {
            order,
            drawn,
            losses,
        };
        self.more_losses(at, &active, &mut view);
        view
    }

    /// The move record `key` goes with: the innermost active one that
    /// took it.
    fn winner(&self, key: Key, active: &[Lv]) -> Option<Lv> {
        active
            .iter()
            .copied()
            .filter(|&m| self.mv(m).set.contains(&key))
            .min_by_key(|&m| self.mv(m).sources.len())
    }

    /// Whether record `key` is on its item's path: each copy from the
    /// insert down to it made by the move that won what it copied.
    fn on_path(&self, mut key: Key, active: &[Lv]) -> bool {
        while key >= self.base {
            let (m, source) = self.copies[key - self.base];
            if !active.contains(&m) || self.winner(source, active) != Some(m) {
                return false;
            }
            key = source;
        }
        true
    }

    /// Why copy `key` isn't on its path: the move that won what's above it
    /// instead, or replaced its move. `None` if it is.
    fn blocker(&self, mut key: Key, active: &[Lv], losses: &[Loss]) -> Option<Lv> {
        while key >= self.base {
            let (m, source) = self.copies[key - self.base];
            if !active.contains(&m) {
                return losses.iter().find(|l| l.op == m).and_then(|l| l.lost_to);
            }
            match self.winner(source, active) {
                Some(w) if w != m => return Some(w),
                _ => key = source,
            }
        }
        None
    }

    /// Lays out every record in `at` and every copy the `active` moves
    /// make: their keys, in order. Or, if moves' copies would each go with
    /// another's, a loop of them, each going with the next.
    fn build(&self, at: &[bool], active: &[Lv]) -> Result<Vec<Key>, Vec<Lv>> {
        let mut spots: HashMap<Lv, Spot> = HashMap::new();
        let mut goes_with: HashMap<Lv, Vec<Lv>> = HashMap::new();
        for lv in (0..self.info.len()).filter(|&lv| at[lv]) {
            let puts = match self.info[lv] {
                Info::Insert(_) => true,
                Info::Move(_) => active.contains(&lv),
                _ => false,
            };
            if puts {
                let (spot, through) = self.redirect(lv, active);
                spots.insert(lv, spot);
                if matches!(self.info[lv], Info::Move(_)) {
                    goes_with.insert(lv, through);
                }
            }
        }
        // A loop of moves each going with the next is refused.
        for &start in active {
            let mut path = vec![start];
            let mut todo = vec![(start, 0usize)];
            while let Some((m, k)) = todo.pop() {
                path.truncate(k + 1);
                path[k] = m;
                for &next in goes_with.get(&m).into_iter().flatten() {
                    if let Some(i) = path.iter().position(|&p| p == next) {
                        return Err(path[i..].to_vec());
                    }
                    todo.push((next, k + 1));
                    path.push(next);
                }
            }
        }
        let origins = |key: Key| -> (Option<Key>, Option<Key>) {
            let by = self.by(key);
            let spot = spots[&by];
            if key < self.base {
                return (spot.left, spot.right);
            }
            // A run: after the copy before it that's laid out.
            let first = self.mv(by).first_copy;
            let left = (first..key).rev().find(|&c| self.lives(c, active));
            (left.or(spot.left), spot.right)
        };
        let mut todo: Vec<Key> = Vec::new();
        for lv in (0..self.info.len()).filter(|lv| spots.contains_key(lv)) {
            match &self.info[lv] {
                Info::Insert(_) => todo.push(lv),
                Info::Move(mv) => todo.extend(
                    (mv.first_copy..mv.first_copy + mv.sources.len())
                        .filter(|&c| self.lives(c, active)),
                ),
                _ => {}
            }
        }
        let mut list: Vec<Rec> = Vec::new();
        let mut placed: HashSet<Key> = HashSet::new();
        // Each record after its origins.
        for key in todo {
            let mut stack = vec![key];
            while let Some(&k) = stack.last() {
                if placed.contains(&k) {
                    stack.pop();
                    continue;
                }
                let (left, right) = origins(k);
                let waiting: Vec<Key> = [left, right]
                    .into_iter()
                    .flatten()
                    .filter(|o| !placed.contains(o))
                    .collect();
                if !waiting.is_empty() {
                    assert!(stack.len() <= 4 * self.base + 8, "origins in a loop");
                    stack.extend(waiting);
                    continue;
                }
                let by = self.by(k);
                let dest = place(
                    &list,
                    left,
                    right,
                    |id| list.iter().position(|r| r.key == id).unwrap(),
                    |other| tie_break(self.log, by, self.by(other)),
                );
                list.insert(
                    dest,
                    Rec {
                        key: k,
                        left,
                        right,
                    },
                );
                placed.insert(k);
                stack.pop();
            }
        }
        Ok(list.into_iter().map(|r| r.key).collect())
    }

    /// Whether some insert or move, made without seeing a move, was put at
    /// that move's edge, or among what it took that the insert didn't see:
    /// where ties among concurrent inserts could put it inside what moved
    /// though its author's neighbours weren't both. The portals decide those
    /// by where ties put it; this reference by the neighbours.
    fn edge_ties(&self) -> bool {
        (0..self.info.len()).any(|x| {
            let spot = match self.info[x] {
                Info::Insert(_) | Info::Move(_) => self.spot(x),
                _ => return false,
            };
            (0..self.info.len()).any(|m| {
                let Info::Move(mv) = &self.info[m] else {
                    return false;
                };
                if !self.concurrent(x, m) {
                    return false;
                }
                let inside = |k: Option<Key>| k.is_some_and(|k| mv.set.contains(&k));
                inside(spot.before) != inside(spot.after)
                    || mv.sources.iter().any(|&k| self.concurrent(self.by(k), x))
            })
        })
    }

    /// The losses that need the drawing: moves whose every item was
    /// deleted by something that didn't see them moved, and marks a later
    /// mark made without seeing them overrode. And where the lost things'
    /// items are now.
    fn more_losses(&self, at: &[bool], active: &[Lv], view: &mut View) {
        let visible: Vec<Lv> = view
            .drawn
            .iter()
            .filter(|(_, shows)| *shows)
            .map(|&(k, _)| self.root(k))
            .collect();
        let position: HashMap<Lv, usize> =
            visible.iter().enumerate().map(|(i, &r)| (r, i)).collect();
        let ranges = |roots: &[Lv]| -> Vec<Range<usize>> {
            let mut at: Vec<usize> = roots
                .iter()
                .filter_map(|r| position.get(r).copied())
                .collect();
            at.sort_unstable();
            let mut out: Vec<Range<usize>> = Vec::new();
            for p in at {
                match out.last_mut() {
                    Some(r) if r.end == p => r.end += 1,
                    _ => out.push(p..p + 1),
                }
            }
            out
        };
        // Deletes, not undone, by root.
        let undone = |d: Lv| {
            (0..self.events())
                .any(|u| at[u] && matches!(self.info[u], Info::Undelete { target } if target == d))
        };
        let mut deletes: HashMap<Lv, Vec<Lv>> = HashMap::new();
        for d in (0..self.events()).filter(|&d| at[d]) {
            if let Info::Delete { root } = self.info[d]
                && !undone(d)
            {
                deletes.entry(root).or_default().push(d);
            }
        }
        for &m in active {
            let mv = self.mv(m);
            // Only copies off their items' paths: nothing moves.
            if mv.sources.iter().all(|&k| !self.on_path(k, active)) {
                let lost_to = self.blocker(mv.sources[0], active, &view.losses);
                view.losses.push(Loss {
                    op: m,
                    lost_to,
                    ranges: Vec::new(),
                });
                continue;
            }
            let shown = &self.mv(m).shown;
            let mut by = Vec::new();
            let all = shown.iter().all(|&k| {
                let killer = deletes
                    .get(&self.root(k))
                    .and_then(|ds| ds.iter().copied().filter(|&d| self.concurrent(d, m)).max());
                by.extend(killer);
                killer.is_some()
            });
            if all && !shown.is_empty() {
                view.losses.push(Loss {
                    op: m,
                    lost_to: by
                        .into_iter()
                        .max_by_key(|&d| (self.lamport[d], self.log.order_key(d))),
                    ranges: Vec::new(),
                });
            }
        }
        let key = |m: Lv| (self.lamport[m], self.log.order_key(m));
        let mut marks: BTreeMap<Lv, Vec<Lv>> = BTreeMap::new();
        for k in (0..self.events()).filter(|&k| at[k]) {
            if let Info::Mark { root } = self.info[k] {
                marks.entry(root).or_default().push(k);
            }
        }
        for ks in marks.values() {
            let winner = *ks.iter().max_by_key(|&&k| key(k)).unwrap();
            for &k in ks {
                if k != winner && !self.past[winner][k] {
                    view.losses.push(Loss {
                        op: k,
                        lost_to: Some(winner),
                        ranges: Vec::new(),
                    });
                }
            }
        }
        for loss in &mut view.losses {
            let roots: Vec<Lv> = match &self.info[loss.op] {
                Info::Move(mv) => mv.shown.iter().map(|&k| self.root(k)).collect(),
                Info::Mark { root } => vec![*root],
                _ => Vec::new(),
            };
            loss.ranges = ranges(&roots);
        }
        view.losses.sort_by_key(|l| l.op);
    }
}

fn replay<T: Clone>(log: &OpLog<T>) -> Replay<'_, T> {
    let n = log.len();
    let mut r = Replay {
        log,
        base: n,
        info: Vec::with_capacity(n),
        past: Vec::with_capacity(n),
        lamport: Vec::with_capacity(n),
        copies: Vec::new(),
    };
    for lv in 0..n {
        let event = log.event(lv);
        let lamport = 1 + event
            .parents
            .iter()
            .map(|&p| r.lamport[p])
            .max()
            .unwrap_or(0);
        r.lamport.push(lamport);
        let at = log.closure(&event.parents);
        // Its own version, drawn from what came before it.
        let view = r.view(&at);
        r.past.push(at);
        let vis: Vec<usize> = (0..view.drawn.len()).filter(|&i| view.drawn[i].1).collect();
        let spot_at = |pos: usize| -> Spot {
            assert!(pos <= vis.len(), "position {pos} is past the end");
            let before = pos.checked_sub(1).map(|p| view.drawn[vis[p]].0);
            // The next record as laid out, whatever it is: moved away,
            // a tombstone, or showing.
            let next = match before {
                None => 0,
                Some(k) => view.order.iter().position(|&o| o == k).unwrap() + 1,
            };
            Spot {
                left: before,
                right: view.order.get(next).copied(),
                before,
                after: vis.get(pos).map(|&i| view.drawn[i].0),
            }
        };
        let item_at = |pos: usize| -> Key {
            view.drawn[*vis
                .get(pos)
                .expect("an event names an item its version shows")]
            .0
        };
        let info = match &event.op {
            Op::Insert { pos, .. } => Info::Insert(spot_at(*pos)),
            Op::Delete { pos } => Info::Delete {
                root: r.root(item_at(*pos)),
            },
            Op::Mark { pos, .. } => Info::Mark {
                root: r.root(item_at(*pos)),
            },
            Op::Undelete { target } => {
                assert!(
                    r.past[lv][*target],
                    "undelete {lv} undoes a delete it includes"
                );
                Info::Undelete { target: *target }
            }
            &Op::Move { pos, len, to } => {
                if len == 0 || (pos..=pos + len).contains(&to) || pos + len > vis.len() {
                    Info::Inert
                } else {
                    let (first, last) = (vis[pos], vis[pos + len - 1]);
                    let sources: Vec<Key> =
                        view.drawn[first..=last].iter().map(|&(k, _)| k).collect();
                    let shown: Vec<Key> = view.drawn[first..=last]
                        .iter()
                        .filter(|(_, s)| *s)
                        .map(|&(k, _)| k)
                        .collect();
                    let first_copy = n + r.copies.len();
                    r.copies.extend(sources.iter().map(|&s| (lv, s)));
                    Info::Move(Move {
                        set: sources.iter().copied().collect(),
                        sources,
                        shown,
                        first_copy,
                        to: spot_at(to),
                    })
                }
            }
        };
        r.info.push(info);
    }
    r
}

/// The document at `version`.
pub fn checkout<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> Vec<T> {
    let r = replay(log);
    let view = r.view(&log.closure(version));
    view.drawn
        .iter()
        .filter(|(_, shows)| *shows)
        .map(|&(k, _)| match log.event(r.root(k)).op {
            Op::Insert { item, .. } => item,
            _ => unreachable!("a root is an insert"),
        })
        .collect()
}

/// Who each item at `version` is, in order: the insert it descends from.
pub fn identities<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> Vec<Lv> {
    let r = replay(log);
    let view = r.view(&log.closure(version));
    view.drawn
        .iter()
        .filter(|(_, shows)| *shows)
        .map(|&(k, _)| r.root(k))
        .collect()
}

/// Each item in `version` with a level that isn't 0 there, deleted or not.
pub fn levels<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> BTreeMap<Lv, u8> {
    let r = replay(log);
    let at = log.closure(version);
    let key = |m: Lv| (r.lamport[m], log.order_key(m));
    let mut latest: BTreeMap<Lv, Lv> = BTreeMap::new();
    for k in (0..log.len()).filter(|&k| at[k]) {
        if let Info::Mark { root } = r.info[k] {
            let e = latest.entry(root).or_insert(k);
            if key(k) > key(*e) {
                *e = k;
            }
        }
    }
    latest
        .into_iter()
        .filter_map(|(root, k)| match log.event(k).op {
            Op::Mark { level, .. } if level > 0 => Some((root, level)),
            _ => None,
        })
        .collect()
}

/// Every move and mark discarded at `version`, by op.
pub fn losses<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> Vec<Loss> {
    let r = replay(log);
    r.view(&log.closure(version)).losses
}

/// Whether the history has an edge tie (see the docs above): where this
/// reference and the portals may differ.
#[doc(hidden)]
pub fn edge_ties<T: Clone>(log: &OpLog<T>) -> bool {
    replay(log).edge_ties()
}

/// The moves inert from the start: of nothing, or to where they are.
pub fn inert<T: Clone>(log: &OpLog<T>) -> Vec<Lv> {
    let r = replay(log);
    (0..log.len())
        .filter(|&lv| matches!(r.info[lv], Info::Inert))
        .collect()
}
