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
//!
//! An undelete counts against its delete: an item is deleted in a version
//! while some delete of it there has no undelete there. Undoing brings the
//! item back as itself, so the change says which insert it came from. The
//! walk only knows that for items it has records for, so a merge always
//! replays the delete an undelete undoes, and never starts afresh between
//! the two; an item from the base document is looked up by its place there
//! (see `ids.rs`).

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use mamre::cursor::Bias;
use mamre::{Item, SeekTarget, SumTree, Summary};

use crate::blocks::Blocks;
use crate::ids::{Identities, Ids};
use crate::key::Key;
use crate::oplog::{Kind, Lv, Op, OpLog};
use crate::recent::Recent;

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
    /// The items in `range` move to `to`, a position in the document as it
    /// was before the move (outside the range), as `Rope::move_range` moves
    /// text. They're the same items, somewhere else: anything keyed to
    /// them goes with them.
    Move { range: Range<usize>, to: usize },
    /// Item `item` now starts a section at `level` (0: no section). It's at
    /// `pos`, if it shows. The document's items don't change: this is for
    /// whatever keeps its outline (see [`Accountant`](crate::Accountant)).
    Mark {
        item: Lv,
        pos: Option<usize>,
        level: u8,
    },
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
            Change::Move { range, to } => Change::Move {
                range: range.clone(),
                to: *to,
            },
            Change::Mark { item, pos, level } => Change::Mark {
                item: *item,
                pos: *pos,
                level: *level,
            },
        }
    }
}

/// A document changes can be applied to: anything that can put items in,
/// take them out, and move them. Positions count items. This is all the
/// CRDT asks of a document; it keeps everything else itself (see
/// [`Accountant`](crate::Accountant)).
///
/// A text buffer counting bytes, say, maps positions to its own offsets.
pub trait Sequence<T> {
    /// `items` go in at `pos`.
    fn insert(&mut self, pos: usize, items: &[T]);
    /// The items in `range` go.
    fn delete(&mut self, range: Range<usize>);
    /// The items in `range` move to `to`, a position before the move,
    /// outside the range: the same items, somewhere else.
    fn move_range(&mut self, range: Range<usize>, to: usize);
}

impl<T: Clone> Sequence<T> for Vec<T> {
    fn insert(&mut self, pos: usize, items: &[T]) {
        self.splice(pos..pos, items.iter().cloned());
    }

    fn delete(&mut self, range: Range<usize>) {
        self.drain(range);
    }

    fn move_range(&mut self, range: Range<usize>, to: usize) {
        let moved: Vec<T> = self.drain(range.clone()).collect();
        let at = if to > range.start {
            to - range.len()
        } else {
            to
        };
        self.splice(at..at, moved);
    }
}

impl<T> Change<'_, T> {
    /// Applies this change to `doc`.
    pub fn apply(&self, doc: &mut impl Sequence<T>) {
        match self {
            Change::Insert { pos, items, .. } => doc.insert(*pos, items),
            Change::Delete { range } => doc.delete(range.clone()),
            Change::Move { range, to } => doc.move_range(range.clone(), *to),
            // A mark changes no items.
            Change::Mark { .. } => {}
        }
    }
}

/// The walk's output as it's made: each change joins the one before it
/// when it carries on its run.
pub(crate) struct Changes<'a, T> {
    log: &'a OpLog<T>,
    pub(crate) out: Vec<Change<'a, T>>,
    /// The identities of the document the changes make, if they're being
    /// kept, and which item each new delete removed, where known.
    pub(crate) ids: Option<Ids>,
    pub(crate) learned: Vec<(Lv, Lv)>,
    /// The moved blocks of the document the changes make, when the log has
    /// moves (see `blocks.rs`).
    pub(crate) blocks: Option<Blocks>,
}

impl<'a, T: Clone> Changes<'a, T> {
    pub(crate) fn new(log: &'a OpLog<T>) -> Self {
        Changes {
            log,
            out: Vec::new(),
            ids: None,
            learned: Vec::new(),
            blocks: None,
        }
    }

    /// Event `lv`'s item goes in at `pos`.
    pub(crate) fn insert(&mut self, pos: usize, lv: Lv) {
        self.insert_run(pos, lv, 1);
    }

    /// The items of events `lv..lv + n`, one run in the log, go in at `pos`.
    pub(crate) fn insert_run(&mut self, pos: usize, lv: Lv, n: usize) {
        if let Some(ids) = &mut self.ids {
            ids.insert(pos, lv, n);
        }
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
            && let Some(more) = self.log.items(*first, items.len() + n)
        {
            *items = more;
            return;
        }
        let items = self.log.items(lv, n).expect("inserts of one run");
        self.out.push(Change::Insert {
            pos,
            items,
            first: lv,
        });
    }

    /// Item `item`, at `pos` if it shows, starts a section at `level`.
    pub(crate) fn mark(&mut self, item: Lv, pos: Option<usize>, level: u8) {
        self.out.push(Change::Mark { item, pos, level });
    }

    /// The items in `range` move to `to`.
    pub(crate) fn move_range(&mut self, range: Range<usize>, to: usize) {
        if let Some(ids) = &mut self.ids {
            ids.move_range(range.clone(), to);
        }
        self.out.push(Change::Move { range, to });
    }

    /// The item at `pos` goes.
    pub(crate) fn delete(&mut self, pos: usize) {
        self.delete_run(pos, 1);
    }

    /// The item at `pos` goes, `n` times over: the `n` items from `pos`.
    pub(crate) fn delete_run(&mut self, pos: usize, n: usize) {
        if let Some(ids) = &mut self.ids {
            ids.remove(pos..pos + n);
        }
        if let Some(Change::Delete { range }) = self.out.last_mut() {
            // Deleting forward: the next items have moved up to the start.
            if pos == range.start {
                range.end += n;
                return;
            }
            // Backspacing: the one before, then what's moved up after it.
            if pos + 1 == range.start {
                range.start -= 1;
                range.end += n - 1;
                return;
            }
        }
        self.out.push(Change::Delete {
            range: pos..pos + n,
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
    /// How many deletes replayed so far removed it, not undone: it shows in
    /// the effect version if none.
    dead: u32,
    /// It shows in the document the merge started from: set once the
    /// events that document already has are replayed.
    in_from: bool,
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
            dead: 0,
            in_from: true,
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
    /// Shown in the document the merge started from.
    from: usize,
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
        self.from += other.from;
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
            effect: if self.dead > 0 { 0 } else { self.len },
            from: if self.in_from { self.len } else { 0 },
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

/// What a walk cost: events replayed, how many of them took a full step
/// (not critical, and first of their run), and records it ended with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub replayed: usize,
    pub stepped: usize,
    pub records: usize,
    /// Checkouts made to find an item's identity, for want of knowing it.
    pub checkouts: usize,
}

struct Walk<'a, T> {
    log: &'a OpLog<T>,
    records: SumTree<Record>,
    /// The prepare version, as its latest events.
    prepared: Vec<Lv>,
    /// Where each run of inserts begins, its first key, and how far it's
    /// got; and the item each delete deleted.
    starts: BTreeMap<Lv, (Key, usize)>,
    targets: HashMap<Lv, usize>,
    /// How many undeletes of each delete the prepare version holds, and
    /// the deletes undone in the effect version.
    undos: HashMap<Lv, u32>,
    undone: HashSet<Lv>,
    /// The version the placeholder stands for, and which insert each of its
    /// items came from, once known.
    base: Vec<Lv>,
    base_ids: Option<Ids>,
    /// The identities of the document the merge started from, once each
    /// record knows whether it shows there; and which old delete removed
    /// each base item it did.
    known: Option<(Ids, Arc<BTreeMap<Lv, Lv>>)>,
    marked: bool,
    killers: HashMap<usize, Lv>,
    /// How many events were replayed, and stepped in full: what a merge
    /// cost.
    replayed: usize,
    stepped: usize,
    checkouts: usize,
    /// Nothing has touched the records since they were made.
    fresh: bool,
    /// Each marked item's winning mark among those replayed, by the order
    /// marks are decided in. One from before the base always loses: it's
    /// earlier in Lamport time than anything replayed.
    marks: HashMap<usize, (u64, (&'a str, u64))>,
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
        let (out, stats) = self.walk_into(from, to, false, None, None);
        (out.out, stats)
    }

    /// The changes, as [`changes`](Self::changes), for a replica that keeps
    /// its document's [`Identities`], at `from`: they're used to bring back
    /// items an undo revives, and brought up to date with the changes.
    pub fn changes_with(&self, from: &[Lv], to: &[Lv], ids: &mut Identities) -> Vec<Change<'_, T>> {
        let (out, _) = self.walk_into(from, to, true, Some(ids), None);
        if let Some(new) = out.ids {
            ids.ids = new;
        }
        ids.learn(out.learned);
        out.out
    }

    /// The changes, as [`changes_with`](Self::changes_with), keeping the
    /// document's moved blocks as well: the [`Accountant`]'s way.
    ///
    /// [`Accountant`]: crate::Accountant
    pub(crate) fn changes_kept(
        &self,
        from: &[Lv],
        to: &[Lv],
        ids: &mut Identities,
        blocks: &mut Blocks,
        recent: &Recent,
    ) -> Vec<Change<'_, T>> {
        let (out, _) = self.walk_into_kept(from, to, Some(ids), Some(blocks), Some(recent));
        if let Some(new) = out.ids {
            ids.ids = new;
        }
        if let Some(new) = out.blocks {
            *blocks = new;
        }
        ids.learn(out.learned);
        out.out
    }

    /// Which insert each item at `version` came from.
    pub(crate) fn ids_at(&self, version: &[Lv]) -> Ids {
        self.walk_into(&[], version, true, None, None)
            .0
            .ids
            .expect("a walk from nothing keeps identities")
    }

    /// The walk, keeping the identities of the document it makes if
    /// `keep_ids` and it starts from nothing, or from a document whose
    /// identities are `known`.
    fn walk_into(
        &self,
        from: &[Lv],
        to: &[Lv],
        keep_ids: bool,
        known: Option<&Identities>,
        known_blocks: Option<&Blocks>,
    ) -> (Changes<'_, T>, Stats) {
        if self.has_moves() {
            let known = known.map(|k| &k.ids);
            return self.walk_moves(from, to, keep_ids, known, known_blocks, None);
        }
        let mut out = Changes::new(self);
        // What to replay, and the base: a version every replayed event
        // includes whole, so none of it is ever retreated.
        let (old, new, base) = self.conflict(from, to);
        // From nothing, the document the changes make is the whole effect
        // version, so its identities can be kept as it goes and handed to
        // each fresh start: an undo needs them to bring back items from
        // before one.
        let undoes = new
            .iter()
            .any(|span| self.kind(span.start) == Kind::Undelete);
        // A mark's change names its item, so marks need identities kept.
        let marks = self.has_marks();
        if let Some(known) = known {
            out.ids = Some(known.ids.clone());
        } else if from.is_empty() && (keep_ids || undoes || marks) {
            out.ids = Some(Ids::default());
        } else if marks {
            out.ids = Some(self.ids_at(from));
        }
        if new.is_empty() {
            return (out, Stats::default());
        }
        // First what the document already holds past the base, to set up
        // its state, then what's new. Parents come first either way:
        // `from` holds all its events' parents. Each span is events of one
        // run, replayed one after another.
        let order: Vec<(Range<Lv>, bool)> = old
            .iter()
            .map(|span| (span.clone(), false))
            .chain(new.iter().map(|span| (span.clone(), true)))
            .collect();
        let critical = self.critical_points(&order, &old, &new, &base);
        let mut critical = critical.iter().peekable();
        // With nothing old to replay, the base is the document the changes
        // start from, whose identities are known.
        let old_len: usize = old.iter().map(|span| span.len()).sum();
        let base_ids = if old_len == 0 { out.ids.clone() } else { None };
        let mut walk = Walk::new(self, base, base_ids);
        walk.known = known.map(|k| (k.ids.clone(), k.removed.clone()));
        // How many events have been replayed: where the next is in `order`.
        let mut at = 0;
        for (span, output) in order {
            let mut lv = span.start;
            while lv < span.end {
                if at == old_len && !walk.marked && walk.known.is_some() {
                    walk.mark_from();
                }
                // The stretch from here that's all critical, or none of it.
                while critical.next_if(|c| c.end <= at).is_some() {}
                let (is_critical, until) = match critical.peek() {
                    Some(c) if c.start <= at => (true, c.end),
                    Some(c) => (false, c.start),
                    None => (false, usize::MAX),
                };
                let n = (span.end - lv).min(until - at);
                if is_critical {
                    // Each saw everything replayed before it, and everything
                    // after sees it: its position is final, and nothing will
                    // ever reach back past it. Apply them as they are, and
                    // start afresh from the last.
                    walk.replayed += n;
                    if output {
                        match self.op(lv) {
                            Op::Insert { pos, .. } => out.insert_run(pos, lv, n),
                            Op::Delete { pos } => {
                                // Applied as they stand, to the document
                                // whose identities are being kept.
                                if let Some(ids) = &out.ids {
                                    let gone: Vec<(Lv, Lv)> =
                                        (0..n).map(|k| (lv + k, ids.get(pos + k))).collect();
                                    out.learned.extend(gone);
                                }
                                out.delete_run(pos, n)
                            }
                            Op::Undelete { .. } => unreachable!("an undelete is never critical"),
                            Op::Move { .. } => {
                                unreachable!("a log with moves takes the moves path")
                            }
                            Op::Mark { pos, level } => {
                                let item = out.ids.as_ref().expect("kept for marks").get(pos);
                                out.mark(item, Some(pos), level);
                            }
                        }
                    }
                    let last = lv + n - 1;
                    // Past the old events, the document the changes make is
                    // the effect version, so its identities are the base's.
                    // (Among the old events there's no fresh start before an
                    // undone delete: the base would have been taken there,
                    // or the range up to its undo forbids one. So the
                    // condition is defensive.)
                    let base_ids = if at + n > old_len {
                        out.ids.clone()
                    } else {
                        None
                    };
                    if walk.fresh {
                        // The placeholder now stands for the document here.
                        walk.prepared.clear();
                        walk.prepared.push(last);
                        walk.base.clone_from(&walk.prepared);
                        walk.base_ids = base_ids;
                    } else {
                        let counts = (walk.replayed, walk.stepped, walk.checkouts);
                        let (known, marked) = (walk.known.take(), walk.marked);
                        walk = Walk::new(self, vec![last], base_ids);
                        (walk.replayed, walk.stepped, walk.checkouts) = counts;
                        (walk.known, walk.marked) = (known, marked);
                    }
                } else {
                    // Each the last one's only child, carrying on what it
                    // did.
                    walk.step_run(lv, n, output.then_some(&mut out));
                }
                lv += n;
                at += n;
            }
        }
        let stats = Stats {
            replayed: walk.replayed,
            stepped: walk.stepped,
            checkouts: walk.checkouts,
            records: walk.records.iter().count(),
        };
        (out, stats)
    }

    /// Whether any event is a mark.
    fn has_marks(&self) -> bool {
        self.runs
            .iter()
            .any(|run| matches!(run.op, crate::oplog::RunOp::Mark { .. }))
    }

    /// Whether any event is a move: if not, the walk needn't know about
    /// blocks at all.
    fn has_moves(&self) -> bool {
        self.runs
            .iter()
            .any(|run| matches!(run.op, crate::oplog::RunOp::Move { .. }))
    }

    /// The walk, as the accountant asks: identities kept, and with moves,
    /// the blocks too, and the base's books found from the `recent` ones.
    fn walk_into_kept(
        &self,
        from: &[Lv],
        to: &[Lv],
        known: Option<&Identities>,
        known_blocks: Option<&Blocks>,
        recent: Option<&Recent>,
    ) -> (Changes<'_, T>, Stats) {
        if self.has_moves() {
            let known = known.map(|k| &k.ids);
            return self.walk_moves(from, to, true, known, known_blocks, recent);
        }
        self.walk_into(from, to, true, known, known_blocks)
    }

    /// The identities and blocks of the document at `version`: from the
    /// latest of the `recent` books at or before it, walked forward, or
    /// from nothing.
    fn books_at(&self, version: &[Lv], recent: Option<&Recent>) -> (Option<Ids>, Blocks) {
        if let Some(recent) = recent {
            if let Some(kept) = recent.at(version) {
                return (Some(kept.ids.clone()), kept.blocks.clone());
            }
            let holds = self.closure(version);
            if let Some(kept) = recent.latest_within(|lv| holds[lv]) {
                // Its base is at or before the kept version, and before
                // `version`: each walk back starts earlier, so this ends.
                let (o, _) = self.walk_moves(
                    &kept.version,
                    version,
                    true,
                    Some(&kept.ids),
                    Some(&kept.blocks),
                    Some(recent),
                );
                return (o.ids, o.blocks.unwrap_or_default());
            }
        }
        let (o, _) = self.walk_moves(&[], version, true, None, None, None);
        (o.ids, o.blocks.unwrap_or_default())
    }

    /// The walk, for a log with moves. Where nothing is concurrent, events
    /// apply as they are, and the document's moved blocks are kept up to
    /// date by position (see `blocks.rs`). Between, each stretch is merged
    /// the simple walk's way, from the document where it starts, whose
    /// identities and blocks are known by then; the first, from the base,
    /// finds them if they aren't: from the `recent` books (see `recent.rs`),
    /// or from nothing.
    fn walk_moves(
        &self,
        from: &[Lv],
        to: &[Lv],
        keep_ids: bool,
        known: Option<&Ids>,
        known_blocks: Option<&Blocks>,
        recent: Option<&Recent>,
    ) -> (Changes<'_, T>, Stats) {
        let mut out = Changes::new(self);
        let (old, new, base) = self.conflict(from, to);
        let marks = self.has_marks();
        if let Some(known) = known {
            out.ids = Some(known.clone());
        } else if from.is_empty() && (keep_ids || marks) {
            out.ids = Some(Ids::default());
        } else if marks {
            out.ids = Some(self.ids_at(from));
        }
        // The document the changes make: its blocks, as it goes.
        let mut blocks = match known_blocks {
            Some(known) => known.clone(),
            None if from.is_empty() => Blocks::default(),
            None => self.blocks_at(from),
        };
        if new.is_empty() {
            out.blocks = Some(blocks);
            return (out, Stats::default());
        }
        let order: Vec<(Range<Lv>, bool)> = old
            .iter()
            .map(|span| (span.clone(), false))
            .chain(new.iter().map(|span| (span.clone(), true)))
            .collect();
        let critical = self.critical_points(&order, &old, &new, &base);
        let mut critical = critical.iter().peekable();
        let old_len: usize = old.iter().map(|span| span.len()).sum();
        // Where the next window starts, and what's known of it there.
        let mut start = base.clone();
        // Whether the books kept so far are the document's at `start`:
        // from the document the changes start from, or just after events
        // applied as they are. (Nothing changes between those and the next
        // window, so they're taken when it opens.)
        let mut fresh = old_len == 0;
        let mut window: Option<crate::simple::Window<'_, T>> = None;
        let mut at = 0;
        for (span, output) in order {
            let mut lv = span.start;
            while lv < span.end {
                // The stretch from here that's all critical, or none of it.
                while critical.next_if(|c| c.end <= at).is_some() {}
                let (is_critical, until) = match critical.peek() {
                    Some(c) if c.start <= at => (true, c.end),
                    Some(c) => (false, c.start),
                    None => (false, usize::MAX),
                };
                let n = (span.end - lv).min(until - at);
                if is_critical && output {
                    if let Some(w) = window.take() {
                        blocks = w.blocks();
                    }
                    self.apply_critical(lv, n, &mut out, &mut blocks);
                    start = vec![lv + n - 1];
                    fresh = true;
                } else {
                    let w = window.get_or_insert_with(|| {
                        let (ids, found) = if fresh {
                            (out.ids.clone(), blocks.clone())
                        } else {
                            self.books_at(&start, recent)
                        };
                        crate::simple::Window::new(self, start.clone(), ids, &found)
                    });
                    for k in 0..n {
                        w.step(lv + k, output.then_some(&mut out));
                    }
                }
                lv += n;
                at += n;
            }
        }
        if let Some(w) = window.take() {
            blocks = w.blocks();
        }
        out.blocks = Some(blocks);
        (out, Stats::default())
    }

    /// Applies events `lv..lv + n`, one run in the log, each of which saw
    /// everything before it and is seen by everything after: their
    /// positions are final.
    fn apply_critical(&self, lv: Lv, n: usize, out: &mut Changes<'_, T>, blocks: &mut Blocks) {
        match self.op(lv) {
            Op::Insert { pos, .. } => {
                out.insert_run(pos, lv, n);
                blocks.insert(pos, n);
            }
            Op::Delete { pos } => {
                // Deleting forward: each removes what's moved up to `pos`.
                if let Some(ids) = &out.ids {
                    let gone: Vec<(Lv, Lv)> = (0..n).map(|k| (lv + k, ids.get(pos + k))).collect();
                    out.learned.extend(gone);
                }
                out.delete_run(pos, n);
                blocks.delete(pos..pos + n);
            }
            Op::Undelete { .. } => unreachable!("an undelete is never critical"),
            Op::Mark { pos, level } => {
                let item = out.ids.as_ref().expect("kept for marks").get(pos);
                out.mark(item, Some(pos), level);
            }
            Op::Move { pos, len, to } => {
                debug_assert_eq!(n, 1, "moves don't run");
                if blocks.apply_move(lv, pos..pos + len, to) {
                    out.move_range(pos..pos + len, to);
                }
            }
        }
    }

    /// The moved blocks of the document at `version`, from nothing.
    fn blocks_at(&self, version: &[Lv]) -> Blocks {
        self.walk_moves(&[], version, false, None, None, None)
            .0
            .blocks
            .unwrap_or_default()
    }

    /// Where a replay is critical, as ranges of places in it: each event
    /// there saw everything replayed before it (its parents are all of it),
    /// and everything replayed after it descends from it. There the walk
    /// needs no history at all.
    ///
    /// Found in one pass: an event is a candidate if its parents are the
    /// whole frontier so far, and a later event that doesn't descend from a
    /// candidate rules it out. A later event descends from a candidate if
    /// any parent was replayed at or after it; candidates form a chain, so
    /// the ones ruled out are the latest. Within a span, each event's only
    /// parent is the one before, replayed just before it: none rules
    /// anything out, and each sees everything if the first did.
    fn critical_points(
        &self,
        order: &[(Range<Lv>, bool)],
        old: &[Range<Lv>],
        new: &[Range<Lv>],
        base: &[Lv],
    ) -> Vec<Range<usize>> {
        // Where an event was replayed, if it was before `i`: `order` is
        // `old` then `new`, each sorted.
        let starts = |spans: &[Range<Lv>]| -> Vec<usize> {
            spans
                .iter()
                .scan(0, |n, span| {
                    let start = *n;
                    *n += span.len();
                    Some(start)
                })
                .collect()
        };
        let (old_at, new_at) = (starts(old), starts(new));
        let old_len: usize = old.iter().map(|s| s.len()).sum();
        let find = |spans: &[Range<Lv>], at: &[usize], lv: Lv| {
            let k = spans.partition_point(|s| s.end <= lv);
            spans
                .get(k)
                .filter(|s| s.start <= lv)
                .map(|s| at[k] + lv - s.start)
        };
        let replayed_at = |lv: Lv, i: usize| {
            find(old, &old_at, lv)
                .or_else(|| find(new, &new_at, lv).map(|k| old_len + k))
                .filter(|&k| k < i)
        };
        let mut frontier: Vec<Lv> = base.to_vec();
        frontier.sort_unstable();
        let mut parents: Vec<Lv> = Vec::new();
        let mut candidates: Vec<Range<usize>> = Vec::new();
        let mut i = 0;
        for (span, _) in order {
            parents.clear();
            parents.extend(self.parents(span.start));
            parents.sort_unstable();
            // Rule out the candidates it doesn't descend from: those after
            // its latest parent.
            match parents.iter().filter_map(|&p| replayed_at(p, i)).max() {
                None => candidates.clear(),
                Some(latest) => {
                    while let Some(c) = candidates.last_mut() {
                        if c.start > latest {
                            candidates.pop();
                        } else {
                            c.end = c.end.min(latest + 1);
                            break;
                        }
                    }
                }
            }
            let last = span.end - 1;
            if parents == frontier {
                // The usual case, typing: the frontier becomes just this.
                frontier.clear();
                frontier.push(last);
                match candidates.last_mut() {
                    Some(c) if c.end == i => c.end = i + span.len(),
                    _ => candidates.push(i..i + span.len()),
                }
            } else {
                frontier.retain(|f| !parents.contains(f));
                frontier.push(last);
                frontier.sort_unstable();
            }
            i += span.len();
        }
        // Never at an undelete, nor anywhere from the delete it undoes to
        // it: a fresh start forgets the records, and the item it brings
        // back must still have one.
        let mut forbidden: Vec<Range<usize>> = Vec::new();
        let mut at = 0;
        for (span, _) in order {
            if self.kind(span.start) == Kind::Undelete {
                for (k, u) in span.clone().enumerate() {
                    let d = self.undeleted(u);
                    let from =
                        replayed_at(d, at + k).expect("a merge replays an undelete's delete");
                    forbidden.push(from..at + k + 1);
                }
            }
            at += span.len();
        }
        without(candidates, forbidden)
    }
}

impl<'a, T: Clone> Walk<'a, T> {
    /// A walk whose prepare version is `base`, with the document there as
    /// the placeholder.
    fn new(log: &'a OpLog<T>, base: Vec<Lv>, base_ids: Option<Ids>) -> Self {
        let mut records = SumTree::new();
        records.push(Record::base_run(0, UNDERWATER));
        Walk {
            log,
            records,
            prepared: base.clone(),
            starts: BTreeMap::new(),
            targets: HashMap::new(),
            undos: HashMap::new(),
            undone: HashSet::new(),
            base,
            base_ids,
            known: None,
            marked: false,
            killers: HashMap::new(),
            replayed: 0,
            stepped: 0,
            checkouts: 0,
            fresh: true,
            marks: HashMap::new(),
        }
    }
}

impl<'a, T: Clone> Walk<'a, T> {
    fn step(&mut self, lv: Lv, out: Option<&mut Changes<'a, T>>) {
        self.replayed += 1;
        self.stepped += 1;
        self.fresh = false;
        let event = self.log.event(lv);
        // Move the prepare version to this event's parents: take back what
        // it shouldn't hold, latest first, then add what it's missing,
        // earliest first, so a delete is never undone before its insert.
        // Both versions hold the base, so the walk between them stops
        // there; for typing, the parents are the last event, and there's
        // nothing to move.
        let (retreat, mut advance) = self.log.diff(&self.prepared, &event.parents);
        self.shift_all(&retreat, false);
        advance.reverse();
        self.shift_all(&advance, true);
        match event.op {
            Op::Insert { pos, .. } => self.insert(lv, pos, out),
            Op::Delete { pos } => self.delete(lv, pos, out),
            Op::Undelete { target } => self.undelete(target, out),
            Op::Move { .. } => unreachable!("a log with moves takes the moves path"),
            Op::Mark { pos, level } => self.mark(lv, pos, level, out),
        }
        self.prepared = vec![lv];
    }

    /// Replays events `lv..lv + n`, one run in the log: each the only child
    /// of the one before, carrying on what it did. The first steps as any
    /// event does; the rest need no shifting (their parents are the
    /// prepare version by then), and nothing can come between their items
    /// (anything that could would include them, and so replay after), so
    /// typing extends one record, and deleting forward deletes what's
    /// there, a record at a time.
    fn step_run(&mut self, lv: Lv, n: usize, mut out: Option<&mut Changes<'a, T>>) {
        self.step(lv, out.as_deref_mut());
        if n == 1 {
            return;
        }
        let rest = lv + 1..lv + n;
        let done = match self.log.kind(lv) {
            Kind::Insert => self.carry_on_typing(lv, n - 1, out.as_deref_mut()),
            Kind::Delete => {
                self.carry_on_deleting(lv, n - 1, out.as_deref_mut());
                true
            }
            // One at a time.
            Kind::Undelete | Kind::Move | Kind::Mark => false,
        };
        if done {
            self.replayed += n - 1;
            self.prepared = vec![lv + n - 1];
        } else {
            for x in rest {
                self.step(x, out.as_deref_mut());
            }
        }
    }

    /// Puts in the `n` inserts after `lv`, which just went in, by extending
    /// its record, if its keys can carry on that far before what's next.
    /// Whether it did.
    fn carry_on_typing(&mut self, lv: Lv, n: usize, out: Option<&mut Changes<'a, T>>) -> bool {
        let (&first, (run_key, len)) = self
            .starts
            .range(..=lv)
            .next_back()
            .expect("the insert just started or carried on a run");
        debug_assert_eq!(first + len, lv + 1, "the insert ends its run");
        let key = self.key_of(lv);
        let last = run_key.extended(lv + n - first);
        let mut cursor = self.records.cursor::<Counts>();
        cursor.seek(&At(&key), Bias::Left);
        let record = cursor.item().expect("the insert is in the tree");
        debug_assert!(
            record.last == key && record.prepare == Prepare::Inserted && record.dead == 0
        );
        // New items aren't in the document the merge started from: they
        // can't join a record that is.
        if self.marked && record.in_from {
            return false;
        }
        cursor.next();
        // Defensive, as in `extend`.
        if cursor.item().is_some_and(|next| next.key <= last) {
            return false;
        }
        self.records.update_at(&At(&key), Bias::Left, |r| {
            r.len += n;
            r.last = last;
        });
        self.starts.get_mut(&first).expect("found above").1 += n;
        if let Some(out) = out {
            out.insert_run(self.effect_pos(&key, lv) + 1, lv + 1, n);
        }
        true
    }

    /// The `n` deletes after `lv`, each at the same place as it: the next
    /// `n` items the prepare version shows there, a record at a time.
    fn carry_on_deleting(&mut self, lv: Lv, n: usize, mut out: Option<&mut Changes<'a, T>>) {
        let Op::Delete { pos } = self.log.op(lv) else {
            unreachable!("a run of deletes")
        };
        let mut x = lv + 1;
        while x <= lv + n {
            let (record, offset) = {
                let mut cursor = self.records.cursor::<Counts>();
                cursor.seek(&Prepared(pos), Bias::Right);
                let record = cursor
                    .item()
                    .expect("the placeholder never runs out")
                    .clone();
                (record, pos - cursor.start().prepared)
            };
            // The item there is the first the prepare version shows after
            // the one just deleted, and a record's items are all in one
            // state: so it starts its record.
            debug_assert_eq!(offset, 0, "an item after a deleted one starts its record");
            let m = (lv + n + 1 - x).min(record.len);
            if m < record.len {
                self.split(&record, m);
            }
            let id = record.id;
            let key = record.item_key(id);
            let was_dead = self
                .records
                .update_at(&At(&key), Bias::Left, |r| {
                    r.prepare = Prepare::Deleted(1);
                    r.dead += 1;
                    r.dead > 1
                })
                .expect("the items are in the tree");
            for k in 0..m {
                self.targets.insert(x + k, id + k);
                self.note_delete(x + k, id + k, out.as_deref_mut());
            }
            if !was_dead && let Some(out) = out.as_deref_mut() {
                out.delete_run(self.effect_pos(&key, id), m);
            }
            x += m;
        }
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

    /// Advances (`forward`) or retreats `events`, in order. Runs of them
    /// shift together: inserts one after another, whose items are too, and
    /// deletes of items side by side (deleting forward, or backspacing).
    /// Each run costs a split at either end and a change per record, not
    /// per item, and leaves its items in as few records as they were.
    fn shift_all(&mut self, events: &[Lv], forward: bool) {
        let mut i = 0;
        while i < events.len() {
            let kind = self.log.kind(events[i]);
            if kind == Kind::Undelete {
                self.shift_undelete(events[i], forward);
                i += 1;
                continue;
            }
            // A mark shows nothing and hides nothing.
            if kind == Kind::Mark {
                i += 1;
                continue;
            }
            let insert = kind == Kind::Insert;
            // The item each event shifts.
            let item = |x: Lv| if insert { x } else { self.targets[&x] };
            let first = item(events[i]);
            let (mut lo, mut hi) = (first, first);
            let mut j = i + 1;
            // Which way the items go, once there are two.
            let mut step: Option<bool> = None;
            while let Some(&x) = events.get(j)
                && self.log.kind(x) == kind
            {
                let next = item(x);
                let up = match step {
                    Some(up) => up,
                    None if next == hi + 1 => true,
                    None if next + 1 == lo => false,
                    None => break,
                };
                if up && next == hi + 1 {
                    hi = next;
                } else if !up && next + 1 == lo {
                    lo = next;
                } else {
                    break;
                }
                step = Some(up);
                j += 1;
            }
            self.shift_items(lo, hi, insert, forward);
            i = j;
        }
    }

    /// Advances (`forward`) or retreats an insert of, or a delete of each
    /// of, items `lo..=hi`.
    fn shift_items(&mut self, lo: usize, hi: usize, insert: bool, forward: bool) {
        let mut id = lo;
        while id <= hi {
            let key = self.item_key(id);
            let (record, _) = self.record_at(&key);
            // This record's share of the items: split it out.
            let end = (record.id + record.len).min(hi + 1);
            if end < record.id + record.len {
                self.split(&record, end - record.id);
            }
            if id > record.id {
                self.split(&record, id - record.id);
            }
            self.records.update_at(&At(&key), Bias::Left, |r| {
                r.prepare = match (insert, forward, r.prepare) {
                    (true, true, Prepare::NotYet) => Prepare::Inserted,
                    (true, false, Prepare::Inserted) => Prepare::NotYet,
                    (false, true, Prepare::Inserted) => Prepare::Deleted(1),
                    (false, true, Prepare::Deleted(n)) => Prepare::Deleted(n + 1),
                    (false, false, Prepare::Deleted(1)) => Prepare::Inserted,
                    (false, false, Prepare::Deleted(n)) => Prepare::Deleted(n - 1),
                    (_, _, state) => unreachable!("shifting item {id} from {state:?}"),
                };
            });
            id = end;
        }
    }

    /// The key of item `id`: an insert's, or the base document's.
    fn item_key(&self, id: usize) -> Key {
        if id >= BASE_ITEM {
            Key::base_item(id - BASE_ITEM)
        } else {
            self.key_of(id)
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
        self.records.splice_at(&At(&record.key), Bias::Left, |r| {
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
            ((), [tail])
        });
    }

    /// How many items the effect version shows before item `id`, whose key
    /// is `key`.
    fn effect_pos(&self, key: &Key, id: usize) -> usize {
        let (record, before) = self.record_at(key);
        before.effect + if record.dead > 0 { 0 } else { id - record.id }
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
                    dead: 0,
                    in_from: false,
                };
                match &next {
                    // In front of `next`: swap in, and it goes after.
                    Some(n) => {
                        self.records.splice_at(&At(n), Bias::Left, |r| {
                            ((), [std::mem::replace(r, record)])
                        });
                    }
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
            && record.dead == 0
            // A new item isn't in the document the merge started from.
            && !(self.marked && record.in_from);
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
        let was_dead = self
            .records
            .update_at(&At(&key), Bias::Left, |r| {
                r.prepare = Prepare::Deleted(1);
                r.dead += 1;
                r.dead > 1
            })
            .expect("the item is in the tree");
        self.targets.insert(lv, id);
        let mut out = out;
        self.note_delete(lv, id, out.as_deref_mut());
        // Already deleted in the effect version by a concurrent delete:
        // nothing more to do to the document.
        if !was_dead && let Some(out) = out {
            out.delete(self.effect_pos(&key, id));
        }
    }

    /// A mark of the item at `pos`: it wins if it's the latest of the item's
    /// marks so far.
    fn mark(&mut self, lv: Lv, pos: usize, level: u8, out: Option<&mut Changes<'a, T>>) {
        let (key, id) = self.prepared_item(pos, true);
        let me = (self.log.lamport(lv), self.log.order_key(lv));
        if self.marks.get(&id).is_some_and(|won| *won > me) {
            return;
        }
        self.marks.insert(id, me);
        if let Some(out) = out {
            let item = self.identity(id);
            let (record, _) = self.record_at(&key);
            let pos = (record.dead == 0).then(|| self.effect_pos(&key, id));
            out.mark(item, pos, level);
        }
    }

    /// Undoes delete `d`: the item it removed shows again, in both
    /// versions, if nothing else still removes it there.
    fn undelete(&mut self, d: Lv, out: Option<&mut Changes<'a, T>>) {
        let count = self.undos.get(&d).copied().unwrap_or(0) + 1;
        self.undos.insert(d, count);
        let item = self.targets[&d];
        let first_here = count == 1;
        // Already undone in the effect version by a concurrent undelete.
        let first_anywhere = self.undone.insert(d);
        let revived = self.update_item(item, |r| {
            if first_here {
                r.prepare = match r.prepare {
                    Prepare::Deleted(1) => Prepare::Inserted,
                    Prepare::Deleted(n) => Prepare::Deleted(n - 1),
                    state => unreachable!("undoing delete {d} of an item {state:?}"),
                };
            }
            if first_anywhere {
                r.dead -= 1;
            }
            first_anywhere && r.dead == 0
        });
        if revived && let Some(out) = out {
            let id = self.identity(item);
            out.insert(self.effect_pos(&self.item_key(item), item), id);
        }
    }

    /// Adds (`forward`) or takes back undelete `x` in the prepare version:
    /// its delete is live there while no undelete of it is.
    fn shift_undelete(&mut self, x: Lv, forward: bool) {
        let d = self.log.undeleted(x);
        let before = self.undos.get(&d).copied().unwrap_or(0);
        let after = if forward { before + 1 } else { before - 1 };
        self.undos.insert(d, after);
        // Only the first undelete of a delete changes anything.
        if (before == 0) == (after == 0) {
            return;
        }
        self.update_item(self.targets[&d], |r| {
            r.prepare = match (forward, r.prepare) {
                (true, Prepare::Deleted(1)) => Prepare::Inserted,
                (true, Prepare::Deleted(n)) => Prepare::Deleted(n - 1),
                (false, Prepare::Inserted) => Prepare::Deleted(1),
                (false, Prepare::Deleted(n)) => Prepare::Deleted(n + 1),
                (_, state) => unreachable!("shifting undelete {x} from {state:?}"),
            };
        });
    }

    /// Splits item `id` out of its record and changes it alone.
    fn update_item<R>(&mut self, id: usize, f: impl FnOnce(&mut Record) -> R) -> R {
        let key = self.item_key(id);
        let (record, _) = self.record_at(&key);
        if id + 1 < record.id + record.len {
            self.split(&record, id + 1 - record.id);
        }
        if id > record.id {
            self.split(&record, id - record.id);
        }
        self.records
            .update_at(&At(&key), Bias::Left, f)
            .expect("the item is in the tree")
    }

    /// The insert item `id` came from: a base item's is found by its place
    /// in the base document.
    fn identity(&mut self, id: usize) -> Lv {
        if let Some(lv) = self.known_identity(id) {
            return lv;
        }
        // Not known: from the history, by a checkout of the base.
        self.checkouts += 1;
        let ids = self.log.ids_at(&self.base);
        let lv = ids.get(id - BASE_ITEM);
        self.base_ids = Some(ids);
        lv
    }

    /// The insert item `id` came from, if it's known without a checkout:
    /// a replayed insert's own, or a base item's from the identities of
    /// the base document, or of the document the merge started from.
    fn known_identity(&self, id: usize) -> Option<Lv> {
        if id < BASE_ITEM {
            return Some(id);
        }
        if let Some(ids) = &self.base_ids {
            return Some(ids.get(id - BASE_ITEM));
        }
        let (ids, removed) = self.known.as_ref().filter(|_| self.marked)?;
        let (record, before) = self.record_at(&self.item_key(id));
        if record.in_from {
            Some(ids.get(before.from + (id - record.id)))
        } else {
            // Removed by a delete the document already had.
            removed.get(self.killers.get(&id)?).copied()
        }
    }

    /// From here on, records know whether they show in the document the
    /// merge started from: it's the effect version now.
    fn mark_from(&mut self) {
        if !self.fresh {
            self.records = self
                .records
                .iter()
                .map(|r| Record {
                    in_from: r.dead == 0,
                    ..r.clone()
                })
                .collect();
        }
        self.marked = true;
    }

    /// Notes what delete `lv` removed: item `id`. A new delete's item is
    /// learned for the replica's identities; an old one's base item, for
    /// finding it again here.
    fn note_delete(&mut self, lv: Lv, id: usize, out: Option<&mut Changes<'a, T>>) {
        match out {
            Some(out) => {
                if let Some(item) = self.known_identity(id) {
                    out.learned.push((lv, item));
                }
            }
            None if id >= BASE_ITEM => {
                self.killers.insert(id, lv);
            }
            None => {}
        }
    }
}

/// `ranges` (sorted, apart) with every place in `cut` taken out.
fn without(ranges: Vec<Range<usize>>, mut cut: Vec<Range<usize>>) -> Vec<Range<usize>> {
    if cut.is_empty() {
        return ranges;
    }
    cut.sort_unstable_by_key(|c| c.start);
    let mut out = Vec::new();
    for range in ranges {
        let mut start = range.start;
        for c in cut
            .iter()
            .filter(|c| c.end > range.start && c.start < range.end)
        {
            if c.start > start {
                out.push(start..c.start);
            }
            start = start.max(c.end);
        }
        if start < range.end {
            out.push(start..range.end);
        }
    }
    out
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
        let count = |spans: &[Range<Lv>]| spans.iter().map(|s| s.len()).sum::<usize>();
        assert_eq!((count(&old), count(&new)), (4, 1));
        let mut doc = log.checkout(&from);
        for change in log.changes(&from, &n) {
            change.apply(&mut doc);
        }
        assert_eq!(doc, log.checkout(&log.union(&from, &n)));
    }

    /// One person typing and deleting never needs the full step: each
    /// event saw everything before it, and everything after sees it.
    #[test]
    fn typing_alone_is_all_critical() {
        let mut log: OpLog<char> = OpLog::new();
        let alice = log.agent("alice");
        let mut v = log.add_insert(alice, &[], 0, "hello world".chars());
        v = log.add_delete(alice, &v, 5..11);
        v = log.add_insert(alice, &v, 0, "oh, ".chars());
        v = log.add_delete(alice, &v, 0..1);
        let (changes, stats) = log.walk(&[], &v);
        assert_eq!(stats.stepped, 0);
        assert_eq!(stats.replayed, log.len());
        // Each edit, one change.
        assert_eq!(changes.len(), 4);
        assert_eq!(log.checkout(&v).into_iter().collect::<String>(), "h, hello");
    }

    /// Concurrent typing steps a run at a time: each run's first event in
    /// full, the rest carrying on from it.
    #[test]
    fn a_concurrent_run_steps_once() {
        let mut log: OpLog<char> = OpLog::new();
        let (alice, bob) = (log.agent("alice"), log.agent("bob"));
        let a = log.add_insert(alice, &[], 0, "hello".chars());
        let b = log.add_insert(bob, &[], 0, "world".chars());
        let mut gone = log.add_delete(bob, &b, 1..4);
        gone = log.union(&a, &gone);
        let (_, stats) = log.walk(&[], &gone);
        assert_eq!(stats.replayed, log.len());
        assert!(stats.stepped <= 3, "{} stepped", stats.stepped);
        assert_eq!(log.checkout(&gone), crate::reference::checkout(&log, &gone));
    }

    /// A fork from inside a run: an event whose parents are partway along
    /// someone's typing, with another branch already going. Syncing whole
    /// logs never makes one, but syncing part of one can. Which ops show a
    /// mistake depends on how the two roots order, so try several.
    #[test]
    fn a_fork_from_inside_a_run() {
        for fork in [
            Op::Insert { pos: 0, item: 'y' },
            Op::Insert { pos: 1, item: 'y' },
            Op::Insert { pos: 2, item: 'y' },
            Op::Delete { pos: 0 },
            Op::Delete { pos: 1 },
        ] {
            let mut log: OpLog<char> = OpLog::new();
            let (bob, alice, carol) = (log.agent("bob"), log.agent("alice"), log.agent("carol"));
            log.add_event(bob, 0, &[], Op::Insert { pos: 0, item: 'x' });
            for (k, c) in "abc".chars().enumerate() {
                let parents: Vec<Lv> = k.checked_sub(1).map(|p| p + 1).into_iter().collect();
                log.add_event(alice, k as u64, &parents, Op::Insert { pos: k, item: c });
            }
            assert_eq!(log.runs(), 2, "alice's typing is one run");
            // Carol saw bob's and alice's first, not the rest of her run.
            log.add_event(carol, 0, &[0, 1], fork.clone());
            for v in [vec![4], vec![3, 4], vec![0, 4]] {
                assert_eq!(
                    log.checkout(&v),
                    crate::reference::checkout(&log, &v),
                    "{fork:?} at {v:?}"
                );
            }
        }
    }

    /// A replica that keeps its identities never needs a checkout to undo:
    /// not for its own undo, not for one from elsewhere of a delete it
    /// already has, and not after many fresh starts. Without them, the
    /// remote undo needs one.
    #[test]
    fn identities_spare_checkouts() {
        let mut log: OpLog<char> = OpLog::new();
        let (alice, bob) = (log.agent("alice"), log.agent("bob"));
        let mut ids = Identities::new();
        let typed = log.add_insert(alice, &[], 0, "hello world".chars());
        ids.insert(0, 0, 11);
        // Bob types at the end meanwhile, so merges have old events.
        let theirs = log.add_insert(bob, &typed, 11, "!".chars());
        let gone = log.add_delete(alice, &typed, 1..4);
        ids.delete(1, gone[0] - 2, 3);
        // Alice's replica: her own undo.
        let undone = log.add_undelete(alice, &gone, gone[0] - 2..=gone[0]);
        let mut mine = ids.clone();
        let (out, stats) = log.walk_into(&gone, &undone, true, Some(&mine), None);
        assert_eq!(stats.checkouts, 0, "local undo");
        drop(out);
        log.changes_with(&gone, &undone, &mut mine);

        // Bob's replica: has the delete, merged with his "!"; the undo
        // arrives.
        let mut bobs = Identities::new();
        log.changes_with(&[], &theirs, &mut bobs);
        let both = log.union(&theirs, &gone);
        log.changes_with(&theirs, &both, &mut bobs);
        let all = log.union(&both, &undone);
        let (_, stats) = log.walk_into(&both, &all, true, Some(&bobs), None);
        assert_eq!(stats.checkouts, 0, "remote undo");
        let (_, stats) = log.walk_into(&both, &all, false, None, None);
        assert!(stats.checkouts > 0, "without identities, a checkout");
        let mut doc = log.checkout(&both);
        for change in log.changes_with(&both, &all, &mut bobs) {
            change.apply(&mut doc);
        }
        assert_eq!(doc.into_iter().collect::<String>(), "hello world!");

        // Many fresh starts later, undoing the same text again.
        let again = log.add_delete(alice, &all, 1..4);
        let mut v = again.clone();
        let mut ids = Identities::new();
        log.changes_with(&[], &v, &mut ids);
        for k in 0..40 {
            let len = ids.len();
            v = log.add_insert(alice, &v, len, [char::from(b'a' + k % 26)]);
            ids.insert(len, v[0], 1);
        }
        let back = log.add_undelete(alice, &v, again[0] - 2..=again[0]);
        let (_, stats) = log.walk_into(&v, &back, true, Some(&ids), None);
        assert_eq!(stats.checkouts, 0, "undo after fresh starts");
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
                Change::Delete { .. } | Change::Move { .. } | Change::Mark { .. } => 0,
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
