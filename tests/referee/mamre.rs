//! Where mamre's tree meets the referee.
//!
//! `Mamre` holds a `SumTree` of [`Line`]s, with [`Lines`] as its summary.
//! Its cursors track the whole `Lines` before them, and seek with [`To`]:
//! any of the referee's targets, compared against the one field of `Lines`
//! its dimension names.
//!
//! Every operation goes through the library's own API: `split` is
//! `split_off`, the edits are `replace`/`insert`/`remove`, `walk` steps a
//! real cursor, and `items` comes from the iterator, cross-checked against
//! `SumTree::items` in `check`.

use std::{cmp::Ordering, fmt};

use mamre::{SeekTarget, SumTree, Summary, cursor, Item};

use crate::model::{Bias, Line, Lines, Step, Target};
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

/// Seek to a referee target: by count, length or height. One type for all
/// three, so the two ends of an edit can be in different dimensions.
pub struct To(pub Target);

impl SeekTarget<Lines, Lines> for To {
    fn cmp_position(&self, position: &Lines) -> Ordering {
        match self.0 {
            Target::Count(n) => n.cmp(&position.count),
            Target::Len(n) => n.cmp(&position.len),
            Target::Height(n) => n.cmp(&position.height),
        }
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
        f.debug_list().entries(self.0.iter()).finish()
    }
}

impl Subject for Mamre {
    fn new() -> Self {
        Mamre(SumTree::new())
    }

    fn push(&mut self, line: Line) {
        self.0.push(line);
    }

    fn extend(&mut self, lines: Vec<Line>) {
        self.0.extend(lines);
    }

    fn append(&mut self, other: Self) {
        self.0.append(other.0);
    }

    fn split(&self, target: Target, bias: Bias) -> (Self, Self) {
        let mut left = self.0.clone();
        let right = left.split_off(&To(target), cursor_bias(bias));
        (Mamre(left), Mamre(right))
    }

    fn seek(&self, target: Target, bias: Bias) -> usize {
        self.prefix(target, bias).count
    }

    fn prefix(&self, target: Target, bias: Bias) -> Lines {
        let mut cursor = self.0.cursor::<Lines>();
        cursor.seek(&To(target), cursor_bias(bias));
        *cursor.start()
    }

    fn replace(&mut self, from: Target, to: Target, bias: Bias, lines: Vec<Line>) {
        self.0.replace(&To(from), &To(to), cursor_bias(bias), lines);
    }

    fn insert(&mut self, at: Target, bias: Bias, lines: Vec<Line>) {
        self.0.insert(&To(at), cursor_bias(bias), lines);
    }

    fn remove(&mut self, from: Target, to: Target, bias: Bias) {
        self.0.remove(&To(from), &To(to), cursor_bias(bias));
    }

    fn update(&mut self, target: Target, bias: Bias, line: Line) -> bool {
        self.0
            .update_at(&To(target), cursor_bias(bias), |l| *l = line)
            .is_some()
    }

    fn walk(&self, start: Option<(Target, Bias)>, steps: &[Step]) -> Vec<(Option<Line>, Lines)> {
        let mut cursor = self.0.cursor::<Lines>();
        if let Some((target, bias)) = start {
            cursor.seek(&To(target), cursor_bias(bias));
        }
        let mut out = vec![(cursor.item().copied(), *cursor.start())];
        for step in steps {
            match step {
                Step::Next => cursor.next(),
                Step::Prev => cursor.prev(),
            }
            out.push((cursor.item().copied(), *cursor.start()));
        }
        out
    }

    fn summary(&self) -> Lines {
        *self.0.summary()
    }

    fn items(&self) -> Vec<Line> {
        self.0.iter().copied().collect()
    }

    fn check(&self) -> Result<(), String> {
        self.0.check()?;
        if self.items() != self.0.items() {
            return Err("iter() and items() disagree".into());
        }
        Ok(())
    }
}
