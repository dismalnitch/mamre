//! Anchors: where watched items are, as the document changes.
//!
//! An anchor is an item: the insert it came from, which undo and redo
//! never change (see `oplog.rs`). Whatever is pinned to it, a stroke of ink
//! to a paragraph's identifying item, say, finds it again after any edit,
//! merge or undo, here or elsewhere.
//!
//! Only watched items are tracked, so it stays small. Each visible one is a
//! mark in a mamre `SumTree`, in document order, holding how many items
//! lie between it and the mark before; anything after the last mark is the
//! tail. An insert grows the gap it lands in. A delete takes items out of
//! gaps, and parks the watched items it removes: their marks go, and they
//! wait by identity. An undo brings an item back as itself (its change
//! names the insert), so a parked item's mark comes back exactly where the
//! change puts it. Marks only ever sit at visible items, so their order is
//! just where they are.
//!
//! Anchors made with [`Anchors::falling_back`] keep a deleted item's place
//! instead: its mark stays as a *ghost*, counting no item of its own, just
//! before whatever followed the item. Text typed at that place goes after
//! the ghost, and a move of the text after it takes the ghost along. The
//! outline keeps a section whose heading was deleted this way (#19), and
//! comments will fall back the same way (#33). A ghost's place is this
//! replica's: an item deleted before this replica saw it has none.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use mamre::codec::{DecodeError, get_usize, put_usize};
use mamre::cursor::Bias;
use mamre::{Item, SeekTarget, SumTree, Summary};

use crate::key::Key;
use crate::oplog::Lv;
use crate::walk::Change;

/// A watched item that's visible, and the items since the mark before.
#[derive(Clone, Debug)]
struct Mark {
    key: Key,
    item: Lv,
    before: usize,
    /// A deleted item's place: it counts no item.
    ghost: bool,
    /// A ghost that text typed at its place goes before (an end's), not
    /// after (a start's): see [`Anchors::falling_back_left`].
    left: bool,
}

/// How many items a stretch of marks covers (theirs and the gaps before
/// them), and the key of the last.
#[derive(Clone, Debug, Default)]
struct Span {
    items: usize,
    last: Option<Key>,
    /// Whether the last mark is a ghost text goes before.
    left: bool,
}

impl Summary for Span {
    fn empty() -> Self {
        Span::default()
    }
    fn concat(&mut self, other: &Self) {
        self.items += other.items;
        if other.last.is_some() {
            self.last.clone_from(&other.last);
            self.left = other.left;
        }
    }
}

impl Item for Mark {
    type Summary = Span;
    fn summary(&self) -> Span {
        Span {
            items: self.before + usize::from(!self.ghost),
            last: Some(self.key.clone()),
            left: self.ghost && self.left,
        }
    }
}

/// Seeks the mark whose gap holds position `0` or whose item is there:
/// with `Bias::Right`, the first whose gap reaches it. Ghosts at a place
/// stand between the items either side of it: those text goes after
/// first, then those it goes before. So marks are ordered by twice where
/// they end, plus one for a ghost text goes before, and `Pos(p)` stops
/// between the two kinds at `p` (where text typed at `p` goes).
struct Pos(usize);

/// As [`Pos`], past every ghost at the place: where the item at it is.
struct PastGhosts(usize);

fn order_of(position: &Span) -> usize {
    2 * position.items + usize::from(position.left)
}

impl SeekTarget<Span, Span> for Pos {
    fn cmp_position(&self, position: &Span) -> Ordering {
        (2 * self.0).cmp(&order_of(position))
    }
}

impl SeekTarget<Span, Span> for PastGhosts {
    fn cmp_position(&self, position: &Span) -> Ordering {
        (2 * self.0 + 1).cmp(&order_of(position))
    }
}

/// Seeks the mark with this key: with `Bias::Left`, the first whose last
/// key reaches it.
struct At<'k>(&'k Key);

impl SeekTarget<Span, Span> for At<'_> {
    fn cmp_position(&self, position: &Span) -> Ordering {
        match &position.last {
            Some(last) => self.0.cmp(last),
            None => Ordering::Greater,
        }
    }
}

/// Where watched items are in a replica's document, kept beside it by the
/// same edits: local ones with [`insert`](Self::insert) and
/// [`delete`](Self::delete), merges and undos with [`apply`](Self::apply).
#[derive(Clone, Default)]
pub struct Anchors {
    marks: SumTree<Mark>,
    /// Items after the last mark.
    tail: usize,
    /// Each visible watched item's mark, and the watched items deleted now.
    keys: HashMap<Lv, Key>,
    parked: BTreeSet<Lv>,
    /// Whether deleted items keep their places, as ghosts.
    fallback: bool,
    /// Whether text typed at a ghost's place goes before it.
    left_ghosts: bool,
}

impl Anchors {
    /// For a document of `len` items, watching none.
    pub fn new(len: usize) -> Self {
        Anchors {
            tail: len,
            ..Anchors::default()
        }
    }

    /// How many items the document has.
    pub fn len(&self) -> usize {
        self.marks.summary().items + self.tail
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// For a document of `len` items, watching none, where a deleted item
    /// keeps its place: see [`place_of`](Self::place_of). Text typed at the
    /// place goes after it: for the start of something.
    pub fn falling_back(len: usize) -> Self {
        Anchors {
            fallback: true,
            ..Anchors::new(len)
        }
    }

    /// As [`falling_back`](Self::falling_back), but text typed at a deleted
    /// item's place goes before it: for the end of something, so a range
    /// whose items are all deleted holds what's typed where they were
    /// (#33: a comment on "big" follows it to "large").
    pub fn falling_back_left(len: usize) -> Self {
        Anchors {
            left_ghosts: true,
            ..Anchors::falling_back(len)
        }
    }

    /// From now on, deleted items keep their places.
    pub(crate) fn fall_back(&mut self) {
        self.fallback = true;
    }

    /// Starts watching `item`, the one at `pos` now (as
    /// [`Identities::item`](crate::Identities::item) names it).
    pub fn watch(&mut self, pos: usize, item: Lv) {
        assert!(pos < self.len(), "item {pos} is past the end");
        if self.keys.contains_key(&item) {
            debug_assert_eq!(self.position(item), Some(pos), "watched elsewhere");
            return;
        }
        self.parked.remove(&item);
        self.place(pos, item);
    }

    /// Starts watching `item`, which is deleted now: it comes back where
    /// an undo puts it. For anchors restored beside a document, say.
    pub fn watch_deleted(&mut self, item: Lv) {
        if !self.keys.contains_key(&item) {
            self.parked.insert(item);
        }
    }

    /// Stops watching `item`.
    pub fn unwatch(&mut self, item: Lv) {
        self.parked.remove(&item);
        if let Some(key) = self.keys.remove(&item) {
            let ghost = self.is_ghost_key(&key);
            self.unmark(&key, !ghost);
        }
    }

    /// Where `item` is: `None` while it's deleted, or if it isn't watched.
    pub fn position(&self, item: Lv) -> Option<usize> {
        self.mark_of(item)
            .filter(|&(_, ghost)| !ghost)
            .map(|(p, _)| p)
    }

    /// Where `item` is, or, while it's deleted, the place it was (the
    /// position of whatever followed it): only for anchors made
    /// [`falling_back`](Self::falling_back), and only if this replica saw
    /// the item before it went. `None` if it isn't watched.
    pub fn place_of(&self, item: Lv) -> Option<usize> {
        self.mark_of(item).map(|(p, _)| p)
    }

    /// Whether `item` is deleted now but keeps its place.
    pub fn is_ghost(&self, item: Lv) -> bool {
        self.mark_of(item).is_some_and(|(_, ghost)| ghost)
    }

    /// The watched item showing at `pos`, if there is one.
    pub fn item_at(&self, pos: usize) -> Option<Lv> {
        let mut cursor = self.marks.cursor::<Span>();
        cursor.seek(&PastGhosts(pos), Bias::Right);
        let m = cursor.item()?;
        (!m.ghost && cursor.start().items + m.before == pos).then_some(m.item)
    }

    /// The ghosts at `pos`, in order.
    pub fn ghosts_at(&self, pos: usize) -> Vec<Lv> {
        let mut out = Vec::new();
        let mut cursor = self.marks.cursor::<Span>();
        cursor.seek(&Pos(pos), Bias::Left);
        while let Some(m) = cursor.item() {
            let at = cursor.start().items + m.before;
            if at > pos || (at == pos && !m.ghost) {
                break;
            }
            if at == pos {
                out.push(m.item);
            }
            cursor.next();
        }
        out
    }

    /// Where `item`'s mark is, and whether it's a ghost.
    fn mark_of(&self, item: Lv) -> Option<(usize, bool)> {
        let key = self.keys.get(&item)?;
        let mut cursor = self.marks.cursor::<Span>();
        cursor.seek(&At(key), Bias::Left);
        let mark = cursor.item().expect("a watched item has a mark");
        Some((cursor.start().items + mark.before, mark.ghost))
    }

    /// Whether the mark with `key` is a ghost.
    fn is_ghost_key(&self, key: &Key) -> bool {
        let mut cursor = self.marks.cursor::<Span>();
        cursor.seek(&At(key), Bias::Left);
        cursor.item().expect("the mark is there").ghost
    }

    /// Turns `item`'s ghost into a plain parked item: it keeps no place, and
    /// comes back only with its item.
    pub fn forget_place(&mut self, item: Lv) {
        if let Some(key) = self.keys.get(&item).cloned()
            && self.is_ghost_key(&key)
        {
            self.keys.remove(&item);
            self.unmark(&key, false);
        }
    }

    /// Whether `item` is watched and deleted now, waiting to come back.
    pub fn is_parked(&self, item: Lv) -> bool {
        self.parked.contains(&item)
    }

    /// Appends these anchors' bytes: the tail, each visible mark's item
    /// and gap in order, and the parked items (ghosts among them). Order
    /// keys are the tree's own, made afresh on load. Ghosts' places are
    /// side data, written as JSON beside the books: see
    /// [`ghosts`](Self::ghosts).
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        // The visible marks as if there were no ghosts: a ghost's gap goes
        // to the next visible mark's, or the tail.
        let mut shown = Vec::new();
        let (mut pos, mut gap) = (0, 0);
        for m in self.marks.items() {
            let at = pos + m.before;
            gap += m.before;
            debug_assert!(!m.left, "only anchors rebuilt from the log stick left");
            if m.ghost {
                pos = at;
            } else {
                shown.push((m.item, gap));
                gap = 0;
                pos = at + 1;
            }
        }
        put_usize(out, self.tail + gap);
        put_usize(out, shown.len());
        for (item, before) in shown {
            put_usize(out, item);
            put_usize(out, before);
        }
        put_usize(out, self.parked.len());
        let mut prev = 0;
        for &item in &self.parked {
            put_usize(out, item - prev);
            prev = item;
        }
    }

    /// Each ghost and its place, in order: what [`encode`](Self::encode)
    /// leaves out, for [`place_ghosts`](Self::place_ghosts) to put back.
    pub(crate) fn ghosts(&self) -> Vec<(Lv, usize)> {
        let mut ghosts = Vec::new();
        let mut pos = 0;
        for m in self.marks.items() {
            let at = pos + m.before;
            if m.ghost {
                ghosts.push((m.item, at));
                pos = at;
            } else {
                pos = at + 1;
            }
        }
        ghosts
    }

    /// Puts back the ghosts [`ghosts`](Self::ghosts) listed, after
    /// [`decode`](Self::decode): each must be a parked item, and fit.
    pub(crate) fn place_ghosts(&mut self, ghosts: &[(Lv, usize)]) -> Result<(), DecodeError> {
        let mut last = 0;
        for &(item, place) in ghosts {
            if place < last
                || !self.parked.contains(&item)
                || self.keys.contains_key(&item)
                || place > self.len()
            {
                return Err(DecodeError::Invalid("a ghost anchor that can't be"));
            }
            self.fallback = true;
            self.place_ghost(place, item);
            last = place;
        }
        Ok(())
    }

    /// Reads anchors [`encode`](Self::encode) wrote, for a log of `events`
    /// events.
    pub(crate) fn decode(input: &mut &[u8], events: usize) -> Result<Self, DecodeError> {
        let tail = get_usize(input)?;
        let n = get_usize(input)?;
        let mut anchors = Anchors {
            tail,
            ..Anchors::default()
        };
        let mut key: Option<Key> = None;
        for _ in 0..n {
            let item = get_usize(input)?;
            let before = get_usize(input)?;
            if item >= events || anchors.keys.contains_key(&item) {
                return Err(DecodeError::Invalid(
                    "an anchor on an item this log doesn't have",
                ));
            }
            let k = Key::between(key.as_ref(), None);
            anchors.keys.insert(item, k.clone());
            anchors.marks.push(Mark {
                key: k.clone(),
                item,
                before,
                ghost: false,
                left: false,
            });
            key = Some(k);
        }
        let n = get_usize(input)?;
        let mut prev: usize = 0;
        for _ in 0..n {
            let item = prev
                .checked_add(get_usize(input)?)
                .ok_or(DecodeError::Overflow)?;
            if item >= events {
                return Err(DecodeError::Invalid(
                    "an anchor on an item this log doesn't have",
                ));
            }
            anchors.parked.insert(item);
            prev = item;
        }
        Ok(anchors)
    }

    /// A change from a merge or an undo.
    pub fn apply<T>(&mut self, change: &Change<'_, T>) {
        match change {
            Change::Insert { pos, items, first } => {
                self.insert(*pos, items.len());
                // Undone items come back as themselves.
                let back: Vec<Lv> = self
                    .parked
                    .range(*first..*first + items.len())
                    .copied()
                    .collect();
                for item in back {
                    self.revive(pos + (item - first), item);
                }
            }
            Change::Delete { range } => {
                self.remove(range.clone());
            }
            Change::Move { range, to } => self.move_range(range.clone(), *to),
            Change::Mark { .. } | Change::Lost { .. } => {}
        }
    }

    /// A change from a merge or an undo, as [`apply`](Self::apply), saying
    /// which watched items it deleted, and which ghosts it brought back.
    pub(crate) fn apply_noting<T>(&mut self, change: &Change<'_, T>) -> (Vec<Lv>, Vec<Lv>) {
        match change {
            Change::Insert { pos, items, first } => {
                self.insert(*pos, items.len());
                let back: Vec<Lv> = self
                    .parked
                    .range(*first..*first + items.len())
                    .copied()
                    .collect();
                let mut ghosts = Vec::new();
                for item in back {
                    if self.keys.contains_key(&item) {
                        ghosts.push(item);
                    }
                    self.revive(pos + (item - first), item);
                }
                (Vec::new(), ghosts)
            }
            Change::Delete { range } => (self.remove(range.clone()), Vec::new()),
            _ => {
                self.apply(change);
                (Vec::new(), Vec::new())
            }
        }
    }

    /// Parked `item` shows again, at `pos`.
    fn revive(&mut self, pos: usize, item: Lv) {
        self.parked.remove(&item);
        if let Some(key) = self.keys.remove(&item) {
            self.unmark(&key, false);
        }
        self.place(pos, item);
    }

    /// The items in `range` move to `to` (a position before the move,
    /// outside the range): watched ones among them go with them.
    pub fn move_range(&mut self, range: Range<usize>, to: usize) {
        let n = range.len();
        if n == 0 || to == range.start || to == range.end {
            return;
        }
        // The watched items inside, and how far in: ghosts at the start too,
        // since they stand before what's moving.
        let mut inside = Vec::new();
        {
            let mut cursor = self.marks.cursor::<Span>();
            cursor.seek(&Pos(range.start), Bias::Left);
            while let Some(m) = cursor.item() {
                let at = cursor.start().items + m.before;
                if at >= range.end {
                    break;
                }
                if at >= range.start {
                    inside.push((m.item, at - range.start, m.ghost, m.left, m.key.clone()));
                }
                cursor.next();
            }
        }
        for (item, _, ghost, _, key) in &inside {
            self.keys.remove(item);
            self.unmark(key, !ghost);
        }
        self.remove(range.clone());
        let at = if to > range.start { to - n } else { to };
        self.insert(at, n);
        for (item, offset, ghost, left, _) in inside {
            self.put_mark(at + offset, item, ghost, left);
        }
    }

    /// `n` new items go in at `pos`.
    pub fn insert(&mut self, pos: usize, n: usize) {
        if n == 0 {
            return;
        }
        let grown = self
            .marks
            .update_at::<Span, _>(&Pos(pos), Bias::Right, |m| m.before += n);
        if grown.is_none() {
            self.tail += n;
        }
    }

    /// The items in `range` go. Watched ones among them are parked.
    pub fn delete(&mut self, range: Range<usize>) {
        self.remove(range);
    }

    /// The items in `range` go. Watched ones among them are parked (as
    /// ghosts, if these anchors fall back), and returned.
    fn remove(&mut self, range: Range<usize>) -> Vec<Lv> {
        let at = range.start;
        let mut left = range.len();
        let mut parked = Vec::new();
        while left > 0 {
            let found = {
                let mut cursor = self.marks.cursor::<Span>();
                cursor.seek(&PastGhosts(at), Bias::Right);
                cursor
                    .item()
                    .map(|m| (m.key.clone(), m.item, m.ghost, at - cursor.start().items))
            };
            let Some((key, item, ghost, offset)) = found else {
                self.tail -= left;
                break;
            };
            // From the gap first, then the mark's own item.
            let mut taken = 0;
            self.marks.update_at::<Span, _>(&At(&key), Bias::Left, |m| {
                taken = (m.before - offset).min(left);
                m.before -= taken;
            });
            left -= taken;
            if left > 0 && !ghost {
                left -= 1;
                self.parked.insert(item);
                parked.push(item);
                if self.fallback {
                    let left = self.left_ghosts;
                    self.marks.update_at::<Span, _>(&At(&key), Bias::Left, |m| {
                        m.ghost = true;
                        m.left = left;
                    });
                } else {
                    self.keys.remove(&item);
                    self.unmark(&key, false);
                }
            }
        }
        parked
    }

    /// Makes a mark for `item` at `pos`, out of the gap that counts it.
    fn place(&mut self, pos: usize, item: Lv) {
        self.put_mark(pos, item, false, false);
    }

    /// Makes a ghost for deleted `item` at place `pos`: after any ghosts
    /// there already, before the item there.
    fn place_ghost(&mut self, pos: usize, item: Lv) {
        self.put_mark(pos, item, true, false);
    }

    fn put_mark(&mut self, pos: usize, item: Lv, ghost: bool, left: bool) {
        let own = usize::from(!ghost);
        let found = {
            let mut cursor = self.marks.cursor::<Span>();
            // A ghost text goes after: before those it goes before. Anything
            // else: after every ghost there.
            if ghost && !left {
                cursor.seek(&Pos(pos), Bias::Right);
            } else {
                cursor.seek(&PastGhosts(pos), Bias::Right);
            }
            let before = cursor.start().items;
            let prev = cursor.start().last.clone();
            cursor.item().map(|m| (m.key.clone(), before, prev))
        };
        match found {
            Some((next, start, prev)) => {
                let key = Key::between(prev.as_ref(), Some(&next));
                let before = pos - start;
                let mark = Mark {
                    key: key.clone(),
                    item,
                    before,
                    ghost,
                    left,
                };
                self.marks
                    .splice_at::<Span, _, _>(&At(&next), Bias::Left, |m| {
                        let mut old = std::mem::replace(m, mark);
                        old.before -= before + own;
                        ((), [old])
                    });
                self.keys.insert(item, key);
            }
            None => {
                let start = self.marks.summary().items;
                let key = Key::between(self.marks.summary().last.as_ref(), None);
                let before = pos - start;
                self.tail -= before + own;
                self.marks.push(Mark {
                    key: key.clone(),
                    item,
                    before,
                    ghost,
                    left,
                });
                self.keys.insert(item, key);
            }
        }
    }

    /// Takes the mark with `key` away: the items its gap counted go to the
    /// next gap, and its own too if it's still in the document (`kept`).
    fn unmark(&mut self, key: &Key, kept: bool) {
        let (before, next) = {
            let mut cursor = self.marks.cursor::<Span>();
            cursor.seek(&At(key), Bias::Left);
            let before = cursor.item().expect("the mark is there").before;
            cursor.next();
            (
                before + usize::from(kept),
                cursor.item().map(|m| m.key.clone()),
            )
        };
        self.marks.remove_at::<Span>(&At(key), Bias::Left);
        match next {
            Some(next) => {
                self.marks
                    .update_at::<Span, _>(&At(&next), Bias::Left, |m| m.before += before);
            }
            None => self.tail += before,
        }
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use proptest::prelude::*;

    /// The model: the document's items in order, each watched or not, with
    /// ghosts (deleted watched items) between them.
    #[derive(Clone, Debug, PartialEq)]
    enum Slot {
        Item(Option<Lv>),
        Ghost(Lv),
    }

    /// The slot index where position `pos` starts: after the ghosts there.
    fn slot_of(model: &[Slot], pos: usize) -> usize {
        let mut seen = 0;
        for (i, s) in model.iter().enumerate() {
            if let Slot::Item(_) = s {
                if seen == pos {
                    return i;
                }
                seen += 1;
            }
        }
        model.len()
    }

    /// The slot index just after the item before `pos`: before the ghosts
    /// there.
    fn ghosts_from(model: &[Slot], pos: usize) -> usize {
        let mut i = slot_of(model, pos);
        while i > 0 && matches!(model[i - 1], Slot::Ghost(_)) {
            i -= 1;
        }
        i
    }

    fn places(model: &[Slot]) -> Vec<(Lv, usize, bool)> {
        let mut out = Vec::new();
        let mut pos = 0;
        for s in model {
            match *s {
                Slot::Item(w) => {
                    if let Some(item) = w {
                        out.push((item, pos, false));
                    }
                    pos += 1;
                }
                Slot::Ghost(item) => out.push((item, pos, true)),
            }
        }
        out.sort_unstable();
        out
    }

    #[derive(Clone, Debug)]
    enum Step {
        Insert(prop::sample::Index, usize),
        Watch(prop::sample::Index),
        Delete(prop::sample::Index, usize),
        Move(
            prop::sample::Index,
            prop::sample::Index,
            prop::sample::Index,
        ),
        Revive(prop::sample::Index, prop::sample::Index),
        Unwatch(prop::sample::Index),
    }

    fn step() -> impl Strategy<Value = Step> {
        let i = || any::<prop::sample::Index>();
        prop_oneof![
            3 => (i(), 1usize..4).prop_map(|(p, n)| Step::Insert(p, n)),
            3 => i().prop_map(Step::Watch),
            3 => (i(), 1usize..4).prop_map(|(p, n)| Step::Delete(p, n)),
            2 => (i(), i(), i()).prop_map(|(a, b, t)| Step::Move(a, b, t)),
            2 => (i(), i()).prop_map(|(g, p)| Step::Revive(g, p)),
            1 => i().prop_map(Step::Unwatch),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]

        /// Falling-back anchors keep every watched item where it is, and
        /// every deleted one at its place, as a plain list of slots does;
        /// and they read back as saved.
        #[test]
        fn ghosts_keep_their_places(steps in prop::collection::vec(step(), 1..60), left in any::<bool>()) {
            let mut a = if left { Anchors::falling_back_left(3) } else { Anchors::falling_back(3) };
            // Where new items go at `pos`: after the ghosts there, or (if
            // they stick left) before them.
            let ins = |model: &[Slot], pos: usize| if left { ghosts_from(model, pos) } else { slot_of(model, pos) };
            let mut model = vec![Slot::Item(None); 3];
            let mut next_item: Lv = 100;
            for s in steps {
                let len = a.len();
                match s {
                    Step::Insert(p, n) => {
                        let pos = p.index(len + 1);
                        a.insert(pos, n);
                        let at = ins(&model, pos);
                        model.splice(at..at, vec![Slot::Item(None); n]);
                    }
                    Step::Watch(p) if len > 0 => {
                        let pos = p.index(len);
                        let at = slot_of(&model, pos);
                        if model[at] == Slot::Item(None) {
                            a.watch(pos, next_item);
                            model[at] = Slot::Item(Some(next_item));
                            next_item += 1;
                        }
                    }
                    Step::Delete(p, n) if len > 0 => {
                        let pos = p.index(len);
                        let end = (pos + n).min(len);
                        a.delete(pos..end);
                        let (from, to) = (slot_of(&model, pos), slot_of(&model, end));
                        let mut kept = Vec::new();
                        for s in &model[from..to] {
                            match *s {
                                Slot::Item(Some(item)) | Slot::Ghost(item) => kept.push(Slot::Ghost(item)),
                                Slot::Item(None) => {}
                            }
                        }
                        model.splice(from..to, kept);
                    }
                    Step::Move(x, y, t) if len > 0 => {
                        let (x, y) = (x.index(len), y.index(len));
                        let (start, end) = (x.min(y), x.max(y) + 1);
                        let to = t.index(len + 1);
                        if to > start && to < end {
                            continue;
                        }
                        a.move_range(start..end, to);
                        if to == start || to == end {
                            continue;
                        }
                        let (from, until) = (ghosts_from(&model, start), ghosts_from(&model, end));
                        let block: Vec<Slot> = model[from..until].to_vec();
                        let at = ins(&model, to);
                        if to > start {
                            model.splice(at..at, block);
                            model.drain(from..until);
                        } else {
                            model.drain(from..until);
                            model.splice(at..at, block);
                        }
                    }
                    Step::Revive(g, p) => {
                        let ghosts: Vec<Lv> = model.iter().filter_map(|s| match s { Slot::Ghost(i) => Some(*i), _ => None }).collect();
                        if ghosts.is_empty() {
                            continue;
                        }
                        let item = ghosts[g.index(ghosts.len())];
                        let pos = p.index(len + 1);
                        // As an undo's insert brings it back: one item, itself.
                        let items = ['x'];
                        a.apply(&Change::Insert { pos, items: &items, first: item });
                        model.retain(|s| *s != Slot::Ghost(item));
                        let at = ins(&model, pos);
                        model.insert(at, Slot::Item(Some(item)));
                    }
                    Step::Unwatch(k) => {
                        let watched: Vec<Lv> = places(&model).into_iter().map(|(i, _, _)| i).collect();
                        if watched.is_empty() {
                            continue;
                        }
                        let item = watched[k.index(watched.len())];
                        a.unwatch(item);
                        for s in model.iter_mut() {
                            if *s == Slot::Item(Some(item)) {
                                *s = Slot::Item(None);
                            }
                        }
                        model.retain(|s| *s != Slot::Ghost(item));
                    }
                    _ => {}
                }
                let want = places(&model);
                let n_items = model.iter().filter(|s| matches!(s, Slot::Item(_))).count();
                prop_assert_eq!(a.len(), n_items);
                for &(item, pos, ghost) in &want {
                    prop_assert_eq!(a.place_of(item), Some(pos), "place of {} in {:?}", item, model);
                    prop_assert_eq!(a.is_ghost(item), ghost);
                    prop_assert_eq!(a.position(item), (!ghost).then_some(pos));
                    if !ghost {
                        prop_assert_eq!(a.item_at(pos), Some(item));
                    }
                }
                for pos in 0..=n_items {
                    let at = ghosts_from(&model, pos);
                    let want_ghosts: Vec<Lv> = model[at..slot_of(&model, pos)].iter().filter_map(|s| match s { Slot::Ghost(i) => Some(*i), _ => None }).collect();
                    prop_assert_eq!(a.ghosts_at(pos), want_ghosts, "ghosts at {} in {:?}", pos, model);
                }
                // Saved and read back: the same places. (Anchors whose
                // ghosts stick left are rebuilt from the log, never saved.)
                if left {
                    continue;
                }
                let mut bytes = Vec::new();
                a.encode(&mut bytes);
                let mut back = Anchors::decode(&mut bytes.as_slice(), 1000).unwrap();
                back.place_ghosts(&a.ghosts()).unwrap();
                prop_assert_eq!(back.len(), a.len());
                for &(item, pos, ghost) in &want {
                    prop_assert_eq!(back.place_of(item), Some(pos));
                    prop_assert_eq!(back.is_ghost(item), ghost);
                }
            }
        }
    }
}
