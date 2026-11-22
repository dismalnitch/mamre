//! Moving text: a stretch cut out and put back elsewhere, whole sections of
//! a document among them, in O(log n) however big they are.
//!
//! The moved text keeps its chunks: it's a slice of the rope (sharing them,
//! not copying), joined back in at its new place, and the tree rebalances
//! as any join does. Only the chunks at the seams change. What moved, and
//! where to, comes back as [`Moved`]: the signal an editor (an outline
//! view, say) needs, and a way to carry any other position (a caret, a
//! selection) across the move.

use std::ops::Range;

use crate::outline::Address;
use crate::{Measure, Rope};

/// Why a section couldn't move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveError {
    /// There's no section at that address.
    NoSection,
    /// Where it was to go isn't the start of a line.
    NotALineStart,
    /// It would run two lines together: the text doesn't end with a
    /// newline, and the move needs one.
    WouldJoinLines,
}

/// What a move did: the text that was at `from` is now at `to`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    pub from: Range<usize>,
    pub to: Range<usize>,
}

impl Moved {
    /// Where byte `offset`, from before the move, is after it. Moved text
    /// goes with it; text between where it was and where it went closes
    /// up or makes room; whatever was where it went ends up just after it,
    /// as if the moved text were typed there.
    pub fn map(&self, offset: usize) -> usize {
        let Range { start: a, end: b } = self.from.clone();
        let n = b - a;
        if (a..b).contains(&offset) {
            return offset - a + self.to.start;
        }
        if self.to.start <= a {
            // Moved earlier, to `t`: what's from there to where it was
            // makes room.
            let t = self.to.start;
            if (t..a).contains(&offset) {
                offset + n
            } else {
                offset
            }
        } else {
            // Moved later: it went in before what was at `to.end` (in the
            // text before), and what's from where it was to there closes up.
            let t = self.to.end;
            if (b..t).contains(&offset) {
                offset - n
            } else {
                offset
            }
        }
    }
}

impl<M: Measure, const N: usize> Rope<M, N> {
    /// Moves the text in `range` to byte `to` (an offset in the text as it
    /// is now, outside the range, or at either end of it). Returns what
    /// moved where.
    ///
    /// Panics if `range` or `to` is out of bounds or not on `char`
    /// boundaries, or if `to` is strictly inside `range`.
    pub fn move_range(&mut self, range: Range<usize>, to: usize) -> Moved {
        self.check_range(&range);
        self.check_range(&(to..to));
        assert!(
            to <= range.start || to >= range.end,
            "can't move {range:?} into itself, to {to}"
        );
        let n = range.len();
        let len = self.len();
        let moved = self.slice(range.clone());
        let (out, landed) = if to <= range.start {
            let mut out = self.slice(0..to);
            out.append(moved);
            out.append(self.slice(to..range.start));
            out.append(self.slice(range.end..len));
            (out, to..to + n)
        } else {
            let mut out = self.slice(0..range.start);
            out.append(self.slice(range.end..to));
            out.append(moved);
            out.append(self.slice(to..len));
            (out, to - n..to)
        };
        *self = out;
        Moved {
            from: range,
            to: landed,
        }
    }

    /// Moves the section at `address` (its heading and everything under it)
    /// to byte `to`: the start of another section's heading, or the end.
    ///
    /// Headings keep their level: moving `3.2` to the top level of the
    /// outline means changing its `#`s too, which is an edit of its own.
    ///
    /// Moving whole lines can't run two together, so the last section of a
    /// text that doesn't end with a newline can't move, and nothing can
    /// move to the end of one: that needs a newline added, an edit too.
    ///
    /// Panics, like [`move_range`](Self::move_range), if `to` is inside the
    /// section; and, like [`heading_at`](Self::heading_at), if the rope's
    /// measure doesn't keep [`Sections`](crate::Sections).
    pub fn move_section(&mut self, address: Address, to: usize) -> Result<Moved, MoveError> {
        let range = self.section_range(address).ok_or(MoveError::NoSection)?;
        // A `\n` byte is only ever a newline, never part of another char.
        let ends_line = |at: usize| {
            at == 0
                || self
                    .chunk_at(at - 1)
                    .is_some_and(|(start, chunk)| chunk.as_bytes()[at - 1 - start] == b'\n')
        };
        // To its own start or end, nothing moves.
        if to == range.start || to == range.end {
            return Ok(self.move_range(range, to));
        }
        if !ends_line(range.end) || to == self.len() && !ends_line(to) {
            return Err(MoveError::WouldJoinLines);
        }
        if !ends_line(to) {
            return Err(MoveError::NotALineStart);
        }
        Ok(self.move_range(range, to))
    }
}
