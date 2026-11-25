//! Seeking by unit: the nth paragraph, the sentences before here, the word
//! under the cursor. Each is a seek in the tree plus a scan of one chunk,
//! so O(log n) whatever the count: `500}` costs what `}` does.
//!
//! Units are numbered from 0 in order of their starts. A position "before"
//! a unit is one before its start.

use std::cmp::Ordering;
use std::ops::Range;

use mamre::SeekTarget;
use mamre::cursor::Bias;

use crate::summary::{ByteOffset, Point, TextSummary};
use crate::units::{Anchor, End, Unit, UnitState};
use crate::{Measure, Rope};

/// Which kind of boundary to look for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Start,
    End,
}

impl Kind {
    fn count(self, s: &UnitState) -> usize {
        match self {
            Kind::Start => s.starts,
            Kind::End => s.ends,
        }
    }

    fn happens(self, starts: bool, ends: bool) -> bool {
        match self {
            Kind::Start => starts,
            Kind::End => ends,
        }
    }
}

/// Seeks to where the `n`th boundary of a kind has been seen (counting
/// from 1), tracking bytes alongside.
struct Boundary {
    unit: Unit,
    kind: Kind,
    n: usize,
}

impl<M: Measure> SeekTarget<TextSummary<M>, (ByteOffset, TextSummary<M>)> for Boundary {
    fn cmp_position(&self, position: &(ByteOffset, TextSummary<M>)) -> Ordering {
        let seen = self.kind.count(&unit_state(&position.1, self.unit));
        self.n.cmp(&seen)
    }
}

fn unit_state<M: Measure>(summary: &TextSummary<M>, unit: Unit) -> UnitState {
    summary.units.unit(unit).unwrap_or_else(|| {
        panic!(
            "this rope's measure doesn't count {unit:?}; use one that does, like Prose or Markdown"
        )
    })
}

impl<M: Measure, const N: usize> Rope<M, N> {
    /// How many of `unit` the text holds.
    ///
    /// Panics, like every method here, if the rope's measure doesn't count
    /// `unit` (a `Rope<Plain>` counts none, a `Rope<Prose>` no sections).
    pub fn count(&self, unit: Unit) -> usize {
        let s = unit_state(self.summary(), unit);
        s.starts + unit.pending(s.state) as usize
    }

    /// What the text before byte `offset` adds up to.
    pub fn summary_before(&self, offset: usize) -> TextSummary<M> {
        assert!(
            offset <= self.len(),
            "byte {offset} is out of bounds for a rope of {} bytes",
            self.len()
        );
        let mut cursor = self.tree.cursor::<(ByteOffset, TextSummary<M>)>();
        cursor.seek(&ByteOffset(offset), Bias::Right);
        let (ByteOffset(start), mut before) = *cursor.start();
        if let Some(chunk) = cursor.item() {
            let k = offset - start;
            assert!(
                chunk.as_str().is_char_boundary(k),
                "byte {offset} is not a char boundary"
            );
            mamre::Summary::concat(&mut before, &TextSummary::of(&chunk.as_str()[..k]));
        }
        before
    }

    /// How many of `unit` start before byte `offset`.
    pub fn units_before(&self, offset: usize, unit: Unit) -> usize {
        let k = self.starts_before(offset, unit);
        // A unit that starts at its line's start is only noticed later on
        // the line (a paragraph at its first non-blank, a heading at the
        // space after its `#`s), so it may start before `offset` without
        // being counted yet.
        match self.unit_start(k, unit) {
            Some(start) if start < offset => k + 1,
            _ => k,
        }
    }

    /// How many of `unit`'s machine has seen start before byte `offset`:
    /// the chunks before it from the tree, then `unit`'s machine alone over
    /// the part of its chunk before it (not the whole summary, which runs
    /// every unit's machine from every state).
    fn starts_before(&self, offset: usize, unit: Unit) -> usize {
        assert!(
            offset <= self.len(),
            "byte {offset} is out of bounds for a rope of {} bytes",
            self.len()
        );
        let mut cursor = self.tree.cursor::<(ByteOffset, TextSummary<M>)>();
        cursor.seek(&ByteOffset(offset), Bias::Right);
        let (ByteOffset(start), before) = *cursor.start();
        let s = unit_state(&before, unit);
        let Some(chunk) = cursor.item() else {
            return s.starts;
        };
        let k = offset - start;
        let text = chunk.as_str();
        assert!(
            text.is_char_boundary(k),
            "byte {offset} is not a char boundary"
        );
        let step = unit.step();
        let (mut state, mut starts) = (s.state, s.starts);
        for c in text[..k].chars() {
            let (next, starts_here, _) = step(state, c);
            starts += starts_here as usize;
            state = next;
        }
        starts
    }

    /// Where unit `n` starts, if there are that many.
    pub fn unit_start(&self, n: usize, unit: Unit) -> Option<usize> {
        let at = match self.find(unit, Kind::Start, n) {
            Some(at) => at,
            None => {
                // A heading with nothing after its `#`s, at the very end.
                let s = unit_state(self.summary(), unit);
                if n == s.starts && unit.pending(s.state) {
                    self.len()
                } else {
                    return None;
                }
            }
        };
        Some(self.place(at, unit.start_anchor()))
    }

    /// Where unit `n` ends (just past it), if there are that many.
    pub fn unit_end(&self, n: usize, unit: Unit) -> Option<usize> {
        if n >= self.count(unit) {
            return None;
        }
        Some(match unit.end() {
            End::NextStart => self.unit_start(n + 1, unit).unwrap_or(self.len()),
            End::Event(anchor) => match self.find(unit, Kind::End, n) {
                Some(at) => self.place(at, anchor),
                // Still open when the text ends.
                None => self.len(),
            },
        })
    }

    /// Where unit `n` starts and ends, if there are that many.
    pub fn unit_range(&self, n: usize, unit: Unit) -> Option<Range<usize>> {
        Some(self.unit_start(n, unit)?..self.unit_end(n, unit)?)
    }

    /// The unit holding byte `offset`, if any: none between words, say.
    pub fn unit_at(&self, offset: usize, unit: Unit) -> Option<usize> {
        let k = self.units_before(offset, unit);
        if self.unit_start(k, unit) == Some(offset) {
            return Some(k);
        }
        let prev = k.checked_sub(1)?;
        (self.unit_end(prev, unit)? > offset).then_some(prev)
    }

    /// The byte offset of the `char` where boundary `n` (from 0) of `kind`
    /// is noticed: a seek to the chunk holding it, then a scan of that
    /// chunk.
    fn find(&self, unit: Unit, kind: Kind, n: usize) -> Option<usize> {
        let mut cursor = self.tree.cursor::<(ByteOffset, TextSummary<M>)>();
        // The first chunk by the end of which n + 1 have been seen.
        cursor.seek(
            &Boundary {
                unit,
                kind,
                n: n + 1,
            },
            Bias::Left,
        );
        let chunk = cursor.item()?;
        let (ByteOffset(start), before) = *cursor.start();
        let s = unit_state(&before, unit);
        let (step, mut state, mut seen) = (unit.step(), s.state, kind.count(&s));
        for (i, c) in chunk.as_str().char_indices() {
            let (next, starts, ends) = step(state, c);
            if kind.happens(starts, ends) {
                seen += 1;
                if seen == n + 1 {
                    return Some(start + i);
                }
            }
            state = next;
        }
        unreachable!("the seek landed on the chunk holding boundary {n}")
    }

    fn place(&self, at: usize, anchor: Anchor) -> usize {
        match anchor {
            Anchor::Here => at,
            Anchor::LineStart => {
                let row = self.offset_to_point(at).row;
                self.point_to_offset(Point::new(row, 0))
            }
        }
    }
}
