//! The fast window: a stretch of a merge with moves, the simple walk's way
//! (see `simple.rs`), with its records in a sum tree.
//!
//! The simple walk keeps its records in a list, and answers every question
//! about the document ("which item is at this position?", "where is this
//! item?") by drawing the whole list. This answers the same questions from
//! the tree, without drawing anything.
//!
//! # How a position is found
//!
//! Records stay in home order, each with an order key that never changes
//! (see `key.rs`), and the tree sums how many items each version shows. A
//! move's interval is the stretch between two keys, so which moves are
//! active, and how their intervals nest, depends only on which moves each
//! version holds: it's laid out again only when that changes.
//!
//! The base's blocks are drawn in place: they never reorder anything. Only
//! the window's own active moves do, each cutting its interval out of where
//! it lies and drawing it at its portal. So a version's document is its
//! home order with those few intervals cut and pasted, and how many items
//! it shows before a record is a home-order sum from the tree, less what
//! was cut out before it, plus what was pasted in: a handful of seeks for
//! each of the window's active moves. Nothing is kept up to date by hand.
//!
//! In debug builds the merge runs the simple walk's window beside this one
//! and checks they make the same changes, step by step: unless built with
//! the `perf` feature, for timing a debug build when speed is the focus.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use mamre::cursor::Bias;
use mamre::{Item, SeekTarget, SumTree, Summary};

use crate::blocks::{Block, Blocks, Edges, MARKER, Piece};
use crate::ids::Ids;
use crate::key::Key;
use crate::oplog::{Kind, Lv, Op, OpLog};
use crate::simple::{Map, Set};
use crate::walk::{BASE_ITEM, Changes, UNDERWATER, cmp_right};

/// A record's state in the prepare version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prepare {
    /// Its insert isn't in the version: concurrent, or later.
    NotYet,
    Inserted,
    /// Deleted by this many deletes in the version.
    Deleted(u32),
}

/// One inserted item, a run of the base document's items, or a portal.
#[derive(Clone, Debug)]
struct Record {
    /// The key of its first item (a portal's own key), and of its last.
    key: Key,
    last: Key,
    /// The insert's local version, or `BASE_ITEM` plus the first base
    /// item's position, or (a portal) the move's.
    id: usize,
    /// 1, except for a run of base items, and 0 for a portal.
    len: usize,
    /// The keys of the items either side of where it was put.
    left: Option<Key>,
    right: Option<Key>,
    prepare: Prepare,
    /// How many deletes replayed so far removed it, not undone: it shows in
    /// the effect version if none.
    dead: u32,
    portal: bool,
}

impl Record {
    /// The key of item `id`, which this record holds.
    fn item_key(&self, id: usize) -> Key {
        debug_assert!(id >= self.id && id < self.id + self.len.max(1));
        if (BASE_ITEM..MARKER).contains(&self.id) {
            Key::base_item(id - BASE_ITEM)
        } else {
            self.key.clone()
        }
    }
}

/// What a run of records holds: how many items each version shows, how
/// many records the prepare version has (shown or not, portals too), and
/// the key of the last.
#[derive(Clone, Debug, Default, PartialEq)]
struct Sums {
    prepared: usize,
    effect: usize,
    present: usize,
    last: Option<Key>,
}

impl Summary for Sums {
    fn empty() -> Self {
        Sums::default()
    }
    fn concat(&mut self, other: &Self) {
        self.prepared += other.prepared;
        self.effect += other.effect;
        self.present += other.present;
        if other.last.is_some() {
            self.last.clone_from(&other.last);
        }
    }
}

impl Item for Record {
    type Summary = Sums;
    fn summary(&self) -> Sums {
        Sums {
            prepared: if self.prepare == Prepare::Inserted {
                self.len
            } else {
                0
            },
            effect: if self.dead > 0 { 0 } else { self.len },
            present: (self.prepare != Prepare::NotYet) as usize,
            last: Some(self.last.clone()),
        }
    }
}

/// Seeks to the record holding the item (or portal) with this key: with
/// `Bias::Left`, the first whose last key reaches it.
struct At<'k>(&'k Key);

impl SeekTarget<Sums, Sums> for At<'_> {
    fn cmp_position(&self, position: &Sums) -> Ordering {
        match &position.last {
            Some(last) => self.0.cmp(last),
            None => Ordering::Greater,
        }
    }
}

/// Seeks, with `Bias::Right`, to the record holding the `n`th item a
/// version shows.
struct Shown(In, usize);

impl SeekTarget<Sums, Sums> for Shown {
    fn cmp_position(&self, position: &Sums) -> Ordering {
        self.1.cmp(&count(position, self.0))
    }
}

/// Seeks, with `Bias::Right`, to the `n`th record the prepare version has.
struct Present(usize);

impl SeekTarget<Sums, Sums> for Present {
    fn cmp_position(&self, position: &Sums) -> Ordering {
        self.0.cmp(&position.present)
    }
}

/// Which version a question is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum In {
    Prepare,
    Effect,
}

/// How many items `sums` shows in version `v`.
fn count(sums: &Sums, v: In) -> usize {
    match v {
        In::Prepare => sums.prepared,
        In::Effect => sums.effect,
    }
}

/// A move that does something: the first and last item of its interval,
/// and whether it's one of the base's, drawn in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Moved {
    id: Lv,
    first: usize,
    last: usize,
    in_place: bool,
}

/// An active move, with its interval's keys, and its portal's.
#[derive(Clone, Debug)]
struct Active {
    m: Moved,
    lo: Key,
    hi: Key,
    portal: Option<Key>,
}

/// Active moves laid out: as no two cross, their intervals nest.
struct Frames {
    active: Vec<Active>,
    by_id: HashMap<Lv, usize>,
    /// The intervals by where they start, the outer first; and their starts.
    order: Vec<usize>,
    starts: Vec<Key>,
    /// The innermost other interval holding each one.
    parent: Vec<Option<usize>>,
    /// The innermost portaled interval strictly holding each one: what it's
    /// cut out of, if it's portaled itself.
    cut_from: Vec<Option<usize>>,
    /// Each portaled interval's portal's frame: where it's pasted.
    pasted_in: Vec<Option<usize>>,
    /// For each frame (`None`, the document; or a portaled interval), the
    /// portaled intervals cut out of it, and those pasted into it, each in
    /// home order.
    cuts: HashMap<Option<usize>, Vec<usize>>,
    pastes: HashMap<Option<usize>, Vec<usize>>,
}

impl Frames {
    /// The innermost interval holding the record with key `key`.
    fn innermost(&self, key: &Key) -> Option<usize> {
        let k = self.starts.partition_point(|s| s <= key);
        let mut c = self.order[..k].last().copied();
        while let Some(i) = c {
            if self.active[i].hi >= *key {
                return Some(i);
            }
            c = self.parent[i];
        }
        None
    }

    /// The frame the record with key `key` is drawn in: the innermost
    /// portaled interval holding it, or the document.
    fn drawn_in(&self, key: &Key) -> Option<usize> {
        let mut c = self.innermost(key);
        while let Some(i) = c {
            if self.active[i].portal.is_some() {
                return Some(i);
            }
            c = self.parent[i];
        }
        None
    }

    fn cuts(&self, frame: Option<usize>) -> &[usize] {
        self.cuts.get(&frame).map_or(&[], |v| v)
    }

    fn pastes(&self, frame: Option<usize>) -> &[usize] {
        self.pastes.get(&frame).map_or(&[], |v| v)
    }

    fn portal(&self, i: usize) -> &Key {
        self.active[i].portal.as_ref().expect("a portaled interval")
    }

    /// Where `a` is drawn: its portal, or (in place) itself.
    fn location(&self, i: usize) -> (Key, Key) {
        let a = &self.active[i];
        match &a.portal {
            Some(p) => (p.clone(), p.clone()),
            None => (a.lo.clone(), a.hi.clone()),
        }
    }
}

/// Where the base items' identities come from: given, or found when first
/// asked.
struct BaseIds {
    frontier: Vec<Lv>,
    ids: Option<Ids>,
}

/// A stretch of a merge with moves: see the module's docs.
pub(crate) struct Window<'a, T> {
    log: &'a OpLog<T>,
    records: SumTree<Record>,
    /// The key of each replayed insert's item, and of each portal.
    keys: Map<Lv, Key>,
    /// The prepare and effect versions beyond the base, as sets.
    prepared: Set,
    effect: Set,
    /// The prepare version, as its latest events: the base at first, and
    /// after each step, the event stepped.
    at: Vec<Lv>,
    /// Which item each delete replayed so far removed.
    targets: Map<Lv, usize>,
    /// How many undeletes of each delete the prepare version holds.
    undos: Map<Lv, u32>,
    /// The deletes undone in the effect version.
    undone: Set,
    base_ids: BaseIds,
    /// The moves that do something, in the order activity is decided in.
    moves: Vec<Moved>,
    /// How many of `moves` are the base's, and which of them are active
    /// among themselves: they're decided first, so it's decided once.
    in_place: usize,
    base_active: Vec<Moved>,
    /// Each version's last answer to which moves are active, with its
    /// candidates, and the layout of those moves.
    active_cache: RefCell<[Option<(Vec<Moved>, Rc<Vec<Moved>>, Rc<Frames>)>; 2]>,
    /// Each marked item's winning mark among those replayed.
    marks: HashMap<usize, (u64, (&'a str, u64))>,
    /// Each base block's length and hidden edges, as laid.
    laid: Map<Lv, (usize, Edges)>,
    /// Where what the effect version shows has changed: records put in,
    /// deleted or brought back, and the ends of moves replayed. A base
    /// block with none of these inside it shows what it did.
    changed: BTreeSet<Key>,
}

impl<'a, T: Clone> Window<'a, T> {
    /// A window from the document at `frontier`, whose items' identities are
    /// `ids` (found from nothing when first asked, if not given) and whose
    /// moved blocks are `blocks`.
    pub(crate) fn new(
        log: &'a OpLog<T>,
        frontier: Vec<Lv>,
        ids: Option<Ids>,
        blocks: &Blocks,
    ) -> Self {
        let mut w = Window {
            log,
            records: SumTree::new(),
            keys: Map::default(),
            prepared: Set::default(),
            effect: Set::default(),
            at: frontier.clone(),
            targets: Map::default(),
            undos: Map::default(),
            undone: Set::default(),
            base_ids: BaseIds { frontier, ids },
            moves: Vec::new(),
            in_place: 0,
            base_active: Vec::new(),
            active_cache: RefCell::new([None, None]),
            marks: HashMap::new(),
            laid: Map::default(),
            changed: BTreeSet::new(),
        };
        w.lay_base(blocks);
        w
    }

    /// The placeholder, laid out from the base's blocks (see `Blocks::lay`):
    /// the base document's items, cut where blocks start and end, with a
    /// marker at each hidden edge; and the blocks as in-place moves.
    fn lay_base(&mut self, blocks: &Blocks) {
        self.laid = blocks
            .0
            .iter()
            .map(|b| (b.lv, (b.span.len(), b.hidden)))
            .collect();
        let laid = blocks.lay();
        // Built at once: every merge lays its base anew.
        let mut records: Vec<Record> = Vec::with_capacity(laid.pieces.len());
        for (i, piece) in laid.pieces.iter().enumerate() {
            let record = match piece {
                Piece::Items(r) => Record {
                    key: Key::base_item(r.start),
                    last: Key::base_item(r.end - 1),
                    id: BASE_ITEM + r.start,
                    len: r.len(),
                    left: None,
                    right: None,
                    prepare: Prepare::Inserted,
                    dead: 0,
                    portal: false,
                },
                // What a block hides at its edge: nothing to show, ever.
                // Its key falls just before the items that follow it.
                &Piece::Marker(id) => {
                    let next = laid.pieces[i..].iter().find_map(|p| match p {
                        Piece::Items(r) => Some(Key::base_item(r.start)),
                        Piece::Marker(_) => None,
                    });
                    let prev = records.last().map(|r| r.last.clone());
                    let key = Key::between(prev.as_ref(), next.as_ref());
                    self.keys.insert(id, key.clone());
                    Record {
                        key: key.clone(),
                        last: key,
                        id,
                        len: 0,
                        left: None,
                        right: None,
                        prepare: Prepare::Deleted(1),
                        dead: 1,
                        portal: false,
                    }
                }
            };
            records.push(record);
        }
        self.records = SumTree::from_items(records);
        // In the order activity is decided in, as `add_move` would put
        // them.
        let order = |id: Lv| (self.log.lamport(id), self.log.order_key(id));
        let mut moves: Vec<(_, Moved)> = laid
            .moves
            .iter()
            .map(|&(id, first, last)| {
                let m = Moved {
                    id,
                    first,
                    last,
                    in_place: true,
                };
                (order(id), m)
            })
            .collect();
        moves.sort_by(|a, b| a.0.cmp(&b.0));
        self.moves = moves.into_iter().map(|(_, m)| m).collect();
        self.in_place = self.moves.len();
        // A document's blocks never cross, and a block drawn in place is
        // never inside itself: so deciding them in order only lets a later
        // block of the same items replace an earlier one.
        let mut active: Vec<Moved> = Vec::with_capacity(self.moves.len());
        let mut spans: HashMap<(usize, usize), ()> = HashMap::new();
        for &m in &self.moves {
            if spans.insert((m.first, m.last), ()).is_some() {
                let k = active
                    .iter()
                    .position(|o| (o.first, o.last) == (m.first, m.last))
                    .expect("seen");
                active.remove(k);
            }
            active.push(m);
        }
        #[cfg(all(debug_assertions, not(feature = "perf")))]
        assert_eq!(active, self.decide(&self.moves), "the base's moves");
        self.base_active = active;
    }

    /// Adds a move, in the order activity is decided in: Lamport time, then
    /// as concurrent inserts are ordered.
    fn add_move(&mut self, m: Moved) {
        let key = |id: Lv| (self.log.lamport(id), self.log.order_key(id));
        let k = self.moves.partition_point(|o| key(o.id) < key(m.id));
        self.moves.insert(k, m);
    }

    pub(crate) fn step(&mut self, lv: Lv, out: Option<&mut Changes<'a, T>>) {
        let event = self.log.event(lv);
        // Move the prepare version to this event's parents: take back what
        // it shouldn't hold, latest first, then add what it's missing,
        // earliest first, so a delete is never undone before its insert.
        // Both are the base and events stepped (every replayed event's
        // history is), so they differ only by stepped events, found by
        // walking back from both until they meet.
        let (back, on) = self.log.diff(&self.at, &event.parents);
        for x in back {
            debug_assert!(self.prepared.contains(&x));
            self.retreat(x);
            self.prepared.remove(&x);
        }
        for &x in on.iter().rev() {
            debug_assert!(!self.prepared.contains(&x));
            self.advance(x);
            self.prepared.insert(x);
        }
        self.at = vec![lv];
        match &event.op {
            Op::Insert { pos, .. } => self.insert(lv, *pos, out),
            Op::Delete { pos } => self.delete(lv, *pos, out),
            Op::Undelete { target } => self.undelete(*target, out),
            &Op::Move { pos, len, to } => self.move_(lv, pos, len, to, out),
            &Op::Mark { pos, level } => self.mark(lv, pos, level, out),
        }
        self.prepared.insert(lv);
        self.effect.insert(lv);
    }

    // ---- Records ----

    /// The key of item (or portal) `id`.
    fn item_key(&self, id: usize) -> Key {
        if (BASE_ITEM..MARKER).contains(&id) {
            Key::base_item(id - BASE_ITEM)
        } else {
            self.keys[&id].clone()
        }
    }

    /// The record holding the item with key `key`, and what's before it.
    fn record_at(&self, key: &Key) -> (Record, Sums) {
        let mut cursor = self.records.cursor::<Sums>();
        cursor.seek(&At(key), Bias::Left);
        let record = cursor.item().expect("the key is in the tree").clone();
        (record, cursor.start().clone())
    }

    fn update(&mut self, key: &Key, f: impl FnOnce(&mut Record)) {
        self.records
            .update_at(&At(key), Bias::Left, f)
            .expect("the key is in the tree");
    }

    /// How many items version `v` shows before the record holding `key`,
    /// and up to the end of it.
    fn before(&self, v: In, key: &Key) -> usize {
        let mut cursor = self.records.cursor::<Sums>();
        cursor.seek(&At(key), Bias::Left);
        count(cursor.start(), v)
    }

    fn through(&self, v: In, key: &Key) -> usize {
        let (record, before) = self.record_at(key);
        count(&before, v) + count(&record.summary(), v)
    }

    /// The record holding the `n`th item version `v` shows, counted in home
    /// order, and the item's place in it.
    fn nth(&self, v: In, n: usize) -> (Record, usize) {
        let mut cursor = self.records.cursor::<Sums>();
        cursor.seek(&Shown(v, n), Bias::Right);
        let record = cursor
            .item()
            .expect("the placeholder never runs out")
            .clone();
        let offset = n - count(cursor.start(), v);
        (record, offset)
    }

    /// The first record the prepare version has after the record with key
    /// `after` (`exclusive`), or from it.
    fn next_present(&self, from: Option<&Key>, exclusive: bool) -> Option<Record> {
        let n = match from {
            None => 0,
            Some(key) => {
                let (record, before) = self.record_at(key);
                before.present
                    + if exclusive {
                        record.summary().present
                    } else {
                        0
                    }
            }
        };
        let mut cursor = self.records.cursor::<Sums>();
        cursor.seek(&Present(n), Bias::Right);
        cursor.item().cloned()
    }

    fn retreat(&mut self, x: Lv) {
        self.shift(x, false);
    }

    fn advance(&mut self, x: Lv) {
        self.shift(x, true);
    }

    fn shift(&mut self, x: Lv, forward: bool) {
        match self.log.kind(x) {
            Kind::Undelete => return self.shift_undelete(x, forward),
            Kind::Move => return self.shift_portal(x, forward),
            Kind::Mark => return,
            _ => {}
        }
        let insert = self.log.is_insert(x);
        let key = if insert {
            self.item_key(x)
        } else {
            self.item_key(self.targets[&x])
        };
        self.update(&key, |r| {
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

    /// Move `x`'s portal joins (`forward`) or leaves the prepare version,
    /// if it has one (a move that does nothing has none).
    fn shift_portal(&mut self, x: Lv, forward: bool) {
        if let Some(key) = self.keys.get(&x).cloned() {
            self.update(&key, |r| {
                debug_assert!(r.portal);
                r.prepare = if forward {
                    Prepare::Inserted
                } else {
                    Prepare::NotYet
                };
            });
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
        let key = self.item_key(self.targets[&d]);
        self.update(&key, |r| {
            r.prepare = match (forward, r.prepare) {
                (true, Prepare::Deleted(1)) => Prepare::Inserted,
                (true, Prepare::Deleted(n)) => Prepare::Deleted(n - 1),
                (false, Prepare::Inserted) => Prepare::Deleted(1),
                (false, Prepare::Deleted(n)) => Prepare::Deleted(n + 1),
                (_, state) => unreachable!("shifting undelete {x} from {state:?}"),
            };
        });
    }

    /// The insert the item with key `key` came from: a base item's is found
    /// by its place in the base document.
    fn identity(&mut self, key: &Key) -> Lv {
        let (record, _) = self.record_at(key);
        if record.id < BASE_ITEM {
            return record.id;
        }
        debug_assert_eq!(record.len, 1, "an item asked about stands alone");
        let log = self.log;
        let BaseIds { frontier, ids } = &mut self.base_ids;
        ids.get_or_insert_with(|| log.ids_at(frontier))
            .get(record.id - BASE_ITEM)
    }

    /// Splits the base run holding item `id` so it starts there.
    fn split_at(&mut self, id: usize) {
        let key = Key::base_item(id - BASE_ITEM);
        self.records.splice_at(&At(&key), Bias::Left, |r| {
            debug_assert!(r.id >= BASE_ITEM && r.id < id && id < r.id + r.len);
            let mut tail = r.clone();
            tail.id = id;
            tail.len = r.id + r.len - id;
            tail.key = key.clone();
            r.len = id - r.id;
            r.last = Key::base_item(id - 1 - BASE_ITEM);
            ((), [tail])
        });
    }

    /// Splits item `offset` of `record` into a record of its own, so
    /// origins and deletes can name it; returns its key.
    fn isolate(&mut self, record: &Record, offset: usize) -> Key {
        let id = record.id + offset;
        if offset > 0 {
            self.split_at(id);
        }
        if record.len - offset > 1 {
            self.split_at(id + 1);
        }
        record.item_key(id)
    }

    // ---- Which moves are active ----

    fn intervals(&self, moves: &[Moved]) -> Vec<Active> {
        moves
            .iter()
            .map(|&m| Active {
                m,
                lo: self.item_key(m.first),
                hi: self.item_key(m.last),
                portal: (!m.in_place).then(|| self.keys[&m.id].clone()),
            })
            .collect()
    }

    /// Which moves are active in version `v`, by the reference's rules, and
    /// their layout.
    fn active(&self, v: In) -> (Rc<Vec<Moved>>, Rc<Frames>) {
        let has = |id: Lv| match v {
            In::Prepare => self.prepared.contains(&id),
            In::Effect => self.effect.contains(&id),
        };
        // The base's moves are always candidates, so the replayed ones the
        // version holds decide which are.
        let replayed: Vec<Moved> = self
            .moves
            .iter()
            .copied()
            .filter(|m| !m.in_place && has(m.id))
            .collect();
        let slot = (v == In::Effect) as usize;
        if let Some((c, answer, frames)) = &self.active_cache.borrow()[slot]
            && *c == replayed
        {
            return (answer.clone(), frames.clone());
        }
        let candidates: Vec<Moved> = self
            .moves
            .iter()
            .copied()
            .filter(|m| m.in_place || has(m.id))
            .collect();
        let answer = Rc::new(self.decide(&candidates));
        let frames = Rc::new(self.frames(&answer));
        self.active_cache.borrow_mut()[slot] = Some((replayed, answer.clone(), frames.clone()));
        (answer, frames)
    }

    /// The rules, over `candidates` in order: each decided against those
    /// before it, so the base's moves, always first, start from what they
    /// decided once.
    fn decide(&self, candidates: &[Moved]) -> Vec<Moved> {
        let n = self.in_place;
        let (start, rest) = if n > 0
            && candidates.len() >= n
            && candidates[..n].iter().all(|m| m.in_place)
            && !self.base_active.is_empty()
        {
            (self.intervals(&self.base_active), &candidates[n..])
        } else {
            (Vec::new(), candidates)
        };
        let mut active: Vec<Active> = start;
        for a in self.intervals(rest) {
            let crosses = active.iter().any(|b| {
                (b.lo < a.lo && a.lo <= b.hi && b.hi < a.hi)
                    || (a.lo < b.lo && b.lo <= a.hi && a.hi < b.hi)
            });
            if crosses {
                continue;
            }
            let replaced = active.iter().position(|b| b.lo == a.lo && b.hi == a.hi);
            let replaced = replaced.map(|k| active.remove(k));
            let id = a.m.id;
            active.push(a);
            if Self::loops(&active, id) {
                active.pop();
                active.extend(replaced);
            }
        }
        active.into_iter().map(|a| a.m).collect()
    }

    /// The innermost of `active`'s intervals holding `lo..=hi`, but not
    /// `skip`. They nest, so it's the one starting latest, then ending
    /// soonest.
    fn holding<'x>(
        active: &'x [Active],
        lo: &Key,
        hi: &Key,
        skip: Option<Lv>,
    ) -> Option<&'x Active> {
        active
            .iter()
            .filter(|b| b.lo <= *lo && *hi <= b.hi && Some(b.m.id) != skip)
            .max_by(|a, b| a.lo.cmp(&b.lo).then(b.hi.cmp(&a.hi)))
    }

    /// Whether move `m` is drawn inside its own interval.
    fn loops(active: &[Active], m: Lv) -> bool {
        let mut cur = active.iter().find(|a| a.m.id == m).unwrap();
        for _ in 0..=active.len() {
            let (lo, hi) = match &cur.portal {
                Some(p) => (p, p),
                None => (&cur.lo, &cur.hi),
            };
            let skip = cur.portal.is_none().then_some(cur.m.id);
            match Self::holding(active, lo, hi, skip) {
                None => return false,
                Some(f) if f.m.id == m => return true,
                Some(f) => cur = f,
            }
        }
        unreachable!("the active moves had a loop before {m}")
    }

    /// Lays out active `moves`.
    fn frames(&self, moves: &[Moved]) -> Frames {
        let active = self.intervals(moves);
        let mut order: Vec<usize> = (0..active.len()).collect();
        order.sort_by(|&a, &b| {
            active[a]
                .lo
                .cmp(&active[b].lo)
                .then(active[b].hi.cmp(&active[a].hi))
        });
        let mut parent = vec![None; active.len()];
        let mut stack: Vec<usize> = Vec::new();
        for &k in &order {
            while stack.last().is_some_and(|&t| active[t].hi < active[k].lo) {
                stack.pop();
            }
            debug_assert!(
                stack.last().is_none_or(|&t| active[k].hi <= active[t].hi),
                "active intervals nest"
            );
            parent[k] = stack.last().copied();
            stack.push(k);
        }
        let starts = order.iter().map(|&k| active[k].lo.clone()).collect();
        let by_id = active
            .iter()
            .enumerate()
            .map(|(k, a)| (a.m.id, k))
            .collect();
        let mut frames = Frames {
            active,
            by_id,
            order,
            starts,
            parent,
            cut_from: Vec::new(),
            pasted_in: Vec::new(),
            cuts: HashMap::new(),
            pastes: HashMap::new(),
        };
        let n = frames.active.len();
        frames.cut_from = (0..n)
            .map(|k| {
                let mut c = frames.parent[k];
                while let Some(i) = c {
                    if frames.active[i].portal.is_some() {
                        return Some(i);
                    }
                    c = frames.parent[i];
                }
                None
            })
            .collect();
        frames.pasted_in = (0..n)
            .map(|k| {
                frames.active[k]
                    .portal
                    .as_ref()
                    .and_then(|p| frames.drawn_in(p))
            })
            .collect();
        let mut cuts: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
        let mut pastes: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
        for k in 0..n {
            if frames.active[k].portal.is_some() {
                cuts.entry(frames.cut_from[k]).or_default().push(k);
                pastes.entry(frames.pasted_in[k]).or_default().push(k);
            }
        }
        for v in cuts.values_mut() {
            v.sort_by(|&a, &b| frames.active[a].lo.cmp(&frames.active[b].lo));
        }
        for v in pastes.values_mut() {
            v.sort_by(|&a, &b| frames.portal(a).cmp(frames.portal(b)));
        }
        frames.cuts = cuts;
        frames.pastes = pastes;
        frames
    }

    // ---- Positions ----

    /// How many items each portaled interval draws, in version `v`.
    fn totals(&self, v: In, frames: &Frames) -> Vec<usize> {
        let mut totals: Vec<Option<usize>> = vec![None; frames.active.len()];
        for k in 0..frames.active.len() {
            if frames.active[k].portal.is_some() {
                self.total(v, frames, k, &mut totals);
            }
        }
        totals.into_iter().map(|t| t.unwrap_or(0)).collect()
    }

    fn total(&self, v: In, frames: &Frames, k: usize, totals: &mut Vec<Option<usize>>) -> usize {
        if let Some(t) = totals[k] {
            return t;
        }
        // What lies in it, less what's cut out of it, plus what's pasted
        // in. No loops, so this ends.
        let a = &frames.active[k];
        let mut t = self.through(v, &a.hi) - self.before(v, &a.lo);
        for &c in frames.cuts(Some(k)) {
            let c = &frames.active[c];
            t -= self.through(v, &c.hi) - self.before(v, &c.lo);
        }
        for &p in frames.pastes(Some(k)) {
            t += self.total(v, frames, p, totals);
        }
        totals[k] = Some(t);
        t
    }

    /// How many items frame `frame` draws before the record with key `key`
    /// (or up to the end of it, if `through`), which it draws itself.
    fn local(
        &self,
        v: In,
        frames: &Frames,
        totals: &[usize],
        frame: Option<usize>,
        key: &Key,
        through: bool,
    ) -> usize {
        let start = frame.map_or(0, |f| self.before(v, &frames.active[f].lo));
        let upto = if through {
            self.through(v, key)
        } else {
            self.before(v, key)
        };
        let mut n = upto - start;
        for &c in frames.cuts(frame) {
            let c = &frames.active[c];
            if c.hi < *key || (through && c.hi == *key) {
                n -= self.through(v, &c.hi) - self.before(v, &c.lo);
            }
        }
        for &p in frames.pastes(frame) {
            let at = frames.portal(p);
            if at < key || (through && at == key) {
                n += totals[p];
            }
        }
        n
    }

    /// How many items version `v` draws before frame `frame` starts.
    fn offset(&self, v: In, frames: &Frames, totals: &[usize], frame: Option<usize>) -> usize {
        match frame {
            None => 0,
            Some(f) => {
                let g = frames.pasted_in[f];
                self.offset(v, frames, totals, g)
                    + self.local(v, frames, totals, g, frames.portal(f), false)
            }
        }
    }

    /// How many items version `v` draws before the record with key `key`.
    fn drawn_before(&self, v: In, frames: &Frames, totals: &[usize], key: &Key) -> usize {
        let frame = frames.drawn_in(key);
        self.offset(v, frames, totals, frame) + self.local(v, frames, totals, frame, key, false)
    }

    /// The record drawing the `pos`th item version `v` shows, and the
    /// item's place in it.
    fn drawn_at(&self, v: In, frames: &Frames, totals: &[usize], pos: usize) -> (Record, usize) {
        self.find(v, frames, totals, None, pos)
    }

    fn find(
        &self,
        v: In,
        frames: &Frames,
        totals: &[usize],
        frame: Option<usize>,
        pos: usize,
    ) -> (Record, usize) {
        // The frame's home stretch, in pieces between what's cut out of it
        // and what's pasted in, in home order.
        enum Piece {
            Cut(usize),
            Paste(usize),
        }
        let mut pieces: Vec<(&Key, Piece)> = frames
            .cuts(frame)
            .iter()
            .map(|&c| (&frames.active[c].lo, Piece::Cut(c)))
            .chain(
                frames
                    .pastes(frame)
                    .iter()
                    .map(|&p| (frames.portal(p), Piece::Paste(p))),
            )
            .collect();
        pieces.sort_by(|a, b| a.0.cmp(b.0));
        let mut from = frame.map_or(0, |f| self.before(v, &frames.active[f].lo));
        let mut seen = 0;
        for (key, piece) in pieces {
            let upto = self.before(v, key);
            if pos < seen + (upto - from) {
                return self.nth(v, from + pos - seen);
            }
            seen += upto - from;
            match piece {
                Piece::Cut(c) => from = self.through(v, &frames.active[c].hi),
                Piece::Paste(p) => {
                    if pos < seen + totals[p] {
                        return self.find(v, frames, totals, Some(p), pos - seen);
                    }
                    seen += totals[p];
                    from = upto;
                }
            }
        }
        let end = match frame {
            None => usize::MAX,
            Some(f) => self.through(v, &frames.active[f].hi),
        };
        debug_assert!(pos - seen < end - from, "the frame draws item {pos}");
        self.nth(v, from + pos - seen)
    }

    /// The active moves the record with key `key` is drawn inside,
    /// outermost first.
    fn path(&self, frames: &Frames, key: &Key) -> Vec<Lv> {
        let mut chain = Vec::new();
        let mut c = frames.innermost(key);
        while let Some(i) = c {
            chain.push(i);
            c = frames.parent[i];
        }
        chain.reverse();
        match chain
            .iter()
            .rposition(|&i| frames.active[i].portal.is_some())
        {
            None => chain.iter().map(|&i| frames.active[i].m.id).collect(),
            Some(p) => {
                let mut path = self.path(frames, frames.portal(chain[p]));
                path.extend(chain[p..].iter().map(|&i| frames.active[i].m.id));
                path
            }
        }
    }

    /// The key of the record holding the `pos`th item the prepare version
    /// shows, split out into a record of its own.
    fn prepared_at(&mut self, pos: usize) -> Key {
        let (_, frames) = self.active(In::Prepare);
        let totals = self.totals(In::Prepare, &frames);
        let (record, offset) = self.drawn_at(In::Prepare, &frames, &totals, pos);
        self.isolate(&record, offset)
    }

    /// How many items the effect version shows before the record with key
    /// `key`.
    fn effect_pos(&self, key: &Key) -> usize {
        let (_, frames) = self.active(In::Effect);
        let totals = self.totals(In::Effect, &frames);
        self.drawn_before(In::Effect, &frames, &totals, key)
    }

    /// What draws the record with key `key` in the frame at `depth` of its
    /// path: its first and last key. An item draws itself; a block, by its
    /// portal, or (in place) by its whole interval.
    fn element(frames: &Frames, path: &[Lv], depth: usize, key: &Key) -> (Key, Key) {
        match path.get(depth) {
            None => (key.clone(), key.clone()),
            Some(g) => frames.location(frames.by_id[g]),
        }
    }

    /// The origins for something put in at visible position `pos` of the
    /// prepare version: the elements either side of it, in their lowest
    /// common frame.
    fn origins(&mut self, pos: usize) -> (Option<Key>, Option<Key>) {
        let left_at = pos.checked_sub(1).map(|p| self.prepared_at(p));
        let (_, frames) = self.active(In::Prepare);
        let totals = self.totals(In::Prepare, &frames);
        // The frame they share is decided by what shows: the visible item
        // after, not a tombstone, so an edge is where it looks to be.
        let (right, _) = self.drawn_at(In::Prepare, &frames, &totals, pos);
        let left_path = left_at
            .as_ref()
            .map_or(Vec::new(), |k| self.path(&frames, k));
        let right_path = self.path(&frames, &right.key);
        let depth = left_path
            .iter()
            .zip(&right_path)
            .take_while(|(a, b)| a == b)
            .count();
        let frame = depth.checked_sub(1).map(|d| left_path[d]);
        let lifted = left_at.map(|k| Self::element(&frames, &left_path, depth, &k).1);
        let right = self.next_element(&frames, frame, lifted.as_ref());
        (lifted, right)
    }

    /// The next element after the record with key `after` (or from the
    /// start) in frame `frame`'s own order: a record of the frame, or the
    /// first of a block in it.
    fn next_element(&self, frames: &Frames, frame: Option<Lv>, after: Option<&Key>) -> Option<Key> {
        let frame = frame.map(|f| frames.by_id[&f]);
        let (lo, hi) = match frame {
            None => (None, None),
            Some(f) => (Some(&frames.active[f].lo), Some(&frames.active[f].hi)),
        };
        let mut from: Option<Key> = after.cloned();
        let mut exclusive = after.is_some();
        if from.is_none() {
            from = lo.cloned();
        }
        loop {
            let r = self.next_present(from.as_ref(), exclusive)?;
            if hi.is_some_and(|h| r.key > *h) {
                return None;
            }
            let inner = frames.innermost(&r.key);
            if inner == frame {
                return Some(r.key);
            }
            // Where a block in it starts: any block, not just the innermost
            // here, as blocks nest and can start at the same record.
            let mut c = inner;
            while let Some(i) = c
                && frames.active[i].lo == r.key
            {
                if frames.parent[i] == frame {
                    return Some(r.key);
                }
                c = frames.parent[i];
            }
            // Inside a block of the frame, and not its start: nothing in
            // it is the frame's own. On after it. (It can't happen: a
            // block's first record is always in the prepare version, a base
            // item, or one its move's author saw. So the first record found
            // in a block is its start. Kept as the simple walk's scan would
            // go on.)
            debug_assert!(false, "a block's first record is in the prepare version");
            let mut c = inner.expect("inside the frame, deeper");
            while frames.parent[c] != frame {
                c = frames.parent[c].expect("the frame holds it");
            }
            from = Some(frames.active[c].hi.clone());
            exclusive = true;
        }
    }

    /// Where an insert by `lv` with origins `left` and `right` goes, by the
    /// YjsMod rule (see `order.rs`, written here over keys): the key of the
    /// record before it, and of the record it goes before.
    fn place(&self, lv: Lv, left: Option<&Key>, right: Option<&Key>) -> (Option<Key>, Option<Key>) {
        let mut cursor = self.records.cursor::<Sums>();
        if let Some(l) = left {
            cursor.seek(&At(l), Bias::Left);
        }
        cursor.next();
        let me = self.log.order_key(lv);
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
            if right == Some(&other.key) {
                break;
            }
            let other_left = other.left.as_ref();
            if other_left < left {
                break;
            }
            if other_left == left {
                match cmp_right(other.right.as_ref(), right) {
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
        (prev, next)
    }

    /// Puts a record for `lv` (an insert's item, or a portal) where its
    /// origins say; returns its key.
    fn put(&mut self, lv: Lv, left: Option<Key>, right: Option<Key>, portal: bool) -> Key {
        let (prev, next) = self.place(lv, left.as_ref(), right.as_ref());
        let key = Key::between(prev.as_ref(), next.as_ref());
        let record = Record {
            key: key.clone(),
            last: key.clone(),
            id: lv,
            len: (!portal) as usize,
            left,
            right,
            prepare: Prepare::Inserted,
            dead: 0,
            portal,
        };
        match &next {
            // In front of `next`: swap in, and it goes after.
            Some(n) => {
                self.records
                    .splice_at(&At(n), Bias::Left, |r| ((), [std::mem::replace(r, record)]));
            }
            None => self.records.push(record),
        }
        self.keys.insert(lv, key.clone());
        self.changed.insert(key.clone());
        key
    }

    // ---- Events ----

    fn insert(&mut self, lv: Lv, pos: usize, out: Option<&mut Changes<'a, T>>) {
        // The origins, as the insert's author saw them, lifted to the frame
        // they share (see `simple.rs`).
        let (left, right) = self.origins(pos);
        let key = self.put(lv, left, right, false);
        if let Some(out) = out {
            out.insert(self.effect_pos(&key), lv);
        }
    }

    fn delete(&mut self, lv: Lv, pos: usize, out: Option<&mut Changes<'a, T>>) {
        let key = self.prepared_at(pos);
        let (record, _) = self.record_at(&key);
        self.update(&key, |r| r.prepare = Prepare::Deleted(1));
        self.targets.insert(lv, record.id);
        self.changed.insert(key.clone());
        let Some(out) = out else {
            self.update(&key, |r| r.dead += 1);
            return;
        };
        // What it removed, for an undo later on.
        let item = self.identity(&key);
        out.learned.push((lv, item));
        // Already deleted in the effect version by a concurrent delete:
        // nothing more to do to the document.
        if record.dead == 0 {
            out.delete(self.effect_pos(&key));
        }
        self.update(&key, |r| r.dead += 1);
    }

    /// Undoes delete `d`: the item it removed shows again if nothing else
    /// still removes it.
    fn undelete(&mut self, d: Lv, out: Option<&mut Changes<'a, T>>) {
        let count = self.undos.get(&d).copied().unwrap_or(0) + 1;
        self.undos.insert(d, count);
        let key = self.item_key(self.targets[&d]);
        if count == 1 {
            self.update(&key, |r| {
                r.prepare = match r.prepare {
                    Prepare::Deleted(1) => Prepare::Inserted,
                    Prepare::Deleted(n) => Prepare::Deleted(n - 1),
                    state => unreachable!("undoing delete {d} of an item {state:?}"),
                };
            });
        }
        // Already undone in the effect version by a concurrent undelete.
        if !self.undone.insert(d) {
            return;
        }
        let mut alive = false;
        self.update(&key, |r| {
            r.dead -= 1;
            alive = r.dead == 0;
        });
        // (Its place is noted as changed already: a merge replays the
        // delete an undelete undoes, and the delete noted it.)
        if alive && let Some(out) = out {
            let id = self.identity(&key);
            out.insert(self.effect_pos(&key), id);
        }
    }

    /// A mark of the item at `pos`: it wins if it's the latest of the item's
    /// marks so far.
    fn mark(&mut self, lv: Lv, pos: usize, level: u8, out: Option<&mut Changes<'a, T>>) {
        let key = self.prepared_at(pos);
        let (record, _) = self.record_at(&key);
        let me = (self.log.lamport(lv), self.log.order_key(lv));
        if self.marks.get(&record.id).is_some_and(|won| *won > me) {
            return;
        }
        self.marks.insert(record.id, me);
        if let Some(out) = out {
            let item = self.identity(&key);
            let pos = (record.dead == 0).then(|| self.effect_pos(&key));
            out.mark(item, pos, level);
        }
    }

    // ---- Moves ----

    /// The interval a move of the `len` visible items at `pos` names, if
    /// its range is whole elements of one frame: its first and last item.
    fn interval(&mut self, pos: usize, len: usize) -> Option<(usize, usize)> {
        if len == 0 {
            return None;
        }
        let f = self.prepared_at(pos);
        let l = self.prepared_at(pos + len - 1);
        let (_, frames) = self.active(In::Prepare);
        let totals = self.totals(In::Prepare, &frames);
        let fp = self.path(&frames, &f);
        let lp = self.path(&frames, &l);
        let depth = fp.iter().zip(&lp).take_while(|(a, b)| a == b).count();
        let lo = Self::element(&frames, &fp, depth, &f).0;
        let hi = Self::element(&frames, &lp, depth, &l).1;
        // The elements of their frame from `lo` to `hi` are drawn side by
        // side: they must draw exactly the range.
        // Counted in the frame that draws them: theirs, or if that's a block
        // in place, the one it lies in.
        let frame = depth.checked_sub(1).map(|d| frames.by_id[&fp[d]]);
        let frame = match frame {
            Some(k) if frames.active[k].portal.is_none() => frames.cut_from[k],
            frame => frame,
        };
        let offset = self.offset(In::Prepare, &frames, &totals, frame);
        let start = offset + self.local(In::Prepare, &frames, &totals, frame, &lo, false);
        let end = offset + self.local(In::Prepare, &frames, &totals, frame, &hi, true);
        if end - start != len || start != pos {
            return None;
        }
        let (first, _) = self.record_at(&lo);
        let (last, _) = self.record_at(&hi);
        Some((first.id, last.id + last.len.saturating_sub(1)))
    }

    fn move_(
        &mut self,
        lv: Lv,
        pos: usize,
        len: usize,
        to: usize,
        out: Option<&mut Changes<'a, T>>,
    ) {
        if (pos..=pos + len).contains(&to) {
            return;
        }
        let Some((first, last)) = self.interval(pos, len) else {
            return;
        };
        let (left, right) = self.origins(to);
        self.put(lv, left, right, true);
        self.changed.insert(self.item_key(first));
        self.changed.insert(self.item_key(last));
        let (before, _) = self.active(In::Effect);
        self.add_move(Moved {
            id: lv,
            first,
            last,
            in_place: false,
        });
        // Counted in the effect version for the question that follows.
        self.effect.insert(lv);
        let (after, _) = self.active(In::Effect);
        if let Some(out) = out {
            self.emit(&before, &after, out);
        }
    }

    /// Each record version `v` shows, as drawn with `moves` active, with
    /// the active moves it's drawn inside; the placeholder's tail past the
    /// real document left out.
    fn drawing(&self, v: In, frames: &Frames) -> Vec<(Record, Vec<Lv>)> {
        let mut out = Vec::new();
        self.draw_frame(v, frames, None, &mut out);
        out
    }

    fn draw_frame(
        &self,
        v: In,
        frames: &Frames,
        frame: Option<usize>,
        out: &mut Vec<(Record, Vec<Lv>)>,
    ) {
        let mut cursor = self.records.cursor::<Sums>();
        match frame {
            None => cursor.next(),
            Some(f) => {
                cursor.seek(&At(&frames.active[f].lo), Bias::Left);
            }
        }
        let hi = frame.map(|f| frames.active[f].hi.clone());
        while let Some(r) = cursor.item() {
            if hi.as_ref().is_some_and(|h| r.key > *h) {
                break;
            }
            let drawn_in = frames.drawn_in(&r.key);
            if drawn_in != frame {
                // Cut out of this frame: on past what's cut.
                let mut c = drawn_in.expect("cut out of the frame");
                while frames.cut_from[c] != frame {
                    c = frames.cut_from[c].expect("the frame holds it");
                }
                cursor.seek_forward(&At(&frames.active[c].hi), Bias::Left);
                cursor.next();
                continue;
            }
            if r.portal {
                let present = match v {
                    In::Prepare => r.prepare != Prepare::NotYet,
                    In::Effect => self.effect.contains(&r.id),
                };
                if present && let Some(&k) = frames.by_id.get(&r.id) {
                    self.draw_frame(v, frames, Some(k), out);
                }
            } else if count(&r.summary(), v) > 0
                && !(r.id >= BASE_ITEM && r.id + r.len == BASE_ITEM + UNDERWATER)
            {
                out.push((r.clone(), self.path(frames, &r.key)));
            }
            cursor.next();
        }
    }

    /// The changes that take the effect document from `before`'s moves to
    /// `after`'s, as in `simple.rs`: runs moved into place, one at a time.
    fn emit(&self, before: &[Moved], after: &[Moved], out: &mut Changes<'a, T>) {
        if before == after {
            return;
        }
        let items = |moves: &[Moved]| -> Vec<(usize, usize)> {
            let frames = self.frames(moves);
            self.drawing(In::Effect, &frames)
                .into_iter()
                .flat_map(|(r, _)| (0..r.len).map(move |k| (r.id, k)))
                .collect()
        };
        let mut now = items(before);
        let want = items(after);
        debug_assert_eq!(now.len(), want.len(), "a move keeps its items");
        for i in 0..want.len() {
            if now[i] == want[i] {
                continue;
            }
            let j = now
                .iter()
                .position(|k| *k == want[i])
                .expect("the same items");
            let mut n = 1;
            while i + n < want.len() && j + n < now.len() && now[j + n] == want[i + n] {
                n += 1;
            }
            out.move_range(j..j + n, i);
            let run: Vec<(usize, usize)> = now.drain(j..j + n).collect();
            now.splice(i..i, run);
        }
    }

    /// The moved blocks of the document so far.
    ///
    /// Each active move with something showing: a block in place covers
    /// its elements of the frame it lies in, and a portaled one what it
    /// draws at its portal.
    pub(crate) fn blocks(&self) -> Blocks {
        let v = In::Effect;
        let (_, frames) = self.active(v);
        let totals = self.totals(v, &frames);
        let blocks = frames
            .active
            .iter()
            .enumerate()
            .filter_map(|(k, a)| {
                let span = match &a.portal {
                    Some(p) => {
                        let g = frames.pasted_in[k];
                        let at = self.offset(v, &frames, &totals, g)
                            + self.local(v, &frames, &totals, g, p, false);
                        at..at + totals[k]
                    }
                    None => {
                        let d = frames.cut_from[k];
                        let at = self.offset(v, &frames, &totals, d);
                        at + self.local(v, &frames, &totals, d, &a.lo, false)
                            ..at + self.local(v, &frames, &totals, d, &a.hi, true)
                    }
                };
                (!span.is_empty()).then_some((k, span))
            })
            .collect::<Vec<_>>();
        #[cfg(all(debug_assertions, not(feature = "perf")))]
        {
            let mut spans: Vec<(Lv, Option<usize>, usize)> =
                frames.active.iter().map(|a| (a.m.id, None, 0)).collect();
            let mut pos = 0;
            for (r, path) in self.drawing(v, &frames) {
                for id in &path {
                    let (_, start, end) = &mut spans[frames.by_id[id]];
                    start.get_or_insert(pos);
                    *end = pos + r.len;
                }
                pos += r.len;
            }
            let drawn: Vec<_> = spans
                .into_iter()
                .filter_map(|(id, start, end)| Some((id, start?..end)))
                .collect();
            let counted: Vec<_> = blocks
                .iter()
                .map(|(k, span)| (frames.active[*k].m.id, span.clone()))
                .collect();
            debug_assert_eq!(counted, drawn, "the blocks, counted and drawn");
        }
        // Whether a block's first record is where its first visible element
        // starts, and its last where its last ends: as it was laid, for a
        // base block nothing changed inside.
        let mut blocks: Vec<(Block, usize)> = blocks
            .into_iter()
            .map(|(k, span)| {
                let a = &frames.active[k];
                if a.portal.is_none()
                    && self
                        .changed
                        .range(a.lo.clone()..=a.hi.clone())
                        .next()
                        .is_none()
                {
                    let (len, hidden) = self.laid[&a.m.id];
                    debug_assert_eq!(span.len(), len, "a block nothing changed in");
                    let depth = self
                        .path(&frames, &a.lo)
                        .iter()
                        .position(|&x| x == a.m.id)
                        .unwrap();
                    #[cfg(all(debug_assertions, not(feature = "perf")))]
                    assert_eq!(
                        (hidden, depth),
                        self.hidden_edges(&frames, &totals, k, &span),
                        "the hidden edges of a block nothing changed in"
                    );
                    return (Block::new(a.m.id, span, hidden), depth);
                }
                let (hidden, depth) = self.hidden_edges(&frames, &totals, k, &span);
                (Block::new(a.m.id, span, hidden), depth)
            })
            .collect();
        blocks.sort_by_key(|(b, depth)| (b.span.start, std::cmp::Reverse(b.span.end), *depth));
        Blocks(blocks.into_iter().map(|(b, _)| b).collect())
    }

    /// Which ends of active block `k`, showing `span` in the effect
    /// version, hide records: whether its first record is where its first
    /// visible element starts, and its last where its last ends. And how
    /// deep it's drawn.
    fn hidden_edges(
        &self,
        frames: &Frames,
        totals: &[usize],
        k: usize,
        span: &std::ops::Range<usize>,
    ) -> (Edges, usize) {
        let v = In::Effect;
        let a = &frames.active[k];
        let (first, _) = self.drawn_at(v, frames, totals, span.start);
        let (last, _) = self.drawn_at(v, frames, totals, span.end - 1);
        let fp = self.path(frames, &first.key);
        let lp = self.path(frames, &last.key);
        let depth = fp.iter().position(|&x| x == a.m.id).unwrap();
        let starts = match fp.get(depth + 1) {
            None => first.key.clone(),
            Some(g) => frames.location(frames.by_id[g]).0,
        };
        let ends = match lp.get(depth + 1) {
            None => last.last.clone(),
            Some(g) => frames.location(frames.by_id[g]).1,
        };
        let hidden = Edges {
            head: starts != a.lo,
            tail: ends != a.hi,
        };
        (hidden, depth)
    }
}

/// The window, and in debug builds the simple walk's beside it, checked
/// against it step by step: the same changes asked for, the same items
/// learned, the same blocks at the end.
pub(crate) struct Checked<'a, T> {
    fast: Window<'a, T>,
    #[cfg(all(debug_assertions, not(feature = "perf")))]
    simple: (crate::simple::Window<'a, T>, Changes<'a, T>),
}

impl<'a, T: Clone> Checked<'a, T> {
    pub(crate) fn new(
        log: &'a OpLog<T>,
        frontier: Vec<Lv>,
        ids: Option<Ids>,
        blocks: &Blocks,
    ) -> Self {
        Checked {
            #[cfg(all(debug_assertions, not(feature = "perf")))]
            simple: (
                crate::simple::Window::new(log, frontier.clone(), ids.clone(), blocks),
                Changes::new(log),
            ),
            fast: Window::new(log, frontier, ids, blocks),
        }
    }

    pub(crate) fn step(&mut self, lv: Lv, mut out: Option<&mut Changes<'a, T>>) {
        #[cfg(all(debug_assertions, not(feature = "perf")))]
        let marks = out.as_ref().map(|o| (o.calls.len(), o.learned.len()));
        self.fast.step(lv, out.as_mut().map(|o| &mut **o));
        #[cfg(all(debug_assertions, not(feature = "perf")))]
        {
            let (simple, theirs) = &mut self.simple;
            let theirs_marks = (theirs.calls.len(), theirs.learned.len());
            simple.step(lv, out.is_some().then_some(&mut *theirs));
            if let (Some(out), Some((calls, learned))) = (out, marks) {
                assert_eq!(
                    out.calls[calls..],
                    theirs.calls[theirs_marks.0..],
                    "the windows' changes for event {lv}"
                );
                assert_eq!(
                    out.learned[learned..],
                    theirs.learned[theirs_marks.1..],
                    "the windows' deletes learned at event {lv}"
                );
            }
        }
    }

    pub(crate) fn blocks(&self) -> Blocks {
        let blocks = self.fast.blocks();
        #[cfg(all(debug_assertions, not(feature = "perf")))]
        {
            assert_eq!(blocks, self.simple.0.blocks(), "the windows' blocks");
        }
        blocks
    }
}
