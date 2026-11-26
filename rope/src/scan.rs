//! Neighbour queries: the next word start, the previous sentence, the end
//! of this paragraph. They scan the text near the cursor, so they work for
//! every [`Unit`] whatever the rope's measure counts, and cost what the
//! distance does: the right tool for motions, which almost always move a
//! unit or two.
//!
//! A scan starts at a nearby point where the unit's machine state is known
//! (see `Unit::sync`), runs forward, and reads off boundaries.
//!
//! Paragraphs and sections can be far apart, so when the measure counts
//! them, they're found by number instead: a few O(log n) seeks. Words and
//! sentences are always scanned; they're close, and a scan of a few bytes
//! beats several seeks.

use std::ops::ControlFlow;

use mamre::cursor::Bias;

use crate::summary::ByteOffset;
use crate::units::{Anchor, End, Unit, joins};
use crate::{Measure, Rope};

/// Which side of a unit: where it starts, or just past where it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Edge {
    Start,
    End,
}

impl<M: Measure, const N: usize> Rope<M, N> {
    /// The first `edge` of a `unit` after byte `offset`: where `w`, `)` or
    /// `}` go, say.
    pub fn next_boundary(&self, offset: usize, unit: Unit, edge: Edge) -> Option<usize> {
        self.check_range(&(offset..offset));
        if self.by_number(unit) {
            return self.next_counted(offset, unit, edge);
        }
        if unit == Unit::Section && edge == Edge::End {
            return self.next_section_end(offset);
        }
        let (from, state) = unit.sync(self.chars_before(offset));
        let mut found = None;
        self.scan(from, state, unit, |at, e, _| {
            if e == edge && at > offset {
                found = Some(at);
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        found
    }

    /// The last `edge` of a `unit` before byte `offset`: where `b`, `(` or
    /// `{` go, say.
    pub fn prev_boundary(&self, offset: usize, unit: Unit, edge: Edge) -> Option<usize> {
        self.check_range(&(offset..offset));
        if self.by_number(unit) {
            return self.prev_counted(offset, unit, edge);
        }
        if unit == Unit::Section && edge == Edge::End {
            // Sections end where the next starts, so an end is any start
            // that has a section before it.
            let start = self.prev_boundary(offset, unit, Edge::Start)?;
            return self.prev_boundary(start, unit, Edge::Start).map(|_| start);
        }
        // Scan from a nearby sync point up to `offset`. If nothing's there,
        // step back to an earlier sync point and scan the stretch before,
        // going twice as far back each time, so a boundary far back costs
        // what the distance does, not a rescan per line.
        let mut until = offset;
        let mut reach = 64;
        // The first scan reads past `offset`: a paragraph's start, say, is
        // only noticed at its first non-blank, and placed back at its line's
        // start, which may be before `offset`. Later scans stop at the
        // stretch already read.
        let mut limit = usize::MAX;
        loop {
            let (from, state) = unit.sync(self.chars_before(until));
            let mut found = None;
            self.scan(from, state, unit, |at, e, char_at| {
                // Boundaries come in order, so the first at or past
                // `offset` means none of the rest will do.
                if at >= offset || char_at >= limit {
                    return ControlFlow::Break(());
                }
                if e == edge {
                    found = Some(at);
                }
                ControlFlow::Continue(())
            });
            if found.is_some() || from == 0 {
                return found;
            }
            // Look for a sync point well before this one: at least a `char`
            // back, or it would find this one again.
            let back = self.chars_before(from).next().map_or(0, |(i, _)| i);
            until = self.floor_char_boundary(back.saturating_sub(reach));
            reach *= 2;
            limit = from;
        }
    }

    /// Whether to find `unit` by number rather than by scanning: when it's
    /// a far-apart unit and this rope's measure counts it.
    fn by_number(&self, unit: Unit) -> bool {
        let far = matches!(unit, Unit::Paragraph | Unit::VimParagraph | Unit::Section);
        far && self.summary().units.unit(unit).is_some()
    }

    /// `next_boundary` for a counted unit: a few O(log n) seeks.
    fn next_counted(&self, offset: usize, unit: Unit, edge: Edge) -> Option<usize> {
        // Units starting before `offset`; the last of them is `k - 1`.
        let k = self.units_before(offset, unit);
        match edge {
            Edge::Start => {
                let at_offset = self.unit_start(k, unit) == Some(offset);
                self.unit_start(k + at_offset as usize, unit)
            }
            // Unit `k - 1` may still be open at `offset`; if not, unit `k`
            // starts at or after it, so ends after it.
            Edge::End => k
                .checked_sub(1)
                .and_then(|prev| self.unit_end(prev, unit))
                .filter(|&end| end > offset)
                .or_else(|| self.unit_end(k, unit)),
        }
    }

    /// `prev_boundary` for a counted unit: a few O(log n) seeks.
    fn prev_counted(&self, offset: usize, unit: Unit, edge: Edge) -> Option<usize> {
        let k = self.units_before(offset, unit);
        let last = k.checked_sub(1)?;
        match edge {
            Edge::Start => self.unit_start(last, unit),
            // Unit `last` may still be open at `offset`; the one before it
            // ends no later than `last` starts, so before `offset`.
            Edge::End => self
                .unit_end(last, unit)
                .filter(|&end| end < offset)
                .or_else(|| self.unit_end(last.checked_sub(1)?, unit)),
        }
    }

    /// The largest `char` boundary at or below byte `at`.
    fn floor_char_boundary(&self, at: usize) -> usize {
        let mut cursor = self.tree.cursor::<ByteOffset>();
        cursor.seek(&ByteOffset(at), Bias::Right);
        match cursor.item() {
            Some(chunk) => {
                let start = cursor.start().0;
                let mut k = at - start;
                while !chunk.as_str().is_char_boundary(k) {
                    k -= 1;
                }
                start + k
            }
            None => at,
        }
    }

    /// Sections have no end of their own: section `n` ends where `n + 1`
    /// starts, and the last at the end of the text.
    fn next_section_end(&self, offset: usize) -> Option<usize> {
        let unit = Unit::Section;
        let mut next = self.next_boundary(offset, unit, Edge::Start);
        // The first section's start ends nothing.
        if let Some(start) = next
            && self.prev_boundary(start, unit, Edge::Start).is_none()
        {
            next = self.next_boundary(start, unit, Edge::Start);
        }
        next.or_else(|| {
            let any = self.prev_boundary(self.len(), unit, Edge::Start).is_some();
            (any && self.len() > offset).then_some(self.len())
        })
    }

    /// Runs `unit`'s machine from `state` at byte `from` to the end of the
    /// text, calling `f(position, edge, at)` for each boundary in order,
    /// where `at` is the byte of the `char` that revealed it, until `f`
    /// breaks.
    fn scan(
        &self,
        from: usize,
        mut state: u8,
        unit: Unit,
        mut f: impl FnMut(usize, Edge, usize) -> ControlFlow<()>,
    ) {
        let step = unit.step();
        // Units placed at their line's start sync at a line start, so this
        // starts out right for them.
        let mut line_start = from;
        // Where a joiner just before the `char` being read starts. A scan
        // starts at a sync point, and none is just past a joiner.
        let mut joiner = None;
        let place = |at: usize, anchor: Anchor, line_start: usize, joiner: Option<usize>| match anchor {
            Anchor::Here => at,
            Anchor::LineStart => line_start,
            Anchor::BeforeJoiner => joiner.unwrap_or(at),
        };
        for (i, c) in self.chars_from(from) {
            let (next, starts, ends) = step(state, c);
            if ends
                && let End::Event(anchor) = unit.end()
                && f(place(i, anchor, line_start, joiner), Edge::End, i).is_break()
            {
                return;
            }
            if starts
                && f(place(i, unit.start_anchor(), line_start, joiner), Edge::Start, i).is_break()
            {
                return;
            }
            if c == '\n' {
                line_start = i + 1;
            }
            joiner = joins(c).then_some(i);
            state = next;
        }
        let len = self.len();
        if unit.pending(state) && f(line_start, Edge::Start, len).is_break() {
            return;
        }
        if let End::Event(anchor) = unit.end()
            && unit.inside(state)
        {
            let at = match anchor {
                Anchor::BeforeJoiner => joiner.unwrap_or(len),
                _ => len,
            };
            let _ = f(at, Edge::End, len);
        }
    }

    /// The `char`s from byte `pos` on, with their offsets.
    pub(crate) fn chars_from(&self, pos: usize) -> impl Iterator<Item = (usize, char)> + '_ {
        let mut cursor = self.tree.cursor::<ByteOffset>();
        cursor.seek(&ByteOffset(pos), Bias::Right);
        let mut first = true;
        std::iter::from_fn(move || {
            let chunk = cursor.item()?.as_str();
            let start = cursor.start().0;
            let skip = if first { pos - start } else { 0 };
            first = false;
            cursor.next();
            Some((start + skip, &chunk[skip..]))
        })
        .flat_map(|(start, text)| text.char_indices().map(move |(i, c)| (start + i, c)))
    }

    /// The `char`s before byte `pos`, nearest first, with their offsets.
    pub(crate) fn chars_before(&self, pos: usize) -> impl Iterator<Item = (usize, char)> + '_ {
        let mut cursor = self.tree.cursor::<ByteOffset>();
        // The chunk holding the byte just before `pos`.
        cursor.seek(&ByteOffset(pos), Bias::Left);
        let mut first = true;
        std::iter::from_fn(move || {
            if pos == 0 {
                return None;
            }
            let chunk = cursor.item()?.as_str();
            let start = cursor.start().0;
            let text = if first { &chunk[..pos - start] } else { chunk };
            first = false;
            cursor.prev();
            Some((start, text))
        })
        .flat_map(|(start, text)| text.char_indices().rev().map(move |(i, c)| (start + i, c)))
    }
}
