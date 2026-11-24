//! The simple walk: the same algorithm as the fast one, with records in a
//! plain list and the prepare version tracked over the whole log. It's
//! kept as an oracle: the fast walk must produce exactly these changes.
//!
//! # Moves
//!
//! As in the reference (see `reference.rs`), a move names an interval of
//! the records' order and draws it at a portal; the document is drawn by
//! frames. The base, though, is a placeholder: the base document's items in
//! the order it shows them, so its moved blocks are already where they're
//! drawn. They're *in-place* frames here: the same frames, drawn where they
//! lie, with no portal. Every replayed move comes after every move in the
//! base (it includes them all, so it's later in Lamport order), so what the
//! base's moves do never changes, except that a replayed move of exactly a
//! block takes it over.
//!
//! When a move changes which moves are active in the effect version, each
//! block that changes place comes out as a `Change::Move`, one at a time.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::{ControlFlow, Range};
use std::rc::Rc;

use crate::blocks::{Block, Blocks, Edges, MARKER, Piece};
use crate::ids::Ids;
use crate::oplog::{Kind, Lv, Op, OpLog};
use crate::order::{Placed, place};
use crate::walk::{Change, Changes};

/// A map keyed by ids or indices, hashed cheaply: the walk looks records
/// up all the time, and nothing here is chosen by an adversary.
pub(crate) type Map<K, V> = HashMap<K, V, BuildHasherDefault<IdHasher>>;
pub(crate) type Set = HashSet<Lv, BuildHasherDefault<IdHasher>>;

#[derive(Default)]
pub(crate) struct IdHasher(u64);

impl Hasher for IdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Ids at or above this name items of the base document, by position.
const BASE_ITEM: usize = 1 << 62;
/// How long the placeholder is: more than any real document.
const UNDERWATER: usize = 1 << 61;

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
#[derive(Clone)]
struct Record {
    /// The insert's local version, or `BASE_ITEM` plus the first base
    /// item's position, or (a portal) the move's.
    id: usize,
    /// 1, except for a run of base items, and 0 for a portal.
    len: usize,
    left: Option<usize>,
    right: Option<usize>,
    prepare: Prepare,
    /// How many deletes replayed so far removed it, not undone: it shows in
    /// the effect version if none.
    dead: u32,
    portal: bool,
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

/// A move that does something: the first and last item (or portal) of its
/// interval, and whether it's one of the base's, drawn in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Moved {
    id: Lv,
    first: usize,
    last: usize,
    in_place: bool,
}

/// An active move, with its interval's indices in `records`.
#[derive(Clone, Copy, Debug)]
struct Active {
    m: Moved,
    lo: usize,
    hi: usize,
}

/// A record as drawn: its index, whether it shows, and the active moves
/// it's drawn inside, outermost first.
#[derive(Clone)]
struct Entry {
    at: usize,
    visible: bool,
    path: Rc<[Lv]>,
}

/// Which version a question is about.
#[derive(Clone, Copy, PartialEq, Eq)]
enum In {
    Prepare,
    Effect,
}

struct Walk<'a, T> {
    log: &'a OpLog<T>,
    records: Vec<Record>,
    /// The prepare version beyond the base, as a set.
    prepared: Set,
    /// The events stepped so far, in order: all the prepare version can
    /// hold beyond the base.
    stepped: Vec<Lv>,
    /// The effect version beyond the base, as a set: everything stepped.
    effect: Set,
    /// Which record each delete replayed so far removed.
    targets: HashMap<Lv, usize>,
    /// How many undeletes of each delete the prepare version holds.
    undos: HashMap<Lv, u32>,
    /// The deletes undone in the effect version.
    undone: HashSet<Lv>,
    /// Which insert each base item came from, once asked.
    base_ids: BaseIds,
    /// The base, as a set, for the oracle walk. A window doesn't keep it:
    /// every replayed event's history is the base and what was stepped
    /// before it, so what wasn't stepped is the base's.
    base: Option<Vec<bool>>,
    /// The moves that do something, in the order activity is decided in.
    moves: Vec<Moved>,
    /// Each marked record's winning mark among those replayed.
    marks: HashMap<usize, (u64, (&'a str, u64))>,
    /// Where each record is, by id, once asked: thrown away whenever a
    /// record goes in.
    index: RefCell<Option<Rc<Index>>>,
    /// The last layout drawn, and its moves: thrown away with the index.
    layout: RefCell<Option<(Vec<Moved>, Rc<Layout>)>>,
    /// How many of `moves` are the base's, drawn in place, and which of
    /// them are active among themselves: decided once, as they're decided
    /// first (see `decide`).
    in_place: usize,
    base_active: Option<Vec<Moved>>,
    /// The last answer `active` gave for each version, and its candidates:
    /// which moves are active depends on nothing else.
    active_cache: RefCell<[Option<(Vec<Moved>, Vec<Moved>)>; 2]>,
}

/// The records' indices: by id, and the base runs by their first item.
struct Index {
    by_id: Map<usize, usize>,
    base: Vec<(usize, usize)>,
}

/// The active moves' intervals, laid out over the records: they nest, as
/// no two active ones cross.
struct Layout {
    active: Vec<Active>,
    /// The innermost interval holding each record, by index in `active`.
    inner: Vec<Option<usize>>,
    /// The innermost other interval holding each interval: its frame.
    parent: Vec<Option<usize>>,
    /// The intervals starting at each record, in `active`'s order: those
    /// at record `i` are `starting[start_at[i]..start_at[i + 1]]`.
    start_at: Vec<usize>,
    starting: Vec<usize>,
    /// Each interval by its move, and each portaled move by its portal.
    by_id: Map<Lv, usize>,
    portal: Map<Lv, usize>,
}

/// What a drawing goes to: each record drawn, in order, inside the
/// blocks entered and not yet left.
trait Sink {
    fn enter(&mut self, block: Lv);
    fn leave(&mut self);
    /// Record `at`, of `len` items, and whether it shows. `Break` stops the
    /// drawing.
    fn record(&mut self, at: usize, visible: bool, len: usize) -> ControlFlow<()>;
}

/// The whole drawing, with each record's path: shared by the records of
/// one frame.
struct Entries {
    out: Vec<Entry>,
    path: Vec<Lv>,
    shared: Vec<Rc<[Lv]>>,
}

impl Sink for Entries {
    fn enter(&mut self, block: Lv) {
        self.path.push(block);
        self.shared.push(Rc::from(&self.path[..]));
    }
    fn leave(&mut self) {
        self.path.pop();
        self.shared.pop();
    }
    fn record(&mut self, at: usize, visible: bool, _: usize) -> ControlFlow<()> {
        let path = self.shared.last().expect("the top frame's").clone();
        self.out.push(Entry { at, visible, path });
        ControlFlow::Continue(())
    }
}

/// A search along the drawing: `stop` is asked of each record, with how
/// many visible items came before it, until it finds something.
struct Find<R, F> {
    seen: usize,
    found: Option<R>,
    stop: F,
}

impl<R, F: FnMut(usize, bool, usize, usize) -> Option<R>> Sink for Find<R, F> {
    fn enter(&mut self, _: Lv) {}
    fn leave(&mut self) {}
    fn record(&mut self, at: usize, visible: bool, len: usize) -> ControlFlow<()> {
        if let Some(found) = (self.stop)(at, visible, len, self.seen) {
            self.found = Some(found);
            return ControlFlow::Break(());
        }
        if visible {
            self.seen += len;
        }
        ControlFlow::Continue(())
    }
}

impl Layout {
    /// The intervals starting at record `i`.
    fn starts(&self, i: usize) -> &[usize] {
        &self.starting[self.start_at[i]..self.start_at[i + 1]]
    }
}

/// Where the base items' identities come from: the reference (the oracle
/// walk), or the fast walk's, given or found when first asked.
enum BaseIds {
    Reference(Option<Vec<Lv>>),
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    Given {
        frontier: Vec<Lv>,
        ids: Option<Ids>,
    },
}

/// A stretch of a merge with moves, the simple walk's way, from a base
/// whose identities and blocks are known (or found): what debug builds
/// check the fast window (`window.rs`) against.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub(crate) struct Window<'a, T>(Walk<'a, T>);

#[cfg_attr(not(debug_assertions), allow(dead_code))]
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
        let mut walk = Walk::new(log, None, BaseIds::Given { frontier, ids });
        walk.lay_base(blocks);
        Window(walk)
    }

    pub(crate) fn step(&mut self, lv: Lv, out: Option<&mut Changes<'a, T>>) {
        self.0.step(lv, out);
    }

    /// The moved blocks of the document so far.
    pub(crate) fn blocks(&self) -> Blocks {
        let walk = &self.0;
        let (entries, lay) = walk.draw(In::Effect, &walk.active(In::Effect));
        // Each block's first and last visible entry, and where they are.
        let mut spans: Vec<Option<(usize, usize, usize, usize)>> = vec![None; lay.active.len()];
        let mut pos = 0;
        for (k, e) in entries.iter().enumerate().filter(|(_, e)| e.visible) {
            let r = &walk.records[e.at];
            if r.id >= BASE_ITEM && r.id + r.len == BASE_ITEM + UNDERWATER {
                continue;
            }
            for id in e.path.iter() {
                let span = &mut spans[lay.by_id[id]];
                let (first, start, _, _) = span.unwrap_or((k, pos, k, pos));
                *span = Some((first, start, k, pos + r.len));
            }
            pos += r.len;
        }
        // Whether a block's first record is where its first visible
        // element starts, and its last where its last ends.
        let mut blocks: Vec<(Block, usize)> = lay
            .active
            .iter()
            .zip(spans)
            .filter_map(|(a, span)| {
                let (first, start, last, end) = span?;
                let (f, l) = (&entries[first], &entries[last]);
                let depth = f.path.iter().position(|&x| x == a.m.id).unwrap();
                let head = walk.element(&lay, f, depth + 1).0 != a.lo;
                let tail = walk.element(&lay, l, depth + 1).1 != a.hi;
                Some((Block::new(a.m.id, start..end, Edges { head, tail }), depth))
            })
            .collect();
        blocks.sort_by_key(|(b, depth)| (b.span.start, std::cmp::Reverse(b.span.end), *depth));
        Blocks(blocks.into_iter().map(|(b, _)| b).collect())
    }
}

impl<T: Clone> OpLog<T> {
    /// `changes`, by the simple walk: for testing the fast one against.
    #[doc(hidden)]
    pub fn changes_simple(&self, from: &[Lv], to: &[Lv]) -> Vec<Change<'_, T>> {
        self.walk_simple(from, to)
    }

    fn walk_simple(&self, from: &[Lv], to: &[Lv]) -> Vec<Change<'_, T>> {
        let have = self.closure(from);
        let mut want = self.closure(to);
        for (w, &h) in want.iter_mut().zip(&have) {
            *w |= h;
        }
        if want == have {
            return Vec::new();
        }
        let base = self.base_simple(&have, &want);
        let mut walk = Walk::new(self, Some(base.clone()), BaseIds::Reference(None));
        walk.lay_base(&crate::reference::blocks(self, &base));
        // First what the document already holds past the base, to set up
        // its state, then what's new. Parents come first either way:
        // `from` holds all its events' parents.
        for lv in (0..self.len()).filter(|&lv| have[lv] && !base[lv]) {
            walk.step(lv, None);
        }
        let mut out = Changes::new(self);
        for lv in (0..self.len()).filter(|&lv| want[lv] && !have[lv]) {
            walk.step(lv, Some(&mut out));
        }
        out.out
    }

    /// The latest version inside `have` that every event to be replayed
    /// includes: the new events, and whatever of `have` isn't in the base.
    /// Shrinking the base can bring more of `have` into the replay, whose
    /// parents may shrink it further, so it's found by shrinking until it
    /// holds.
    fn base_simple(&self, have: &[bool], want: &[bool]) -> Vec<bool> {
        let mut base = have.to_vec();
        loop {
            let mut next = base.clone();
            for lv in (0..self.len()).filter(|&lv| want[lv] && !base[lv]) {
                let parents = self.closure(&self.parents(lv).collect::<Vec<_>>());
                for (b, p) in next.iter_mut().zip(parents) {
                    *b &= p;
                }
                // An undelete needs its delete replayed too, so the item it
                // brings back is known: the base can't hold the delete.
                if self.kind(lv) == Kind::Undelete {
                    let d = self.undeleted(lv);
                    if base[d] {
                        for x in (0..self.len()).filter(|&x| base[x]) {
                            if self.closure(&[x])[d] {
                                next[x] = false;
                            }
                        }
                    }
                }
            }
            if next == base {
                return base;
            }
            base = next;
        }
    }
}

impl<'a, T: Clone> Walk<'a, T> {
    /// A walk from `base`, with nothing laid yet.
    fn new(log: &'a OpLog<T>, base: Option<Vec<bool>>, base_ids: BaseIds) -> Self {
        Walk {
            log,
            records: Vec::new(),
            prepared: Set::default(),
            stepped: Vec::new(),
            effect: Set::default(),
            targets: HashMap::new(),
            undos: HashMap::new(),
            undone: HashSet::new(),
            base_ids,
            base,
            moves: Vec::new(),
            marks: HashMap::new(),
            index: RefCell::new(None),
            layout: RefCell::new(None),
            in_place: 0,
            base_active: None,
            active_cache: RefCell::new([None, None]),
        }
    }

    /// The records' index, found again if a record went in since.
    fn index(&self) -> Rc<Index> {
        if let Some(index) = &*self.index.borrow() {
            return index.clone();
        }
        let mut by_id = Map::with_capacity_and_hasher(self.records.len(), Default::default());
        let mut base = Vec::new();
        for (i, r) in self.records.iter().enumerate() {
            by_id.insert(r.id, i);
            if !r.portal && (BASE_ITEM..MARKER).contains(&r.id) {
                base.push((r.id, i));
            }
        }
        base.sort_unstable();
        let index = Rc::new(Index { by_id, base });
        *self.index.borrow_mut() = Some(index.clone());
        index
    }

    /// Puts `record` in at `i`.
    fn insert_record(&mut self, i: usize, record: Record) {
        self.records.insert(i, record);
        self.moved_records();
    }

    /// Records went in: indices have changed.
    fn moved_records(&mut self) {
        *self.index.get_mut() = None;
        *self.layout.get_mut() = None;
    }

    /// The placeholder, cut wherever a base block starts or ends (its last
    /// item on its own, so it can be an origin), and the blocks as
    /// in-place moves.
    fn lay_base(&mut self, blocks: &Blocks) {
        let laid = blocks.lay();
        self.moved_records();
        for piece in laid.pieces {
            self.records.push(match piece {
                Piece::Items(r) => Record {
                    id: BASE_ITEM + r.start,
                    len: r.len(),
                    left: None,
                    right: None,
                    prepare: Prepare::Inserted,
                    dead: 0,
                    portal: false,
                },
                // What a block hides at its edge: nothing to show, ever.
                Piece::Marker(id) => Record {
                    id,
                    len: 0,
                    left: None,
                    right: None,
                    prepare: Prepare::Deleted(1),
                    dead: 1,
                    portal: false,
                },
            });
        }
        for (id, first, last) in laid.moves {
            self.add_move(Moved {
                id,
                first,
                last,
                in_place: true,
            });
        }
        // The base's moves come before any replayed one (each replayed
        // event includes them all, so it's later in Lamport time): what
        // they decide among themselves is decided once.
        self.in_place = self.moves.len();
        self.base_active = Some(self.decide(&self.moves));
    }

    /// Adds a move, in the order activity is decided in: Lamport time, then
    /// as concurrent inserts are ordered.
    fn add_move(&mut self, m: Moved) {
        let key = |id: Lv| (self.log.lamport(id), self.log.order_key(id));
        let k = self.moves.partition_point(|o| key(o.id) < key(m.id));
        self.moves.insert(k, m);
    }

    fn step(&mut self, lv: Lv, out: Option<&mut Changes<'a, T>>) {
        let event = self.log.event(lv);
        // Move the prepare version to this event's parents: take back what
        // it shouldn't hold, latest first, then add what it's missing,
        // earliest first, so a delete is never undone before its insert.
        // The base is in every replayed event's parents, so it never moves,
        // and only stepped events can: what the parents hold above the
        // base is found by walking back to it.
        let mut above: HashSet<Lv, BuildHasherDefault<IdHasher>> = HashSet::default();
        let mut stack: Vec<Lv> = event.parents.clone();
        while let Some(x) = stack.pop() {
            if !self.in_base(x) && above.insert(x) {
                stack.extend(self.log.parents(x));
            }
        }
        for k in (0..self.stepped.len()).rev() {
            let x = self.stepped[k];
            if self.prepared.contains(&x) && !above.contains(&x) {
                self.retreat(x);
                self.prepared.remove(&x);
            }
        }
        for k in 0..self.stepped.len() {
            let x = self.stepped[k];
            if !self.prepared.contains(&x) && above.contains(&x) {
                self.advance(x);
                self.prepared.insert(x);
            }
        }
        let at = self.stepped.partition_point(|&x| x < lv);
        self.stepped.insert(at, lv);
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

    /// Whether event `x`, in the history of an event being stepped, is in
    /// the base.
    fn in_base(&self, x: Lv) -> bool {
        match &self.base {
            Some(base) => base[x],
            None => self.stepped.binary_search(&x).is_err(),
        }
    }

    fn index_of(&self, id: usize) -> usize {
        *self
            .index()
            .by_id
            .get(&id)
            .expect("every event's record is in the list")
    }

    /// The index of the record holding item (or portal) `item`.
    fn index_of_item(&self, item: usize) -> usize {
        let index = self.index();
        if !(BASE_ITEM..MARKER).contains(&item) {
            return *index.by_id.get(&item).expect("every item is in the list");
        }
        let k = index.base.partition_point(|&(first, _)| first <= item);
        let (first, i) = index.base[k.checked_sub(1).expect("every item is in the list")];
        debug_assert!(
            item < first + self.records[i].len,
            "every item is in the list"
        );
        i
    }

    fn retreat(&mut self, x: Lv) {
        match self.log.kind(x) {
            Kind::Undelete => return self.shift_undelete(x, false),
            Kind::Move => return self.shift_portal(x, false),
            Kind::Mark => return,
            _ => {}
        }
        let insert = self.log.is_insert(x);
        let i = if insert {
            self.index_of(x)
        } else {
            self.index_of(self.targets[&x])
        };
        let record = &mut self.records[i];
        record.prepare = match (insert, record.prepare) {
            (true, Prepare::Inserted) => Prepare::NotYet,
            (false, Prepare::Deleted(1)) => Prepare::Inserted,
            (false, Prepare::Deleted(n)) => Prepare::Deleted(n - 1),
            (_, state) => unreachable!("retreating {x} from {state:?}"),
        };
    }

    fn advance(&mut self, x: Lv) {
        match self.log.kind(x) {
            Kind::Undelete => return self.shift_undelete(x, true),
            Kind::Move => return self.shift_portal(x, true),
            Kind::Mark => return,
            _ => {}
        }
        let insert = self.log.is_insert(x);
        let i = if insert {
            self.index_of(x)
        } else {
            self.index_of(self.targets[&x])
        };
        let record = &mut self.records[i];
        record.prepare = match (insert, record.prepare) {
            (true, Prepare::NotYet) => Prepare::Inserted,
            (false, Prepare::Inserted) => Prepare::Deleted(1),
            (false, Prepare::Deleted(n)) => Prepare::Deleted(n + 1),
            (_, state) => unreachable!("advancing {x} from {state:?}"),
        };
    }

    /// Move `x`'s portal joins (`forward`) or leaves the prepare version,
    /// if it has one (a move that does nothing has none).
    fn shift_portal(&mut self, x: Lv, forward: bool) {
        if let Some(&i) = self.index().by_id.get(&x) {
            let r = &mut self.records[i];
            debug_assert!(r.portal);
            r.prepare = if forward {
                Prepare::Inserted
            } else {
                Prepare::NotYet
            };
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
        let i = self.index_of(self.targets[&d]);
        let record = &mut self.records[i];
        record.prepare = match (forward, record.prepare) {
            (true, Prepare::Deleted(1)) => Prepare::Inserted,
            (true, Prepare::Deleted(n)) => Prepare::Deleted(n - 1),
            (false, Prepare::Inserted) => Prepare::Deleted(1),
            (false, Prepare::Deleted(n)) => Prepare::Deleted(n + 1),
            (_, state) => unreachable!("shifting undelete {x} from {state:?}"),
        };
    }

    /// Undoes delete `d`: the item it removed shows again if nothing else
    /// still removes it.
    fn undelete(&mut self, d: Lv, out: Option<&mut Changes<'a, T>>) {
        let count = self.undos.get(&d).copied().unwrap_or(0) + 1;
        self.undos.insert(d, count);
        let first_here = count == 1;
        let i = self.index_of(self.targets[&d]);
        if first_here {
            let record = &mut self.records[i];
            record.prepare = match record.prepare {
                Prepare::Deleted(1) => Prepare::Inserted,
                Prepare::Deleted(n) => Prepare::Deleted(n - 1),
                state => unreachable!("undoing delete {d} of an item {state:?}"),
            };
        }
        // Already undone in the effect version by a concurrent undelete.
        if !self.undone.insert(d) {
            return;
        }
        self.records[i].dead -= 1;
        if self.records[i].dead == 0
            && let Some(out) = out
        {
            let id = self.identity(i);
            out.insert(self.effect_pos(i), id);
        }
    }

    /// The insert record `i`'s item came from: a base item's is found by
    /// its place in the base document.
    fn identity(&mut self, i: usize) -> Lv {
        let id = self.records[i].id;
        if id < BASE_ITEM {
            return id;
        }
        let (log, base) = (self.log, &self.base);
        match &mut self.base_ids {
            BaseIds::Reference(ids) => ids.get_or_insert_with(|| {
                crate::reference::visible_ids(
                    log,
                    base.as_ref().expect("the oracle keeps its base"),
                )
            })[id - BASE_ITEM],
            BaseIds::Given { frontier, ids } => ids
                .get_or_insert_with(|| log.ids_at(frontier))
                .get(id - BASE_ITEM),
        }
    }

    // ---- Frames ----

    fn present(&self, r: &Record, v: In) -> bool {
        match v {
            In::Prepare => r.prepare != Prepare::NotYet,
            In::Effect => !r.portal || self.effect.contains(&r.id),
        }
    }

    fn shows(&self, r: &Record, v: In) -> bool {
        !r.portal
            && match v {
                In::Prepare => r.prepare == Prepare::Inserted,
                In::Effect => r.dead == 0,
            }
    }

    fn intervals(&self, moves: &[Moved]) -> Vec<Active> {
        moves
            .iter()
            .map(|&m| Active {
                m,
                lo: self.index_of_item(m.first),
                hi: self.index_of_item(m.last),
            })
            .collect()
    }

    /// Which moves are active in version `v`, by the reference's rules.
    fn active(&self, v: In) -> Vec<Moved> {
        let has = |id: Lv| match v {
            In::Prepare => self.prepared.contains(&id),
            In::Effect => self.effect.contains(&id),
        };
        let candidates: Vec<Moved> = self
            .moves
            .iter()
            .copied()
            .filter(|m| m.in_place || has(m.id))
            .collect();
        let slot = (v == In::Effect) as usize;
        if let Some((c, answer)) = &self.active_cache.borrow()[slot]
            && *c == candidates
        {
            return answer.clone();
        }
        let answer = self.decide(&candidates);
        self.active_cache.borrow_mut()[slot] = Some((candidates, answer.clone()));
        answer
    }

    /// The rules, over `candidates` in order. They're decided one by one,
    /// each against those before it, so the base's moves, always first,
    /// start from what they decided once.
    fn decide(&self, candidates: &[Moved]) -> Vec<Moved> {
        let n = self.in_place;
        let (start, rest) = match &self.base_active {
            Some(base) if candidates.len() >= n && candidates[..n].iter().all(|m| m.in_place) => {
                (self.intervals(base), &candidates[n..])
            }
            _ => (Vec::new(), candidates),
        };
        let mut active: Vec<Active> = start;
        for a in self.intervals(rest) {
            let (lo, hi) = (a.lo, a.hi);
            let crosses = active.iter().any(|b| {
                (b.lo < lo && lo <= b.hi && b.hi < hi) || (lo < b.lo && b.lo <= hi && hi < b.hi)
            });
            if crosses {
                continue;
            }
            let replaced = active.iter().position(|b| (b.lo, b.hi) == (lo, hi));
            let replaced = replaced.map(|k| active.remove(k));
            active.push(a);
            if self.loops(&active, a.m.id) {
                active.pop();
                active.extend(replaced);
            }
        }
        active.into_iter().map(|a| a.m).collect()
    }

    /// The smallest of `active`'s intervals holding `lo..=hi`, but not
    /// `skip`.
    fn holding(active: &[Active], lo: usize, hi: usize, skip: Option<Lv>) -> Option<Active> {
        active
            .iter()
            .filter(|b| b.lo <= lo && hi <= b.hi && Some(b.m.id) != skip)
            .min_by_key(|b| b.hi - b.lo)
            .copied()
    }

    /// Where move `a` is drawn: its portal, or (in place) itself.
    fn location(&self, a: &Active) -> (usize, usize) {
        if a.m.in_place {
            (a.lo, a.hi)
        } else {
            let p = self.index_of_item(a.m.id);
            (p, p)
        }
    }

    /// Whether move `m` is drawn inside its own interval.
    fn loops(&self, active: &[Active], m: Lv) -> bool {
        let mut cur = *active.iter().find(|a| a.m.id == m).unwrap();
        for _ in 0..=active.len() {
            let (lo, hi) = self.location(&cur);
            let skip = cur.m.in_place.then_some(cur.m.id);
            match Self::holding(active, lo, hi, skip) {
                None => return false,
                Some(f) if f.m.id == m => return true,
                Some(f) => cur = f,
            }
        }
        unreachable!("the active moves had a loop before {m}")
    }

    /// Lays out `moves`' intervals: as they nest, one sweep over the
    /// records, intervals sorted by where they start (the outer first),
    /// finds what holds what.
    fn layout(&self, moves: &[Moved]) -> Layout {
        let active = self.intervals(moves);
        let mut order: Vec<usize> = (0..active.len()).collect();
        order.sort_unstable_by_key(|&k| (active[k].lo, Reverse(active[k].hi)));
        let mut inner = Vec::with_capacity(self.records.len());
        let mut parent = vec![None; active.len()];
        let mut stack: Vec<usize> = Vec::new();
        let mut next = order.iter().peekable();
        for i in 0..self.records.len() {
            while stack.last().is_some_and(|&t| active[t].hi < i) {
                stack.pop();
            }
            while let Some(&k) = next.next_if(|&&k| active[k].lo == i) {
                debug_assert!(active[k].lo <= active[k].hi);
                debug_assert!(
                    stack.last().is_none_or(|&t| active[k].hi <= active[t].hi),
                    "active intervals nest"
                );
                parent[k] = stack.last().copied();
                stack.push(k);
            }
            inner.push(stack.last().copied());
        }
        // Counted, then placed: by start, each start's in `active`'s order.
        let mut start_at = vec![0; self.records.len() + 1];
        for a in &active {
            start_at[a.lo + 1] += 1;
        }
        for i in 0..self.records.len() {
            start_at[i + 1] += start_at[i];
        }
        let mut fill = start_at.clone();
        let mut starting = vec![0; active.len()];
        let mut by_id = Map::with_capacity_and_hasher(active.len(), Default::default());
        let mut portal = Map::default();
        for (k, a) in active.iter().enumerate() {
            starting[fill[a.lo]] = k;
            fill[a.lo] += 1;
            by_id.insert(a.m.id, k);
            if !a.m.in_place {
                portal.insert(a.m.id, k);
            }
        }
        Layout {
            active,
            inner,
            parent,
            start_at,
            starting,
            by_id,
            portal,
        }
    }

    /// The layout of `moves`, kept until a record goes in.
    fn laid_out(&self, moves: &[Moved]) -> Rc<Layout> {
        let cached = match &*self.layout.borrow() {
            Some((m, lay)) if m == moves => Some(lay.clone()),
            _ => None,
        };
        cached.unwrap_or_else(|| {
            let lay = Rc::new(self.layout(moves));
            *self.layout.borrow_mut() = Some((moves.to_vec(), lay.clone()));
            lay
        })
    }

    /// The records of version `v`, drawn with `moves` active.
    fn draw(&self, v: In, moves: &[Moved]) -> (Vec<Entry>, Rc<Layout>) {
        let lay = self.laid_out(moves);
        let mut sink = Entries {
            out: Vec::with_capacity(self.records.len()),
            path: Vec::new(),
            shared: vec![Rc::from([])],
        };
        let _ = self.draw_frame(v, &lay, None, 0..self.records.len(), &mut sink);
        (sink.out, lay)
    }

    /// Draws the records of frame `frame` in `range` into `sink`, in
    /// order, until it says to stop.
    fn draw_frame(
        &self,
        v: In,
        lay: &Layout,
        frame: Option<usize>,
        range: Range<usize>,
        sink: &mut impl Sink,
    ) -> ControlFlow<()> {
        let mut i = range.start;
        while i < range.end {
            let r = &self.records[i];
            if lay.inner[i] == frame {
                if self.present(r, v) {
                    if r.portal {
                        if let Some(&k) = lay.portal.get(&r.id) {
                            let a = &lay.active[k];
                            sink.enter(a.m.id);
                            self.draw_frame(v, lay, Some(k), a.lo..a.hi + 1, sink)?;
                            sink.leave();
                        }
                    } else {
                        sink.record(i, self.shows(r, v), r.len)?;
                    }
                }
                i += 1;
                continue;
            }
            // A block of the base, drawn where it lies.
            let here = lay
                .starts(i)
                .iter()
                .copied()
                .find(|&k| lay.active[k].m.in_place && lay.parent[k] == frame);
            if let Some(k) = here {
                let a = &lay.active[k];
                sink.enter(a.m.id);
                self.draw_frame(v, lay, Some(k), a.lo..a.hi + 1, sink)?;
                sink.leave();
                i = a.hi + 1;
            } else {
                i += 1;
            }
        }
        ControlFlow::Continue(())
    }

    /// The index of the record holding the `pos`th item visible in the
    /// prepare version, split out into a record of its own if it's inside
    /// a run of base items. The placeholder never runs out, so there
    /// always is one.
    fn prepared_at(&mut self, pos: usize) -> usize {
        let lay = self.laid_out(&self.active(In::Prepare));
        let mut find = Find {
            seen: 0,
            found: None,
            stop: |at, visible, len, seen| {
                (visible && pos < seen + len).then_some((at, pos - seen))
            },
        };
        let _ = self.draw_frame(In::Prepare, &lay, None, 0..self.records.len(), &mut find);
        let (at, offset) = find
            .found
            .expect("the placeholder is longer than any document");
        self.isolate(at, offset)
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
        self.insert_record(i + 1, rest);
    }

    /// How many items the effect version shows before record `i`.
    fn effect_pos(&self, i: usize) -> usize {
        let lay = self.laid_out(&self.active(In::Effect));
        let mut find = Find {
            seen: 0,
            found: None,
            stop: |at, _, _, seen| (at == i).then_some(seen),
        };
        let _ = self.draw_frame(In::Effect, &lay, None, 0..self.records.len(), &mut find);
        find.found
            .unwrap_or_else(|| unreachable!("record {i} is drawn"))
    }

    /// What draws `entry` in the frame at `depth` of its path: its first
    /// and last record index. An item draws itself; a block, by its
    /// portal, or (in place) by its whole interval.
    fn element(&self, lay: &Layout, entry: &Entry, depth: usize) -> (usize, usize) {
        match entry.path.get(depth) {
            None => (entry.at, entry.at),
            Some(g) => self.location(&lay.active[lay.by_id[g]]),
        }
    }

    /// The origins for something put in at visible position `pos` of the
    /// prepare version: the elements either side of it, in their lowest
    /// common frame.
    fn origins(&mut self, pos: usize) -> (Option<usize>, Option<usize>) {
        let left_at = pos.checked_sub(1).map(|p| self.prepared_at(p));
        let (entries, lay) = self.draw(In::Prepare, &self.active(In::Prepare));
        let k = left_at.map(|i| entries.iter().position(|e| e.at == i).unwrap());
        // The frame they share is decided by what shows: the visible item
        // after, not a tombstone, so an edge is where it looks to be.
        let from = k.map_or(0, |k| k + 1);
        let right = entries[from..].iter().find(|e| e.visible);
        let left_path: &[Lv] = k.map_or(&[], |k| &entries[k].path);
        let right_path: &[Lv] = right.map_or(&[], |e| &e.path);
        let depth = left_path
            .iter()
            .zip(right_path)
            .take_while(|(a, b)| a == b)
            .count();
        let frame = depth.checked_sub(1).map(|d| left_path[d]);
        let lifted = k.map(|k| self.element(&lay, &entries[k], depth).1);
        let left = lifted.map(|i| {
            let r = &self.records[i];
            // A block's last item stands on its own (see `lay_base`).
            r.id + r.len.saturating_sub(1)
        });
        (left, self.next_element(&lay, frame, lifted))
    }

    /// The next element after record `after` (or from the start) in frame
    /// `frame`'s own order: a record of the frame, or the first of a block
    /// in it.
    fn next_element(&self, lay: &Layout, frame: Option<Lv>, after: Option<usize>) -> Option<usize> {
        let frame = frame.map(|f| lay.by_id[&f]);
        let (lo, end) = match frame {
            None => (0, self.records.len()),
            Some(k) => (lay.active[k].lo, lay.active[k].hi + 1),
        };
        let from = after.map_or(lo, |a| a + 1);
        (from..end).find_map(|i| {
            let r = &self.records[i];
            if !self.present(r, In::Prepare) {
                return None;
            }
            // Its own record, or where a block in it starts: any block, not
            // just the innermost here, as blocks nest and can start at the
            // same record.
            let ours =
                lay.inner[i] == frame || lay.starts(i).iter().any(|&g| lay.parent[g] == frame);
            ours.then_some(r.id)
        })
    }

    fn put(&mut self, lv: Lv, left: Option<usize>, right: Option<usize>, record: Record) -> usize {
        let index = self.index();
        let me = self.log.order_key(lv);
        let dest = place(
            &self.records,
            left,
            right,
            |id| index.by_id[&id],
            |other| me.cmp(&self.log.order_key(other)),
        );
        self.insert_record(dest, record);
        dest
    }

    fn insert(&mut self, lv: Lv, pos: usize, out: Option<&mut Changes<'a, T>>) {
        // The origins, as the insert's author saw them. An insert sticks to
        // what's before it: its left origin is the visible item just before
        // the position (none at the start), and its right is whatever came
        // next in that version, deleted or not. Concurrent items, not in
        // the version, don't count. (diamond-types' convention, and the
        // intent: "at the start" stays at the start, wherever tombstones
        // lie.) With moves, both are lifted to the frame they share.
        let (left, right) = self.origins(pos);
        let record = Record {
            id: lv,
            len: 1,
            left,
            right,
            prepare: Prepare::Inserted,
            dead: 0,
            portal: false,
        };
        let dest = self.put(lv, left, right, record);
        if let Some(out) = out {
            out.insert(self.effect_pos(dest), lv);
        }
    }

    fn delete(&mut self, lv: Lv, pos: usize, out: Option<&mut Changes<'a, T>>) {
        let at = self.prepared_at(pos);
        self.records[at].prepare = Prepare::Deleted(1);
        self.targets.insert(lv, self.records[at].id);
        let Some(out) = out else {
            self.records[at].dead += 1;
            return;
        };
        // What it removed, for an undo later on.
        let item = self.identity(at);
        out.learned.push((lv, item));
        // Already deleted in the effect version by a concurrent delete:
        // nothing more to do to the document.
        if self.records[at].dead == 0 {
            out.delete(self.effect_pos(at));
        }
        self.records[at].dead += 1;
    }

    /// A mark of the item at `pos`: it wins if it's the latest of the item's
    /// marks so far.
    fn mark(&mut self, lv: Lv, pos: usize, level: u8, out: Option<&mut Changes<'a, T>>) {
        let at = self.prepared_at(pos);
        let id = self.records[at].id;
        let me = (self.log.lamport(lv), self.log.order_key(lv));
        if self.marks.get(&id).is_some_and(|won| *won > me) {
            return;
        }
        self.marks.insert(id, me);
        if let Some(out) = out {
            let item = self.identity(at);
            let pos = (self.records[at].dead == 0).then(|| self.effect_pos(at));
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
        // By id: splitting out the second can move the first, which may
        // lie after it in the records' order.
        let f = self.prepared_at(pos);
        let f = self.records[f].id;
        let l = self.prepared_at(pos + len - 1);
        let l = self.records[l].id;
        let (f, l) = (self.index_of_item(f), self.index_of_item(l));
        let (entries, lay) = self.draw(In::Prepare, &self.active(In::Prepare));
        let fe = entries.iter().find(|e| e.at == f).unwrap();
        let le = entries.iter().find(|e| e.at == l).unwrap();
        let depth = fe
            .path
            .iter()
            .zip(le.path.iter())
            .take_while(|(a, b)| a == b)
            .count();
        let prefix = &fe.path[..depth];
        let lo = self.element(&lay, fe, depth).0;
        let hi = self.element(&lay, le, depth).1;
        // What the elements from `lo` to `hi` draw must be the range.
        let mut at = 0;
        let mut covered = Vec::new();
        for e in entries.iter().filter(|e| e.visible) {
            let n = self.records[e.at].len;
            if e.path.starts_with(prefix) {
                let (a, b) = self.element(&lay, e, depth);
                if lo <= a && b <= hi {
                    covered.push((at, n));
                }
            }
            at += n;
        }
        let count: usize = covered.iter().map(|&(_, n)| n).sum();
        if count != len || covered.first().map(|&(p, _)| p) != Some(pos) {
            return None;
        }
        let first = self.records[lo].id;
        let last = {
            let r = &self.records[hi];
            r.id + r.len.saturating_sub(1)
        };
        Some((first, last))
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
        let portal = Record {
            id: lv,
            len: 0,
            left,
            right,
            prepare: Prepare::Inserted,
            dead: 0,
            portal: true,
        };
        self.put(lv, left, right, portal);
        let before = self.active(In::Effect);
        self.add_move(Moved {
            id: lv,
            first,
            last,
            in_place: false,
        });
        // Counted in the effect version for the question that follows.
        self.effect.insert(lv);
        let after = self.active(In::Effect);
        if let Some(out) = out {
            self.emit(&before, &after, out);
        }
    }

    /// The changes that take the effect document from `before`'s moves to
    /// `after`'s. Both are drawn (each a valid state, as the rules made
    /// them), and the items' order in the first is turned into the order in
    /// the second by moving runs: each run of the second that's out of
    /// place, as long as it can be, moved to where it goes. The items keep
    /// who they are; only their order changes.
    fn emit(&self, before: &[Moved], after: &[Moved], out: &mut Changes<'a, T>) {
        if before == after {
            return;
        }
        let items = |moves: &[Moved]| -> Vec<(usize, usize)> {
            let (entries, _) = self.draw(In::Effect, moves);
            let mut items = Vec::new();
            for e in entries.iter().filter(|e| e.visible) {
                let r = &self.records[e.at];
                // The placeholder's tail past the real document is always
                // last, and never moves.
                if r.id >= BASE_ITEM && r.id + r.len == BASE_ITEM + UNDERWATER {
                    continue;
                }
                items.extend((0..r.len).map(|k| (e.at, k)));
            }
            items
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
}
