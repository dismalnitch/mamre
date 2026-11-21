//! Where mamre's tree meets the referee.
//!
//! `Mamre` holds a `SumTree` of [`Line`]s, with [`Lines`] as its summary.
//! Its cursors track the whole `Lines` before them, and seek by `Count`,
//! `Len` or `Height`: targets that each compare against one field of it.
//! `split` is a `slice` to the target, then a `suffix` for the rest.

use std::{cmp::Ordering, fmt};

use mamre::{SeekTarget, SumTree, Summary, cursor, Item};

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
pub struct Count(pub usize);

/// Seek by total text length.
pub struct Len(pub u64);

/// Seek by total height.
pub struct Height(pub u64);

impl SeekTarget<Lines, Lines> for Count {
    fn cmp_position(&self, position: &Lines) -> Ordering {
        self.0.cmp(&position.count)
    }
}

impl SeekTarget<Lines, Lines> for Len {
    fn cmp_position(&self, position: &Lines) -> Ordering {
        self.0.cmp(&position.len)
    }
}

impl SeekTarget<Lines, Lines> for Height {
    fn cmp_position(&self, position: &Lines) -> Ordering {
        self.0.cmp(&position.height)
    }
}

fn cursor_bias(bias: Bias) -> cursor::Bias {
    match bias {
        Bias::Left => cursor::Bias::Left,
        Bias::Right => cursor::Bias::Right,
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

    fn append(&mut self, other: Self) {
        self.0.append(other.0);
    }

    fn split(&self, target: Target, bias: Bias) -> (Self, Self) {
        let bias = cursor_bias(bias);
        let mut cursor = self.0.cursor::<Lines>();
        let left = match target {
            Target::Count(n) => cursor.slice(&Count(n), bias),
            Target::Len(n) => cursor.slice(&Len(n), bias),
            Target::Height(n) => cursor.slice(&Height(n), bias),
        };
        (Mamre(left), Mamre(cursor.suffix()))
    }

    fn seek(&self, target: Target, bias: Bias) -> usize {
        self.prefix(target, bias).count
    }

    fn prefix(&self, target: Target, bias: Bias) -> Lines {
        let bias = cursor_bias(bias);
        let mut cursor = self.0.cursor::<Lines>();
        match target {
            Target::Count(n) => cursor.seek(&Count(n), bias),
            Target::Len(n) => cursor.seek(&Len(n), bias),
            Target::Height(n) => cursor.seek(&Height(n), bias),
        };
        *cursor.start()
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
