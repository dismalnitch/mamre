//! Where mamre's tree meets the referee.
//!
//! To wire it up:
//! 1. Write an adapter: a type holding a mamre tree of [`Line`]s, with
//!    [`Lines`] as its summary and `Count`, `Len` and `Height` as dimensions
//!    to seek by, implementing [`Subject`] by calling the tree.
//! 2. Point `Tree` at it.
//! 3. Take the `#[ignore]`s off the tests in `main.rs`.
//!
//! Implement `Subject::check` too: it's where the tree's own shape rules get
//! checked after every operation.

#[allow(unused_imports)]
use crate::model::{Bias, Line, Lines, Target};
use crate::subject::Subject;

pub type Tree = Unwired;

/// Stands in until the tree exists.
#[derive(Clone, Debug)]
pub struct Unwired;

impl Subject for Unwired {
    fn new() -> Self {
        todo!("wire mamre's tree up in tests/referee/mamre.rs")
    }
    fn push(&mut self, _: Line) {
        todo!()
    }
    fn append(&mut self, _: Self) {
        todo!()
    }
    fn split(&self, _: Target, _: Bias) -> (Self, Self) {
        todo!()
    }
    fn seek(&self, _: Target, _: Bias) -> usize {
        todo!()
    }
    fn prefix(&self, _: Target, _: Bias) -> Lines {
        todo!()
    }
    fn summary(&self) -> Lines {
        todo!()
    }
    fn items(&self) -> Vec<Line> {
        todo!()
    }
}
