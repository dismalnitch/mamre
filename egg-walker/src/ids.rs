//! Identities: which insert each item of a document came from.
//!
//! A document's items, in order, as runs of consecutive inserts (typing
//! makes long ones), in a mamre `SumTree` summed by how many items each
//! run holds, so the item at a position is a seek. It's kept by applying
//! the same changes the document gets.
//!
//! The walk only knows identities for events since the versions it merges
//! met; older items are anonymous, by position. When it needs one (an undo
//! bringing back an older item), it asks here: a replica keeps its
//! document's `Identities` beside the document.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use mamre::codec::{Codec, DecodeError, get_usize, put_usize};
use mamre::cursor::Bias;
use mamre::{Item, SumTree, Summary};

use crate::oplog::Lv;

/// Items `first..first + len`, side by side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    first: Lv,
    len: usize,
}

/// How many items.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Count(usize);

impl Summary for Count {
    fn empty() -> Self {
        Count(0)
    }
    fn concat(&mut self, other: &Self) {
        self.0 += other.0;
    }
}

impl Codec for Run {
    fn encode(&self, out: &mut Vec<u8>) {
        put_usize(out, self.first);
        put_usize(out, self.len);
    }
    fn decode(input: &mut &[u8]) -> Result<Self, DecodeError> {
        Ok(Run {
            first: get_usize(input)?,
            len: get_usize(input)?,
        })
    }
}

impl Item for Run {
    type Summary = Count;
    fn summary(&self) -> Count {
        Count(self.len)
    }
}

/// What a replica knows about who its document's items are: which insert
/// each item came from, and which item each recent delete removed. Kept
/// beside the document, at its version, by the same edits: local ones with
/// [`insert`](Self::insert) and [`delete`](Self::delete), merges with
/// [`OpLog::changes_with`](crate::OpLog::changes_with).
///
/// An undo that brings back an item a merge doesn't otherwise know (one
/// from before the versions met) finds it here, so it costs what the merge
/// does. Deletes older than the window are forgotten: undoing one of those
/// still works, by a checkout of the history, but slower.
#[derive(Clone)]
pub struct Identities {
    pub(crate) ids: Ids,
    /// Delete to the insert of the item it removed, for the latest deletes.
    pub(crate) removed: Arc<BTreeMap<Lv, Lv>>,
    window: usize,
}

impl Default for Identities {
    fn default() -> Self {
        Self::with_window(Self::WINDOW)
    }
}

impl Identities {
    /// How many deletes are remembered by default: days to weeks of heavy
    /// editing, at a few dozen bytes each.
    pub const WINDOW: usize = 50_000;

    /// An empty document's.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty document's, remembering the latest `window` deletes.
    pub fn with_window(window: usize) -> Self {
        Identities {
            ids: Ids::default(),
            removed: Arc::new(BTreeMap::new()),
            window,
        }
    }

    /// How many items the document has.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The insert the item at `pos` came from.
    pub fn item(&self, pos: usize) -> Lv {
        self.ids.get(pos)
    }

    /// The item delete `delete` removed, if it's remembered.
    pub fn removed(&self, delete: Lv) -> Option<Lv> {
        self.removed.get(&delete).copied()
    }

    /// A local insert: events `first..first + len`, typed forward from
    /// `pos`, as [`OpLog::add_insert`](crate::OpLog::add_insert) makes.
    pub fn insert(&mut self, pos: usize, first: Lv, len: usize) {
        self.ids.insert(pos, first, len);
    }

    /// A local delete: events `first..first + len`, each removing the item
    /// at `pos`, as [`OpLog::add_delete`](crate::OpLog::add_delete) makes.
    pub fn delete(&mut self, pos: usize, first: Lv, len: usize) {
        let gone: Vec<(Lv, Lv)> = (0..len)
            .map(|k| (first + k, self.ids.get(pos + k)))
            .collect();
        self.ids.remove(pos..pos + len);
        self.learn(gone);
    }

    /// Appends these identities' bytes: the window, the runs (a sum tree,
    /// saved as its items), and the remembered deletes, by how far each is
    /// past the one before.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_usize(out, self.window);
        self.ids.runs.encode(out);
        put_usize(out, self.removed.len());
        let mut prev = 0;
        for (&delete, &item) in self.removed.iter() {
            put_usize(out, delete - prev);
            put_usize(out, item);
            prev = delete;
        }
    }

    /// Reads identities [`encode`](Self::encode) wrote, for a log of
    /// `events` events.
    pub(crate) fn decode(input: &mut &[u8], events: usize) -> Result<Self, DecodeError> {
        let window = get_usize(input)?;
        let runs = SumTree::<Run>::decode(input)?;
        if runs
            .items()
            .iter()
            .any(|r| r.len == 0 || r.first.checked_add(r.len).is_none_or(|end| end > events))
        {
            return Err(DecodeError::Invalid("an identity this log doesn't have"));
        }
        let n = get_usize(input)?;
        let mut removed = BTreeMap::new();
        let mut prev: usize = 0;
        for _ in 0..n {
            let delete = prev
                .checked_add(get_usize(input)?)
                .ok_or(DecodeError::Overflow)?;
            let item = get_usize(input)?;
            if delete >= events || item >= events {
                return Err(DecodeError::Invalid("a delete this log doesn't have"));
            }
            removed.insert(delete, item);
            prev = delete;
        }
        Ok(Identities {
            ids: Ids { runs },
            removed: Arc::new(removed),
            window,
        })
    }

    /// Remembers what deletes removed, forgetting the oldest past the
    /// window.
    pub(crate) fn learn(&mut self, removed: impl IntoIterator<Item = (Lv, Lv)>) {
        let map = Arc::make_mut(&mut self.removed);
        map.extend(removed);
        while map.len() > self.window {
            map.pop_first();
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct Ids {
    runs: SumTree<Run>,
}

impl Ids {
    pub(crate) fn len(&self) -> usize {
        self.runs.summary().0
    }

    /// The items, in order, as runs of consecutive inserts.
    pub(crate) fn runs(&self) -> impl Iterator<Item = std::ops::Range<Lv>> + '_ {
        self.runs.iter().map(|r| r.first..r.first + r.len)
    }

    /// The insert the item at `pos` came from.
    pub(crate) fn get(&self, pos: usize) -> Lv {
        let mut cursor = self.runs.cursor::<Count>();
        cursor.seek(&Count(pos), Bias::Right);
        let run = cursor.item().expect("the position is in the document");
        run.first + (pos - cursor.start().0)
    }

    /// Items `first..first + len` go in at `pos`.
    pub(crate) fn insert(&mut self, pos: usize, first: Lv, len: usize) {
        if len == 0 {
            return;
        }
        if pos > 0 {
            self.cut(pos);
            // Carrying on the run before it: typing.
            let extended = self
                .runs
                .update_at::<Count, _>(&Count(pos), Bias::Left, |run| {
                    let carries_on = run.first + run.len == first;
                    if carries_on {
                        run.len += len;
                    }
                    carries_on
                })
                .expect("there's an item before");
            if !extended {
                self.runs
                    .splice_at::<Count, _, _>(&Count(pos), Bias::Left, |_| {
                        ((), [Run { first, len }])
                    });
            }
        } else if self.len() == 0 {
            self.runs.push(Run { first, len });
        } else {
            // In front: swap in, and the old first run goes after.
            self.runs
                .splice_at::<Count, _, _>(&Count(0), Bias::Right, |run| {
                    ((), [std::mem::replace(run, Run { first, len })])
                });
        }
    }

    /// The items in `range` go.
    pub(crate) fn remove(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.cut(range.start);
        self.cut(range.end);
        let mut left = range.len();
        while left > 0 {
            let run = self
                .runs
                .remove_at::<Count>(&Count(range.start), Bias::Right)
                .expect("the range is in the document");
            left -= run.len;
        }
    }

    /// The items in `range` move to `to` (a position before the move,
    /// outside the range or at either end), as `Rope::move_range` moves
    /// text.
    pub(crate) fn move_range(&mut self, range: Range<usize>, to: usize) {
        debug_assert!(to <= range.start || to >= range.end);
        if range.is_empty() || to == range.start || to == range.end {
            return;
        }
        // Cut out, and spliced back in: whole subtrees, so it costs the
        // tree's height, not the runs moved.
        let (lo, mid, hi) = if to < range.start {
            (to, range.start, range.end)
        } else {
            (range.start, range.end, to)
        };
        for p in [lo, mid, hi] {
            self.cut(p);
        }
        let mut cursor = self.runs.cursor::<Count>();
        let a = cursor.slice(&Count(lo), Bias::Right);
        let b = cursor.slice(&Count(mid), Bias::Right);
        let c = cursor.slice(&Count(hi), Bias::Right);
        let d = cursor.suffix();
        // [a][b][c][d] becomes [a][c][b][d]: the two middle pieces swap.
        let mut out = a;
        for piece in [c, b, d] {
            join(&mut out, piece);
        }
        self.runs = out;
    }

    /// Makes `pos` a boundary between runs.
    fn cut(&mut self, pos: usize) {
        if pos == 0 || pos >= self.len() {
            return;
        }
        // The run holding the item before `pos`: split it there, unless it
        // ends there already.
        let mut cursor = self.runs.cursor::<Count>();
        cursor.seek(&Count(pos), Bias::Left);
        let start = cursor.start().0;
        let run = *cursor.item().expect("pos is inside the document");
        if start + run.len == pos {
            return;
        }
        let at = pos - start;
        self.runs
            .splice_at::<Count, _, _>(&Count(pos), Bias::Left, |r| {
                r.len = at;
                (
                    (),
                    [Run {
                        first: run.first + at,
                        len: run.len - at,
                    }],
                )
            });
    }
}

/// `b` after `a`, joining the run where they meet if it carries on.
fn join(a: &mut SumTree<Run>, mut b: SumTree<Run>) {
    if let (Some(last), Some(first)) = (a.last().copied(), b.first().copied())
        && last.first + last.len == first.first
    {
        a.update_last(|r| r.len += first.len);
        b.remove_at::<Count>(&Count(0), Bias::Right);
    }
    a.append(b);
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Inserting and removing runs keeps the same identities, in the
        /// same order, as a plain list.
        #[test]
        fn matches_a_vec(ops in prop::collection::vec((any::<prop::sample::Index>(), 0usize..6, any::<bool>()), 1..200)) {
            let mut ids = Ids::default();
            let mut want: Vec<Lv> = Vec::new();
            let mut next = 0;
            for (at, n, insert) in ops {
                if insert || want.is_empty() {
                    let pos = if want.is_empty() { 0 } else { at.index(want.len() + 1) };
                    // Sometimes carry on the last run, to exercise extending.
                    let first = if n % 2 == 0 { next } else { next + 1000 };
                    ids.insert(pos, first, n);
                    want.splice(pos..pos, first..first + n);
                    next = first + n;
                } else {
                    let a = at.index(want.len());
                    let b = (a + n).min(want.len());
                    ids.remove(a..b);
                    want.drain(a..b);
                }
                prop_assert_eq!(ids.len(), want.len());
                ids.runs.check().map_err(TestCaseError::fail)?;
                for (k, &lv) in want.iter().enumerate() {
                    prop_assert_eq!(ids.get(k), lv);
                }
            }
        }

        /// Moving runs (spliced as subtrees) keeps the same identities as
        /// moving them in a plain list, and joins runs that meet.
        #[test]
        fn moves_match_a_vec(
            runs in prop::collection::vec((1usize..5, any::<bool>()), 1..60),
            moves in prop::collection::vec((any::<prop::sample::Index>(), any::<prop::sample::Index>(), any::<prop::sample::Index>()), 1..40),
        ) {
            let mut ids = Ids::default();
            let mut want: Vec<Lv> = Vec::new();
            let mut next = 0;
            for (n, gap) in runs {
                let first = if gap { next + 1 } else { next };
                ids.insert(want.len(), first, n);
                want.extend(first..first + n);
                next = first + n;
            }
            for (a, b, t) in moves {
                let (a, b) = (a.index(want.len()), b.index(want.len()));
                let range = a.min(b)..a.max(b) + 1;
                let to = t.index(want.len() + 1);
                if to > range.start && to < range.end {
                    continue;
                }
                ids.move_range(range.clone(), to);
                let moved: Vec<Lv> = want.drain(range.clone()).collect();
                let at = if to > range.start { to - moved.len() } else { to };
                want.splice(at..at, moved);
                ids.runs.check().map_err(TestCaseError::fail)?;
                prop_assert_eq!(ids.runs().flatten().collect::<Vec<_>>(), want.clone());
                let runs: Vec<_> = ids.runs().collect();
                prop_assert!(runs.windows(2).all(|w| w[0].end != w[1].start), "runs that meet are joined: {:?}", runs);
            }
        }
    }
}
