//! Comments (#33): a range of the text (named by its items, as #32's
//! ranges are) with a body, and anchors that fall back.
//!
//! When every item a comment covers is deleted, it doesn't vanish: its
//! start falls back to where its first item was, keeping its identity, and
//! so does its end. Text typed at that place goes between them, so a
//! comment on "big", the word deleted and "large" typed in its place,
//! covers "large". The starts are watched by anchors whose ghosts text goes
//! after, the ends by anchors whose ghosts it goes before (see
//! `anchors.rs`). An undo of the delete brings the items back as
//! themselves, so the comment's ends are exactly where they were.
//!
//! Adding a comment and setting its state are events beside the text
//! ([`Aside::CommentAdd`], [`Aside::CommentSet`]); its body is a small text
//! of its own, edited by [`Aside::BodyInsert`] and [`Aside::BodyDelete`]
//! (see [`OpLog::comment_body`]). All of it is in the one log, so it's
//! undone in order with the text (#14).
//!
//! A paste carries history, not attachments (#33's decision): a comment
//! names its items by who they are, and stays on the original.
//!
//! The books rebuild comments from the log after a load, as ranges.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use crate::anchors::Anchors;
use crate::follow::Moved;
use crate::ids::Identities;
use crate::oplog::{Aside, CommentState, Lv, Op, OpLog};
use crate::walk::Change;

/// A comment, as the document shows it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    /// The event that added it: its identity.
    pub id: Lv,
    pub state: CommentState,
    /// The positions it covers.
    pub span: Range<usize>,
}

#[derive(Clone, Debug)]
struct Entry {
    start: Lv,
    end: Lv,
    state: CommentState,
    set_by: Lv,
}

/// Anchors that count how many comments watch each item.
#[derive(Clone)]
struct Watched {
    anchors: Anchors,
    count: HashMap<Lv, usize>,
}

impl Watched {
    fn new(anchors: Anchors) -> Self {
        Watched {
            anchors,
            count: HashMap::new(),
        }
    }

    fn watch(&mut self, item: Lv, ids: &Identities) {
        let n = self.count.entry(item).or_insert(0);
        *n += 1;
        if *n > 1 {
            return;
        }
        match ids.positions(&BTreeSet::from([item])).first() {
            Some(&(_, pos)) => self.anchors.watch(pos, item),
            None => self.anchors.watch_deleted(item),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Comments {
    entries: BTreeMap<Lv, Entry>,
    starts: Watched,
    ends: Watched,
    pub(crate) stale: bool,
}

impl Comments {
    pub(crate) fn new(len: usize) -> Self {
        Comments {
            entries: BTreeMap::new(),
            starts: Watched::new(Anchors::falling_back(len)),
            ends: Watched::new(Anchors::falling_back_left(len)),
            stale: false,
        }
    }

    /// Books read from a file: rebuilt from the log before the next merge.
    pub(crate) fn stale() -> Self {
        Comments {
            stale: true,
            ..Comments::new(0)
        }
    }

    /// A change to the text.
    pub(crate) fn apply<T>(&mut self, change: &Change<'_, T>) {
        self.starts.anchors.apply(change);
        self.ends.anchors.apply(change);
    }

    /// Takes in events `events` (in order) at a document whose identities
    /// are `ids`.
    pub(crate) fn take_in<T: Clone>(&mut self, log: &OpLog<T>, events: &[Lv], ids: &Identities) {
        for &lv in events {
            match log.event(lv).op {
                Op::Aside(Aside::CommentAdd { start, end }) => {
                    self.entries.insert(
                        lv,
                        Entry {
                            start,
                            end,
                            state: CommentState::Open,
                            set_by: lv,
                        },
                    );
                    self.starts.watch(start, ids);
                    self.ends.watch(end, ids);
                }
                Op::Aside(Aside::CommentSet { comment, state }) => {
                    let key = |e: Lv| (log.lamport(e), log.order_key(e));
                    if let Some(entry) = self.entries.get_mut(&comment)
                        && key(lv) > key(entry.set_by)
                    {
                        entry.state = state;
                        entry.set_by = lv;
                    }
                }
                _ => {}
            }
        }
    }

    fn view(&self, id: Lv, e: &Entry, moved: Moved) -> Option<Comment> {
        if e.state == CommentState::Removed {
            return None;
        }
        // Both ends moved, and still in order: it goes with them (#85).
        if let (Some(start), Some(end)) = (moved(e.start), moved(e.end))
            && start <= end
        {
            return Some(Comment {
                id,
                state: e.state,
                span: start..end + 1,
            });
        }
        let start = self.starts.anchors.place_of(e.start)?;
        let end = match self.ends.anchors.position(e.end) {
            Some(p) => p + 1,
            None => self.ends.anchors.place_of(e.end)?,
        };
        Some(Comment {
            id,
            state: e.state,
            span: start..end.max(start),
        })
    }

    pub(crate) fn all(&self, moved: Moved) -> Vec<Comment> {
        let mut out: Vec<Comment> = self
            .entries
            .iter()
            .filter_map(|(&id, e)| self.view(id, e, moved))
            .collect();
        out.sort_by_key(|c| (c.span.start, c.span.end, c.id));
        out
    }

    pub(crate) fn get(&self, id: Lv, moved: Moved) -> Option<Comment> {
        self.view(id, self.entries.get(&id)?, moved)
    }

    pub(crate) fn state(&self, id: Lv) -> Option<CommentState> {
        self.entries.get(&id).map(|e| e.state)
    }

    pub(crate) fn set_by(&self, id: Lv) -> Option<Lv> {
        self.entries.get(&id).map(|e| e.set_by)
    }
}
