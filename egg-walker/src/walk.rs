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
//! This first walk replays the whole history every time and keeps records
//! in a plain list: it's here to be right, and to be the oracle a faster
//! one is checked against.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::oplog::{Lv, Op, OpLog};
use crate::order::{Placed, place};

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

struct Record {
    id: Lv,
    left: Option<Lv>,
    right: Option<Lv>,
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
    targets: HashMap<Lv, Lv>,
}

impl<T: Clone> OpLog<T> {
    /// The changes that bring a document holding version `from` up to
    /// everything in `from` and `to`, in the order to apply them.
    pub fn changes(&self, from: &[Lv], to: &[Lv]) -> Vec<Change<T>> {
        let have = self.closure(from);
        let want = self.closure(to);
        let mut walk = Walk {
            log: self,
            records: Vec::new(),
            prepared: vec![false; self.len()],
            targets: HashMap::new(),
        };
        // First what the document already holds, to set up its state, then
        // what's new. Parents always come first either way: `from` holds
        // all its events' parents.
        for lv in (0..self.len()).filter(|&lv| have[lv]) {
            walk.step(lv, None);
        }
        let mut out = Vec::new();
        for lv in (0..self.len()).filter(|&lv| want[lv] && !have[lv]) {
            walk.step(lv, Some(&mut out));
        }
        out
    }

    /// The document at `version`, from nothing.
    pub fn checkout(&self, version: &[Lv]) -> Vec<T> {
        let mut doc = Vec::new();
        for change in self.changes(&[], version) {
            change.apply(&mut doc);
        }
        doc
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
        let event = &self.log.events()[lv];
        let parents = self.log.closure(&event.parents);
        // Move the prepare version to this event's parents: take back what
        // it shouldn't hold, latest first, then add what it's missing,
        // earliest first, so a delete is never undone before its insert.
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

    fn index_of(&self, id: Lv) -> usize {
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

    /// The index of the `pos`th record visible in the prepare version, or
    /// the end if `pos` is just past the last.
    fn prepared_at(&self, pos: usize) -> usize {
        let mut seen = 0;
        for (i, r) in self.records.iter().enumerate() {
            if r.prepare == Prepare::Inserted {
                if seen == pos {
                    return i;
                }
                seen += 1;
            }
        }
        assert_eq!(seen, pos, "position {pos} is past the end of its version");
        self.records.len()
    }

    /// How many records the effect version shows before index `i`.
    fn effect_pos(&self, i: usize) -> usize {
        self.records[..i].iter().filter(|r| !r.gone).count()
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
                assert!(
                    i < self.records.len(),
                    "insert {lv} at {pos} is past the end"
                );
                (Some(self.records[i].id), i + 1)
            }
            None => (None, 0),
        };
        let right = self.records[from..]
            .iter()
            .find(|r| r.prepare != Prepare::NotYet)
            .map(|r| r.id);

        let index: HashMap<Lv, usize> = self
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
        assert!(
            at < self.records.len(),
            "delete {lv} at {pos} is past the end"
        );
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
