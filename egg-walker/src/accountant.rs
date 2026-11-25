//! The Accountant: what a replica keeps beside its document, on the CRDT's
//! side, so merges stay cheap and nothing loses track of itself.
//!
//! The document is the replica's own: a `Vec`, a rope, anything that can
//! insert, delete and move items. It never holds CRDT state. The
//! accountant keeps the books instead: the version the document is at, who
//! each item is ([`Identities`]), the watched items ([`Anchors`]), the
//! outline (which items start sections: see `outline.rs`), and the moved
//! blocks (see `blocks.rs`). [`merge`](Accountant::merge) brings
//! them all up to a new version and hands back the [`Change`]s that bring
//! the document there too. That's the whole contract: changes out, books
//! kept.
//!
//! A local edit is an event like any other, made at the accountant's
//! version: the document makes the edit itself, adds the event to the log,
//! and [`record`](Accountant::record)s it, which keeps the books without
//! handing the edit back. Everything from elsewhere is
//! [`merge`](Accountant::merge)d, and its changes applied.

use mamre::codec::{DecodeError, get_usize, put_usize};

use crate::anchors::Anchors;
use crate::blocks::Blocks;
use crate::ids::Identities;
use crate::oplog::{Lv, OpLog};
use crate::outline::{Ambiguity, Next, Outline, Policies, Policy, Section};
use crate::recent::Recent;
use crate::walk::Change;

#[derive(Clone)]
pub struct Accountant {
    version: Vec<Lv>,
    ids: Identities,
    anchors: Anchors,
    outline: Outline,
    blocks: Blocks,
    /// The books at the latest versions, for a merge to start from.
    recent: Recent,
    /// Each local writer's undo and redo stacks (see `undo.rs`): this
    /// replica's, not saved.
    pub(crate) history: std::collections::HashMap<crate::oplog::AgentId, crate::undo::History>,
    /// The ranges the author defined (#32).
    pub(crate) ranges: crate::ranges::Ranges,
}

impl Default for Accountant {
    fn default() -> Self {
        Self::with_window(Identities::WINDOW)
    }
}

impl Accountant {
    /// The books of an empty document.
    pub fn new() -> Self {
        Self::default()
    }

    /// The books of an empty document, remembering the latest `window`
    /// deletes (see [`Identities::with_window`]).
    pub fn with_window(window: usize) -> Self {
        Accountant {
            version: Vec::new(),
            ids: Identities::with_window(window),
            anchors: Anchors::new(0),
            outline: Outline::default(),
            blocks: Blocks::default(),
            recent: Recent::default(),
            history: Default::default(),
            ranges: Default::default(),
        }
    }

    /// The version the document is at.
    pub fn version(&self) -> &[Lv] {
        &self.version
    }

    /// Who each of the document's items is.
    pub fn identities(&self) -> &Identities {
        &self.ids
    }

    /// The watched items, and where they are.
    pub fn anchors(&self) -> &Anchors {
        &self.anchors
    }

    /// How many items the document has.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Watches the item at `pos`, so it can be found again after any edit,
    /// merge or undo. Returns who it is.
    pub fn watch(&mut self, pos: usize) -> Lv {
        let item = self.ids.item(pos);
        self.anchors.watch(pos, item);
        item
    }

    /// Stops watching `item`.
    pub fn unwatch(&mut self, item: Lv) {
        self.anchors.unwatch(item);
    }

    /// The sections the document shows, in order: each marked item, where
    /// it is (or, its heading deleted, was), its level, and how it starts.
    pub fn sections(&self) -> Vec<Section> {
        self.outline.sections()
    }

    /// The section `item` starts: from it to the next section at its level
    /// or above, or the end. `None` if it starts none now. Moving a section
    /// is moving this range: [`OpLog::add_move`], then [`merge`](Self::merge).
    pub fn section_range(&self, item: Lv) -> Option<std::ops::Range<usize>> {
        self.outline.range(item)
    }

    /// The moved blocks the document shows, sorted: each active move with
    /// something visible in it, and the positions it covers. For tests.
    #[doc(hidden)]
    pub fn moved_blocks(&self) -> Vec<(Lv, std::ops::Range<usize>, bool, bool)> {
        self.blocks
            .sorted()
            .into_iter()
            .map(|b| (b.lv, b.span, b.hidden.head, b.hidden.tail))
            .collect()
    }

    /// What's been reported since the last time this was called: each
    /// heading deleted, and each such ambiguity resolved (see
    /// [`Ambiguity`]). [`merge`](Self::merge) and [`record`](Self::record)
    /// both report, in the order of the changes that caused them.
    pub fn take_ambiguities(&mut self) -> Vec<Ambiguity> {
        self.outline.take_reports()
    }

    /// The sections whose heading is deleted, and which this replica keeps
    /// a place for (whatever the policy shows): the open ambiguities.
    pub fn headless(&self) -> Vec<Lv> {
        self.outline.headless()
    }

    /// What the outline makes of a section whose heading is deleted, for
    /// each case. The replica's own setting: it isn't saved with the books,
    /// and replicas needn't agree.
    pub fn policies(&self) -> Policies {
        self.outline.policies
    }

    /// Sets what the outline makes of a deleted heading followed by `next`.
    pub fn set_policy(&mut self, next: Next, policy: Policy) {
        self.outline.policies.set(next, policy);
    }

    /// The ranges the author defined that the document shows, by where they
    /// start (see `ranges.rs`).
    pub fn ranges(&self) -> Vec<crate::AuthorRange> {
        self.ranges.all()
    }

    /// Range `id`, if the document shows it.
    pub fn range(&self, id: Lv) -> Option<crate::AuthorRange> {
        self.ranges.get(id)
    }

    /// The ranges covering position `pos`.
    pub fn ranges_at(&self, pos: usize) -> Vec<crate::AuthorRange> {
        self.ranges
            .all()
            .into_iter()
            .filter(|r| r.span.contains(&pos))
            .collect()
    }

    /// The ranges overlapping positions `span`.
    pub fn ranges_overlapping(&self, span: std::ops::Range<usize>) -> Vec<crate::AuthorRange> {
        self.ranges
            .all()
            .into_iter()
            .filter(|r| r.span.start < span.end && span.start < r.span.end)
            .collect()
    }

    /// The level of the section `item` starts: 0 if none.
    pub fn level(&self, item: Lv) -> u8 {
        self.outline.level(item)
    }

    /// Brings the books up to everything in the current version and `to`,
    /// and returns the changes that bring the document there.
    pub fn merge<'a, T: Clone>(&mut self, log: &'a OpLog<T>, to: &[Lv]) -> Vec<Change<'a, T>> {
        let to = log.union(&self.version, to);
        // Ranges read from a file are rebuilt from the log first.
        if self.ranges.stale {
            self.ranges = Default::default();
            self.ranges.resize(self.ids.len());
            let all: Vec<Lv> = (0..log.len())
                .filter(|&lv| {
                    log.kind(lv) == crate::oplog::Kind::Aside && log.contains(&self.version, lv)
                })
                .collect();
            self.ranges.take_in(log, &all, &self.ids);
        }
        let (_, mut new) = log.diff(&self.version, &to);
        new.retain(|&lv| log.kind(lv) == crate::oplog::Kind::Aside);
        new.sort_unstable();
        let out = log.changes_kept(
            &self.version,
            &to,
            &mut self.ids,
            &mut self.blocks,
            &self.recent,
        );
        for change in &out {
            self.anchors.apply(change);
            self.outline.apply(change);
            self.ranges.apply(change);
        }
        self.ranges.take_in(log, &new, &self.ids);
        self.version = to;
        self.recent.keep(&self.version, &self.ids.ids, &self.blocks);
        out
    }

    /// Records events the document has already made: local edits, added to
    /// the log at [`version`](Self::version). The books come up to date,
    /// and nothing is handed back, so the edit isn't made twice. For
    /// anything from elsewhere, use [`merge`](Self::merge).
    ///
    /// ```
    /// use mamre_egg_walker::{Accountant, OpLog};
    ///
    /// let mut log = OpLog::new();
    /// let me = log.agent("me");
    /// let mut books = Accountant::new();
    /// let mut doc = String::new();
    ///
    /// // The user types: the document has it already.
    /// doc.insert_str(0, "hello");
    /// let v = log.add_insert(me, books.version(), 0, "hello".chars());
    /// books.record(&log, &v);
    /// assert_eq!(books.len(), doc.chars().count());
    /// ```
    pub fn record<T: Clone>(&mut self, log: &OpLog<T>, to: &[Lv]) {
        self.merge(log, to);
    }

    /// Appends the books' bytes: the version, the identities, the anchors,
    /// the outline, the blocks.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_usize(out, self.version.len());
        for &lv in &self.version {
            put_usize(out, lv);
        }
        self.ids.encode(out);
        self.anchors.encode(out);
        self.outline.encode(out);
        self.blocks.encode(out);
    }

    /// Reads books [`encode`](Self::encode) wrote, for a log of `events`
    /// events.
    pub(crate) fn decode(input: &mut &[u8], events: usize) -> Result<Self, DecodeError> {
        let n = get_usize(input)?;
        let mut version = Vec::with_capacity(n.min(input.len()));
        for _ in 0..n {
            let lv = get_usize(input)?;
            if lv >= events {
                return Err(DecodeError::Invalid("a version this log doesn't have"));
            }
            version.push(lv);
        }
        let ids = Identities::decode(input, events)?;
        let anchors = Anchors::decode(input, events)?;
        let outline = Outline::decode(input, events)?;
        let blocks = Blocks::decode(input, events, ids.len())?;
        let mut recent = Recent::default();
        recent.keep(&version, &ids.ids, &blocks);
        Ok(Accountant {
            version,
            ids,
            anchors,
            outline,
            blocks,
            recent,
            history: Default::default(),
            ranges: crate::ranges::Ranges::stale(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The books, moved blocks and all, read back as saved.
    #[test]
    fn the_books_round_trip() {
        let mut log = OpLog::<char>::new();
        let a = log.agent("a");
        let v = log.add_insert(a, &[], 0, "abcdef".chars());
        let v = log.add_move(a, &v, 1..3, 5);
        let mut books = Accountant::new();
        books.merge(&log, &v);
        books.watch(2);
        let mut bytes = Vec::new();
        books.encode(&mut bytes);
        let back = Accountant::decode(&mut bytes.as_slice(), log.len()).unwrap();
        assert_eq!(back.version, books.version);
        assert_eq!(back.len(), books.len());
        assert_eq!(back.anchors.position(back.ids.item(2)), Some(2));
    }
}

#[cfg(test)]
mod outline_tests {
    use super::*;

    /// The outline is in the books, and reads back as saved.
    #[test]
    fn the_outline_round_trips() {
        let mut log = OpLog::<char>::new();
        let a = log.agent("a");
        let v = log.add_insert(a, &[], 0, "abcdef".chars());
        let v = log.add_mark(a, &v, 1, 2);
        let v = log.add_mark(a, &v, 4, 1);
        let v = log.add_delete(a, &v, 4..5);
        let mut books = Accountant::new();
        books.merge(&log, &v);
        let mut bytes = Vec::new();
        books.encode(&mut bytes);
        let back = Accountant::decode(&mut bytes.as_slice(), log.len()).unwrap();
        assert_eq!(back.sections(), books.sections());
        assert_eq!(back.level(4), 1, "a deleted item keeps its mark");
        // And its section, headless, at its place (#19).
        assert_eq!(back.sections().len(), 2);
        assert_eq!(back.sections()[1].start, crate::Start::Headless);
        assert_eq!(back.sections()[1].pos, 4);
        assert_eq!(back.headless(), vec![4]);
    }
}
