//! The walk: turning events, each at a position in its own version of the
//! document, into plain changes that bring one document up to date.
//!
//! Events are replayed into records, one per inserted item, deleted ones
//! included. Each record has two states:
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
//! A merge only replays what it has to: what happened since the two
//! versions met, found by walking back from both. Its *base* is a version
//! every replayed event includes whole, so nothing in the base is ever
//! retreated; branches neither version holds don't count. The document at the base needs no records, only a
//! placeholder run of anonymous items standing for it, split wherever an
//! edit touches it. The placeholder is longer than any real document, so
//! the base's length never needs knowing; edits only ever touch its front.
//!
//! Records live in a mamre `SumTree`, summed by how many items each version
//! shows, so finding the nth visible item and an item's effect position are
//! seeks. Every record has an order key (see `key.rs`), so a record is
//! found by key, and the ordering rule compares origins by key. A record
//! is a run: items inserted one after another (or the base document's),
//! split wherever an edit, an origin or a state change lands inside it, so
//! typing is a handful of records, not one per item. The prepare
//! version is kept as its latest events and moved by walking the graph
//! back only as far as the two versions differ: for typing, nothing.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use mamre::cursor::Bias;
use mamre::{Item, SeekTarget, SumTree, Summary};

use crate::key::Key;
use crate::oplog::{Lv, Op, OpLog};

/// Ids at or above this name items of the base document, by position.
pub(crate) const BASE_ITEM: usize = 1 << 62;
/// How long the placeholder is: more than any real document.
pub(crate) const UNDERWATER: usize = 1 << 61;

/// A plain change to a document: what the walk produces, in the order to
/// apply them. A run of typing, or of deleting, comes out as one change.
#[derive(Debug, PartialEq, Eq)]
pub enum Change<'a, T> {
    /// `items` go in at `pos`. They're the items of events
    /// `first..first + items.len()`, borrowed from the log. An item always
    /// comes back as its own event, so anything keyed to it, an anchor, say,
    /// finds it again: undoing a delete will revive the old events rather
    /// than make new ones.
    Insert {
        pos: usize,
        items: &'a [T],
        first: Lv,
    },
    /// The items in `range` go.
    Delete { range: Range<usize> },
}

impl<T> Clone for Change<'_, T> {
    fn clone(&self) -> Self {
        match self {
            Change::Insert { pos, items, first } => Change::Insert {
                pos: *pos,
                items,
                first: *first,
            },
            Change::Delete { range } => Change::Delete {
                range: range.clone(),
            },
        }
    }
}

impl<T: Clone> Change<'_, T> {
    pub fn apply(&self, doc: &mut Vec<T>) {
        match self {
            Change::Insert { pos, items, .. } => {
                doc.splice(*pos..*pos, items.iter().cloned());
            }
            Change::Delete { range } => {
                doc.drain(range.clone());
            }
        }
    }
}

/// The walk's output as it's made: each change joins the one before it
/// when it carries on its run.
pub(crate) struct Changes<'a, T> {
    log: &'a OpLog<T>,
    pub(crate) out: Vec<Change<'a, T>>,
}

impl<'a, T: Clone> Changes<'a, T> {
    pub(crate) fn new(log: &'a OpLog<T>) -> Self {
        Changes {
            log,
            out: Vec::new(),
        }
    }

    /// Event `lv`'s item goes in at `pos`.
    pub(crate) fn insert(&mut self, pos: usize, lv: Lv) {
        // Typing: the next event of the same run, just after the last.
        // Today the next event of a run always lands right after: it sticks
        // to the last, and anything that could come between would include
        // that and so be replayed before it, and the walk goes in order with
        // nothing between the two. The position check is defensive, for
        // when that changes (reviving items, say).
        if let Some(Change::Insert {
            pos: at,
            items,
            first,
        }) = self.out.last_mut()
            && *first + items.len() == lv
            && *at + items.len() == pos
            && let Some(more) = self.log.items(*first, items.len() + 1)
        {
            *items = more;
            return;
        }
        let items = self.log.items(lv, 1).expect("an insert has an item");
        self.out.push(Change::Insert {
            pos,
            items,
            first: lv,
        });
    }

    /// The item at `pos` goes.
    pub(crate) fn delete(&mut self, pos: usize) {
        if let Some(Change::Delete { range }) = self.out.last_mut() {
            // Deleting forward: the next item has moved up to the start.
            if pos == range.start {
                range.end += 1;
                return;
            }
            // Backspacing: the one before.
            if pos + 1 == range.start {
                range.start -= 1;
                return;
            }
        }
        self.out.push(Change::Delete {
            range: pos..pos + 1,
        });
    }
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

/// How a record's items get their keys.
#[derive(Clone, Debug)]
enum Keys {
    /// Base items, by position: see `Key::base_item`.
    Base,
    /// Items inserted one after another: the item numbered `first` has
    /// `key`, and the one `n` after it has `key` with `n` appended.
    Run { first: Lv, key: Key },
}

/// A run of consecutive items: inserted one after another, or the base
/// document's.
#[derive(Clone, Debug)]
struct Record {
    /// The keys of its first and last items.
    key: Key,
    last: Key,
    /// Its first item's local version, or `BASE_ITEM` plus its position.
    id: usize,
    len: usize,
    keys: Keys,
    /// The keys of the items either side of where its first item was put.
    /// Each later item has the one before it on its left and the same
    /// item on its right.
    left: Option<Key>,
    right: Option<Key>,
    prepare: Prepare,
    /// Deleted in the effect version: by any delete replayed so far.
    gone: bool,
}

impl Record {
    fn base_run(start: usize, len: usize) -> Self {
        Record {
            key: Key::base_item(start),
            last: Key::base_item(start + len - 1),
            id: BASE_ITEM + start,
            len,
            keys: Keys::Base,
            left: None,
            right: None,
            prepare: Prepare::Inserted,
            gone: false,
        }
    }

    fn item_key(&self, id: usize) -> Key {
        debug_assert!(id >= self.id && id < self.id + self.len);
        match &self.keys {
            Keys::Base => Key::base_item(id - BASE_ITEM),
            Keys::Run { first, key } if id == *first => key.clone(),
            Keys::Run { first, key } => key.extended(id - first),
        }
    }
}

/// What a run of records holds: items, how many of them each version shows,
/// and the key of the last.
#[derive(Clone, Debug, Default, PartialEq)]
struct Counts {
    items: usize,
    prepared: usize,
    effect: usize,
    last: Option<Key>,
}

impl Summary for Counts {
    fn empty() -> Self {
        Counts::default()
    }
    fn concat(&mut self, other: &Self) {
        self.items += other.items;
        self.prepared += other.prepared;
        self.effect += other.effect;
        if other.last.is_some() {
            self.last.clone_from(&other.last);
        }
    }
}

impl Item for Record {
    type Summary = Counts;
    fn summary(&self) -> Counts {
        Counts {
            items: self.len,
            prepared: if self.prepare == Prepare::Inserted {
                self.len
            } else {
                0
            },
            effect: if self.gone { 0 } else { self.len },
            last: Some(self.last.clone()),
        }
    }
}

/// Seeks by how many items the prepare version shows.
struct Prepared(usize);

impl SeekTarget<Counts, Counts> for Prepared {
    fn cmp_position(&self, position: &Counts) -> Ordering {
        self.0.cmp(&position.prepared)
    }
}

/// Seeks to the record holding the item with this key: with `Bias::Left`,
/// the first whose last key reaches it.
struct At<'k>(&'k Key);

impl SeekTarget<Counts, Counts> for At<'_> {
    fn cmp_position(&self, position: &Counts) -> Ordering {
        match &position.last {
            Some(last) => self.0.cmp(last),
            None => Ordering::Greater,
        }
    }
}

/// What a walk cost: events replayed, and records it ended with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub replayed: usize,
    pub records: usize,
}

struct Walk<'a, T> {
    log: &'a OpLog<T>,
    records: SumTree<Record>,
    /// The prepare version, as its latest events.
    prepared: Vec<Lv>,
    /// Where each run of inserts begins, its first key, and how far it's
    /// got; and each delete's target, by key and id.
    starts: BTreeMap<Lv, (Key, usize)>,
    targets: HashMap<Lv, (Key, usize)>,
    /// How many events were replayed: what a merge cost.
    replayed: usize,
    /// Nothing has touched the records since they were made.
    fresh: bool,
}

impl<T: Clone> OpLog<T> {
    /// The changes that bring a document holding version `from` up to
    /// everything in `from` and `to`, in the order to apply them.
    pub fn changes(&self, from: &[Lv], to: &[Lv]) -> Vec<Change<'_, T>> {
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

    /// The changes, and what finding them cost.
    pub(crate) fn walk(&self, from: &[Lv], to: &[Lv]) -> (Vec<Change<'_, T>>, Stats) {
        // What to replay, and the base: a version every replayed event
        // includes whole, so none of it is ever retreated.
        let (old, new, base) = self.conflict(from, to);
        if new.is_empty() {
            return (Vec::new(), Stats::default());
        }
        // First what the document already holds past the base, to set up
        // its state, then what's new. Parents come first either way:
        // `from` holds all its events' parents.
        let order: Vec<(Lv, bool)> = old
            .iter()
            .map(|&lv| (lv, false))
            .chain(new.iter().map(|&lv| (lv, true)))
            .collect();
        let critical = self.critical_points(&order, &old, &new, &base);
        let mut walk = Walk::new(self, base);
        let mut out = Changes::new(self);
        for (&(lv, output), &critical) in order.iter().zip(&critical) {
            if critical {
                // It saw everything replayed so far, and everything after
                // it sees it: its position is final, and nothing will ever
                // reach back past it. Apply it as it is, and start afresh
                // from it.
                walk.replayed += 1;
                if output {
                    match self.op(lv) {
                        Op::Insert { pos, .. } => out.insert(pos, lv),
                        Op::Delete { pos } => out.delete(pos),
                    }
                }
                if walk.fresh {
                    walk.prepared.clear();
                    walk.prepared.push(lv);
                } else {
                    let replayed = walk.replayed;
                    walk = Walk::new(self, vec![lv]);
                    walk.replayed = replayed;
                }
            } else {
                walk.step(lv, output.then_some(&mut out));
            }
        }
        let stats = Stats {
            replayed: walk.replayed,
            records: walk.records.iter().count(),
        };
        (out.out, stats)
    }

    /// Which events in a replay are critical: each saw everything replayed
    /// before it (its parents are all of it), and everything replayed after
    /// it descends from it. There the walk needs no history at all.
    ///
    /// Found in one pass: an event is a candidate if its parents are the
    /// whole frontier so far, and a later event that doesn't descend from a
    /// candidate rules it out. A later event descends from a candidate if
    /// any parent was replayed at or after it; candidates form a chain, so
    /// the ones ruled out are the latest.
    fn critical_points(
        &self,
        order: &[(Lv, bool)],
        old: &[Lv],
        new: &[Lv],
        base: &[Lv],
    ) -> Vec<bool> {
        // Where an event was replayed, if it was: `order` is `old` then
        // `new`, each sorted, and only what's replayed so far counts.
        let at = |lv: Lv, i: usize| {
            let found = match old.binary_search(&lv) {
                Ok(k) => Some(k),
                Err(_) => new.binary_search(&lv).ok().map(|k| old.len() + k),
            };
            found.filter(|&k| k < i)
        };
        let mut frontier: Vec<Lv> = base.to_vec();
        frontier.sort_unstable();
        let mut parents: Vec<Lv> = Vec::new();
        let mut candidates: Vec<usize> = Vec::new();
        for (i, &(lv, _)) in order.iter().enumerate() {
            parents.clear();
            parents.extend(self.parents(lv));
            parents.sort_unstable();
            let latest = parents.iter().filter_map(|&p| at(p, i)).max();
            while let Some(&c) = candidates.last()
                && latest.is_none_or(|l| l < c)
            {
                candidates.pop();
            }
            let sees_everything = parents == frontier;
            if sees_everything {
                // The usual case, typing: the frontier becomes just this.
                frontier.clear();
                frontier.push(lv);
                candidates.push(i);
            } else {
                frontier.retain(|f| !parents.contains(f));
                frontier.push(lv);
                frontier.sort_unstable();
            }
        }
        let mut critical = vec![false; order.len()];
        for c in candidates {
            critical[c] = true;
        }
        critical
    }
}

impl<'a, T: Clone> Walk<'a, T> {
    /// A walk whose prepare version is `base`, with the document there as
    /// the placeholder.
    fn new(log: &'a OpLog<T>, base: Vec<Lv>) -> Self {
        let mut records = SumTree::new();
        records.push(Record::base_run(0, UNDERWATER));
        Walk {
            log,
            records,
            prepared: base,
            starts: BTreeMap::new(),
            targets: HashMap::new(),
            replayed: 0,
            fresh: true,
        }
    }
}

impl<'a, T: Clone> Walk<'a, T> {
    fn step(&mut self, lv: Lv, out: Option<&mut Changes<'a, T>>) {
        self.replayed += 1;
        self.fresh = false;
        let event = self.log.event(lv);
        // Move the prepare version to this event's parents: take back what
        // it shouldn't hold, latest first, then add what it's missing,
        // earliest first, so a delete is never undone before its insert.
        // Both versions hold the base, so the walk between them stops
        // there; for typing, the parents are the last event, and there's
        // nothing to move.
        let (retreat, mut advance) = self.log.diff(&self.prepared, &event.parents);
        for x in retreat {
            self.shift(x, false);
        }
        advance.reverse();
        for x in advance {
            self.shift(x, true);
        }
        match event.op {
            Op::Insert { pos, .. } => self.insert(lv, pos, out),
            Op::Delete { pos } => self.delete(lv, pos, out),
        }
        self.prepared = vec![lv];
    }

    /// The key of insert `lv`'s item.
    fn key_of(&self, lv: Lv) -> Key {
        let (&first, (key, len)) = self
            .starts
            .range(..=lv)
            .next_back()
            .expect("every replayed insert started or carried on a run");
        debug_assert!(lv < first + len, "insert {lv} isn't in its run");
        if lv == first {
            key.clone()
        } else {
            key.extended(lv - first)
        }
    }

    /// The record holding the item with this key.
    fn record_at(&self, key: &Key) -> (Record, Counts) {
        let mut cursor = self.records.cursor::<Counts>();
        cursor.seek(&At(key), Bias::Left);
        let record = cursor.item().expect("the key is in the tree").clone();
        (record, cursor.start().clone())
    }

    /// Advances (`forward`) or retreats event `x` in the prepare version.
    fn shift(&mut self, x: Lv, forward: bool) {
        let insert = self.log.is_insert(x);
        let (key, id) = if insert {
            (self.key_of(x), x)
        } else {
            self.targets[&x].clone()
        };
        self.isolate(&key, id);
        self.records.update_at(&At(&key), Bias::Left, |r| {
            r.prepare = match (insert, forward, r.prepare) {
                (true, true, Prepare::NotYet) => Prepare::Inserted,
                (true, false, Prepare::Inserted) => Prepare::NotYet,
                (false, true, Prepare::Inserted) => Prepare::Deleted(1),
                (false, true, Prepare::Deleted(n)) => Prepare::Deleted(n + 1),
                (false, false, Prepare::Deleted(1)) => Prepare::Inserted,
                (false, false, Prepare::Deleted(n)) => Prepare::Deleted(n - 1),
                (_, _, state) => unreachable!("shifting {x} from {state:?}"),
            };
        });
    }

    /// Splits item `id` (whose key is `key`) out of its record, so its state
    /// can change alone.
    fn isolate(&mut self, key: &Key, id: usize) {
        let (record, _) = self.record_at(key);
        let offset = id - record.id;
        if offset + 1 < record.len {
            self.split(&record, offset + 1);
        }
        if offset > 0 {
            self.split(&record, offset);
        }
    }

    /// The key and id of the `pos`th item the prepare version shows, split
    /// into a record of its own if `alone`, or else made the last of its
    /// record. The placeholder never runs out, so there always is one.
    fn prepared_item(&mut self, pos: usize, alone: bool) -> (Key, usize) {
        let (record, offset) = {
            let mut cursor = self.records.cursor::<Counts>();
            cursor.seek(&Prepared(pos), Bias::Right);
            let record = cursor
                .item()
                .expect("the placeholder never runs out")
                .clone();
            (record, pos - cursor.start().prepared)
        };
        let id = record.id + offset;
        if offset + 1 < record.len {
            self.split(&record, offset + 1);
        }
        if alone && offset > 0 {
            self.split(&record, offset);
        }
        (record.item_key(id), id)
    }

    /// Splits the record starting with `record`'s first item at `at`,
    /// counted from that item. (The record may have shrunk since `record`
    /// was read, but never from its front.)
    fn split(&mut self, record: &Record, at: usize) {
        let mut rest = None;
        self.records.update_at(&At(&record.key), Bias::Left, |r| {
            debug_assert!(r.id == record.id && at < r.len, "splitting inside a record");
            let mut tail = r.clone();
            tail.id = r.id + at;
            tail.len = r.len - at;
            tail.key = r.item_key(tail.id);
            if let Keys::Run { .. } = r.keys {
                tail.left = Some(r.item_key(tail.id - 1));
            }
            r.len = at;
            r.last = r.item_key(r.id + at - 1);
            rest = Some(tail);
        });
        let before = record.item_key(record.id + at - 1);
        self.records.insert(&At(&before), Bias::Right, rest);
    }

    /// How many items the effect version shows before item `id`, whose key
    /// is `key`.
    fn effect_pos(&self, key: &Key, id: usize) -> usize {
        let (record, before) = self.record_at(key);
        before.effect + if record.gone { 0 } else { id - record.id }
    }

    fn insert(&mut self, lv: Lv, pos: usize, out: Option<&mut Changes<'a, T>>) {
        // The origins, as the insert's author saw them. An insert sticks to
        // what's before it: its left origin is the visible item just before
        // the position (none at the start), and its right is whatever came
        // next in that version, deleted or not. Concurrent items, not in
        // the version, don't count. (diamond-types' convention, and the
        // intent: "at the start" stays at the start, wherever tombstones
        // lie.)
        let left = pos.checked_sub(1).map(|p| self.prepared_item(p, false).0);
        let (prev, next, right) = self.place(lv, left.as_ref());
        let key = match self.extend(
            lv,
            left.as_ref(),
            prev.as_ref(),
            next.as_ref(),
            right.as_ref(),
        ) {
            Some(key) => key,
            None => {
                let key = Key::between(prev.as_ref(), next.as_ref());
                let record = Record {
                    key: key.clone(),
                    last: key.clone(),
                    id: lv,
                    len: 1,
                    keys: Keys::Run {
                        first: lv,
                        key: key.clone(),
                    },
                    left,
                    right,
                    prepare: Prepare::Inserted,
                    gone: false,
                };
                match &next {
                    Some(n) => self.records.insert(&At(n), Bias::Left, [record]),
                    None => self.records.push(record),
                }
                self.starts.insert(lv, (key.clone(), 1));
                key
            }
        };
        if let Some(out) = out {
            out.insert(self.effect_pos(&key, lv), lv);
        }
    }

    /// Carries on the run of the item just before `lv`, if `lv` goes right
    /// after it, as its author saw it, with the same right origin, and the
    /// key it would get falls before what's next. Returns that key.
    ///
    /// When events replay in number order these all hold whenever `lv`'s
    /// left origin ends the run: nothing can be put between two
    /// consecutive events. They don't when a merge replays new events
    /// numbered among old ones (fetched before, merged after, with local
    /// edits between): an old event may have landed right after the run,
    /// on its right, or deleted its end.
    fn extend(
        &mut self,
        lv: Lv,
        left: Option<&Key>,
        prev: Option<&Key>,
        next: Option<&Key>,
        right: Option<&Key>,
    ) -> Option<Key> {
        let before = lv.checked_sub(1)?;
        let (&first, (run_key, len)) = self.starts.range(..=before).next_back()?;
        if first + len != lv {
            return None;
        }
        let before_key = if before == first {
            run_key.clone()
        } else {
            run_key.extended(before - first)
        };
        if left != Some(&before_key) || prev != Some(&before_key) {
            return None;
        }
        let key = run_key.extended(lv - first);
        // Defensive: the way `between` picks digits, no record gets a key
        // here, but the tree's order depends on keys never colliding.
        if next.is_some_and(|n| *n <= key) {
            return None;
        }
        let (record, _) = self.record_at(&before_key);
        let carries_on = record.last == before_key
            && record.right.as_ref() == right
            && record.prepare == Prepare::Inserted
            && !record.gone;
        if !carries_on {
            return None;
        }
        self.records.update_at(&At(&before_key), Bias::Left, |r| {
            r.len += 1;
            r.last = key.clone();
        });
        self.starts.get_mut(&first).expect("found above").1 += 1;
        Some(key)
    }

    /// Where an insert by `lv` with left origin `left` goes: the key of the
    /// item it goes after and of the record it goes before, and its right
    /// origin.
    ///
    /// The right origin is the first record after `left` that the prepare
    /// version has, deleted or not. Between them are only concurrent
    /// inserts, and the YjsMod rule (see `order.rs`, written here over keys
    /// instead of indices) picks the place among them.
    fn place(&self, lv: Lv, left: Option<&Key>) -> (Option<Key>, Option<Key>, Option<Key>) {
        let mut cursor = self.records.cursor::<Counts>();
        // The first record after `left`, which always ends its record.
        if let Some(l) = left {
            cursor.seek(&At(l), Bias::Left);
        }
        cursor.next();

        let right = {
            let mut c = cursor.clone();
            loop {
                match c.item() {
                    Some(r) if r.prepare == Prepare::NotYet => c.next(),
                    Some(r) => break Some(r.key.clone()),
                    None => break None,
                }
            }
        };

        let me = self.log.order_key(lv);
        // Where it goes so far: after `prev`, before `next`.
        let (mut prev, mut next) = (left.cloned(), None);
        let mut last = left.cloned();
        let mut scanning = false;
        loop {
            let Some(other) = cursor.item() else {
                if !scanning {
                    (prev, next) = (last, None);
                }
                break;
            };
            if !scanning {
                (prev, next) = (last.clone(), Some(other.key.clone()));
            }
            if right.as_ref() == Some(&other.key) {
                break;
            }
            // Origins compare by key: `None` on the left is before
            // everything, on the right after everything.
            let other_left = other.left.as_ref();
            if other_left < left {
                break;
            }
            if other_left == left {
                match cmp_right(other.right.as_ref(), right.as_ref()) {
                    Ordering::Equal => {
                        // Same origins both sides: a true tie, by id.
                        if me < self.log.order_key(other.id) {
                            break;
                        }
                        scanning = false;
                    }
                    nearer => scanning = nearer == Ordering::Less,
                }
            }
            last = Some(other.last.clone());
            cursor.next();
        }
        (prev, next, right)
    }

    fn delete(&mut self, lv: Lv, pos: usize, out: Option<&mut Changes<'a, T>>) {
        let (key, id) = self.prepared_item(pos, true);
        let was_gone = self
            .records
            .update_at(&At(&key), Bias::Left, |r| {
                r.prepare = Prepare::Deleted(1);
                std::mem::replace(&mut r.gone, true)
            })
            .expect("the item is in the tree");
        self.targets.insert(lv, (key.clone(), id));
        // Already deleted in the effect version by a concurrent delete:
        // nothing more to do to the document.
        if !was_gone && let Some(out) = out {
            out.delete(self.effect_pos(&key, id));
        }
    }
}

/// Compares right origins, where `None` is after everything.
fn cmp_right(a: Option<&Key>, b: Option<&Key>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => a.cmp(b),
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
        let (changes, stats) = log.walk(&a, &b);
        let replayed = stats.replayed;
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
        let (changes, stats) = log.walk(&a, &b);
        let replayed = stats.replayed;
        assert_eq!(replayed, 10, "five events each side, none before the fork");
        let mut doc = log.checkout(&a);
        for change in changes {
            change.apply(&mut doc);
        }
        assert_eq!(doc, log.checkout(&log.union(&a, &b)));
    }

    /// A draft branch forked early and never merged: merges on the main
    /// line don't involve it, so they shouldn't replay what happened since
    /// it forked.
    #[test]
    fn an_unmerged_branch_doesnt_slow_other_merges() {
        let mut log: OpLog<u32> = OpLog::new();
        let (alice, bob, carol) = (log.agent("alice"), log.agent("bob"), log.agent("carol"));
        let mut v = Vec::new();
        for i in 0..100 {
            v = log.add_insert(alice, &v, i, [i as u32]);
        }
        // Bob's draft, forked from early on, never merged by anyone.
        log.add_insert(bob, &[10], 3, [900, 901, 902]);
        for i in 100..200 {
            v = log.add_insert(alice, &v, i, [i as u32]);
        }
        let a = log.add_insert(alice, &v, 50, [1000, 1001]);
        let c = log.add_delete(carol, &v, 20..21);
        let (changes, stats) = log.walk(&a, &c);
        assert_eq!(stats.replayed, 3, "Alice's two inserts and Carol's delete");
        let mut doc = log.checkout(&a);
        for change in changes {
            change.apply(&mut doc);
        }
        assert_eq!(doc, log.checkout(&log.union(&a, &c)));
    }

    /// Two people typing at once, each merging the other's work now and
    /// then: each merge replays only what's happened since they last met.
    #[test]
    fn live_co_editing_merges_stay_small() {
        let mut log: OpLog<u32> = OpLog::new();
        let (alice, bob) = (log.agent("alice"), log.agent("bob"));
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let mut worst = 0;
        for round in 0..50u32 {
            // Each types three items at the start of their own copy.
            a = log.add_insert(alice, &a, 0, [round * 10, round * 10 + 1, round * 10 + 2]);
            b = log.add_insert(
                bob,
                &b,
                0,
                [5000 + round * 10, 5001 + round * 10, 5002 + round * 10],
            );
            // Alice merges Bob's work; Bob merges what Alice had before.
            let (_, stats) = log.walk(&a, &b);
            worst = worst.max(stats.replayed);
            let merged = log.union(&a, &b);
            if round % 2 == 0 {
                b = merged.clone();
            }
            a = merged;
        }
        // Never more than the last couple of rounds' typing.
        assert!(worst <= 12, "a merge replayed {worst} events");
        eprintln!("live co-editing: worst merge replayed {worst} events");
    }

    /// Where the obvious base is wrong: two roots `a` and `b`, then `c`
    /// after `a` and `d` after `b`; merging `n` (after `c` only) into
    /// `{c, d}` leaves `{a, b}` as what both reached, but `c` doesn't
    /// include `b`, so it can't be the base. The walk must go further.
    #[test]
    fn a_base_every_replayed_event_includes() {
        let mut log: OpLog<u32> = OpLog::new();
        let (alice, bob) = (log.agent("alice"), log.agent("bob"));
        let a = log.add_insert(alice, &[], 0, [1]);
        let b = log.add_insert(bob, &[], 0, [2]);
        let c = log.add_insert(alice, &a, 1, [3]);
        let d = log.add_insert(bob, &b, 1, [4]);
        let from = log.union(&c, &d);
        let n = log.add_insert(alice, &c, 0, [5]);
        let (old, new, base) = log.conflict(&from, &n);
        assert!(base.is_empty(), "nothing is under every replayed event");
        assert_eq!((old.len(), new.len()), (4, 1));
        let mut doc = log.checkout(&from);
        for change in log.changes(&from, &n) {
            change.apply(&mut doc);
        }
        assert_eq!(doc, log.checkout(&log.union(&from, &n)));
    }

    /// Typing makes runs: a document typed in a few stretches, with the
    /// cursor moved between them, is a few records, not one per item.
    #[test]
    fn typing_makes_few_records() {
        let mut log: OpLog<char> = OpLog::new();
        let alice = log.agent("alice");
        let mut v = Vec::new();
        let mut len = 0;
        for (at, text) in [(0, "hello world"), (5, ", dear"), (0, "Oh, "), (21, "!")] {
            for (k, c) in text.chars().enumerate() {
                v = log.add_insert(alice, &v, at + k, [c]);
            }
            len += text.len();
        }
        let (changes, stats) = log.walk(&[], &v);
        // One change per stretch typed, holding every item.
        assert_eq!(changes.len(), 4);
        let items: usize = changes
            .iter()
            .map(|c| match c {
                Change::Insert { items, .. } => items.len(),
                Change::Delete { .. } => 0,
            })
            .sum();
        assert_eq!(items, len);
        assert_eq!(
            log.checkout(&v).into_iter().collect::<String>(),
            "Oh, hello, dear world!"
        );
        // Four stretches typed, two of them splitting what was there, plus
        // the placeholder: a handful of records for 22 items.
        assert!(stats.records <= 8, "{} records", stats.records);
    }
}
