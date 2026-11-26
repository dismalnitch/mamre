//! Counting and seeking by unit (#31): words, sentences, paragraphs, for
//! the whole text, any range, and each section of the outline, in byte
//! offsets. Each is a seek in the rope's tree, O(log n) whatever the count,
//! so they're the base for vim motions with counts (`3)`, `500}`).
//!
//! The text's measure must count the unit: a `Text<Units>` counts every
//! [`Unit`] but markdown's sections. Asking a text for a unit its measure
//! doesn't count panics, as asking its rope does.
//!
//! Units are numbered from 0 in order of their starts. "Units in a range"
//! are those that start in it, so a word straddling a section's start is
//! counted where it starts.

use std::ops::Range;

use mamre_rope::{Measure, Unit};

use crate::{SectionId, Text, TextError};

impl<M: Measure> Text<M> {
    /// How many of `unit` the text holds.
    pub fn count(&self, unit: Unit) -> usize {
        self.rope.count(unit)
    }

    /// How many of `unit` start in `range`.
    pub fn count_in(&self, range: Range<usize>, unit: Unit) -> Result<usize, TextError> {
        self.check_range(&range)?;
        Ok(self.starts_in(range, unit))
    }

    fn starts_in(&self, range: Range<usize>, unit: Unit) -> usize {
        let before = |at: usize| {
            if at == self.len() {
                self.count(unit)
            } else {
                self.rope.units_before(at, unit)
            }
        };
        before(range.end) - before(range.start)
    }

    /// How many of `unit` start in section `id` (its subsections too): a
    /// chapter's word count, say. `None` if there's no such section now.
    pub fn section_count(&self, id: SectionId, unit: Unit) -> Option<usize> {
        let range = self.section_range(id)?;
        Some(self.starts_in(range, unit))
    }

    /// How many of `unit` start before byte `at`: the number of the unit
    /// at or after it.
    pub fn units_before(&self, at: usize, unit: Unit) -> Result<usize, TextError> {
        self.check_at(at)?;
        Ok(if at == self.len() {
            self.count(unit)
        } else {
            self.rope.units_before(at, unit)
        })
    }

    /// Where unit `n` starts, if there are that many.
    pub fn unit_start(&self, n: usize, unit: Unit) -> Option<usize> {
        self.rope.unit_start(n, unit)
    }

    /// Where unit `n` ends (just past it), if there are that many.
    pub fn unit_end(&self, n: usize, unit: Unit) -> Option<usize> {
        self.rope.unit_end(n, unit)
    }

    /// Where unit `n` starts and ends, if there are that many.
    pub fn unit_range(&self, n: usize, unit: Unit) -> Option<Range<usize>> {
        self.rope.unit_range(n, unit)
    }

    /// The unit holding byte `at`, if any: none between words, say.
    pub fn unit_at(&self, at: usize, unit: Unit) -> Result<Option<usize>, TextError> {
        self.check_at(at)?;
        Ok(if at == self.len() {
            None
        } else {
            self.rope.unit_at(at, unit)
        })
    }

    /// Where the unit holding byte `at` starts: "the start of this
    /// paragraph". `None` between units.
    pub fn start_of(&self, at: usize, unit: Unit) -> Result<Option<usize>, TextError> {
        Ok(self
            .unit_at(at, unit)?
            .and_then(|n| self.unit_start(n, unit)))
    }

    /// Where the `n`th unit starting after byte `at` starts (`n` from 1):
    /// "the 3rd sentence after here" is `next_start(at, Sentence, 3)`.
    /// `None` if there aren't that many.
    pub fn next_start(&self, at: usize, unit: Unit, n: usize) -> Result<Option<usize>, TextError> {
        let mut k = self.units_before(at, unit)?;
        if self.unit_start(k, unit) == Some(at) {
            k += 1;
        }
        Ok(n.checked_sub(1).and_then(|n| self.unit_start(k + n, unit)))
    }

    /// Where the `n`th unit starting before byte `at` starts (`n` from 1).
    /// `None` if there aren't that many.
    pub fn prev_start(&self, at: usize, unit: Unit, n: usize) -> Result<Option<usize>, TextError> {
        let k = self.units_before(at, unit)?;
        Ok(k.checked_sub(n).and_then(|i| self.unit_start(i, unit)))
    }

    /// Each section the text shows, in order, and how many of `unit`
    /// start in it: every chapter's or scene's word count.
    ///
    /// One pass over the outline: each section runs to the next at its
    /// level or above, found from the end with a stack.
    pub fn section_counts(&self, unit: Unit) -> Vec<(SectionId, usize)> {
        let sections = self.sections();
        let mut ends = vec![self.len(); sections.len()];
        let mut after: Vec<(u8, usize)> = Vec::new();
        for (k, s) in sections.iter().enumerate().rev() {
            while after.last().is_some_and(|&(level, _)| level > s.level) {
                after.pop();
            }
            if let Some(&(_, at)) = after.last() {
                ends[k] = at;
            }
            after.push((s.level, s.at));
        }
        sections
            .iter()
            .zip(ends)
            .map(|(s, end)| (s.id, self.starts_in(s.at..end, unit)))
            .collect()
    }
}
