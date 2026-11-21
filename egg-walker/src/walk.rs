//! The walk: turning events, each at a position in its own version of the
//! document, into plain changes that bring one document up to date.
//!
//! Events are replayed into a list of records, one per inserted item,
//! deleted ones included. Each record has two states:
//!
//! - in the *prepare* version: the event being applied's parents, which is
//!   what its position means. Moving it from one event's parents to the
//!   next's *retreats* events not in the new parents and *advances* ones
//!   newly in them.
//! - in the *effect* version: everything replayed so far, which is what the
//!   output positions mean.
//!
//! An insert's position, read in the prepare version, gives the items
//! either side of it (its origins); the shared ordering rule places it
//! among concurrent inserts; its effect position is where that lands now.
//!
//! A merge only replays what it has to. Its *base* is the latest version
//! that every event it replays includes: nothing in the base is ever
//! retreated, so the document at the base needs no records, only a
//! placeholder run of anonymous items standing for it, split wherever an
//! edit touches it. The placeholder is longer than any real document, so
//! the base's length never needs knowing; edits only ever touch its front.
//!
//! Records are still in a plain list, and finding the base still reads the
//! whole graph; both get faster next, checked against this.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::oplog::{Lv, Op, OpLog};
use crate::order::{Placed, place};

/// Ids at or above this name items of the base document, by position.
const BASE_ITEM: usize = 1 << 62;
/// How long the placeholder is: more than any real document.
const UNDERWATER: usize = 1 << 61;

/// A plain change to a document: what the walk produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change<T> {
    Insert { pos: usize, item: T },
    Delete { pos: usize },
}

/// A record's state in the prepare version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prepare {
    /// Its insert isn't in the version: concurrent, or later.
    NotYet,
    Inserted,
    /// Deleted by this many deletes in the version.
    Deleted(u32),
}

/// One inserted item, or a run of the base document's items.
#[derive(Clone)]
struct Record {
    /// The insert's local version, or `BASE_ITEM` plus the first base
    /// item's position.
    id: usize,
    /// 1, except for a run of base items.
    len: usize,
    left: Option<usize>,
    right: Option<usize>,
    prepare: Prepare,
    /// Deleted in the effect version: by any delete replayed so far.
    gone: bool,
}

impl Placed for Record {
    fn id(&self) -> Lv {
        self.id
    }
    fn left(&self) -> Option<Lv> {
        self.left
    }
    fn right(&self) -> Option<Lv> {
        self.right
    }
}

struct Walk<'a, T> {
    log: &'a OpLog<T>,
    records: Vec<Record>,
    /// The prepare version, as a set.
    prepared: Vec<bool>,
    /// Which record each delete replayed so far removed.
    targets: HashMap<Lv, usize>,
    /// How many events were replayed: what a merge cost.
    replayed: usize,
}

impl<T: Clone> OpLog<T> {
    /// The changes that bring a document holding version `from` up to
    /// everything in `from` and `to`, in the order to apply them.
    pub fn changes(&self, from: &[Lv], to: &[Lv]) -> Vec<Change<T>> {
        self.walk(from, to).0
    }

    /// The document at `version`, from nothing.
    pub fn checkout(&self, version: &[Lv]) -> Vec<T> {
        let mut doc = Vec::new();
        for change in self.changes(&[], version) {
            change.apply(&mut doc);
        }
        doc
    }

    /// The changes, and how many events the walk replayed to find them.
    pub(crate) fn walk(&self, from: &[Lv], to: &[Lv]) -> (Vec<Change<T>>, usize) {
        let have = self.closure(from);
        let mut want = self.closure(to);
        for (w, &h) in want.iter_mut().zip(&have) {
            *w |= h;
        }
        if want == have {
            return (Vec::new(), 0);
        }
        let base = self.base(&have, &want);
        let mut walk = Walk {
            log: self,
            records: vec![Record {
                id: BASE_ITEM,
                len: UNDERWATER,
                left: None,
                right: None,
                prepare: Prepare::Inserted,
                gone: false,
            }],
            prepared: base.clone(),
            targets: HashMap::new(),
            replayed: 0,
        };
        // First what the document already holds past the base, to set up
        // its state, then what's new. Parents come first either way:
        // `from` holds all its events' parents.
        for lv in (0..self.len()).filter(|&lv| have[lv] && !base[lv]) {
            walk.step(lv, None);
        }
        let mut out = Vec::new();
        for lv in (0..self.len()).filter(|&lv| want[lv] && !have[lv]) {
            walk.step(lv, Some(&mut out));
        }
        (out, walk.replayed)
    }

    /// The latest version inside `have` that every event to be replayed
    /// includes: the new events, and whatever of `have` isn't in the base.
    /// Shrinking the base can bring more of `have` into the replay, whose
    /// parents may shrink it further, so it's found by shrinking until it
    /// holds.
    fn base(&self, have: &[bool], want: &[bool]) -> Vec<bool> {
        let mut base = have.to_vec();
        loop {
            let mut next = base.clone();
            for lv in (0..self.len()).filter(|&lv| want[lv] && !base[lv]) {
                let parents = self.closure(&self.events()[lv].parents);
                for (b, p) in next.iter_mut().zip(parents) {
                    *b &= p;
                }
            }
            if next == base {
                return base;
            }
            base = next;
        }
    }
}

impl<T> Change<T> {
    pub fn apply(self, doc: &mut Vec<T>) {
        match self {
            Change::Insert { pos, item } => doc.insert(pos, item),
            Change::Delete { pos } => {
                doc.remove(pos);
            }
        }
    }
}

impl<T: Clone> Walk<'_, T> {
    fn step(&mut self, lv: Lv, out: Option<&mut Vec<Change<T>>>) {
        self.replayed += 1;
        let event = &self.log.events()[lv];
        let parents = self.log.closure(&event.parents);
        // Move the prepare version to this event's parents: take back what
        // it shouldn't hold, latest first, then add what it's missing,
        // earliest first, so a delete is never undone before its insert.
        // The base is in every replayed event's parents, so it never moves.
        for x in (0..self.prepared.len()).rev() {
            if self.prepared[x] && !parents[x] {
                self.retreat(x);
                self.prepared[x] = false;
            }
        }
        for (x, &wanted) in parents.iter().enumerate() {
            if wanted && !self.prepared[x] {
                self.advance(x);
                self.prepared[x] = true;
            }
        }
        match &event.op {
            Op::Insert { pos, item } => self.insert(lv, *pos, item.clone(), out),
            Op::Delete { pos } => self.delete(lv, *pos, out),
        }
        self.prepared[lv] = true;
    }

    fn index_of(&self, id: usize) -> usize {
        self.records
            .iter()
            .position(|r| r.id == id)
            .expect("every event's record is in the list")
    }

    fn retreat(&mut self, x: Lv) {
        let i = match self.log.events()[x].op {
            Op::Insert { .. } => self.index_of(x),
            Op::Delete { .. } => self.index_of(self.targets[&x]),
        };
        let record = &mut self.records[i];
        record.prepare = match (self.log.events()[x].op.is_insert(), record.prepare) {
            (true, Prepare::Inserted) => Prepare::NotYet,
            (false, Prepare::Deleted(1)) => Prepare::Inserted,
            (false, Prepare::Deleted(n)) => Prepare::Deleted(n - 1),
            (_, state) => unreachable!("retreating {x} from {state:?}"),
        };
    }

    fn advance(&mut self, x: Lv) {
        let i = match self.log.events()[x].op {
            Op::Insert { .. } => self.index_of(x),
            Op::Delete { .. } => self.index_of(self.targets[&x]),
        };
        let record = &mut self.records[i];
        record.prepare = match (self.log.events()[x].op.is_insert(), record.prepare) {
            (true, Prepare::NotYet) => Prepare::Inserted,
            (false, Prepare::Inserted) => Prepare::Deleted(1),
            (false, Prepare::Deleted(n)) => Prepare::Deleted(n + 1),
            (_, state) => unreachable!("advancing {x} from {state:?}"),
        };
    }

    /// The index of the record holding the `pos`th item visible in the
    /// prepare version, split out into a record of its own if it's inside
    /// a run of base items. The placeholder never runs out, so there
    /// always is one.
    fn prepared_at(&mut self, pos: usize) -> usize {
        let mut seen = 0;
        for i in 0..self.records.len() {
            let r = &self.records[i];
            if r.prepare == Prepare::Inserted {
                if pos < seen + r.len {
                    return self.isolate(i, pos - seen);
                }
                seen += r.len;
            }
        }
        unreachable!("the placeholder is longer than any document")
    }

    /// Splits item `offset` of record `i` into a record of its own, so
    /// origins and deletes can name it; returns its index.
    fn isolate(&mut self, i: usize, offset: usize) -> usize {
        let i = if offset > 0 {
            self.split(i, offset);
            i + 1
        } else {
            i
        };
        if self.records[i].len > 1 {
            self.split(i, 1);
        }
        i
    }

    /// Splits a run of base items in two at `at`.
    fn split(&mut self, i: usize, at: usize) {
        let r = &mut self.records[i];
        debug_assert!(r.id >= BASE_ITEM && at < r.len, "only base runs split");
        let mut rest = r.clone();
        r.len = at;
        rest.id += at;
        rest.len -= at;
        self.records.insert(i + 1, rest);
    }

    /// How many items the effect version shows before record `i`.
    fn effect_pos(&self, i: usize) -> usize {
        self.records[..i]
            .iter()
            .filter(|r| !r.gone)
            .map(|r| r.len)
            .sum()
    }

    fn insert(&mut self, lv: Lv, pos: usize, item: T, out: Option<&mut Vec<Change<T>>>) {
        // The origins, as the insert's author saw them. An insert sticks to
        // what's before it: its left origin is the visible item just before
        // the position (none at the start), and its right is whatever came
        // next in that version, deleted or not. Concurrent items, not in
        // the version, don't count. (diamond-types' convention, and the
        // intent: "at the start" stays at the start, wherever tombstones
        // lie.)
        let (left, from) = match pos.checked_sub(1) {
            Some(before) => {
                let i = self.prepared_at(before);
                (Some(self.records[i].id), i + 1)
            }
            None => (None, 0),
        };
        // The right origin is the first item of its record, so it needs no
        // splitting out: a run that's split later keeps its first item at
        // its front, so the id still names where a record starts.
        let right = self.records[from..]
            .iter()
            .find(|r| r.prepare != Prepare::NotYet)
            .map(|r| r.id);

        let index: HashMap<usize, usize> = self
            .records
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id, i))
            .collect();
        let me = self.log.order_key(lv);
        let dest = place(
            &self.records,
            left,
            right,
            |id| index[&id],
            |other| me.cmp(&self.log.order_key(other)),
        );
        self.records.insert(
            dest,
            Record {
                id: lv,
                len: 1,
                left,
                right,
                prepare: Prepare::Inserted,
                gone: false,
            },
        );
        if let Some(out) = out {
            out.push(Change::Insert {
                pos: self.effect_pos(dest),
                item,
            });
        }
    }

    fn delete(&mut self, lv: Lv, pos: usize, out: Option<&mut Vec<Change<T>>>) {
        let at = self.prepared_at(pos);
        self.records[at].prepare = Prepare::Deleted(1);
        self.targets.insert(lv, self.records[at].id);
        // Already deleted in the effect version by a concurrent delete:
        // nothing more to do to the document.
        if !self.records[at].gone {
            if let Some(out) = out {
                out.push(Change::Delete {
                    pos: self.effect_pos(at),
                });
            }
            self.records[at].gone = true;
        }
    }
}

impl<T> Op<T> {
    fn is_insert(&self) -> bool {
        matches!(self, Op::Insert { .. })
    }
}

/// Orders two ids by the log's tie-break, for the reference to share.
pub(crate) fn tie_break<T>(log: &OpLog<T>, a: Lv, b: Lv) -> Ordering {
    log.order_key(a).cmp(&log.order_key(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A long history everyone shares, then two concurrent edits: merging
    /// them replays only the concurrent work, not the history.
    #[test]
    fn a_merge_replays_only_what_is_concurrent() {
        let mut log: OpLog<u32> = OpLog::new();
        let alice = log.agent("alice");
        let bob = log.agent("bob");
        let mut v = Vec::new();
        for i in 0..200 {
            v = log.add_insert(alice, &v, i as usize, [i]);
        }
        let a = log.add_insert(alice, &v, 50, [1000, 1001]);
        let b = log.add_delete(bob, &v, 10..13);
        let (changes, replayed) = log.walk(&a, &b);
        // Bob's three deletes are new, and Alice's two inserts are
        // concurrent with them, so replayed too; the 200 before are the base.
        assert_eq!(replayed, 5);
        let mut doc = log.checkout(&a);
        for change in changes {
            change.apply(&mut doc);
        }
        assert_eq!(doc, log.checkout(&log.union(&a, &b)));
        assert_eq!(doc.len(), 199);
    }

    /// Concurrent work on both sides: the base is the fork point, and both
    /// sides' events are replayed, but not the history before it.
    #[test]
    fn a_merge_of_two_branches_starts_at_the_fork() {
        let mut log: OpLog<u32> = OpLog::new();
        let alice = log.agent("alice");
        let bob = log.agent("bob");
        let mut v = Vec::new();
        for i in 0..200 {
            v = log.add_insert(alice, &v, i as usize, [i]);
        }
        let mut a = v.clone();
        let mut b = v.clone();
        for k in 0..5 {
            a = log.add_insert(alice, &a, k * 3, [500 + k as u32]);
            b = log.add_delete(bob, &b, 100..101);
        }
        let (changes, replayed) = log.walk(&a, &b);
        assert_eq!(replayed, 10, "five events each side, none before the fork");
        let mut doc = log.checkout(&a);
        for change in changes {
            change.apply(&mut doc);
        }
        assert_eq!(doc, log.checkout(&log.union(&a, &b)));
    }
}
