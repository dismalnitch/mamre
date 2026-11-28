//! Following moved text (#68): what's watched on a character goes with it
//! when a move carries it somewhere else.
//!
//! A move is a delete and an insert in one step, and a
//! [`CameFrom`](crate::Aside::CameFrom) record pairs each copy with the
//! character it carries on. The merge never reads that record; this does.
//! A watched character that's deleted follows its copies: if exactly one
//! of them shows (or one of theirs, for text moved again), that's where it
//! is now. Every replica works it out from the log alone, so none can
//! disagree, and it needs nothing written per anchor: an anchor made in
//! the range at the same moment as the move follows too.
//!
//! Undoing the move brings the originals back as themselves (undo revives
//! deleted items) and deletes the copies, so what's watched is found on
//! the originals again, exactly. A paste is a copy without the delete: the
//! originals still show, so nothing follows it.
//!
//! To know where a copy is, it's watched too, for as long as anything
//! watched has it among its copies. The record of copies is rebuilt from
//! the log after a load, as ranges are.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::anchors::Anchors;
use crate::ids::Identities;
use crate::oplog::{Aside, Lv, Op, OpLog};

/// Where a watched item is, following moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Follow {
    /// It shows, at this position.
    Here(usize),
    /// It's deleted, and a move carried it here: its one copy that shows.
    Moved(usize),
    /// It's deleted, and more than one of its copies shows (two writers
    /// moved it at once, say): these positions, in order. It goes with
    /// none of them until it's put somewhere.
    Ambiguous(Vec<usize>),
    /// It's deleted, and no copy of it shows.
    Gone,
}

/// Moves' copies, and why each item is watched.
#[derive(Clone, Default)]
pub(crate) struct Moves {
    /// Each moved item's copies, in the order they were made.
    copies: HashMap<Lv, Vec<Lv>>,
    /// Each copy's source.
    sources: HashMap<Lv, Lv>,
    /// Items watched for their own sake.
    direct: HashSet<Lv>,
    /// How many reasons each watched item has: being watched for its own
    /// sake, and each watched item it's a copy of.
    reasons: HashMap<Lv, usize>,
    /// Read from a file: rebuilt from the log before it's next used.
    pub(crate) stale: bool,
}

impl Moves {
    pub(crate) fn stale() -> Self {
        Moves {
            stale: true,
            ..Moves::default()
        }
    }

    /// The source copy `item` came from, if a move made it.
    pub(crate) fn source(&self, item: Lv) -> Option<Lv> {
        self.sources.get(&item).copied()
    }

    /// Watches `item`, the one at `pos` now, for its own sake.
    pub(crate) fn watch(&mut self, anchors: &mut Anchors, ids: &Identities, pos: usize, item: Lv) {
        if self.direct.insert(item) {
            self.add_reason(anchors, ids, item, pos);
        }
    }

    /// Stops watching `item` for its own sake.
    pub(crate) fn unwatch(&mut self, anchors: &mut Anchors, item: Lv) {
        if self.direct.remove(&item) {
            self.remove_reason(anchors, item);
        }
    }

    /// One more reason to watch `item`, at `pos`, and so its copies.
    fn add_reason(&mut self, anchors: &mut Anchors, ids: &Identities, item: Lv, pos: usize) {
        let n = self.reasons.entry(item).or_insert(0);
        *n += 1;
        if *n > 1 {
            return;
        }
        anchors.watch(pos, item);
        // Its copies, theirs, and so on: newly watched, placed in one pass
        // (text that was never moved has none, and costs nothing).
        let mut new = BTreeSet::new();
        let mut todo: Vec<Lv> = self.copies.get(&item).into_iter().flatten().copied().collect();
        while let Some(x) = todo.pop() {
            let n = self.reasons.entry(x).or_insert(0);
            *n += 1;
            if *n == 1 {
                new.insert(x);
                todo.extend(self.copies.get(&x).into_iter().flatten());
            }
        }
        place(anchors, ids, &new);
    }

    fn remove_reason(&mut self, anchors: &mut Anchors, item: Lv) {
        let mut todo = vec![item];
        while let Some(x) = todo.pop() {
            let n = self
                .reasons
                .get_mut(&x)
                .expect("an item that's watched has a reason");
            *n -= 1;
            if *n == 0 {
                self.reasons.remove(&x);
                anchors.unwatch(x);
                todo.extend(self.copies.get(&x).into_iter().flatten());
            }
        }
    }

    /// Takes in the moves among `events` (asides, in order): their copies
    /// are watched wherever their sources are.
    pub(crate) fn take_in<T: Clone>(
        &mut self,
        log: &OpLog<T>,
        events: &[Lv],
        anchors: &mut Anchors,
        ids: &Identities,
    ) {
        let mut new = BTreeSet::new();
        for &lv in events {
            let Op::Aside(Aside::CameFrom { copies, sources }) = log.event(lv).op else {
                continue;
            };
            for (&c, &s) in copies.iter().zip(&sources) {
                self.copies.entry(s).or_default().push(c);
                self.sources.insert(c, s);
                if self.reasons.contains_key(&s) {
                    // Watched through its source: one reason, from it.
                    let n = self.reasons.entry(c).or_insert(0);
                    *n += 1;
                    if *n == 1 {
                        new.insert(c);
                    }
                }
            }
        }
        place(anchors, ids, &new);
    }

    /// Rebuilds the copies from every move in the log up to `version`.
    /// Nothing's watched for its own sake after a load.
    pub(crate) fn rebuild<T: Clone>(&mut self, log: &OpLog<T>, version: &[Lv]) {
        *self = Moves::default();
        let holds = log.closure(version);
        for lv in (0..log.len()).filter(|&lv| holds[lv]) {
            if let Op::Aside(Aside::CameFrom { copies, sources }) = log.event(lv).op {
                for (c, s) in copies.into_iter().zip(sources) {
                    self.copies.entry(s).or_default().push(c);
                    self.sources.insert(c, s);
                }
            }
        }
    }

    /// Where watched `item` is, following moves.
    pub(crate) fn follow(&self, anchors: &Anchors, item: Lv) -> Follow {
        if let Some(pos) = anchors.position(item) {
            return Follow::Here(pos);
        }
        let mut found = Vec::new();
        let mut todo: Vec<Lv> = self.copies.get(&item).into_iter().flatten().copied().collect();
        while let Some(x) = todo.pop() {
            match anchors.position(x) {
                Some(pos) => found.push(pos),
                None => todo.extend(self.copies.get(&x).into_iter().flatten()),
            }
        }
        found.sort_unstable();
        found.dedup();
        match found.len() {
            0 => Follow::Gone,
            1 => Follow::Moved(found[0]),
            _ => Follow::Ambiguous(found),
        }
    }
}

/// Watches each of `items`: where it shows, or deleted (it comes back where
/// an undo or redo puts it).
fn place(anchors: &mut Anchors, ids: &Identities, items: &BTreeSet<Lv>) {
    if items.is_empty() {
        return;
    }
    let shown = ids.positions(items);
    let mut left: BTreeSet<Lv> = items.clone();
    for (item, pos) in shown {
        anchors.watch(pos, item);
        left.remove(&item);
    }
    for item in left {
        anchors.watch_deleted(item);
    }
}
