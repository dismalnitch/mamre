//! Random operations, applied to a tree and to the model side by side, with
//! everything checked after each one.

use proptest::collection::vec;
use proptest::prelude::*;

use crate::model::{self, Bias, Line, Step, Target};
use crate::subject::Subject;

/// A target to seek to, chosen relative to whatever the tree holds when the
/// operation runs, so that seeks land inside the tree, exactly on
/// boundaries, and past the end.
#[derive(Clone, Copy, Debug)]
pub struct Aim {
    dim: Dim,
    pick: Pick,
    bias: Bias,
}

#[derive(Clone, Copy, Debug)]
enum Dim {
    Count,
    Len,
    Height,
}

#[derive(Clone, Copy, Debug)]
enum Pick {
    /// Exactly on the boundary before item `k % (n + 1)`.
    Boundary(u16),
    /// This fraction of the way through the total.
    Fraction(u16),
    /// This far past the end.
    Past(u8),
}

impl Aim {
    fn resolve(&self, lines: &[Line]) -> (Target, Bias) {
        let make = |v: u64| match self.dim {
            Dim::Count => Target::Count(v as usize),
            Dim::Len => Target::Len(v),
            Dim::Height => Target::Height(v),
        };
        let total = make(0).measure(&model::summarize(lines));
        let value = match self.pick {
            Pick::Boundary(k) => {
                let before = &lines[..k as usize % (lines.len() + 1)];
                make(0).measure(&model::summarize(before))
            }
            Pick::Fraction(f) => total * f as u64 / u16::MAX as u64,
            Pick::Past(d) => total + d as u64,
        };
        (make(value), self.bias)
    }
}

#[derive(Clone, Debug)]
pub enum Op {
    Push(Line),
    Extend(Vec<Line>),
    /// Build a tree from these and append it.
    Append(Vec<Line>),
    /// Build a tree from these and append this one to it.
    Prepend(Vec<Line>),
    /// Split, and keep the left part.
    KeepLeft(Aim),
    /// Split, and keep the right part.
    KeepRight(Aim),
    /// Replace the items between two seeks with new ones, by splitting and
    /// joining.
    Splice {
        from: Aim,
        to: Aim,
        insert: Vec<Line>,
    },
    /// Replace the items between two seeks with new ones, with the tree's
    /// own edit. Both ends use `from`'s bias.
    Replace {
        from: Aim,
        to: Aim,
        insert: Vec<Line>,
    },
    /// Put new items in where a seek lands.
    Insert {
        at: Aim,
        insert: Vec<Line>,
    },
    /// Remove the items between two seeks. Both ends use `from`'s bias.
    Remove {
        from: Aim,
        to: Aim,
    },
    /// Change the item a seek lands on, in place.
    Update {
        at: Aim,
        line: Line,
    },
    /// Seek, and check where it lands and the summary before it.
    Seek(Aim),
    /// Seek (or start fresh, before the first item), then step, checking
    /// the item and the summary before it after every step.
    Walk {
        start: Option<Aim>,
        steps: Vec<Step>,
    },
}

/// Applies `op` to both, and returns what went wrong, if anything.
fn apply<S: Subject>(tree: &mut S, model: &mut Vec<Line>, op: &Op) -> Result<(), String> {
    match op {
        Op::Push(line) => {
            tree.push(*line);
            model.push(*line);
        }
        Op::Extend(lines) => {
            tree.extend(lines.clone());
            model.extend(lines);
        }
        Op::Append(lines) => {
            tree.append(S::from_items(lines.clone()));
            model.extend(lines);
        }
        Op::Prepend(lines) => {
            let mut front = S::from_items(lines.clone());
            front.append(tree.clone());
            *tree = front;
            model.splice(0..0, lines.iter().copied());
        }
        Op::KeepLeft(aim) | Op::KeepRight(aim) => {
            let (target, bias) = aim.resolve(model);
            let at = model::seek(model, target, bias);
            let (left, right) = tree.split(target, bias);
            if matches!(op, Op::KeepLeft(_)) {
                *tree = left;
                model.truncate(at);
            } else {
                *tree = right;
                model.drain(..at);
            }
        }
        Op::Splice { from, to, insert } => {
            let (a, a_bias) = from.resolve(model);
            let (b, b_bias) = to.resolve(model);
            let mut a = model::seek(model, a, a_bias);
            let mut b = model::seek(model, b, b_bias);
            if b < a {
                std::mem::swap(&mut a, &mut b);
            }
            let (mut left, rest) = tree.split(Target::Count(a), Bias::Right);
            let (_, right) = rest.split(Target::Count(b - a), Bias::Right);
            left.append(S::from_items(insert.clone()));
            left.append(right);
            *tree = left;
            model.splice(a..b, insert.iter().copied());
        }
        Op::Replace { from, to, insert } => {
            let (a, bias) = from.resolve(model);
            let (b, _) = to.resolve(model);
            let range = model::range(model, a, b, bias);
            tree.replace(a, b, bias, insert.clone());
            model.splice(range, insert.iter().copied());
        }
        Op::Insert { at, insert } => {
            let (target, bias) = at.resolve(model);
            let at = model::seek(model, target, bias);
            tree.insert(target, bias, insert.clone());
            model.splice(at..at, insert.iter().copied());
        }
        Op::Remove { from, to } => {
            let (a, bias) = from.resolve(model);
            let (b, _) = to.resolve(model);
            let range = model::range(model, a, b, bias);
            tree.remove(a, b, bias);
            model.drain(range);
        }
        Op::Update { at, line } => {
            let (target, bias) = at.resolve(model);
            let i = model::seek(model, target, bias);
            let want = i < model.len();
            if want {
                model[i] = *line;
            }
            let got = tree.update(target, bias, *line);
            if got != want {
                return Err(format!(
                    "update({target:?}, {bias:?}) found an item: {got}, want {want}"
                ));
            }
        }
        Op::Seek(aim) => {
            let (target, bias) = aim.resolve(model);
            check_seek(tree, model, target, bias)?;
        }
        Op::Walk { start, steps } => {
            let start = start.map(|aim| aim.resolve(model));
            let want = model::walk(model, start, steps);
            let got = tree.walk(start, steps);
            if let Some(i) = (0..want.len()).find(|&i| got.get(i) != Some(&want[i])) {
                let after = match i {
                    0 => "before any step".to_string(),
                    i => format!("after step {} ({:?})", i - 1, steps[i - 1]),
                };
                return Err(format!(
                    "walk from {start:?}, {after}: at {:?}, want {:?}",
                    got.get(i),
                    want[i]
                ));
            }
        }
    }
    Ok(())
}

fn check_seek<S: Subject>(
    tree: &S,
    model: &[Line],
    target: Target,
    bias: Bias,
) -> Result<(), String> {
    let want = model::seek(model, target, bias);
    let got = tree.seek(target, bias);
    if got != want {
        return Err(format!(
            "seek({target:?}, {bias:?}) landed on {got}, want {want}"
        ));
    }
    let want = model::prefix(model, target, bias);
    let got = tree.prefix(target, bias);
    if got != want {
        return Err(format!(
            "prefix({target:?}, {bias:?}) is {got:?}, want {want:?}"
        ));
    }
    Ok(())
}

/// Everything that must hold between a tree and the model at any moment.
fn verify<S: Subject>(tree: &S, model: &[Line]) -> Result<(), String> {
    tree.check().map_err(|e| format!("shape: {e}"))?;
    if tree.items() != model {
        return Err(format!(
            "items differ: tree has {} items, model has {}",
            tree.items().len(),
            model.len()
        ));
    }
    let want = model::summarize(model);
    if tree.summary() != want {
        return Err(format!("summary is {:?}, want {want:?}", tree.summary()));
    }
    // In a small tree, seek to every boundary, both ways, by every
    // dimension: empty items make the boundaries the likeliest place to go
    // wrong.
    if model.len() <= 64 {
        let mut before = model::Lines::default();
        for i in 0..=model.len() {
            for target in [
                Target::Count(before.count),
                Target::Len(before.len),
                Target::Height(before.height),
            ] {
                for bias in [Bias::Left, Bias::Right] {
                    check_seek(tree, model, target, bias)?;
                }
            }
            if let Some(line) = model.get(i) {
                before = before.add(&model::Lines::of(line));
            }
        }
    }
    Ok(())
}

/// Starts a tree from `start`, then applies `ops`, checking everything after
/// each one. Also checks that no operation changes a copy of the tree made
/// before it: copies must be independent.
pub fn run<S: Subject>(start: Vec<Line>, ops: &[Op]) -> Result<(), String> {
    let mut tree = S::from_items(start.clone());
    let mut model = start;
    verify(&tree, &model).map_err(|e| format!("after building: {e}"))?;
    for (step, op) in ops.iter().enumerate() {
        let copy = tree.clone();
        let copy_model = model.clone();
        let at = |e: String| format!("step {step}, {op:?}: {e}");
        apply(&mut tree, &mut model, op).map_err(at)?;
        verify(&tree, &model).map_err(at)?;
        verify(&copy, &copy_model)
            .map_err(|e| at(format!("a copy made before it changed: {e}")))?;
    }
    Ok(())
}

pub fn line() -> impl Strategy<Value = Line> {
    let len = prop_oneof![3 => Just(0u32), 7 => 1u32..=8];
    let height = prop_oneof![2 => Just(0u32), 8 => 1u32..=4];
    (len, height).prop_map(|(len, height)| Line { len, height })
}

pub fn lines(max: usize) -> impl Strategy<Value = Vec<Line>> {
    vec(line(), 0..=max)
}

fn aim() -> impl Strategy<Value = Aim> {
    let dim = prop_oneof![Just(Dim::Count), Just(Dim::Len), Just(Dim::Height)];
    let pick = prop_oneof![
        4 => any::<u16>().prop_map(Pick::Boundary),
        4 => any::<u16>().prop_map(Pick::Fraction),
        1 => (1u8..=3).prop_map(Pick::Past),
    ];
    let bias = prop_oneof![Just(Bias::Left), Just(Bias::Right)];
    (dim, pick, bias).prop_map(|(dim, pick, bias)| Aim { dim, pick, bias })
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => line().prop_map(Op::Push),
        2 => lines(40).prop_map(Op::Extend),
        2 => lines(40).prop_map(Op::Append),
        1 => lines(40).prop_map(Op::Prepend),
        2 => aim().prop_map(Op::KeepLeft),
        2 => aim().prop_map(Op::KeepRight),
        3 => (aim(), aim(), lines(20)).prop_map(|(from, to, insert)| Op::Splice { from, to, insert }),
        3 => (aim(), aim(), lines(20)).prop_map(|(from, to, insert)| Op::Replace { from, to, insert }),
        2 => (aim(), lines(20)).prop_map(|(at, insert)| Op::Insert { at, insert }),
        2 => (aim(), aim()).prop_map(|(from, to)| Op::Remove { from, to }),
        3 => (aim(), line()).prop_map(|(at, line)| Op::Update { at, line }),
        4 => aim().prop_map(Op::Seek),
        3 => (prop::option::weighted(0.75, aim()), steps()).prop_map(|(start, steps)| Op::Walk { start, steps }),
    ]
}

/// Mostly runs one way, so walks reach the ends and step off them, with
/// turns mixed in.
fn steps() -> impl Strategy<Value = Vec<Step>> {
    let run = (prop_oneof![Just(Step::Next), Just(Step::Prev)], 1usize..=12)
        .prop_map(|(step, n)| vec![step; n]);
    vec(run, 0..=4).prop_map(|runs| runs.concat())
}

pub fn ops() -> impl Strategy<Value = Vec<Op>> {
    vec(op(), 1..48)
}
