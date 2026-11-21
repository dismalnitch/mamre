//! The slow, obviously-correct version: a plain list CRDT with tombstones,
//! replaying the whole history and rebuilding each event's own version of
//! the document from scratch, by definition, before applying it.
//!
//! No retreating, advancing or effect positions: an item exists at a
//! version if its insert is in it, and is visible if no delete in it
//! removed it. The walk must agree with this on every history. They share
//! only the ordering rule for concurrent inserts.

use crate::oplog::{Lv, Op, OpLog};
use crate::order::{Placed, place};
use crate::walk::tie_break;

struct Item<T> {
    id: Lv,
    left: Option<Lv>,
    right: Option<Lv>,
    deleted_by: Vec<Lv>,
    value: T,
}

impl<T> Placed for Item<T> {
    fn id(&self) -> Lv {
        self.id
    }
    fn left(&self) -> Option<Lv> {
        self.left
    }
    fn right(&self) -> Option<Lv> {
        self.right
    }
}

/// The document at `version`, the slow way.
pub fn checkout<T: Clone>(log: &OpLog<T>, version: &[Lv]) -> Vec<T> {
    let mut list: Vec<Item<T>> = Vec::new();
    for (lv, event) in log.events().iter().enumerate() {
        let at = log.closure(&event.parents);
        // The document this event saw: items its parents inserted, in
        // order, each with whether its parents also deleted it.
        let seen: Vec<(usize, bool)> = list
            .iter()
            .enumerate()
            .filter(|(_, item)| at[item.id])
            .map(|(i, item)| (i, !item.deleted_by.iter().any(|&d| at[d])))
            .collect();
        // The index, in `seen`, of the `pos`th visible item, or its end.
        let nth_visible = |pos: usize| {
            seen.iter()
                .enumerate()
                .filter(|(_, (_, visible))| *visible)
                .nth(pos)
                .map(|(k, _)| k)
        };
        match &event.op {
            Op::Insert { pos, item } => {
                let visible = seen.iter().filter(|(_, v)| *v).count();
                assert!(*pos <= visible, "insert {lv} at {pos} is past the end");
                // Just after the visible item before the position (or at the
                // very start), and before whatever came next in this
                // version, deleted or not.
                let (left, next) = match pos.checked_sub(1) {
                    Some(before) => {
                        let k = nth_visible(before).expect("checked above");
                        (Some(list[seen[k].0].id), k + 1)
                    }
                    None => (None, 0),
                };
                let right = seen.get(next).map(|&(i, _)| list[i].id);
                let index_of = |id: Lv| list.iter().position(|item| item.id == id).unwrap();
                let dest = place(&list, left, right, index_of, |other| {
                    tie_break(log, lv, other)
                });
                list.insert(
                    dest,
                    Item {
                        id: lv,
                        left,
                        right,
                        deleted_by: Vec::new(),
                        value: item.clone(),
                    },
                );
            }
            Op::Delete { pos } => {
                let k = nth_visible(*pos).expect("a delete is of an item its version shows");
                list[seen[k].0].deleted_by.push(lv);
            }
        }
    }
    let at = log.closure(version);
    list.into_iter()
        .filter(|item| at[item.id] && !item.deleted_by.iter().any(|&d| at[d]))
        .map(|item| item.value)
        .collect()
}
