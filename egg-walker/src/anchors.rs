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
}

/// How many items a stretch of marks covers (theirs and the gaps before
/// them), and the key of the last.
#[derive(Clone, Debug, Default)]
struct Span {
    items: usize,
    last: Option<Key>,
}

impl Summary for Span {
    fn empty() -> Self {
        Span::default()
    }
    fn concat(&mut self, other: &Self) {
        self.items += other.items;
        if other.last.is_some() {
            self.last.clone_from(&other.last);
        }
    }
}

impl Item for Mark {
    type Summary = Span;
    fn summary(&self) -> Span {
        Span {
            items: self.before + 1,
            last: Some(self.key.clone()),
        }
    }
}

/// Seeks the mark whose gap holds position `0` or whose item is there:
/// with `Bias::Right`, the first whose gap reaches it.
struct Pos(usize);

impl SeekTarget<Span, Span> for Pos {
    fn cmp_position(&self, position: &Span) -> Ordering {
        self.0.cmp(&position.items)
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
            self.unmark(&key, true);
        }
    }

    /// Where `item` is: `None` while it's deleted, or if it isn't watched.
    pub fn position(&self, item: Lv) -> Option<usize> {
        let key = self.keys.get(&item)?;
        let mut cursor = self.marks.cursor::<Span>();
        cursor.seek(&At(key), Bias::Left);
        let mark = cursor.item().expect("a watched item has a mark");
        Some(cursor.start().items + mark.before)
    }

    /// Whether `item` is watched and deleted now, waiting to come back.
    pub fn is_parked(&self, item: Lv) -> bool {
        self.parked.contains(&item)
    }

    /// Appends these anchors' bytes: the tail, each mark's item and gap in
    /// order, and the parked items. Order keys are the tree's own, made
    /// afresh on load.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_usize(out, self.tail);
        let marks = self.marks.items();
        put_usize(out, marks.len());
        for m in &marks {
            put_usize(out, m.item);
            put_usize(out, m.before);
        }
        put_usize(out, self.parked.len());
        let mut prev = 0;
        for &item in &self.parked {
            put_usize(out, item - prev);
            prev = item;
        }
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
                    self.parked.remove(&item);
                    self.place(pos + (item - first), item);
                }
            }
            Change::Delete { range } => self.delete(range.clone()),
            Change::Move { range, to } => self.move_range(range.clone(), *to),
            Change::Mark { .. } => {}
        }
    }

    /// The items in `range` move to `to` (a position before the move,
    /// outside the range): watched ones among them go with them.
    pub fn move_range(&mut self, range: Range<usize>, to: usize) {
        let n = range.len();
        if n == 0 || to == range.start || to == range.end {
            return;
        }
        // The watched items inside, and how far in.
        let mut inside = Vec::new();
        {
            let mut cursor = self.marks.cursor::<Span>();
            cursor.seek(&Pos(range.start), Bias::Right);
            while let Some(m) = cursor.item() {
                let at = cursor.start().items + m.before;
                if at >= range.end {
                    break;
                }
                if at >= range.start {
                    inside.push((m.item, at - range.start));
                }
                cursor.next();
            }
        }
        self.delete(range.clone());
        let at = if to > range.start { to - n } else { to };
        self.insert(at, n);
        for (item, offset) in inside {
            self.parked.remove(&item);
            self.place(at + offset, item);
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
        let at = range.start;
        let mut left = range.len();
        while left > 0 {
            let found = {
                let mut cursor = self.marks.cursor::<Span>();
                cursor.seek(&Pos(at), Bias::Right);
                cursor
                    .item()
                    .map(|m| (m.key.clone(), m.item, at - cursor.start().items))
            };
            let Some((key, item, offset)) = found else {
                self.tail -= left;
                return;
            };
            // From the gap first, then the mark's own item.
            let mut taken = 0;
            self.marks.update_at::<Span, _>(&At(&key), Bias::Left, |m| {
                taken = (m.before - offset).min(left);
                m.before -= taken;
            });
            left -= taken;
            if left > 0 {
                left -= 1;
                self.keys.remove(&item);
                self.parked.insert(item);
                self.unmark(&key, false);
            }
        }
    }

    /// Makes a mark for `item` at `pos`, out of the gap that counts it.
    fn place(&mut self, pos: usize, item: Lv) {
        let found = {
            let mut cursor = self.marks.cursor::<Span>();
            cursor.seek(&Pos(pos), Bias::Right);
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
                };
                self.marks
                    .splice_at::<Span, _, _>(&At(&next), Bias::Left, |m| {
                        let mut old = std::mem::replace(m, mark);
                        old.before -= before + 1;
                        ((), [old])
                    });
                self.keys.insert(item, key);
            }
            None => {
                let start = self.marks.summary().items;
                let key = Key::between(self.marks.summary().last.as_ref(), None);
                let before = pos - start;
                self.tail -= before + 1;
                self.marks.push(Mark {
                    key: key.clone(),
                    item,
                    before,
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
