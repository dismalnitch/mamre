pub mod cursor;
pub mod hash;
pub mod tree;
pub mod units;
use arrayvec::ArrayVec;
use std::{error::Error, sync::Arc};
use units::TREE_BASE;

pub trait Item: Clone {
    type Summary: Summary;
    fn summary(&self) -> Self::Summary;
}

pub trait Summary {
    fn empty() -> Self;
    fn concat(&mut self, other: &Self);
}

pub trait Dimension<S: Summary>: Ord + Clone {
    fn zero() -> Self;
    fn add_summary(&mut self, summary: &S);
}

struct SumTree<T: Item>(pub Arc<Node<T>>);

enum Node<T: Item> {
    Internal {
        summary: T::Summary,
        children: ArrayVec<SumTree<T>, { 2 * TREE_BASE }>,
        child_summaries: ArrayVec<T::Summary, { 2 * TREE_BASE }>,
    },
    Leaf {
        summary: T::Summary,
        items: ArrayVec<T, { 2 * TREE_BASE }>,
        item_summaries: ArrayVec<T::Summary, { 2 * TREE_BASE }>,
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
