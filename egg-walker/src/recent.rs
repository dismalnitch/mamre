//! The books a replica had at its latest versions: who each item was, and
//! the moved blocks, at each.
//!
//! A merge starts from the base: the latest version that its document
//! holds and every event it replays includes. That's before this replica's
//! unsent edits, so the books the replica keeps (at its own version) aren't
//! the base's. With moves, a merge needs the base's blocks and identities,
//! and finding them from nothing means walking the whole history.
//!
//! The replica was at, or just past, the base a moment ago, though. So the
//! accountant keeps its books at each of its latest versions. A merge takes
//! the latest of those the base holds and walks forward to the base: a
//! walk of the events since, which a live merge replays anyway. The books
//! share nearly everything (identities are a persistent tree), so each
//! costs little to keep.

use std::collections::VecDeque;

use crate::blocks::Blocks;
use crate::ids::Ids;
use crate::oplog::Lv;

/// The books at one version.
#[derive(Clone)]
pub(crate) struct Kept {
    /// Sorted.
    pub(crate) version: Vec<Lv>,
    pub(crate) ids: Ids,
    pub(crate) blocks: Blocks,
}

/// The books at the latest versions, newest last.
#[derive(Clone, Default)]
pub(crate) struct Recent(VecDeque<Kept>);

impl Recent {
    /// How many versions are kept: a live replica moves on a version an
    /// edit, so this is some seconds of typing, and a base older than all of
    /// them is found from nothing.
    pub(crate) const KEEP: usize = 256;

    /// The books at `version`.
    pub(crate) fn keep(&mut self, version: &[Lv], ids: &Ids, blocks: &Blocks) {
        let mut version = version.to_vec();
        version.sort_unstable();
        if self.0.back().is_some_and(|k| k.version == version) {
            return;
        }
        if self.0.len() == Self::KEEP {
            self.0.pop_front();
        }
        self.0.push_back(Kept {
            version,
            ids: ids.clone(),
            blocks: blocks.clone(),
        });
    }

    /// The books at `version` exactly, if kept.
    pub(crate) fn at(&self, version: &[Lv]) -> Option<&Kept> {
        let mut version = version.to_vec();
        version.sort_unstable();
        self.0.iter().rev().find(|k| k.version == version)
    }

    /// The latest books kept at a version `holds` holds whole (each of its
    /// events in it): not the empty version, which needs no books.
    pub(crate) fn latest_within(&self, holds: impl Fn(Lv) -> bool) -> Option<&Kept> {
        self.0
            .iter()
            .rev()
            .find(|k| !k.version.is_empty() && k.version.iter().all(|&lv| holds(lv)))
    }
}
