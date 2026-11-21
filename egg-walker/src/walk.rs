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
//! A merge only replays what it has to. Its *base* is everything up to the
//! latest critical event `from` holds: a point the whole history narrows
//! to, with every later event descending from it, so nothing in the base is
//! ever retreated. The document at the base needs no records, only a
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
use std::collections::{BTreeMap, HashMap, HashSet};

use mamre::cursor::Bias;
use mamre::{Item, SeekTarget, SumTree, Summary};

use crate::key::Key;
use crate::oplog::{Lv, Op, OpLog};

/// Ids at or above this name items of the base document, by position.
pub(crate) const BASE_ITEM: usize = 1 << 62;
/// How long the placeholder is: more than any real document.
pub(crate) const UNDERWATER: usize = 1 << 61;

/// A plain change to a document: what the walk produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change<T> {
    Insert { pos: usize, item: T },
    Delete { pos: usize },
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

    /// The changes, and what finding them cost.
    pub(crate) fn walk(&self, from: &[Lv], to: &[Lv]) -> (Vec<Change<T>>, Stats) {
        // The base: everything up to the latest critical event `from`
        // holds. Every event after a critical one descends from it, so none
        // of them ever retreats anything in the base.
        let floor = self.critical_in(from);
        let old = self.events_after(from, floor);
        let known: HashSet<Lv> = old.iter().copied().collect();
        let new: Vec<Lv> = self
            .events_after(to, floor)
            .into_iter()
            .filter(|lv| !known.contains(lv))
            .collect();
        if new.is_empty() {
            return (Vec::new(), Stats::default());
        }
        let mut records = SumTree::new();
        records.push(Record::base_run(0, UNDERWATER));
        let mut walk = Walk {
            log: self,
            records,
            prepared: floor.into_iter().collect(),
            starts: BTreeMap::new(),
            targets: HashMap::new(),
            replayed: 0,
        };
        // First what the document already holds past the base, to set up
        // its state, then what's new. Parents come first either way:
        // `from` holds all its events' parents.
        for &lv in &old {
            walk.step(lv, None);
        }
        let mut out = Vec::new();
        for &lv in &new {
            walk.step(lv, Some(&mut out));
        }
        let stats = Stats {
            replayed: walk.replayed,
            records: walk.records.iter().count(),
        };
        (out, stats)
    }
}

impl<T: Clone> Walk<'_, T> {
    fn step(&mut self, lv: Lv, out: Option<&mut Vec<Change<T>>>) {
        self.replayed += 1;
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
            Op::Insert { pos, item } => self.insert(lv, pos, item, out),
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

    fn insert(&mut self, lv: Lv, pos: usize, item: T, out: Option<&mut Vec<Change<T>>>) {
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
            out.push(Change::Insert {
                pos: self.effect_pos(&key, lv),
                item,
            });
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

    fn delete(&mut self, lv: Lv, pos: usize, out: Option<&mut Vec<Change<T>>>) {
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
            out.push(Change::Delete {
                pos: self.effect_pos(&key, id),
            });
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
        assert_eq!(changes.len(), len);
        assert_eq!(
            log.checkout(&v).into_iter().collect::<String>(),
            "Oh, hello, dear world!"
        );
        // Four stretches typed, two of them splitting what was there, plus
        // the placeholder: a handful of records for 22 items.
        assert!(stats.records <= 8, "{} records", stats.records);
    }
}
