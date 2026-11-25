//! An event-graph CRDT after Eg-walker (Gentle & Kleppmann, 2025), for
//! sequences of any item, not just text.
//!
//! Gentle, J., & Kleppmann, M. (2025). Collaborative text editing with
//! Eg-walker: Better, faster, smaller. In *Proceedings of the Twentieth
//! European Conference on Computer Systems (EuroSys '25)* (pp. 311–328).
//! Association for Computing Machinery.
//! <https://doi.org/10.1145/3689031.3696076>
//! (Preprint: [arXiv:2409.14252](https://arxiv.org/abs/2409.14252).)
//!
//! Every edit is an event: an insert or delete at a plain position, in the
//! document *as its author saw it*, plus the events it came after. The
//! document itself is just items, with no IDs or tombstones in it. To merge,
//! the walk replays events into a temporary list that tracks each item in
//! two versions at once, and comes out with plain changes to apply.
//!
//! Undo and redo never make new items: an undelete names the delete it
//! undoes, and the item comes back as itself. So anything pinned to an item
//! (ink, a comment) finds it again, here or on any other replica.
//!
//! - [`OpLog`]: every event a replica knows, and syncing them between logs.
//! - [`OpLog::changes`]: the walk, from one version to another.
//! - [`Identities`]: kept beside a replica's document, which insert each item
//!   came from; [`OpLog::changes_with`] uses it so an undo of old text costs
//!   what the merge does.
//! - [`Anchors`]: kept beside it too, where watched items are; deleted ones
//!   wait, and come back with their item.
//! - [`file`](mod@file): a document's file, one append-only container of its history
//!   and the state to open it with; and [`OpLog::encode_chunk`] /
//!   [`OpLog::decode_chunk`], a stretch of history as bytes, for saving and
//!   for sync.
//! - [`reference`](mod@reference): the slow, obviously-correct version the walk is tested
//!   against.
//!
//! The walk keeps its records in a mamre `SumTree` with order keys, starts
//! each merge from the latest point the history narrows to, and moves its
//! prepare version by walking only as far back as it changes. A simpler walk
//! over a plain list is kept to check it against, change for change.
//!
//! # Built on mamre
//!
//! This crate is mamre's CRDT, not a standalone one. The document stays
//! anyone's (all a merge asks of it is [`Sequence`]), but the CRDT's own
//! state is built on mamre, on purpose:
//!
//! - the walk's records, [`Identities`] and [`Anchors`] are mamre
//!   `SumTree`s, sought by their summaries;
//! - the log, sync messages, files and the accountant's books are written
//!   with `mamre::codec`, whose `DecodeError` the decoding functions return.
//!
//! The parts that don't need either are the reference ([`reference`](mod@reference)) and
//! the simple walk: plain lists, correct and slow. A CRDT without mamre
//! would start from those and bring its own tree.

mod accountant;
mod anchors;
// Kept in the books' format until it's cleaned out (#20): always empty.
#[allow(dead_code)]
mod blocks;
#[doc(hidden)]
pub mod copies;
mod encode;
pub mod file;
mod ids;
mod key;
mod merge;
mod oplog;
mod order;
mod outline;
mod recent;
pub mod reference;
#[doc(hidden)]
pub mod runs;
// Its moves (portals) retire with #20: it stays the oracle for merges
// without moves.
#[allow(dead_code)]
mod simple;
pub mod sync;
mod walk;

pub use accountant::Accountant;
pub use anchors::Anchors;
pub use ids::Identities;
pub use oplog::{AgentId, Event, Lv, Op, OpLog};
pub use outline::Section;
pub use walk::{Change, Sequence};
