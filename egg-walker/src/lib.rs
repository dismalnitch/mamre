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
//! - [`OpLog`]: every event a replica knows, and syncing them between logs.
//! - [`OpLog::changes`]: the walk, from one version to another.
//! - [`reference`](mod@reference): the slow, obviously-correct version the walk is tested
//!   against.
//!
//! The walk keeps its records in a mamre `SumTree` with order keys, starts
//! each merge from the latest point the history narrows to, and moves its
//! prepare version by walking only as far back as it changes. A simpler walk
//! over a plain list is kept to check it against, change for change.

mod ids;
mod key;
mod oplog;
mod order;
pub mod reference;
mod simple;
mod walk;

pub use ids::Identities;
pub use oplog::{AgentId, Event, Lv, Op, OpLog};
pub use walk::Change;
