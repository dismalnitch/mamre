//! Ranges the author defines (#32): POV, flashback, "needs work", scene
//! type. They can cross sections, so they sit beside the outline, not in
//! it.
//!
//! A range is an event ([`Op::RangeAdd`]): from one item to another, both
//! included, named by who they are (their inserts), so it follows them
//! through edits, moves and merges. Setting its tag or removing it is an
//! event too ([`Op::RangeSet`]), so all of it is undoable with everything
//! else (#14). Of a range's sets, the latest wins: Lamport time, then the
//! order concurrent inserts use.
//!
//! The [`Accountant`](crate::Accountant) keeps them, as it keeps the
//! outline: their ends are watched by anchors of their own that fall back
//! (see `anchors.rs`), so when an end's item is deleted the range ends at
//! the nearest surviving item inside it, until #33's rule. An end deleted
//! before this replica showed it has no place, and the range isn't shown
//! until it comes back.
//!
//! Queries look at every range: O(ranges · log n). An index by position
//! (an interval tree over the anchors' order) would make them O(log n) plus
//! the output; that's for when documents have many.
//!
//! In a chunk: not yet. The encoding is proposed on #32, and until it's
//! decided, a chunk with a range event can't be written.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use crate::anchors::Anchors;
use crate::ids::Identities;
use crate::oplog::{Lv, Op, OpLog};
use crate::walk::Change;

/// A range the author defined, as the document shows it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorRange {
    /// The event that added it: its identity.
    pub id: Lv,
    pub tag: String,
    /// The positions it covers.
    pub span: Range<usize>,
}

#[derive(Clone, Debug)]
struct Entry {
    start: Lv,
    end: Lv,
    /// `None`: removed.
    tag: Option<String>,
    /// The event that set `tag`: the add, or the latest set.
    set_by: Lv,
}

#[derive(Clone)]
pub(crate) struct Ranges {
    entries: BTreeMap<Lv, Entry>,
    /// Where the ends are: each item watched once, however many ranges
    /// end there.
    anchors: Anchors,
    watchers: HashMap<Lv, usize>,
    /// Read from a file: rebuilt from the log before the next merge.
    pub(crate) stale: bool,
}

impl Default for Ranges {
    fn default() -> Self {
        Ranges {
            entries: BTreeMap::new(),
            anchors: Anchors::falling_back(0),
            watchers: HashMap::new(),
            stale: false,
        }
    }
}

impl Ranges {
    /// Books read from a file: rebuilt from the log before the next merge.
    pub(crate) fn stale() -> Self {
        Ranges {
            stale: true,
            ..Ranges::default()
        }
    }

    /// A change to the text, from a merge or a local edit.
    pub(crate) fn apply<T>(&mut self, change: &Change<'_, T>) {
        self.anchors.apply(change);
    }

    /// Takes in range events `events` (in order), at a document whose
    /// identities are `ids`.
    pub(crate) fn take_in<T: Clone>(&mut self, log: &OpLog<T>, events: &[Lv], ids: &Identities) {
        for &lv in events {
            match log.event(lv).op {
                Op::RangeAdd { start, end, tag } => {
                    self.entries.insert(
                        lv,
                        Entry {
                            start,
                            end,
                            tag: Some(tag),
                            set_by: lv,
                        },
                    );
                    self.watch(start, ids);
                    self.watch(end, ids);
                }
                Op::RangeSet { range, tag } => {
                    let key = |e: Lv| (log.lamport(e), log.order_key(e));
                    if let Some(entry) = self.entries.get_mut(&range)
                        && key(lv) > key(entry.set_by)
                    {
                        entry.tag = tag;
                        entry.set_by = lv;
                    }
                }
                _ => {}
            }
        }
    }

    /// The document's length, kept by the text's changes: for a fresh set
    /// of books.
    pub(crate) fn resize(&mut self, len: usize) {
        if self.anchors.len() != len {
            self.anchors = Anchors::falling_back(len);
            self.watchers.clear();
        }
    }

    fn watch(&mut self, item: Lv, ids: &Identities) {
        let n = self.watchers.entry(item).or_insert(0);
        *n += 1;
        if *n > 1 {
            return;
        }
        match ids.positions(&BTreeSet::from([item])).first() {
            Some(&(_, pos)) => self.anchors.watch(pos, item),
            None => self.anchors.watch_deleted(item),
        }
    }

    /// Where range `id` is now, if it's there and its ends have places.
    fn view(&self, id: Lv, e: &Entry) -> Option<AuthorRange> {
        let tag = e.tag.clone()?;
        let start = self.anchors.place_of(e.start)?;
        let end = match self.anchors.position(e.end) {
            Some(p) => p + 1,
            // Deleted: up to the nearest surviving item inside.
            None => self.anchors.place_of(e.end)?,
        };
        Some(AuthorRange {
            id,
            tag,
            span: start..end.max(start),
        })
    }

    /// Every range shown, by where it starts.
    pub(crate) fn all(&self) -> Vec<AuthorRange> {
        let mut out: Vec<AuthorRange> = self
            .entries
            .iter()
            .filter_map(|(&id, e)| self.view(id, e))
            .collect();
        out.sort_by_key(|r| (r.span.start, r.span.end, r.id));
        out
    }

    pub(crate) fn get(&self, id: Lv) -> Option<AuthorRange> {
        self.view(id, self.entries.get(&id)?)
    }

    /// Range `id`'s tag as set now (`Some(None)`: removed), if it's known.
    pub(crate) fn tag(&self, id: Lv) -> Option<Option<String>> {
        self.entries.get(&id).map(|e| e.tag.clone())
    }

    /// The event that set range `id`'s tag now.
    pub(crate) fn set_by(&self, id: Lv) -> Option<Lv> {
        self.entries.get(&id).map(|e| e.set_by)
    }
}

/// The ranges at `version`, by definition, for testing the books: each
/// range added there, its ends, and its tag by the latest set there (`None`:
/// removed).
#[doc(hidden)]
pub fn reference<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> Vec<(Lv, Lv, Lv, Option<String>)> {
    let mut out: BTreeMap<Lv, (Lv, Lv, Option<String>, Lv)> = BTreeMap::new();
    let key = |e: Lv| (log.lamport(e), log.order_key(e));
    for lv in (0..log.len()).filter(|&lv| log.contains(version, lv)) {
        match log.event(lv).op {
            Op::RangeAdd { start, end, tag } => {
                out.insert(lv, (start, end, Some(tag), lv));
            }
            Op::RangeSet { range, tag } => {
                let e = out.get_mut(&range).expect("a set after its add");
                if key(lv) > key(e.3) {
                    e.2 = tag;
                    e.3 = lv;
                }
            }
            _ => {}
        }
    }
    out.into_iter()
        .map(|(id, (s, e, tag, _))| (id, s, e, tag))
        .collect()
}
