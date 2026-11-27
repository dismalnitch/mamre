//! Where a new item goes among concurrent ones: the YjsMod rule, from
//! Gentle and Kleppmann's reference CRDTs (the rule diamond-types uses).
//!
//! Every item remembers the items either side of where it was put, in the
//! document its insert saw: its left and right origin. Inserts made without
//! seeing each other can land between the same two items; the rule orders
//! them the same way on every replica, whatever order they arrive in.
//!
//! The walker and the reference share this, so it's the one piece they
//! don't check against each other. Convergence tests (every replica ends
//! the same, however events arrive) and, later, agreement with
//! diamond-types are what check it.

use std::cmp::Ordering;

use crate::oplog::Lv;

/// What the rule needs to know about an item already in the list.
pub(crate) trait Placed {
    fn id(&self) -> Lv;
    fn left(&self) -> Option<Lv>;
    fn right(&self) -> Option<Lv>;
}

/// Where in `list` a new item with origins `left` and `right` goes.
/// `index_of` finds an item's index by id; `cmp` orders the new item
/// against another by their ids, to break ties.
pub(crate) fn place<P: Placed>(
    list: &[P],
    left: Option<Lv>,
    right: Option<Lv>,
    index_of: impl Fn(Lv) -> usize,
    cmp: impl Fn(Lv) -> Ordering,
) -> usize {
    let len = list.len() as isize;
    let left_at = left.map_or(-1, |l| index_of(l) as isize);
    // A right origin laid out before the left one (moves' rules can map an
    // origin far off: see `copies.rs`) says nothing: as if there were none.
    // Then every pair of origins is in order. That isn't quite enough for
    // where an item lands not to depend on the order things were laid out
    // in: a right origin carried deep into a sibling's subtree can still
    // put a later sibling either side of it (mamre#59). So both layouts
    // lay records out in the same order, by the event that made each.
    let right_at = right
        .map(|r| index_of(r) as isize)
        .filter(|&r| r > left_at)
        .unwrap_or(len);

    let mut dest = left_at + 1;
    let mut scanning = false;
    let mut i = dest;
    loop {
        if !scanning {
            dest = i;
        }
        if i == len || i == right_at {
            break;
        }
        let other = &list[i as usize];
        let other_left = other.left().map_or(-1, |l| index_of(l) as isize);
        let other_right = other
            .right()
            .map(|r| index_of(r) as isize)
            .filter(|&r| r > other_left)
            .unwrap_or(len);

        // Its left origin is before ours: we're past the items that could
        // go before us.
        if other_left < left_at {
            break;
        }
        if other_left == left_at {
            if other_right == right_at {
                // Same origins both sides: a true tie, by id.
                if cmp(other.id()) == Ordering::Less {
                    break;
                }
                scanning = false;
            } else {
                // Its right origin is nearer: we might go after it, but
                // can't tell until we see what follows.
                scanning = other_right < right_at;
            }
        }
        i += 1;
    }
    dest as usize
}
