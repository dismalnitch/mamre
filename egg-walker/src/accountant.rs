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
use crate::ext::{Ghost, SnapshotExt};
use crate::ids::Identities;
use crate::oplog::{Aside, Copied, Lv, Op, OpLog};
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
    /// Comments (#33).
    pub(crate) comments: crate::comments::Comments,
    /// Moves' copies (#68), and why each anchor is watched.
    moves: crate::follow::Moves,
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
            comments: crate::comments::Comments::new(0),
            moves: Default::default(),
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
        self.moves.watch(&mut self.anchors, &self.ids, pos, item);
        item
    }

    /// Stops watching `item`.
    pub fn unwatch(&mut self, item: Lv) {
        self.moves.unwatch(&mut self.anchors, item);
    }

    /// Where watched `item` is now, following moves (#68): where it shows,
    /// or, if a move took it, where its copy shows. See `follow.rs`.
    pub fn follow(&self, item: Lv) -> crate::Follow {
        self.moves.follow(&self.anchors, item)
    }

    /// The item a move copied `item` from, if a move made it (#68).
    pub fn moved_from(&self, item: Lv) -> Option<Lv> {
        self.moves.source(item)
    }

    /// The copies moves made of `item`.
    pub fn copies(&self, item: Lv) -> Vec<Lv> {
        self.moves.copies_of(item)
    }

    /// The moves `events` (not merged yet) make, undo or redo, in runs:
    /// from their came-from records, and from the deletes they undo. For
    /// saying a move happened (`mamre-text`'s `Edit::Moved`): the merge
    /// never needs it.
    pub fn moves_among<T: Clone>(&self, log: &OpLog<T>, events: std::ops::Range<Lv>) -> Vec<Copied> {
        let mut runs = Vec::new();
        let mut undone = Vec::new();
        for (first, len, run) in log.run_pieces(events) {
            match &run.op {
                crate::oplog::RunOp::Aside(_) => {
                    for x in first..first + len {
                        if let Op::Aside(Aside::CameFrom { runs: r }) = log.event(x).op {
                            runs.extend(r);
                        }
                    }
                }
                crate::oplog::RunOp::Undelete { .. } => {
                    for x in first..first + len {
                        let Some(item) = self.ids.removed(log.undeleted(x)) else {
                            continue;
                        };
                        if let Some(source) = self.moved_from(item) {
                            undone.push((item, source));
                        }
                        undone.extend(self.copies(item).into_iter().map(|c| (c, item)));
                    }
                }
                _ => {}
            }
        }
        undone.sort_unstable();
        undone.dedup();
        runs.extend(Copied::runs(undone));
        runs
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

    /// The comments the document shows, by where they start (see
    /// `comments.rs`): open and resolved, not removed.
    pub fn comments(&self) -> Vec<crate::Comment> {
        self.comments.all()
    }

    /// Comment `id`, if the document shows it.
    pub fn comment(&self, id: Lv) -> Option<crate::Comment> {
        self.comments.get(id)
    }

    /// The comments covering position `pos`.
    pub fn comments_at(&self, pos: usize) -> Vec<crate::Comment> {
        self.comments
            .all()
            .into_iter()
            .filter(|c| c.span.contains(&pos))
            .collect()
    }

    /// Comment `id`'s body, at this version.
    pub fn comment_body<T: Clone>(&self, log: &OpLog<T>, id: Lv) -> String {
        log.comment_body(id, &self.version)
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
        if self.moves.stale {
            self.moves.rebuild(log, &self.version);
        }
        if self.comments.stale {
            self.comments = crate::comments::Comments::new(self.ids.len());
            let all: Vec<Lv> = (0..log.len())
                .filter(|&lv| {
                    log.kind(lv) == crate::oplog::Kind::Aside && log.contains(&self.version, lv)
                })
                .collect();
            self.comments.take_in(log, &all, &self.ids);
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
            self.comments.apply(change);
        }
        self.ranges.take_in(log, &new, &self.ids);
        self.comments.take_in(log, &new, &self.ids);
        self.moves.take_in(log, &new, &mut self.anchors, &self.ids);
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

    /// What the books carry beside their bytes: side data, written as
    /// JSON after them (see `ext.rs`).
    pub(crate) fn side(&self) -> SnapshotExt {
        SnapshotExt {
            ghosts: self
                .outline
                .ghosts()
                .into_iter()
                .map(|(item, place)| Ghost { item, place })
                .collect(),
        }
    }

    /// Puts back what [`side`](Self::side) took out, after
    /// [`decode`](Self::decode).
    pub(crate) fn apply_side(&mut self, side: &SnapshotExt) -> Result<(), DecodeError> {
        let ghosts: Vec<(Lv, usize)> = side.ghosts.iter().map(|g| (g.item, g.place)).collect();
        self.outline.place_ghosts(&ghosts)
    }

    /// Reads books [`encode`](Self::encode) wrote, for a log of `events`
    /// events. Side data comes after: [`apply_side`](Self::apply_side).
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
            comments: crate::comments::Comments::stale(),
            moves: crate::follow::Moves::stale(),
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
        let mut back = Accountant::decode(&mut bytes.as_slice(), log.len()).unwrap();
        // The ghost goes beside the books, as JSON.
        let side = serde_json::to_string(&books.side()).unwrap();
        assert_eq!(side, r#"{"ghosts":[{"item":4,"place":4}]}"#);
        back.apply_side(&serde_json::from_str(&side).unwrap()).unwrap();
        assert_eq!(back.sections(), books.sections());
        assert_eq!(back.level(4), 1, "a deleted item keeps its mark");
        // And its section, headless, at its place (#19).
        assert_eq!(back.sections().len(), 2);
        assert_eq!(back.sections()[1].start, crate::Start::Headless);
        assert_eq!(back.sections()[1].pos, 4);
        assert_eq!(back.headless(), vec![4]);
    }
}
