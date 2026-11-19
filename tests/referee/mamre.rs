//! Where mamre's tree meets the referee.
//!
//! `Mamre` holds a `SumTree` of [`Line`]s, with [`Lines`] as its summary and
//! `Count`, `Len` and `Height` as dimensions to seek by.
//!
//! Still to wire, as the tree grows them:
//! - `append`: needs `SumTree::append`.
//! - `seek`, `prefix`, `split`: need `Cursor` (`seek`, `slice`, `suffix`).
//!
//! The referee seeks after every operation, even the first build, so the
//! tests in `main.rs` stay `#[ignore]`d until all of these are in.

use std::fmt;

use mamre::{Dimension, Item, SumTree, Summary};

use crate::model::{Bias, Line, Lines, Target};
use crate::subject::Subject;

pub type Tree = Mamre;

impl Item for Line {
    type Summary = Lines;
    fn summary(&self) -> Lines {
        Lines::of(self)
    }
}

impl Summary for Lines {
    fn empty() -> Self {
        Lines::default()
    }
    fn concat(&mut self, other: &Self) {
        *self = self.add(other);
    }
}

/// Seek by number of items.
#[allow(dead_code)] // until seek is wired
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Count(pub usize);

/// Seek by total text length.
#[allow(dead_code)] // until seek is wired
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Len(pub u64);

/// Seek by total height.
#[allow(dead_code)] // until seek is wired
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Height(pub u64);

impl Dimension<Lines> for Count {
    fn zero() -> Self {
        Count(0)
    }
    fn add_summary(&mut self, summary: &Lines) {
        self.0 += summary.count;
    }
}

impl Dimension<Lines> for Len {
    fn zero() -> Self {
        Len(0)
    }
    fn add_summary(&mut self, summary: &Lines) {
        self.0 += summary.len;
    }
}

impl Dimension<Lines> for Height {
    fn zero() -> Self {
        Height(0)
    }
    fn add_summary(&mut self, summary: &Lines) {
        self.0 += summary.height;
    }
}

#[derive(Clone)]
pub struct Mamre(SumTree<Line>);

/// Shows the items, so a failing case shrunk by proptest is readable.
impl fmt::Debug for Mamre {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.0.items()).finish()
    }
}

impl Subject for Mamre {
    fn new() -> Self {
        Mamre(SumTree::new())
    }

    fn push(&mut self, line: Line) {
        self.0.push(line);
    }

    fn append(&mut self, _other: Self) {
        todo!("needs SumTree::append")
    }

    fn split(&self, _target: Target, _bias: Bias) -> (Self, Self) {
        todo!("needs Cursor::slice and Cursor::suffix")
    }

    fn seek(&self, _target: Target, _bias: Bias) -> usize {
        todo!("needs Cursor::seek")
    }

    fn prefix(&self, _target: Target, _bias: Bias) -> Lines {
        todo!("needs Cursor::seek")
    }

    fn summary(&self) -> Lines {
        *self.0.summary()
    }

    fn items(&self) -> Vec<Line> {
        self.0.items()
    }

    fn check(&self) -> Result<(), String> {
        self.0.check()
    }
}
