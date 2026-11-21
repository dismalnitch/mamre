//! What the referee needs from a tree. Wire mamre's tree to this trait in
//! `mamre.rs`; the referee checks it against the model.

use crate::model::{self, Bias, Line, Lines, Step, Target};

pub trait Subject: Clone + std::fmt::Debug {
    fn new() -> Self;

    fn push(&mut self, line: Line);

    fn extend(&mut self, lines: Vec<Line>) {
        for line in lines {
            self.push(line);
        }
    }

    fn from_items(lines: Vec<Line>) -> Self {
        let mut tree = Self::new();
        tree.extend(lines);
        tree
    }

    /// Adds all of `other` after this tree's items.
    fn append(&mut self, other: Self);

    /// Splits where a seek for `target` lands: the items before it, and the
    /// rest. Leaves `self` as it was.
    fn split(&self, target: Target, bias: Bias) -> (Self, Self);

    /// See [`model::seek`].
    fn seek(&self, target: Target, bias: Bias) -> usize;

    /// See [`model::prefix`].
    fn prefix(&self, target: Target, bias: Bias) -> Lines;

    /// Replaces the items in [`model::range`] with `lines`.
    ///
    /// The default splits and joins; a tree with its own edits should use
    /// them instead, so the referee checks those.
    fn replace(&mut self, from: Target, to: Target, bias: Bias, lines: Vec<Line>) {
        let a = self.seek(from, bias);
        let b = self.seek(to, bias).max(a);
        let (mut left, rest) = self.split(Target::Count(a), Bias::Right);
        let (_, right) = rest.split(Target::Count(b - a), Bias::Right);
        left.append(Self::from_items(lines));
        left.append(right);
        *self = left;
    }

    /// Puts `lines` in where a seek for `at` lands.
    fn insert(&mut self, at: Target, bias: Bias, lines: Vec<Line>) {
        self.replace(at, at, bias, lines);
    }

    /// Removes the items in [`model::range`].
    fn remove(&mut self, from: Target, to: Target, bias: Bias) {
        self.replace(from, to, bias, Vec::new());
    }

    /// See [`model::walk`]. The default asks the model; a tree with a cursor
    /// should walk one.
    fn walk(&self, start: Option<(Target, Bias)>, steps: &[Step]) -> Vec<(Option<Line>, Lines)> {
        model::walk(&self.items(), start, steps)
    }

    /// The summary of the whole tree.
    fn summary(&self) -> Lines;

    /// Every item, in order.
    fn items(&self) -> Vec<Line>;

    /// The tree's own rules about its shape, checked after every operation.
    /// For a B+ tree:
    /// - every leaf is at the same depth;
    /// - every node except the root has between `BASE` and `2 * BASE`
    ///   children or items;
    /// - every node's cached summary equals the sum of its children's.
    ///
    /// Return what's wrong, if anything.
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

/// The model in `Subject` form. The referee runs against this too, to show
/// the harness itself works.
#[derive(Clone, Debug, Default)]
pub struct Model(pub Vec<Line>);

impl Subject for Model {
    fn new() -> Self {
        Model(Vec::new())
    }

    fn push(&mut self, line: Line) {
        self.0.push(line);
    }

    fn append(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    fn split(&self, target: Target, bias: Bias) -> (Self, Self) {
        let at = model::seek(&self.0, target, bias);
        (Model(self.0[..at].to_vec()), Model(self.0[at..].to_vec()))
    }

    fn seek(&self, target: Target, bias: Bias) -> usize {
        model::seek(&self.0, target, bias)
    }

    fn prefix(&self, target: Target, bias: Bias) -> Lines {
        model::prefix(&self.0, target, bias)
    }

    fn summary(&self) -> Lines {
        model::summarize(&self.0)
    }

    fn items(&self) -> Vec<Line> {
        self.0.clone()
    }
}
