//! Moving text: a stretch cut out and put back elsewhere, whole sections of
//! a document among them, in O(log n) however big they are.
//!
//! The moved text keeps its chunks: it's a slice of the rope (sharing them,
//! not copying), joined back in at its new place, and the tree rebalances
//! as any join does. Only the chunks at the seams change. What moved, and
//! where to, comes back as [`Moved`]: the signal an editor (an outline
//! view, say) needs, and a way to carry any other position (a caret, a
//! selection) across the move.
//!
//! A document is lines, and its last line ends with a newline whether the
//! text has one or not, as in Vim: the final newline is implied, not
//! stored. So moving a whole section never runs two lines together. When
//! the text doesn't end with a newline and a move involves its last line,
//! the newline that kept two lines apart moves with them: the moved block
//! *turns*, its newline going from one end of it to the other.

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
}

/// What a move did: the text that was at `from` is now at `to`, turned as
/// `newline` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    pub from: Range<usize>,
    pub to: Range<usize>,
    pub newline: Newline,
}

/// Where a move put the newline at one end of the moved text. Only a
/// section move in a text that doesn't end with a newline turns: the
/// newline that separated its last line from the next goes to the other
/// end, so lines stay whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Newline {
    /// The text moved as it was.
    Stays,
    /// The moved text began with a newline, and now ends with it: the last
    /// section, moved earlier, takes the newline before it along to end it.
    ToEnd,
    /// The moved text ended with a newline, and now begins with it: a
    /// section moved to the end goes after the text's last line, its
    /// newline now ending that line.
    ToStart,
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
            let k = offset - a;
            let k = match self.newline {
                Newline::Stays => k,
                Newline::ToEnd if k == 0 => n - 1,
                Newline::ToEnd => k - 1,
                Newline::ToStart if k == n - 1 => 0,
                Newline::ToStart => k + 1,
            };
            return k + self.to.start;
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
            newline: Newline::Stays,
        }
    }

    /// Moves the section at `address` (its heading and everything under it)
    /// to byte `to`: the start of another section's heading, or the end.
    ///
    /// Headings keep their level: moving `3.2` to the top level of the
    /// outline means changing its `#`s too, which is an edit of its own.
    ///
    /// Whole lines move, the last one with its implied newline, so lines
    /// never run together. In a text that doesn't end with a newline, the
    /// last section moved earlier takes the newline before it along, and a
    /// section moved to the end puts its newline before itself: see
    /// [`Newline`].
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
        let len = self.len();
        if to != len && !ends_line(to) {
            return Err(MoveError::NotALineStart);
        }
        if !ends_line(range.end) {
            // The last section, with no newline of its own: it goes with
            // the one before it (there is one, as it isn't at the start:
            // it would be the whole text, with nowhere else to go).
            let block = range.start - 1..len;
            Ok(self.turn(block, to, Newline::ToEnd))
        } else if to == len && !ends_line(len) {
            Ok(self.turn(range, to, Newline::ToStart))
        } else {
            Ok(self.move_range(range, to))
        }
    }

    /// Moves `block` to `to`, as [`move_range`](Self::move_range) does,
    /// and turns it: the newline at one end goes to the other.
    fn turn(&mut self, block: Range<usize>, to: usize, newline: Newline) -> Moved {
        let Range { start: a, end: b } = block.clone();
        let n = b - a;
        let len = self.len();
        let (head, tail) = match newline {
            Newline::ToEnd => (a + 1..b, a..a + 1),
            Newline::ToStart => (b - 1..b, a..b - 1),
            Newline::Stays => unreachable!(),
        };
        let (out, landed) = if to <= a {
            let mut out = self.slice(0..to);
            out.append(self.slice(head));
            out.append(self.slice(tail));
            out.append(self.slice(to..a));
            out.append(self.slice(b..len));
            (out, to..to + n)
        } else {
            let mut out = self.slice(0..a);
            out.append(self.slice(b..to));
            out.append(self.slice(head));
            out.append(self.slice(tail));
            out.append(self.slice(to..len));
            (out, to - n..to)
        };
        *self = out;
        Moved {
            from: block,
            to: landed,
            newline,
        }
    }
}
