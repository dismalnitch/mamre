//! Moves as copies (#20): the reference, slow and obviously right, and
//! the same rules replaying a window of a merge.
//!
//! The reference replays every event against its own version of the
//! document, rebuilt from scratch, by definition. A merge's window replays
//! only the events after a version every one of them saw (the window's
//! start, where the document is known), the same way, but builds each
//! event's version from the one before where it can: an event that saw
//! everything before it changes its version's document flatly.
//!
//! # Records
//!
//! An insert makes a record. A move makes a *copy* of every record it
//! covers, in a run at its destination, and *takes* the originals: they
//! don't show any more. A record has a place (its key: where it is) and a
//! root (the insert it descends from: who it is). Deletes, undeletes and
//! marks act on the root, so they follow an item wherever it's moved. A
//! window's starting document is a record for each item, its key the
//! item's root.
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
//! own version) the move both took or both went with it, goes with it: its
//! origins are redirected to their copies. So typing in a section someone is moving
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
//! A move that doesn't stand still lays out its copies, but never draws
//! them: something put beside one stays there. Put between two, it goes
//! with whatever took what they copied: what decides is what the copies
//! stood for, so typing in a moved section someone else moved too goes
//! where the winner put it. A move dropped for a loop is the exception:
//! its copies went where the other move took them, not where its author
//! put them, so something put beside one goes by what it copied instead
//! (mamre#27).
//!
//! That exception is kept to loops on purpose. It could cover every losing
//! move (replaced, crossing) by counting those moves in `looped`, but that
//! changes where typing beside a replaced or crossing move's copies lands,
//! which the losing-move tests pin as "it stays there". Widen it only as a
//! decision of its own.
//!
//! Every move or mark discarded is a [`Loss`]: what was lost, to what, and
//! where its items are now.
//!
//! # Undoing a move (#30)
//!
//! An undo names a move, or an undo of one. An undo is in force at a
//! version unless an undo of it is in force there, and a move with an undo
//! in force is *cancelled*: it isn't among the moves that stand, and isn't
//! lost either. So its originals show where they lie, with their own
//! keys, and its copies are laid out but never drawn. Its copies stand for
//! what they copied, as a looped move's do: something put beside one goes
//! by the original instead, so typing among the copies goes back with the
//! originals. A redo is an undo of the undo: the move stands again, with
//! the same copies, so undo and redo never make new ones. A move that had
//! lost already shows nothing different undone. A later move that took a
//! cancelled move's copies moves nothing, and is lost (to nothing).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use crate::oplog::{Lv, Op, OpLog};
use crate::order::{Placed, place};
use crate::walk::tie_break;

/// A record's key: an insert's own event (for the window's starting
/// document, the item's root), or past the log's events, a copy.
type Key = usize;

/// An event, numbered in the replay: the window's events in order.
type Ix = usize;

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
    /// Undoing delete `target` (an event of the window).
    Undelete {
        target: Ix,
    },
    Mark {
        root: Lv,
        level: u8,
    },
    /// Undoing `target`: a move, or an undo of one (events of the window).
    Undo {
        target: Ix,
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

/// Redirects found, and the moves they were found for: those standing,
/// those dropped for loops, those undone.
type Redirects = (Vec<Ix>, Vec<Ix>, HashMap<Ix, (Spot, Vec<Ix>)>, Vec<Ix>);

/// Why a window can't be replayed from its start alone.
#[derive(Debug)]
pub(crate) struct Unsupported;

pub(crate) struct Replay<'a, T> {
    log: &'a OpLog<T>,
    /// The starting document's records, in order: each item's root.
    base: Vec<Key>,
    /// The replayed events, in order, and where each is.
    events: Vec<Lv>,
    ix: HashMap<Lv, Ix>,
    info: Vec<Info>,
    /// What each event saw, of the replayed ones.
    past: Vec<Vec<bool>>,
    /// Where copies' keys start: the log's length.
    copy_base: Key,
    /// Each copy (by key less `copy_base`): its move, and its source.
    copies: Vec<(Ix, Key)>,
    /// Each event's own version, as a set of replayed events.
    versions: Vec<Vec<bool>>,
    /// Where each event goes, among the active moves it was worked out
    /// for: each depends only on earlier ones, so it's found once.
    redirects: std::cell::RefCell<Redirects>,
    /// The moves dropped for loops, in the layout being built.
    looped: std::cell::RefCell<Vec<Ix>>,
    /// The moves undone, in the layout being built (#30).
    cancelled: std::cell::RefCell<Vec<Ix>>,
}

/// The document drawn at a version.
#[derive(Clone)]
pub(crate) struct View {
    /// Every record laid out, in order, drawn or not.
    order: Vec<Key>,
    /// Every record that shows or is a tombstone where it lies, in order,
    /// and whether it shows.
    drawn: Vec<(Key, bool)>,
    /// The roots deleted (and not undone).
    deleted: HashSet<Lv>,
    pub(crate) losses: Vec<Loss>,
    active: Vec<Ix>,
}

impl View {
    /// The items that show, by root, in order.
    pub(crate) fn roots<T: Clone>(&self, r: &Replay<'_, T>) -> Vec<Lv> {
        self.drawn
            .iter()
            .filter(|(_, shows)| *shows)
            .map(|&(k, _)| r.root(k))
            .collect()
    }
}

impl<'a, T: Clone> Replay<'a, T> {
    fn n(&self) -> usize {
        self.info.len()
    }

    /// The replayed event that made record `key`, if one did.
    fn by(&self, key: Key) -> Option<Ix> {
        if key >= self.copy_base {
            Some(self.copies[key - self.copy_base].0)
        } else {
            self.ix.get(&key).copied()
        }
    }

    /// The log's event that made record `key` (for a starting record, its
    /// insert): what ties are broken by.
    fn made_by(&self, key: Key) -> Lv {
        match self.by(key) {
            Some(i) => self.events[i],
            None => key,
        }
    }

    /// Who record `key` is: the insert it descends from.
    fn root(&self, mut key: Key) -> Lv {
        while key >= self.copy_base {
            key = self.copies[key - self.copy_base].1;
        }
        key
    }

    fn mv(&self, m: Ix) -> &Move {
        match &self.info[m] {
            Info::Move(mv) => mv,
            _ => unreachable!("{m} is a move"),
        }
    }

    fn concurrent(&self, a: Ix, b: Ix) -> bool {
        a != b && !self.past[a][b] && !self.past[b][a]
    }

    /// Whether the record made by `by` (none: one the window started with)
    /// is concurrent with event `m`.
    fn concurrent_with(&self, by: Option<Ix>, m: Ix) -> bool {
        by.is_some_and(|b| self.concurrent(b, m))
    }

    fn key(&self, i: Ix) -> (u64, (&str, u64)) {
        let lv = self.events[i];
        (self.log.lamport(lv), self.log.order_key(lv))
    }

    /// Whether record `key` is laid out among the `active` moves: an
    /// insert's always is; a copy is if its move stands and its source is.
    fn lives(&self, mut key: Key, active: &[Ix]) -> bool {
        while key >= self.copy_base {
            let (m, source) = self.copies[key - self.copy_base];
            if !active.contains(&m) {
                return false;
            }
            key = source;
        }
        true
    }

    /// `key`, or if it isn't laid out, the nearest thing it copied that is.
    fn live(&self, mut key: Key, active: &[Ix]) -> Key {
        while !self.lives(key, active) {
            key = self.copies[key - self.copy_base].1;
        }
        key
    }

    fn spot(&self, x: Ix) -> Spot {
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
    fn redirect(&self, x: Ix, active: &[Ix]) -> (Spot, Vec<Ix>) {
        {
            let mut memo = self.redirects.borrow_mut();
            let looped = self.looped.borrow();
            let cancelled = self.cancelled.borrow();
            if memo.0 != active || memo.1 != *looped || memo.3 != *cancelled {
                *memo = (
                    active.to_vec(),
                    looped.clone(),
                    HashMap::new(),
                    cancelled.clone(),
                );
            } else if let Some(found) = memo.2.get(&x) {
                return found.clone();
            }
        }
        let found = self.find_redirect(x, active);
        self.redirects.borrow_mut().2.insert(x, found.clone());
        found
    }

    fn find_redirect(&self, x: Ix, active: &[Ix]) -> (Spot, Vec<Ix>) {
        // What a copy that's not drawn stood for, to decide with.
        let live = |k: Option<Key>| k.map(|k| self.live(k, active));
        let raw = self.spot(x);
        let mut spot = Spot {
            left: live(raw.left),
            right: live(raw.right),
            before: live(raw.before),
            after: live(raw.after),
        };
        let mut through: Vec<Ix> = Vec::new();
        loop {
            let holder = active
                .iter()
                .copied()
                .filter(|&m| self.concurrent(x, m) && !through.contains(&m))
                .filter(|&m| {
                    // What the move took, or what went with it.
                    let inside = |k: Key| {
                        self.mv(m).set.contains(&k)
                            || self.by(k).is_some_and(|b| {
                                b != x
                                    && self.concurrent(b, m)
                                    && self.redirect(b, active).1.contains(&m)
                            })
                    };
                    spot.before.is_some_and(inside) && spot.after.is_some_and(inside)
                })
                .min_by_key(|&m| self.mv(m).sources.len());
            let Some(m) = holder else {
                if !through.is_empty() {
                    return (spot, through);
                }
                // Not redirected: where its author put it, among what
                // stayed. An origin carried off by a move it didn't see is
                // passed over for that origin's own, and so on.
                let carried = |k: Key| {
                    let by = self.by(k)?;
                    let (_, gone) = self.redirect(by, active);
                    gone.iter().any(|&g| !self.past[x][g]).then_some(by)
                };
                // An origin that's a copy made by a move dropped for a
                // loop stands for what it copied: the copy went where the
                // other move took it, not where its author put it
                // (mamre#27). Loops only, on purpose: see the module docs
                // before widening it to other losing moves.
                let unloop = |k: Option<Key>| {
                    let looped = self.looped.borrow();
                    k.map(|mut k| {
                        while k >= self.copy_base
                            && looped.contains(&self.copies[k - self.copy_base].0)
                        {
                            k = self.copies[k - self.copy_base].1;
                        }
                        k
                    })
                };
                // Typed among an undone move's copies (#30), both its
                // neighbours copies of it: it goes back with the originals,
                // by what the copies stood for. At an edge of them it stays
                // where its author put it, beside the copies, which stay
                // laid out (undrawn).
                let cancelled = self.cancelled.borrow().clone();
                let undone_by = |k: Option<Key>| {
                    k.filter(|&k| k >= self.copy_base)
                        .map(|k| self.copies[k - self.copy_base].0)
                        .filter(|m| cancelled.binary_search(m).is_ok())
                };
                let among = matches!(
                    (undone_by(raw.before), undone_by(raw.after)),
                    (Some(a), Some(b)) if a == b
                );
                let uncancel = |k: Option<Key>| -> Option<Key> {
                    let mut k = k?;
                    while among && undone_by(Some(k)).is_some() {
                        k = self.copies[k - self.copy_base].1;
                    }
                    Some(k)
                };
                let mut left = uncancel(unloop(raw.left));
                while let Some(k) = left {
                    let Some(by) = carried(k) else { break };
                    left = if k >= self.copy_base && k != self.mv(by).first_copy {
                        Some(k - 1)
                    } else {
                        self.spot(by).left
                    };
                }
                let mut right = uncancel(unloop(raw.right));
                while let Some(k) = right {
                    let Some(by) = carried(k) else { break };
                    right = self.spot(by).right;
                }
                return (Spot { left, right, ..raw }, through);
            };
            let mv = self.mv(m);
            let map = |k: Option<Key>| {
                k.and_then(|k| {
                    mv.sources
                        .iter()
                        .position(|&s| s == k)
                        .map(|i| mv.first_copy + i)
                })
            };
            // Among the copies: after the copy of what was left of it,
            // before the copy of what was right (or, if what was next
            // isn't among them, of what showed next).
            let after = map(spot.after);
            spot = Spot {
                left: map(spot.left).or(spot.left),
                right: map(spot.right).or(after),
                before: map(spot.before),
                after,
            };
            through.push(m);
        }
    }

    /// Which moves stand at `at`, by the rules above, leaving out
    /// `dropped` (moves that made a loop), and the losses.
    fn active(&self, at: &[bool], dropped: &HashSet<Ix>, losses: &mut Vec<Loss>) -> Vec<Ix> {
        let cancelled = self.cancelled(at);
        let mut moves: Vec<Ix> = (0..self.n())
            .filter(|&i| at[i] && !dropped.contains(&i) && !cancelled.contains(&i))
            .filter(|&i| match &self.info[i] {
                Info::Move(_) => true,
                Info::Inert => {
                    losses.push(Loss {
                        op: self.events[i],
                        lost_to: None,
                        ranges: Vec::new(),
                    });
                    false
                }
                _ => false,
            })
            .collect();
        moves.sort_by_key(|&m| self.key(m));
        let mut active: Vec<Ix> = Vec::new();
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
                        op: self.events[m],
                        lost_to: Some(self.events[a]),
                        ranges: Vec::new(),
                    });
                    continue 'next;
                }
            }
            if let Some(i) = replaced {
                let a = active.remove(i);
                losses.push(Loss {
                    op: self.events[a],
                    lost_to: Some(self.events[m]),
                    ranges: Vec::new(),
                });
            }
            active.push(m);
        }
        active
    }

    /// The moves cancelled at `at`: each with an undo in force there. An
    /// undo is in force unless an undo of it is; every undo of an undo
    /// comes after it, so one pass from the last settles them all.
    fn cancelled(&self, at: &[bool]) -> HashSet<Ix> {
        let mut undone: HashSet<Ix> = HashSet::new();
        for u in (0..self.n()).rev().filter(|&u| at[u]) {
            if let Info::Undo { target } = self.info[u]
                && !undone.contains(&u)
            {
                undone.insert(target);
            }
        }
        undone
            .into_iter()
            .filter(|&m| matches!(self.info[m], Info::Move(_) | Info::Inert))
            .collect()
    }

    /// The roots deleted at `at`: by a delete there that isn't undone.
    fn deleted(&self, at: &[bool]) -> HashSet<Lv> {
        let undone: HashSet<Ix> = (0..self.n())
            .filter(|&u| at[u])
            .filter_map(|u| match self.info[u] {
                Info::Undelete { target } => Some(target),
                _ => None,
            })
            .collect();
        (0..self.n())
            .filter(|&d| at[d] && !undone.contains(&d))
            .filter_map(|d| match self.info[d] {
                Info::Delete { root } => Some(root),
                _ => None,
            })
            .collect()
    }

    /// The document at `at` (a set of replayed events), from scratch.
    pub(crate) fn rebuild(&self, at: &[bool]) -> View {
        // Moves dropped for making a loop, and what each would have gone
        // with.
        let mut dropped: HashMap<Ix, Ix> = HashMap::new();
        let (active, order, mut losses) = loop {
            let mut losses = Vec::new();
            let skip: HashSet<Ix> = dropped.keys().copied().collect();
            let active = self.active(at, &skip, &mut losses);
            let mut looped: Vec<Ix> = skip.iter().copied().collect();
            looped.sort_unstable();
            *self.looped.borrow_mut() = looped;
            let mut cancelled: Vec<Ix> = self.cancelled(at).into_iter().collect();
            cancelled.sort_unstable();
            *self.cancelled.borrow_mut() = cancelled;
            match self.build(at, &active) {
                Ok(order) => break (active, order, losses),
                Err(cycle) => {
                    let (k, &latest) = cycle
                        .iter()
                        .enumerate()
                        .max_by_key(|(_, m)| self.key(**m))
                        .unwrap();
                    dropped.insert(latest, cycle[(k + 1) % cycle.len()]);
                }
            }
        };
        for (&m, &with) in &dropped {
            losses.push(Loss {
                op: self.events[m],
                lost_to: Some(self.events[with]),
                ranges: Vec::new(),
            });
        }
        let deleted = self.deleted(at);

        // What doesn't show where it lies: what an active move took, and
        // copies off their item's path.
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
            deleted,
            losses,
            active,
        };
        self.more_losses(at, &mut view);
        view
    }

    /// The move record `key` goes with: the innermost active one that
    /// took it.
    fn winner(&self, key: Key, active: &[Ix]) -> Option<Ix> {
        active
            .iter()
            .copied()
            .filter(|&m| self.mv(m).set.contains(&key))
            .min_by_key(|&m| self.mv(m).sources.len())
    }

    /// Whether record `key` is on its item's path: each copy from the
    /// insert down to it made by the move that won what it copied.
    fn on_path(&self, mut key: Key, active: &[Ix]) -> bool {
        while key >= self.copy_base {
            let (m, source) = self.copies[key - self.copy_base];
            if !active.contains(&m) || self.winner(source, active) != Some(m) {
                return false;
            }
            key = source;
        }
        true
    }

    /// Why copy `key` isn't on its path: the move that won what's above it
    /// instead, or replaced its move. `None` if it is.
    fn blocker(&self, mut key: Key, active: &[Ix], losses: &[Loss]) -> Option<Lv> {
        while key >= self.copy_base {
            let (m, source) = self.copies[key - self.copy_base];
            if !active.contains(&m) {
                return losses
                    .iter()
                    .find(|l| l.op == self.events[m])
                    .and_then(|l| l.lost_to);
            }
            match self.winner(source, active) {
                Some(w) if w != m => return Some(self.events[w]),
                _ => key = source,
            }
        }
        None
    }

    /// Lays out the starting document's records, every record in `at` and
    /// every copy the `active` moves make: their keys, in order. Or, if
    /// moves' copies would each go with another's, a loop of them, each
    /// going with the next.
    fn build(&self, at: &[bool], active: &[Ix]) -> Result<Vec<Key>, Vec<Ix>> {
        let mut spots: HashMap<Ix, Spot> = HashMap::new();
        let mut goes_with: HashMap<Ix, Vec<Ix>> = HashMap::new();
        for i in (0..self.n()).filter(|&i| at[i]) {
            let puts = matches!(self.info[i], Info::Insert(_) | Info::Move(_));
            if puts {
                let (spot, through) = self.redirect(i, active);
                spots.insert(i, spot);
                if active.contains(&i) {
                    goes_with.insert(i, through);
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
            let by = self.by(key).expect("a replayed record");
            let spot = spots[&by];
            if key < self.copy_base {
                return (spot.left, spot.right);
            }
            // A run, after the copy before it.
            if key == self.mv(by).first_copy {
                (spot.left, spot.right)
            } else {
                (Some(key - 1), spot.right)
            }
        };
        let mut todo: Vec<Key> = Vec::new();
        for i in (0..self.n()).filter(|i| spots.contains_key(i)) {
            match &self.info[i] {
                Info::Insert(_) => todo.push(self.events[i]),
                Info::Move(mv) => todo.extend(mv.first_copy..mv.first_copy + mv.sources.len()),
                _ => {}
            }
        }
        // The starting document, as if typed in order.
        let mut list: Vec<Rec> = Vec::with_capacity(self.base.len() + todo.len());
        let mut index: HashMap<Key, usize> = HashMap::with_capacity(self.base.len() + todo.len());
        for (i, &k) in self.base.iter().enumerate() {
            let left = i.checked_sub(1).map(|p| self.base[p]);
            list.push(Rec {
                key: k,
                left,
                right: None,
            });
            index.insert(k, i);
        }
        // Each record after its origins.
        for key in todo {
            let mut stack = vec![key];
            while let Some(&k) = stack.last() {
                if index.contains_key(&k) {
                    stack.pop();
                    continue;
                }
                let (left, right) = origins(k);
                let waiting: Vec<Key> = [left, right]
                    .into_iter()
                    .flatten()
                    .filter(|o| !index.contains_key(o))
                    .collect();
                if !waiting.is_empty() {
                    assert!(stack.len() <= 4 * self.n() + 8, "origins in a loop");
                    stack.extend(waiting);
                    continue;
                }
                let by = self.made_by(k);
                let dest = place(
                    &list,
                    left,
                    right,
                    |id| index[&id],
                    |other| tie_break(self.log, by, self.made_by(other)),
                );
                list.insert(
                    dest,
                    Rec {
                        key: k,
                        left,
                        right,
                    },
                );
                for (i, r) in list.iter().enumerate().skip(dest) {
                    index.insert(r.key, i);
                }
                stack.pop();
            }
        }
        Ok(list.into_iter().map(|r| r.key).collect())
    }

    /// The losses that need the drawing: moves whose every item was
    /// deleted by something that didn't see them moved, or that only took
    /// copies off their items' paths; and marks a later mark made without
    /// seeing them overrode. And where the lost things' items are now.
    fn more_losses(&self, at: &[bool], view: &mut View) {
        let active = view.active.clone();
        let undone: HashSet<Ix> = (0..self.n())
            .filter(|&u| at[u])
            .filter_map(|u| match self.info[u] {
                Info::Undelete { target } => Some(target),
                _ => None,
            })
            .collect();
        let mut deletes: HashMap<Lv, Vec<Ix>> = HashMap::new();
        for d in (0..self.n()).filter(|&d| at[d] && !undone.contains(&d)) {
            if let Info::Delete { root } = self.info[d] {
                deletes.entry(root).or_default().push(d);
            }
        }
        for &m in &active {
            let mv = self.mv(m);
            if mv.sources.iter().all(|&k| !self.on_path(k, &active)) {
                let lost_to = self.blocker(mv.sources[0], &active, &view.losses);
                view.losses.push(Loss {
                    op: self.events[m],
                    lost_to,
                    ranges: Vec::new(),
                });
                continue;
            }
            let mut by = Vec::new();
            let all = mv.shown.iter().all(|&k| {
                let killer = deletes
                    .get(&self.root(k))
                    .and_then(|ds| ds.iter().copied().filter(|&d| self.concurrent(d, m)).max());
                by.extend(killer);
                killer.is_some()
            });
            if all && !mv.shown.is_empty() {
                view.losses.push(Loss {
                    op: self.events[m],
                    lost_to: by
                        .into_iter()
                        .max_by_key(|&d| self.key(d))
                        .map(|d| self.events[d]),
                    ranges: Vec::new(),
                });
            }
        }
        for (root, (winner, ks)) in self.marks(at) {
            let _ = root;
            for k in ks {
                if k != winner && !self.past[winner][k] {
                    view.losses.push(Loss {
                        op: self.events[k],
                        lost_to: Some(self.events[winner]),
                        ranges: Vec::new(),
                    });
                }
            }
        }
        let position: HashMap<Lv, usize> = view
            .roots(self)
            .into_iter()
            .enumerate()
            .map(|(i, r)| (r, i))
            .collect();
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
        for loss in &mut view.losses {
            let roots: Vec<Lv> = match &self.info[self.ix[&loss.op]] {
                Info::Move(mv) => mv.shown.iter().map(|&k| self.root(k)).collect(),
                Info::Mark { root, .. } => vec![*root],
                _ => Vec::new(),
            };
            loss.ranges = ranges(&roots);
        }
        view.losses.sort_by_key(|l| l.op);
    }

    /// The marks at `at`, by root: the latest, and all of them.
    fn marks(&self, at: &[bool]) -> BTreeMap<Lv, (Ix, Vec<Ix>)> {
        let mut marks: BTreeMap<Lv, (Ix, Vec<Ix>)> = BTreeMap::new();
        for k in (0..self.n()).filter(|&k| at[k]) {
            if let Info::Mark { root, .. } = self.info[k] {
                let e = marks.entry(root).or_insert((k, Vec::new()));
                if self.key(k) > self.key(e.0) {
                    e.0 = k;
                }
                e.1.push(k);
            }
        }
        marks
    }

    /// The level the latest mark at `at` gives each marked root.
    pub(crate) fn levels(&self, at: &[bool]) -> BTreeMap<Lv, (Lv, u8)> {
        self.marks(at)
            .into_iter()
            .map(|(root, (winner, _))| match self.info[winner] {
                Info::Mark { level, .. } => (root, (self.events[winner], level)),
                _ => unreachable!("a mark"),
            })
            .collect()
    }

    /// The document after event `x`, from the document at its own
    /// version: flatly, as `x` saw everything there.
    fn after(&self, view: &View, x: Ix) -> View {
        let mut v = View {
            order: view.order.clone(),
            drawn: view.drawn.clone(),
            deleted: view.deleted.clone(),
            losses: Vec::new(),
            active: view.active.clone(),
        };
        let place_after = |v: &mut View, left: Option<Key>, keys: &[(Key, bool)]| {
            let o = left.map_or(0, |l| v.order.iter().position(|&k| k == l).unwrap() + 1);
            v.order.splice(o..o, keys.iter().map(|&(k, _)| k));
            let d = left.map_or(0, |l| {
                v.drawn.iter().position(|&(k, _)| k == l).unwrap() + 1
            });
            v.drawn.splice(d..d, keys.iter().copied());
        };
        match &self.info[x] {
            Info::Insert(spot) => place_after(&mut v, spot.left, &[(self.events[x], true)]),
            Info::Delete { root } => {
                v.deleted.insert(*root);
                for d in &mut v.drawn {
                    if self.root(d.0) == *root {
                        d.1 = false;
                    }
                }
            }
            Info::Undelete { target } => {
                let Info::Delete { root } = self.info[*target] else {
                    unreachable!("an undelete undoes a delete")
                };
                let mut at = self.versions[x].clone();
                at[x] = true;
                if !self.deleted(&at).contains(&root) {
                    v.deleted.remove(&root);
                    for d in &mut v.drawn {
                        if self.root(d.0) == root {
                            d.1 = true;
                        }
                    }
                }
            }
            Info::Mark { .. } | Info::Inert => {}
            Info::Undo { .. } => {
                // What it cancels or brings back reaches everything the
                // move did: drawn again from scratch.
                let mut at = self.versions[x].clone();
                at[x] = true;
                return self.rebuild(&at);
            }
            Info::Move(mv) => {
                v.active.push(x);
                let taken: HashSet<Key> = mv.sources.iter().copied().collect();
                let shows: HashMap<Key, bool> = v.drawn.iter().copied().collect();
                v.drawn.retain(|(k, _)| !taken.contains(k));
                let copies: Vec<(Key, bool)> = mv
                    .sources
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (mv.first_copy + i, shows[s]))
                    .collect();
                place_after(&mut v, mv.to.left, &copies);
            }
        }
        v
    }
}

/// A replay of `events` (sorted, every one of them after a version every
/// one of them saw) from the document there, whose items are `base`, by
/// root. With `flat`, each event's
/// version is built from the one before where it can; without, from
/// scratch every time, as the reference does.
pub(crate) fn replay<'a, T: Clone>(
    log: &'a OpLog<T>,
    base: Vec<Lv>,
    events: Vec<Lv>,
    flat: bool,
) -> Result<Replay<'a, T>, Unsupported> {
    replay_checked(log, base, events, flat, false)
}

/// As [`replay`]; with `check`, each flat step is checked against the
/// document rebuilt from scratch.
fn replay_checked<'a, T: Clone>(
    log: &'a OpLog<T>,
    base: Vec<Lv>,
    events: Vec<Lv>,
    flat: bool,
    check: bool,
) -> Result<Replay<'a, T>, Unsupported> {
    let n = events.len();
    let ix: HashMap<Lv, Ix> = events.iter().enumerate().map(|(i, &lv)| (lv, i)).collect();
    let mut r = Replay {
        log,
        base,
        events,
        ix,
        info: Vec::with_capacity(n),
        past: Vec::with_capacity(n),
        copy_base: log.len(),
        copies: Vec::new(),
        versions: Vec::with_capacity(n),
        redirects: Default::default(),
        looped: Default::default(),
        cancelled: Default::default(),
    };
    // The document after each event, while a later one still builds on it.
    let mut after: HashMap<Ix, View> = HashMap::new();
    let mut uses: Vec<usize> = vec![0; n];
    let parents_of = |lv: Lv, ix: &HashMap<Lv, Ix>| -> Vec<Ix> {
        let mut p: Vec<Ix> = log
            .event(lv)
            .parents
            .iter()
            .filter_map(|p| ix.get(p).copied())
            .collect();
        p.sort_unstable();
        p
    };
    if flat {
        for i in 0..n {
            if let [p] = parents_of(r.events[i], &r.ix)[..] {
                uses[p] += 1;
            }
        }
    }
    let empty = View {
        order: r.base.clone(),
        drawn: r.base.iter().map(|&k| (k, true)).collect(),
        deleted: HashSet::new(),
        losses: Vec::new(),
        active: Vec::new(),
    };
    for i in 0..n {
        let lv = r.events[i];
        let event = log.event(lv);
        let parents = parents_of(lv, &r.ix);
        let mut at = vec![false; n];
        for &p in &parents {
            at[p] = true;
            for (j, &seen) in r.past[p].iter().enumerate() {
                if seen {
                    at[j] = true;
                }
            }
        }
        // Its own version, drawn from what came before it.
        let view = match (&parents[..], flat) {
            ([], _) => empty.clone(),
            ([p], true) => {
                uses[*p] -= 1;
                if uses[*p] == 0 {
                    after.remove(p).expect("kept for it")
                } else {
                    after[p].clone()
                }
            }
            _ => r.rebuild(&at),
        };
        if check && flat && parents.len() == 1 {
            let from_scratch = r.rebuild(&at);
            assert_eq!(view.order, from_scratch.order, "laid out flatly at {lv}");
            assert_eq!(view.drawn, from_scratch.drawn, "drawn flatly at {lv}");
        }
        r.past.push(at.clone());
        r.versions.push(at);
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
            &Op::Mark { pos, level } => Info::Mark {
                root: r.root(item_at(pos)),
                level,
            },
            Op::Undelete { target } => match r.ix.get(target) {
                Some(&t) => {
                    assert!(r.past[i][t], "undelete {lv} undoes a delete it includes");
                    Info::Undelete { target: t }
                }
                // A delete from before the start: where its item lay, the
                // start doesn't say.
                None => return Err(Unsupported),
            },
            Op::Undo { target } => match r.ix.get(target) {
                Some(&t) => {
                    assert!(r.past[i][t], "undo {lv} undoes an event it includes");
                    Info::Undo { target: t }
                }
                // A move from before the start: its copies' sources, the
                // start doesn't say.
                None => return Err(Unsupported),
            },
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
                    let first_copy = r.copy_base + r.copies.len();
                    r.copies.extend(sources.iter().map(|&s| (i, s)));
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
        if flat && uses[i] > 0 {
            let next = r.after(&view, i);
            after.insert(i, next);
        }
    }
    Ok(r)
}

impl<T: Clone> Replay<'_, T> {
    /// The set of replayed events that are in `version`.
    pub(crate) fn at(&self, version: &[Lv]) -> Vec<bool> {
        self.events
            .iter()
            .map(|&lv| self.log.contains(version, lv))
            .collect()
    }

    /// Every replayed event.
    pub(crate) fn all(&self) -> Vec<bool> {
        vec![true; self.n()]
    }

    /// Which delete removed each item gone at `at`, for those the window
    /// deleted: (delete, root).
    // For the window's debug check against runs.rs.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pub(crate) fn deletes_of(&self, at: &[bool]) -> HashMap<Lv, Lv> {
        let deleted = self.deleted(at);
        let mut out = HashMap::new();
        for d in (0..self.n()).filter(|&d| at[d]) {
            if let Info::Delete { root } = self.info[d]
                && deleted.contains(&root)
            {
                out.insert(root, self.events[d]);
            }
        }
        out
    }

    fn edge_ties(&self) -> bool {
        (0..self.n()).any(|x| {
            let spot = match self.info[x] {
                Info::Insert(_) | Info::Move(_) => self.spot(x),
                _ => return false,
            };
            (0..self.n()).any(|m| {
                let Info::Move(mv) = &self.info[m] else {
                    return false;
                };
                if !self.concurrent(x, m) {
                    return false;
                }
                let inside = |k: Option<Key>| k.is_some_and(|k| mv.set.contains(&k));
                // A neighbour that's a copy of something the move took.
                let carried = |k: Option<Key>| {
                    let Some(mut k) = k else { return false };
                    while k >= self.copy_base {
                        k = self.copies[k - self.copy_base].1;
                        if mv.set.contains(&k) {
                            return true;
                        }
                    }
                    false
                };
                inside(spot.before) != inside(spot.after)
                    || carried(spot.before)
                    || carried(spot.after)
                    || mv
                        .sources
                        .iter()
                        .any(|&k| self.concurrent_with(self.by(k), x))
            })
        })
    }
}

/// The whole log, as the reference replays it.
fn reference<T: Clone>(log: &OpLog<T>) -> Replay<'_, T> {
    replay(log, Vec::new(), (0..log.len()).collect(), false)
        .expect("from nothing, every delete is replayed")
}

/// The document at `version`.
pub fn checkout<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> Vec<T> {
    let r = reference(log);
    r.rebuild(&r.at(version))
        .roots(&r)
        .into_iter()
        .map(|root| match log.event(root).op {
            Op::Insert { item, .. } => item,
            _ => unreachable!("a root is an insert"),
        })
        .collect()
}

/// Who each item at `version` is, in order: the insert it descends from.
pub fn identities<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> Vec<Lv> {
    let r = reference(log);
    r.rebuild(&r.at(version)).roots(&r)
}

/// Each item in `version` with a level that isn't 0 there, deleted or not.
pub fn levels<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> BTreeMap<Lv, u8> {
    let r = reference(log);
    r.levels(&r.at(version))
        .into_iter()
        .filter(|(_, (_, level))| *level > 0)
        .map(|(root, (_, level))| (root, level))
        .collect()
}

/// Every move and mark discarded at `version`, by op.
pub fn losses<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> Vec<Loss> {
    let r = reference(log);
    r.rebuild(&r.at(version)).losses
}

/// Whether the history has an edge tie (see the docs above): where this
/// reference and the portals may differ.
#[doc(hidden)]
pub fn edge_ties<T: Clone>(log: &OpLog<T>) -> bool {
    reference(log).edge_ties()
}

/// The moves inert from the start: of nothing, or to where they are.
pub fn inert<T: Clone>(log: &OpLog<T>) -> Vec<Lv> {
    let r = reference(log);
    (0..log.len())
        .filter(|&lv| matches!(r.info[lv], Info::Inert))
        .collect()
}

/// The items at `version`, by root, replayed from `start` flatly (every
/// event after `start` must include it), each flat step checked against a
/// rebuild: for testing the window against the reference.
#[doc(hidden)]
pub fn from_start<T: Clone>(log: &OpLog<T>, start: &[Lv], version: &[Lv]) -> Option<Vec<Lv>> {
    let base = identities(log, start);
    let events: Vec<Lv> = (0..log.len())
        .filter(|&lv| log.contains(version, lv) && !log.contains(start, lv))
        .collect();
    assert!(
        events
            .iter()
            .all(|&lv| start.iter().all(|&s| log.contains(&[lv], s))),
        "every event after the start includes it"
    );
    let r = replay_checked(log, base, events, true, true).ok()?;
    Some(r.rebuild(&r.all()).roots(&r))
}
