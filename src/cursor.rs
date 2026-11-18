//! Cursor: walks a `SumTree` and seeks by any `Dimension`.
//!
//! A cursor is the read side of the tree. `seek` finds a position, and
//! `slice`/`suffix` cut the tree there. Together with `SumTree::append`, they
//! are all you need to build insert, delete and replace.

use crate::{Dimension, Item, SumTree};

/// Which item a seek lands on when the target falls exactly on the boundary
/// between two items. `Left` stops at the end of the earlier item; `Right`
/// stops at the start of the later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bias {
    Left,
    Right,
}

/// One level of the cursor's path from the root down to the current leaf.
struct StackEntry<'a, T: Item, D> {
    /// The node at this level.
    tree: &'a SumTree<T>,
    /// Which child (internal node) or item (leaf) the cursor is on.
    index: usize,
    /// The total, in dimension `D`, of everything before `tree`.
    position: D,
}

pub struct Cursor<'a, T: Item, D: Dimension<T::Summary>> {
    tree: &'a SumTree<T>,
    /// Root-to-leaf path. Empty means the cursor hasn't started yet.
    stack: Vec<StackEntry<'a, T, D>>,
    /// The total, in dimension `D`, of everything before the current item.
    position: D,
    /// True once the cursor has moved past the last item.
    at_end: bool,
}

impl<'a, T: Item, D: Dimension<T::Summary>> Cursor<'a, T, D> {
    pub(crate) fn new(tree: &'a SumTree<T>) -> Self {
        Self {
            tree,
            stack: Vec::new(),
            position: D::zero(),
            at_end: false,
        }
    }

    /// Where the cursor is, measured in `D`: the total of everything before
    /// the current item.
    pub fn start(&self) -> &D {
        &self.position
    }

    /// The item under the cursor, or `None` before the start or past the end.
    pub fn item(&self) -> Option<&'a T> {
        // TODO: look at the top of `stack`; if it's a leaf, return items[index].
        todo!()
    }

    /// Moves to the next item.
    pub fn next(&mut self) {
        // TODO: advance the leaf index and add the item's summary to `position`.
        // When a leaf runs out, pop up until a node has another child, then
        // descend to that child's leftmost leaf. Set `at_end` if the root runs out.
        todo!()
    }

    /// Moves to the previous item.
    pub fn prev(&mut self) {
        // TODO: mirror of `next`. `position` has to go backwards, which
        // `Dimension` can't do (no subtraction). Either store each level's start
        // in `StackEntry` and recompute from there, or add subtraction to the trait.
        todo!()
    }

    /// Moves forward to `target`. Returns true if it landed exactly on it.
    pub fn seek_forward(&mut self, target: &D, bias: Bias) -> bool {
        // TODO: the core walk. At each internal node, skip whole children while
        // `position + child_summary` is still before `target` (use `bias` to
        // decide what "before" means at equality), then descend into the child
        // that contains it. Do the same over item summaries in the leaf.
        // Skipped children cost nothing but a summary add: that's the point.
        let _ = (target, bias);
        todo!()
    }

    /// Restarts from the beginning, then seeks to `target`.
    pub fn seek(&mut self, target: &D, bias: Bias) -> bool {
        // TODO: reset `stack`, `position` and `at_end`, then `seek_forward`.
        let _ = (target, bias);
        todo!()
    }

    /// Returns everything from the cursor up to `end` as a new tree, leaving
    /// the cursor at `end`.
    pub fn slice(&mut self, end: &D, bias: Bias) -> SumTree<T> {
        // TODO: same walk as `seek_forward`, but collect what you pass. Whole
        // children you skip can be reused as-is (clone the `Arc`, no copying);
        // only the partial leaf at each edge needs new nodes. Build the result
        // with `SumTree::append` / `push`.
        let _ = (end, bias);
        todo!()
    }

    /// Returns everything from the cursor to the end as a new tree.
    pub fn suffix(&mut self) -> SumTree<T> {
        // TODO: `slice` to the end of the tree (`self.tree.extent::<D>()`).
        todo!()
    }
}
