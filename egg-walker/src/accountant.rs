//! The Accountant: what a replica keeps beside its document, on the CRDT's
//! side, so merges stay cheap and nothing loses track of itself.
//!
//! The document is the replica's own: a `Vec`, a rope, anything that can
//! insert, delete and move items. It never holds CRDT state. The
//! accountant keeps the books instead: the version the document is at, who
//! each item is ([`Identities`]), the watched items ([`Anchors`]), and the
//! moved blocks (see `blocks.rs`). [`merge`](Accountant::merge) brings
//! them all up to a new version and hands back the [`Change`]s that bring
//! the document there too. That's the whole contract: changes out, books
//! kept.
//!
//! A local edit is an event like any other: add it to the log at the
//! accountant's version, and merge it. The changes are the edit itself, so
//! a document that already made it can skip them.

use mamre::codec::{DecodeError, get_usize, put_usize};

use crate::anchors::Anchors;
use crate::blocks::Blocks;
use crate::ids::Identities;
use crate::oplog::{Lv, OpLog};
use crate::walk::Change;

#[derive(Clone)]
pub struct Accountant {
    version: Vec<Lv>,
    ids: Identities,
    anchors: Anchors,
    blocks: Blocks,
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
            blocks: Blocks::default(),
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

    /// Brings the books up to everything in the current version and `to`,
    /// and returns the changes that bring the document there.
    pub fn merge<'a, T: Clone>(&mut self, log: &'a OpLog<T>, to: &[Lv]) -> Vec<Change<'a, T>> {
        let to = log.union(&self.version, to);
        let out = log.changes_kept(&self.version, &to, &mut self.ids, &mut self.blocks);
        for change in &out {
            self.anchors.apply(change);
        }
        self.version = to;
        out
    }

    /// Appends the books' bytes: the version, the identities, the anchors,
    /// the blocks.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_usize(out, self.version.len());
        for &lv in &self.version {
            put_usize(out, lv);
        }
        self.ids.encode(out);
        self.anchors.encode(out);
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
        let blocks = Blocks::decode(input, events, ids.len())?;
        Ok(Accountant {
            version,
            ids,
            anchors,
            blocks,
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
        assert_eq!(books.blocks.0, vec![(6, 3..5)]);
        let mut bytes = Vec::new();
        books.encode(&mut bytes);
        let back = Accountant::decode(&mut bytes.as_slice(), log.len()).unwrap();
        assert_eq!(back.version, books.version);
        assert_eq!(back.blocks, books.blocks);
        assert_eq!(back.len(), books.len());
        assert_eq!(back.anchors.position(back.ids.item(2)), Some(2));
    }
}
