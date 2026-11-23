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

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::blocks::Blocks;
use crate::ids::Ids;
use crate::oplog::{Kind, Lv, Op, OpLog};
use crate::order::{Placed, place};
use crate::walk::{Change, Changes};

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
    path: Vec<Lv>,
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
    /// The prepare version, as a set.
    prepared: Vec<bool>,
    /// The events stepped so far, in order: all the prepare version can
    /// hold beyond the base.
    stepped: Vec<Lv>,
    /// The effect version, as a set: the base, and everything stepped.
    effect: Vec<bool>,
    /// Which record each delete replayed so far removed.
    targets: HashMap<Lv, usize>,
    /// How many undeletes of each delete the prepare version holds.
    undos: HashMap<Lv, u32>,
    /// The deletes undone in the effect version.
    undone: HashSet<Lv>,
    /// Which insert each base item came from, once asked.
    base_ids: BaseIds,
    base: Vec<bool>,
    /// The moves that do something, in the order activity is decided in.
    moves: Vec<Moved>,
}

/// Where the base items' identities come from: the reference (the oracle
/// walk), or the fast walk's, given or found when first asked.
enum BaseIds {
    Reference(Option<Vec<Lv>>),
    Given { frontier: Vec<Lv>, ids: Option<Ids> },
}

/// A stretch of a merge with moves, for the fast walk: the simple walk's
/// way, from a base whose identities and blocks are known (or found).
pub(crate) struct Window<'a, T>(Walk<'a, T>);

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
        let base = log.closure(&frontier);
        let mut walk = Walk {
            log,
            records: Vec::new(),
            prepared: base.clone(),
            stepped: Vec::new(),
            effect: base.clone(),
            targets: HashMap::new(),
            undos: HashMap::new(),
            undone: HashSet::new(),
            base_ids: BaseIds::Given { frontier, ids },
            base,
            moves: Vec::new(),
        };
        walk.lay_base(blocks.0.clone());
        Window(walk)
    }

    pub(crate) fn step(&mut self, lv: Lv, out: Option<&mut Changes<'a, T>>) {
        self.0.step(lv, out);
    }

    /// The moved blocks of the document so far.
    pub(crate) fn blocks(&self) -> Blocks {
        let walk = &self.0;
        let (entries, active) = walk.draw(In::Effect, &walk.active(In::Effect));
        let mut spans: Vec<(Lv, Option<usize>, usize)> =
            active.iter().map(|a| (a.m.id, None, 0)).collect();
        let mut pos = 0;
        for e in entries.iter().filter(|e| e.visible) {
            let r = &walk.records[e.at];
            if r.id >= BASE_ITEM && r.id + r.len == BASE_ITEM + UNDERWATER {
                continue;
            }
            for (id, start, end) in &mut spans {
                if e.path.contains(id) {
                    start.get_or_insert(pos);
                    *end = pos + r.len;
                }
            }
            pos += r.len;
        }
        Blocks(
            spans
                .into_iter()
                .filter_map(|(id, start, end)| Some((id, start?..end)))
                .collect(),
        )
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
        let mut walk = Walk {
            log: self,
            records: Vec::new(),
            prepared: base.clone(),
            stepped: Vec::new(),
            effect: base.clone(),
            targets: HashMap::new(),
            undos: HashMap::new(),
            undone: HashSet::new(),
            base_ids: BaseIds::Reference(None),
            base: base.clone(),
            moves: Vec::new(),
        };
        walk.lay_base(crate::reference::blocks(self, &base));
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
    /// The placeholder, cut wherever a base block starts or ends (its last
    /// item on its own, so it can be an origin), and the blocks as
    /// in-place moves.
    fn lay_base(&mut self, blocks: Vec<(Lv, Range<usize>)>) {
        let mut cuts: Vec<usize> = blocks
            .iter()
            .flat_map(|(_, r)| [r.start, r.end - 1, r.end])
            .collect();
        cuts.push(0);
        cuts.sort_unstable();
        cuts.dedup();
        cuts.push(UNDERWATER);
        for w in cuts.windows(2) {
            self.records.push(Record {
                id: BASE_ITEM + w[0],
                len: w[1] - w[0],
                left: None,
                right: None,
                prepare: Prepare::Inserted,
                dead: 0,
                portal: false,
            });
        }
        for (id, r) in blocks {
            self.add_move(Moved {
                id,
                first: BASE_ITEM + r.start,
                last: BASE_ITEM + r.end - 1,
                in_place: true,
            });
        }
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
        let mut above: HashSet<Lv> = HashSet::new();
        let mut stack: Vec<Lv> = event.parents.clone();
        while let Some(x) = stack.pop() {
            if !self.base[x] && above.insert(x) {
                stack.extend(self.log.parents(x));
            }
        }
        for k in (0..self.stepped.len()).rev() {
            let x = self.stepped[k];
            if self.prepared[x] && !above.contains(&x) {
                self.retreat(x);
                self.prepared[x] = false;
            }
        }
        for k in 0..self.stepped.len() {
            let x = self.stepped[k];
            if !self.prepared[x] && above.contains(&x) {
                self.advance(x);
                self.prepared[x] = true;
            }
        }
        let at = self.stepped.partition_point(|&x| x < lv);
        self.stepped.insert(at, lv);
        match &event.op {
            Op::Insert { pos, .. } => self.insert(lv, *pos, out),
            Op::Delete { pos } => self.delete(lv, *pos, out),
            Op::Undelete { target } => self.undelete(*target, out),
            &Op::Move { pos, len, to } => self.move_(lv, pos, len, to, out),
        }
        self.prepared[lv] = true;
        self.effect[lv] = true;
    }

    fn index_of(&self, id: usize) -> usize {
        self.records
            .iter()
            .position(|r| r.id == id)
            .expect("every event's record is in the list")
    }

    /// The index of the record holding item (or portal) `item`.
    fn index_of_item(&self, item: usize) -> usize {
        self.records
            .iter()
            .position(|r| {
                if r.portal {
                    r.id == item
                } else {
                    r.id <= item && item < r.id + r.len
                }
            })
            .expect("every item is in the list")
    }

    fn retreat(&mut self, x: Lv) {
        match self.log.kind(x) {
            Kind::Undelete => return self.shift_undelete(x, false),
            Kind::Move => return self.shift_portal(x, false),
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
        if let Some(r) = self.records.iter_mut().find(|r| r.portal && r.id == x) {
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
            BaseIds::Reference(ids) => {
                ids.get_or_insert_with(|| crate::reference::visible_ids(log, base))[id - BASE_ITEM]
            }
            BaseIds::Given { frontier, ids } => ids
                .get_or_insert_with(|| log.ids_at(frontier))
                .get(id - BASE_ITEM),
        }
    }

    // ---- Frames ----

    fn present(&self, r: &Record, v: In) -> bool {
        match v {
            In::Prepare => r.prepare != Prepare::NotYet,
            In::Effect => !r.portal || self.effect[r.id],
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
            In::Prepare => self.prepared[id],
            In::Effect => self.effect[id],
        };
        let candidates: Vec<Moved> = self
            .moves
            .iter()
            .copied()
            .filter(|m| m.in_place || has(m.id))
            .collect();
        self.decide(&candidates)
    }

    /// The rules, over `candidates` in order.
    fn decide(&self, candidates: &[Moved]) -> Vec<Moved> {
        let mut active: Vec<Active> = Vec::new();
        for a in self.intervals(candidates) {
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

    /// The frame `a` lies in.
    fn parent(active: &[Active], a: &Active) -> Option<Lv> {
        Self::holding(active, a.lo, a.hi, Some(a.m.id)).map(|b| b.m.id)
    }

    /// The records of version `v`, drawn with `moves` active.
    fn draw(&self, v: In, moves: &[Moved]) -> (Vec<Entry>, Vec<Active>) {
        let active = self.intervals(moves);
        let mut out = Vec::new();
        let mut path = Vec::new();
        self.draw_frame(v, &active, None, 0..self.records.len(), &mut path, &mut out);
        (out, active)
    }

    fn draw_frame(
        &self,
        v: In,
        active: &[Active],
        frame: Option<Lv>,
        range: Range<usize>,
        path: &mut Vec<Lv>,
        out: &mut Vec<Entry>,
    ) {
        let mut i = range.start;
        while i < range.end {
            let r = &self.records[i];
            let inner = Self::holding(active, i, i, None).map(|a| a.m.id);
            if inner == frame {
                if self.present(r, v) {
                    if r.portal {
                        if let Some(a) = active.iter().find(|a| a.m.id == r.id && !a.m.in_place) {
                            path.push(a.m.id);
                            self.draw_frame(v, active, Some(a.m.id), a.lo..a.hi + 1, path, out);
                            path.pop();
                        }
                    } else {
                        out.push(Entry {
                            at: i,
                            visible: self.shows(r, v),
                            path: path.clone(),
                        });
                    }
                }
                i += 1;
                continue;
            }
            // A block of the base, drawn where it lies.
            let here = active
                .iter()
                .find(|a| a.m.in_place && a.lo == i && Self::parent(active, a) == frame);
            if let Some(a) = here {
                path.push(a.m.id);
                self.draw_frame(v, active, Some(a.m.id), a.lo..a.hi + 1, path, out);
                path.pop();
                i = a.hi + 1;
            } else {
                i += 1;
            }
        }
    }

    /// The index of the record holding the `pos`th item visible in the
    /// prepare version, split out into a record of its own if it's inside
    /// a run of base items. The placeholder never runs out, so there
    /// always is one.
    fn prepared_at(&mut self, pos: usize) -> usize {
        let (entries, _) = self.draw(In::Prepare, &self.active(In::Prepare));
        let mut seen = 0;
        for e in entries.iter().filter(|e| e.visible) {
            let len = self.records[e.at].len;
            if pos < seen + len {
                return self.isolate(e.at, pos - seen);
            }
            seen += len;
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
        let (entries, _) = self.draw(In::Effect, &self.active(In::Effect));
        let mut pos = 0;
        for e in &entries {
            if e.at == i {
                return pos;
            }
            if e.visible {
                pos += self.records[e.at].len;
            }
        }
        unreachable!("record {i} is drawn")
    }

    /// What draws `entry` in the frame at `depth` of its path: its first
    /// and last record index. An item draws itself; a block, by its
    /// portal, or (in place) by its whole interval.
    fn element(&self, active: &[Active], entry: &Entry, depth: usize) -> (usize, usize) {
        match entry.path.get(depth) {
            None => (entry.at, entry.at),
            Some(&g) => {
                let a = active.iter().find(|a| a.m.id == g).unwrap();
                self.location(a)
            }
        }
    }

    /// The origins for something put in at visible position `pos` of the
    /// prepare version: the elements either side of it, in their lowest
    /// common frame.
    fn origins(&mut self, pos: usize) -> (Option<usize>, Option<usize>) {
        let left_at = pos.checked_sub(1).map(|p| self.prepared_at(p));
        let (entries, active) = self.draw(In::Prepare, &self.active(In::Prepare));
        let k = left_at.map(|i| entries.iter().position(|e| e.at == i).unwrap());
        // The frame they share is decided by what shows: the visible item
        // after, not a tombstone, so an edge is where it looks to be.
        let from = k.map_or(0, |k| k + 1);
        let right = entries[from..].iter().find(|e| e.visible);
        let empty = Vec::new();
        let left_path = k.map_or(&empty, |k| &entries[k].path);
        let right_path = right.map_or(&empty, |e| &e.path);
        let depth = left_path
            .iter()
            .zip(right_path)
            .take_while(|(a, b)| a == b)
            .count();
        let frame = depth.checked_sub(1).map(|d| left_path[d]);
        let lifted = k.map(|k| self.element(&active, &entries[k], depth).1);
        let left = lifted.map(|i| {
            let r = &self.records[i];
            // A block's last item stands on its own (see `lay_base`).
            r.id + r.len.saturating_sub(1)
        });
        (left, self.next_element(&active, frame, lifted))
    }

    /// The next element after record `after` (or from the start) in frame
    /// `frame`'s own order: a record of the frame, or the first of a block
    /// in it.
    fn next_element(
        &self,
        active: &[Active],
        frame: Option<Lv>,
        after: Option<usize>,
    ) -> Option<usize> {
        let (lo, end) = match frame {
            None => (0, self.records.len()),
            Some(f) => {
                let a = active.iter().find(|a| a.m.id == f).unwrap();
                (a.lo, a.hi + 1)
            }
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
            let ours = Self::holding(active, i, i, None).map(|a| a.m.id) == frame
                || active
                    .iter()
                    .any(|g| g.lo == i && Self::parent(active, g) == frame);
            ours.then_some(r.id)
        })
    }

    fn put(&mut self, lv: Lv, left: Option<usize>, right: Option<usize>, record: Record) -> usize {
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
        self.records.insert(dest, record);
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
        let (entries, active) = self.draw(In::Prepare, &self.active(In::Prepare));
        let fe = entries.iter().find(|e| e.at == f).unwrap();
        let le = entries.iter().find(|e| e.at == l).unwrap();
        let depth = fe
            .path
            .iter()
            .zip(&le.path)
            .take_while(|(a, b)| a == b)
            .count();
        let prefix = &fe.path[..depth];
        let lo = self.element(&active, fe, depth).0;
        let hi = self.element(&active, le, depth).1;
        // What the elements from `lo` to `hi` draw must be the range.
        let mut at = 0;
        let mut covered = Vec::new();
        for e in entries.iter().filter(|e| e.visible) {
            let n = self.records[e.at].len;
            if e.path.starts_with(prefix) {
                let (a, b) = self.element(&active, e, depth);
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
        self.effect[lv] = true;
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
