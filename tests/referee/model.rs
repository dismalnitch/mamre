//! The model: what a sum tree is supposed to do, written the slow, obvious
//! way. The items are a plain `Vec`, a summary is a fold, and a seek is a
//! linear scan. The tree must agree with this on every operation.
//!
//! The rules live here. If you disagree with one (bias at a boundary, say),
//! change it here, and the tree is then held to the new rule.

/// The item the tests store: something like a laid-out line, with a length
/// in text and a height on screen. Either can be zero, which is where seeks
/// get interesting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub len: u32,
    pub height: u32,
}

/// The summary of a run of lines. Three totals, each one a dimension you can
/// seek by, plus a max: not a sum, but it still combines the same way
/// however the lines are grouped, so it's a fair summary too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lines {
    pub count: usize,
    pub len: u64,
    pub height: u64,
    pub max_height: u32,
}

impl Lines {
    pub fn of(line: &Line) -> Self {
        Lines {
            count: 1,
            len: line.len as u64,
            height: line.height as u64,
            max_height: line.height,
        }
    }

    pub fn add(&self, other: &Lines) -> Lines {
        Lines {
            count: self.count + other.count,
            len: self.len + other.len,
            height: self.height + other.height,
            max_height: self.max_height.max(other.max_height),
        }
    }
}

pub fn summarize(lines: &[Line]) -> Lines {
    lines
        .iter()
        .fold(Lines::default(), |sum, line| sum.add(&Lines::of(line)))
}

/// Which dimension to seek by, and to where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// By number of items: the nth item.
    Count(usize),
    /// By total length: the item holding this text offset.
    Len(u64),
    /// By total height: the item at this y.
    Height(u64),
}

impl Target {
    /// This target's dimension, read from a summary.
    pub fn measure(&self, summary: &Lines) -> u64 {
        match self {
            Target::Count(_) => summary.count as u64,
            Target::Len(_) => summary.len,
            Target::Height(_) => summary.height,
        }
    }

    pub fn value(&self) -> u64 {
        match *self {
            Target::Count(n) => n as u64,
            Target::Len(n) | Target::Height(n) => n,
        }
    }
}

/// Which way to lean when a target falls exactly on the boundary between
/// two items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bias {
    /// Land on the item that ends at the boundary.
    Left,
    /// Land on the item that starts at the boundary.
    Right,
}

/// Where a seek lands: the index of an item, or `lines.len()` if the target
/// is past the end.
///
/// Item `i` covers `[start, end)` in the target's dimension, where `start`
/// is the total of the items before it.
/// - `Bias::Right` lands on the first item whose end is past the target.
///   At a boundary, that's the item starting there. Empty items at the
///   target are passed over.
/// - `Bias::Left` lands on the first item whose end reaches the target. At a
///   boundary, that's the item ending there.
pub fn seek(lines: &[Line], target: Target, bias: Bias) -> usize {
    let mut end = 0u64;
    for (i, line) in lines.iter().enumerate() {
        end += target.measure(&Lines::of(line));
        let lands = match bias {
            Bias::Right => end > target.value(),
            Bias::Left => end >= target.value(),
        };
        if lands {
            return i;
        }
    }
    lines.len()
}

/// The summary of everything before where a seek lands.
pub fn prefix(lines: &[Line], target: Target, bias: Bias) -> Lines {
    summarize(&lines[..seek(lines, target, bias)])
}

/// Where a seek lands, then where a second seek lands with the same bias,
/// never behind the first: the range an edit between them covers.
pub fn range(lines: &[Line], from: Target, to: Target, bias: Bias) -> std::ops::Range<usize> {
    let a = seek(lines, from, bias);
    a..seek(lines, to, bias).max(a)
}

/// One step of a cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Next,
    Prev,
}

/// Where a cursor is, after seeking to `start` (or starting fresh, before the
/// first item, if there's none) and then after each step: the item it's on,
/// and the summary of everything before it.
///
/// Before the first item, there's no item and nothing before it. Past the
/// last, there's no item and everything is before it. A step off either end
/// lands there, and a step further the same way stays.
pub fn walk(
    lines: &[Line],
    start: Option<(Target, Bias)>,
    steps: &[Step],
) -> Vec<(Option<Line>, Lines)> {
    // `None` is before the first item; `Some(lines.len())` is past the last.
    let mut at = start.map(|(target, bias)| seek(lines, target, bias));
    let here = |at: Option<usize>| match at {
        None => (None, Lines::default()),
        Some(i) => (lines.get(i).copied(), summarize(&lines[..i])),
    };
    let mut out = vec![here(at)];
    for step in steps {
        at = match (step, at) {
            (Step::Next, None) => Some(0),
            (Step::Next, Some(i)) => Some((i + 1).min(lines.len())),
            (Step::Prev, None | Some(0)) => None,
            (Step::Prev, Some(i)) => Some(i - 1),
        };
        out.push(here(at));
    }
    out
}
