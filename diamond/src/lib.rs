//! Plays [diamond-types]' merged edits onto a mamre [`Rope`].
//!
//! diamond-types keeps the edit history (its `OpLog`) and does the merging;
//! the rope is the document everything else reads: layout, search, summaries.
//! Bringing a rope up to date is asking the oplog for the *transformed*
//! operations between the version the rope is at and the new one, and
//! applying each as a plain insert or delete.
//!
//! This is the whole adapter, and it's the pattern for any CRDT that can
//! hand back positional edits: convert its positions to the rope's byte
//! offsets, then insert and remove.
//!
//! ```
//! use diamond_types::list::OpLog;
//! use mamre_rope::Rope;
//!
//! let mut oplog = OpLog::new();
//! let fred = oplog.get_or_create_agent_id("fred");
//! oplog.add_insert(fred, 0, "hello world");
//!
//! let mut rope: Rope = Rope::new();
//! mamre_diamond::merge(&mut rope, &oplog, &[], oplog.local_version_ref());
//! assert_eq!(rope.to_string(), "hello world");
//! ```
//!
//! [diamond-types]: https://crates.io/crates/diamond-types

use diamond_types::Time;
use diamond_types::list::OpLog;
use diamond_types::list::operation::{OpKind, Operation};
use mamre_rope::{Measure, Rope};

/// Brings `rope`, which holds the document at version `from`, up to version
/// `to`.
pub fn merge<M: Measure, const N: usize>(
    rope: &mut Rope<M, N>,
    oplog: &OpLog,
    from: &[Time],
    to: &[Time],
) {
    for (_, op) in oplog.iter_xf_operations_from(from, to) {
        // `None`: a delete that already happened on another branch.
        if let Some(op) = op {
            apply(rope, &op);
        }
    }
}

/// Applies one transformed operation. diamond-types counts positions in
/// `char`s; the rope converts them to bytes.
pub fn apply<M: Measure, const N: usize>(rope: &mut Rope<M, N>, op: &Operation) {
    let span = op.loc.span;
    match op.kind {
        OpKind::Ins => {
            let content = op.content_as_str().expect("inserts carry their content");
            let at = rope.char_to_offset(span.start);
            if op.loc.fwd {
                rope.insert(at, content);
            } else {
                // A reversed run is stored backwards; diamond-types' own
                // `Branch::merge` flips it the same way. (In practice only
                // backspaces come out reversed, and deletes have no order.)
                let flipped: String = content.chars().rev().collect();
                rope.insert(at, &flipped);
            }
        }
        OpKind::Del => {
            let a = rope.char_to_offset(span.start);
            let b = rope.char_to_offset(span.end);
            rope.remove(a..b);
        }
    }
}
