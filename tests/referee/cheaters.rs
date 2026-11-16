//! Trees that are wrong on purpose, each in one way. The referee must catch
//! every one; if it lets one through, the referee has a blind spot.

use std::cell::RefCell;
use std::rc::Rc;

use proptest::test_runner::{Config, TestCaseError, TestRunner};

use crate::model::{self, Bias, Line, Lines, Target};
use crate::ops;
use crate::subject::{Model, Subject};

fn caught<S: Subject>() {
    let mut runner = TestRunner::new(Config {
        cases: 512,
        failure_persistence: None,
        ..Config::default()
    });
    let result = runner.run(&(ops::lines(80), ops::ops()), |(start, ops)| {
        ops::run::<S>(start, &ops).map_err(TestCaseError::fail)
    });
    assert!(result.is_err(), "the referee let {} through", std::any::type_name::<S>());
}

/// Ignores bias: always leans right.
#[derive(Clone, Debug, Default)]
struct LeansRight(Model);

impl Subject for LeansRight {
    fn new() -> Self {
        LeansRight(Model::new())
    }
    fn push(&mut self, line: Line) {
        self.0.push(line)
    }
    fn append(&mut self, other: Self) {
        self.0.append(other.0)
    }
    fn split(&self, target: Target, _: Bias) -> (Self, Self) {
        let (l, r) = self.0.split(target, Bias::Right);
        (LeansRight(l), LeansRight(r))
    }
    fn seek(&self, target: Target, _: Bias) -> usize {
        self.0.seek(target, Bias::Right)
    }
    fn prefix(&self, target: Target, _: Bias) -> Lines {
        self.0.prefix(target, Bias::Right)
    }
    fn summary(&self) -> Lines {
        self.0.summary()
    }
    fn items(&self) -> Vec<Line> {
        self.0.items()
    }
}

/// Copies share their items, so changing one changes the others: what
/// happens when a tree forgets to copy a node before editing it.
#[derive(Clone, Debug, Default)]
struct SharesCopies(Rc<RefCell<Vec<Line>>>);

impl Subject for SharesCopies {
    fn new() -> Self {
        SharesCopies::default()
    }
    fn push(&mut self, line: Line) {
        self.0.borrow_mut().push(line)
    }
    fn append(&mut self, other: Self) {
        let other = other.0.borrow().clone();
        self.0.borrow_mut().extend(other)
    }
    fn split(&self, target: Target, bias: Bias) -> (Self, Self) {
        let lines = self.0.borrow();
        let at = model::seek(&lines, target, bias);
        let fresh = |v: &[Line]| SharesCopies(Rc::new(RefCell::new(v.to_vec())));
        (fresh(&lines[..at]), fresh(&lines[at..]))
    }
    fn seek(&self, target: Target, bias: Bias) -> usize {
        model::seek(&self.0.borrow(), target, bias)
    }
    fn prefix(&self, target: Target, bias: Bias) -> Lines {
        model::prefix(&self.0.borrow(), target, bias)
    }
    fn summary(&self) -> Lines {
        model::summarize(&self.0.borrow())
    }
    fn items(&self) -> Vec<Line> {
        self.0.borrow().clone()
    }
}

/// Keeps a running summary, but forgets the max when appending a tree: a
/// cached summary gone stale.
#[derive(Clone, Debug, Default)]
struct StaleMax(Model, Lines);

impl Subject for StaleMax {
    fn new() -> Self {
        StaleMax::default()
    }
    fn push(&mut self, line: Line) {
        self.1 = self.1.add(&Lines::of(&line));
        self.0.push(line)
    }
    fn append(&mut self, other: Self) {
        let max = self.1.max_height;
        self.1 = self.1.add(&other.1);
        self.1.max_height = max;
        self.0.append(other.0)
    }
    fn split(&self, target: Target, bias: Bias) -> (Self, Self) {
        let (l, r) = self.0.split(target, bias);
        let (ls, rs) = (l.summary(), r.summary());
        (StaleMax(l, ls), StaleMax(r, rs))
    }
    fn seek(&self, target: Target, bias: Bias) -> usize {
        self.0.seek(target, bias)
    }
    fn prefix(&self, target: Target, bias: Bias) -> Lines {
        self.0.prefix(target, bias)
    }
    fn summary(&self) -> Lines {
        self.1
    }
    fn items(&self) -> Vec<Line> {
        self.0.items()
    }
}

/// Loses an item when splitting exactly on a boundary.
#[derive(Clone, Debug, Default)]
struct DropsAtBoundary(Model);

impl Subject for DropsAtBoundary {
    fn new() -> Self {
        DropsAtBoundary(Model::new())
    }
    fn push(&mut self, line: Line) {
        self.0.push(line)
    }
    fn append(&mut self, other: Self) {
        self.0.append(other.0)
    }
    fn split(&self, target: Target, bias: Bias) -> (Self, Self) {
        let (l, mut r) = self.0.split(target, bias);
        let on_boundary = target.value() == target.measure(&l.summary());
        if on_boundary && !l.0.is_empty() && !r.0.is_empty() {
            r.0.remove(0);
        }
        (DropsAtBoundary(l), DropsAtBoundary(r))
    }
    fn seek(&self, target: Target, bias: Bias) -> usize {
        self.0.seek(target, bias)
    }
    fn prefix(&self, target: Target, bias: Bias) -> Lines {
        self.0.prefix(target, bias)
    }
    fn summary(&self) -> Lines {
        self.0.summary()
    }
    fn items(&self) -> Vec<Line> {
        self.0.items()
    }
}

#[test]
fn catches_ignoring_bias() {
    caught::<LeansRight>();
}

#[test]
fn catches_copies_that_share() {
    caught::<SharesCopies>();
}

#[test]
fn catches_a_stale_summary() {
    caught::<StaleMax>();
}

#[test]
fn catches_dropping_an_item_at_a_boundary() {
    caught::<DropsAtBoundary>();
}
