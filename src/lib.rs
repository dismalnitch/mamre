pub mod cursor;
pub mod hash;
pub mod tree;
pub mod units;
use arrayvec::ArrayVec;
use std::{cmp::Ordering, error::Error, sync::Arc};
use units::CAP_SZ;

pub trait Item: Clone {
    type Summary: Summary;
    fn summary(&self) -> Self::Summary;
}

pub trait Summary: Clone {
    fn empty() -> Self;
    fn concat(&mut self, other: &Self);
}

/// Something a cursor keeps a running total of as it moves: everything before
/// its position, measured this way. What you *seek by* is a separate question;
/// see [`SeekTarget`].
pub trait Dimension<S: Summary>: Clone {
    fn zero() -> Self;
    fn add_summary(&mut self, summary: &S);
}

/// A summary is a dimension of itself: tracking it tracks everything.
impl<S: Summary> Dimension<S> for S {
    fn zero() -> Self {
        S::empty()
    }
    fn add_summary(&mut self, summary: &S) {
        self.concat(summary);
    }
}

/// Track two dimensions at once.
impl<S: Summary, A: Dimension<S>, B: Dimension<S>> Dimension<S> for (A, B) {
    fn zero() -> Self {
        (A::zero(), B::zero())
    }
    fn add_summary(&mut self, summary: &S) {
        self.0.add_summary(summary);
        self.1.add_summary(summary);
    }
}

/// Track nothing: for when you only want to find an item.
impl<S: Summary> Dimension<S> for () {
    fn zero() -> Self {}
    fn add_summary(&mut self, _: &S) {}
}

/// Something a cursor can seek to. It only has to say where a position, in the
/// cursor's dimension `D`, stands relative to it: before (`Greater`, the target
/// is further on), at (`Equal`), or past (`Less`).
///
/// It needn't be a total. A key can be a target against a "largest key so far"
/// summary, which is how an ordered map or set can sit on top of the tree.
pub trait SeekTarget<S: Summary, D: Dimension<S>> {
    fn cmp_position(&self, position: &D) -> Ordering;
}

/// An ordered dimension is a target in itself.
impl<S: Summary, D: Dimension<S> + Ord> SeekTarget<S, D> for D {
    fn cmp_position(&self, position: &D) -> Ordering {
        Ord::cmp(self, position)
    }
}

/// Seek by the first of a pair while tracking both.
impl<S: Summary, A: Dimension<S> + Ord, B: Dimension<S>> SeekTarget<S, (A, B)> for A {
    fn cmp_position(&self, position: &(A, B)) -> Ordering {
        Ord::cmp(self, &position.0)
    }
}

#[derive(Clone)]
pub struct SumTree<T: Item, const CAP: usize = CAP_SZ> {
    root: Arc<Node<T, CAP>>,
}

#[derive(Clone)]
enum Node<T: Item, const CAP: usize = CAP_SZ> {
    Internal {
        /// Levels above the leaves: 1 for a node whose children are leaves.
        /// Fixed at creation; a node never moves to another level.
        height: u8,
        summary: T::Summary,
        children: ArrayVec<SumTree<T, CAP>, CAP>,
        child_summaries: ArrayVec<T::Summary, CAP>,
    },
    Leaf {
        summary: T::Summary,
        items: ArrayVec<T, CAP>,
        item_summaries: ArrayVec<T::Summary, CAP>,
    },
}

/// Errors produced by tree operations.
///
/// `#[non_exhaustive]` lets new variants be added later without breaking
/// downstream `match`es; callers must include a `_ =>` arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TreeError {
    /// The operation would not change the tree (e.g. splitting a node
    /// that doesn't need it).
    NoEffectError,
}

impl std::fmt::Display for TreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TreeError::NoEffectError => f.write_str("operation had no effect"),
        }
    }
}

impl Error for TreeError {}

/// Convenience alias for results of tree operations.
pub type TreeResult<T> = Result<T, TreeError>;

// struct TextSummary {
//     /// Length in UTF-8
//     len: usize,
//     /// Length in UTF-16 code units
//     len_utf16: OffsetUtf16,
//     /// A point representing the number of lines and the length of the last line
//     lines: Point,
//     /// How many `char`s are in the first line
//     first_line_chars: u32,
//     /// How many `char`s are in the last line
//     last_line_chars: u32,
//     /// How many UTF-16 code units are in the last line
//     last_line_len_utf16: u32,
//     /// The row idx of the longest row
//     longest_row: u32,
//     /// How many `char`s are in the longest row
//     longest_row_chars: u32,
// }
