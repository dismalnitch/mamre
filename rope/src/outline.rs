//! Markdown outlines: a document's headings, counted, and every position's
//! address in the outline (`[3, 2]` is chapter 3, section 2), both ways in
//! O(log n): which section is this, and where is section 3.2.
//!
//! An address works like a [`Point`](crate::Point): a level-L heading adds
//! one at level L and resets every deeper level, as a newline resets the
//! column. So what a run of text does to an address is a small effect, and
//! two effects combine without reading the text again.

use std::cmp::Ordering;
use std::fmt;

use mamre::SeekTarget;
use mamre::cursor::Bias;

use crate::summary::{ByteOffset, Point, TextSummary};
use crate::units::{Unit, UnitState, section};
use crate::{Measure, Rope};

/// Levels of heading: `#` to `######`.
const LEVELS: usize = 6;

/// A place in a document's outline: for each heading level, how many of
/// them since the last shallower one. `[3, 2]` is the second section of the
/// third chapter; `[]` is before any heading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address([u32; LEVELS]);

impl Address {
    /// The address with these counts, from the top level down; `None` for
    /// more than six levels.
    pub fn new(levels: &[u32]) -> Option<Self> {
        let mut out = [0; LEVELS];
        out.get_mut(..levels.len())?.copy_from_slice(levels);
        Some(Address(out))
    }

    /// The counts, from the top level down, without trailing zeros.
    pub fn levels(&self) -> &[u32] {
        let depth = self.0.iter().rposition(|&n| n > 0).map_or(0, |i| i + 1);
        &self.0[..depth]
    }
}

/// `3.2.1`, or `0` before any heading.
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let levels = self.levels();
        if levels.is_empty() {
            return f.write_str("0");
        }
        for (i, n) in levels.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{n}")?;
        }
        Ok(())
    }
}

/// What a run of headings does to an address: levels shallower than its
/// shallowest heading (`min`) are untouched, that level gains `counts` at
/// it, and every deeper level is set to its `counts`. Plus how many
/// headings there were in all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Headings {
    /// The shallowest level, 1 to 6; 0 for no headings.
    min: u8,
    counts: [u32; LEVELS],
    total: u32,
}

impl Headings {
    const NONE: Self = Headings {
        min: 0,
        counts: [0; LEVELS],
        total: 0,
    };

    /// One heading of `level`, or none for 0.
    fn one(level: u8) -> Self {
        if level == 0 {
            return Self::NONE;
        }
        let mut counts = [0; LEVELS];
        counts[level as usize - 1] = 1;
        Headings {
            min: level,
            counts,
            total: 1,
        }
    }

    /// These headings, then `next`'s.
    fn then(self, next: Self) -> Self {
        let total = self.total + next.total;
        if next.min == 0 {
            return Headings { total, ..self };
        }
        if self.min == 0 {
            return Headings { total, ..next };
        }
        let (a, b) = (self.min as usize, next.min as usize);
        // Below `next`'s shallowest level, `next` sets everything.
        let mut counts = next.counts;
        // Above it, `next` touches nothing: what `self` left stands.
        if b > a {
            counts[..b - 1].copy_from_slice(&self.counts[..b - 1]);
        }
        // At it, `next` adds to whatever was there.
        counts[b - 1] += self.counts[b - 1];
        Headings {
            min: a.min(b) as u8,
            counts,
            total,
        }
    }

    fn apply(self, address: Address) -> Address {
        if self.min == 0 {
            return address;
        }
        let m = self.min as usize;
        let mut out = address.0;
        out[m - 1] += self.counts[m - 1];
        out[m..].copy_from_slice(&self.counts[m..]);
        Address(out)
    }
}

/// A run of text's headings, kept so runs combine even when a heading's
/// `#`s and the space confirming it fall in different runs.
///
/// Only the first line depends on what came before (it may be the end of a
/// heading begun earlier), so that's kept for every state the run could
/// start in; after the first newline, every state is back at a line start,
/// and the rest is kept once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sections {
    /// For each start state, the level of a heading confirmed on the first
    /// line, or 0.
    first_level: [u8; section::STATES],
    /// For each start state, where the run ends if it has no newline.
    first_end: [u8; section::STATES],
    newline: bool,
    /// The headings after the first newline, and the state at the end.
    rest: Headings,
    rest_end: u8,
}

impl Sections {
    /// Headings, each of which starts a section.
    pub fn count(&self) -> usize {
        let (headings, end) = self.from(section::START);
        headings.total as usize + section::pending(end).is_some() as usize
    }

    /// The address at the end of this text, read from the start of a
    /// document, counting a heading still open at the end.
    pub fn address(&self) -> Address {
        let (headings, end) = self.from(section::START);
        let address = headings.apply(Address::default());
        match section::pending(end) {
            Some(level) => Headings::one(level).apply(address),
            None => address,
        }
    }

    /// Starting in `state`: the headings this text confirms, and the state
    /// it ends in.
    fn from(&self, state: u8) -> (Headings, u8) {
        let s = state as usize;
        let first = Headings::one(self.first_level[s]);
        if self.newline {
            (first.then(self.rest), self.rest_end)
        } else {
            (first, self.first_end[s])
        }
    }
}

impl Measure for Sections {
    fn empty() -> Self {
        let mut first_end = [0; section::STATES];
        for (state, end) in first_end.iter_mut().enumerate() {
            *end = state as u8;
        }
        Sections {
            first_level: [0; section::STATES],
            first_end,
            newline: false,
            rest: Headings::NONE,
            rest_end: section::LINE_START,
        }
    }

    fn measure(text: &str) -> Self {
        let mut out = Self::empty();
        let (first, rest) = match text.find('\n') {
            Some(i) => (&text[..=i], Some(&text[i + 1..])),
            None => (text, None),
        };
        for start in 0..section::STATES {
            let mut state = start as u8;
            for c in first.chars() {
                let (next, level) = section::heading(state, c);
                if level > 0 {
                    out.first_level[start] = level;
                }
                state = next;
            }
            out.first_end[start] = state;
        }
        if let Some(rest) = rest {
            out.newline = true;
            let mut state = section::LINE_START;
            for c in rest.chars() {
                let (next, level) = section::heading(state, c);
                out.rest = out.rest.then(Headings::one(level));
                state = next;
            }
            out.rest_end = state;
        }
        out
    }

    fn concat(&mut self, next: &Self) {
        if self.newline {
            let (headings, end) = next.from(self.rest_end);
            self.rest = self.rest.then(headings);
            self.rest_end = end;
        } else {
            // Still on the first line: each start state carries on into
            // `next`'s first line. A line confirms one heading at most.
            for start in 0..section::STATES {
                let mid = self.first_end[start] as usize;
                if self.first_level[start] == 0 {
                    self.first_level[start] = next.first_level[mid];
                }
                self.first_end[start] = next.first_end[mid];
            }
            if next.newline {
                self.newline = true;
                self.rest = next.rest;
                self.rest_end = next.rest_end;
            }
        }
    }

    fn unit(&self, unit: Unit) -> Option<UnitState> {
        (unit == Unit::Section).then(|| {
            let (headings, end) = self.from(section::START);
            UnitState {
                state: end,
                starts: headings.total as usize,
                ends: 0,
            }
        })
    }
}

fn sections<M: Measure>(summary: &TextSummary<M>) -> &Sections {
    summary.units.find::<Sections>().unwrap_or_else(|| {
        panic!("this rope's measure doesn't keep sections; use one that does, like Sections or Markdown")
    })
}

/// Seeks to where the outline address first reaches `0`, tracking bytes.
struct Reaches(Address);

impl<M: Measure> SeekTarget<TextSummary<M>, (ByteOffset, TextSummary<M>)> for Reaches {
    fn cmp_position(&self, position: &(ByteOffset, TextSummary<M>)) -> Ordering {
        let (headings, _) = sections(&position.1).from(section::START);
        self.0.cmp(&headings.apply(Address::default()))
    }
}

impl<M: Measure, const N: usize> Rope<M, N> {
    /// The address of the section holding byte `offset`: the headings on
    /// the lines up to and including its own.
    ///
    /// Panics, like `heading_at`, if the rope's measure doesn't keep
    /// [`Sections`].
    pub fn address_at(&self, offset: usize) -> Address {
        let line = self.line_start_of(offset);
        let (headings, _) = sections(&self.summary_before(line)).from(section::START);
        let address = headings.apply(Address::default());
        // This line's own heading, if it is one, wherever `offset` is on it.
        let mut state = section::LINE_START;
        for (_, c) in self.chars_from(line) {
            let (next, level) = section::heading(state, c);
            if level > 0 {
                return Headings::one(level).apply(address);
            }
            if c == '\n' {
                return address;
            }
            state = next;
        }
        match section::pending(state) {
            Some(level) => Headings::one(level).apply(address),
            None => address,
        }
    }

    /// Where the heading at `address` starts, if there is one: where "go to
    /// 3.2" goes.
    pub fn heading_at(&self, address: Address) -> Option<usize> {
        if address == Address::default() {
            return None;
        }
        let mut cursor = self.tree.cursor::<(ByteOffset, TextSummary<M>)>();
        // The first chunk by the end of which the address reaches `address`.
        cursor.seek(&Reaches(address), Bias::Left);
        let Some(chunk) = cursor.item() else {
            // Not reached by any confirmed heading; maybe by one still open
            // at the very end.
            let found = sections(self.summary()).address() == address
                && section::pending(sections(self.summary()).from(section::START).1).is_some();
            return found.then(|| self.line_start_of(self.len()));
        };
        let (ByteOffset(start), before) = *cursor.start();
        let (headings, mut state) = sections(&before).from(section::START);
        let mut here = headings.apply(Address::default());
        for (i, c) in chunk.as_str().char_indices() {
            let (next, level) = section::heading(state, c);
            if level > 0 {
                here = Headings::one(level).apply(here);
                if here >= address {
                    return (here == address).then(|| self.line_start_of(start + i));
                }
            }
            state = next;
        }
        unreachable!("the seek landed on the chunk reaching {address}")
    }

    fn line_start_of(&self, offset: usize) -> usize {
        let row = self.offset_to_point(offset).row;
        self.point_to_offset(Point::new(row, 0))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn address(text: &str) -> Address {
        Sections::measure(text).address()
    }

    fn at(levels: &[u32]) -> Address {
        Address::new(levels).unwrap()
    }

    #[test]
    fn addresses() {
        assert_eq!(address(""), at(&[]));
        assert_eq!(address("# A\n## B\n## C\n"), at(&[1, 2]));
        assert_eq!(
            address("# A\n## B\n# D\n"),
            at(&[2]),
            "a chapter resets sections"
        );
        assert_eq!(
            address("# A\n### C\n"),
            at(&[1, 0, 1]),
            "levels can be skipped"
        );
        assert_eq!(address("## Before\n# A\n"), at(&[1]));
        assert_eq!(
            address("# A\n##"),
            at(&[1, 1]),
            "a heading open at the end counts"
        );
        assert_eq!(address("####### Seven\n"), at(&[]));
        assert_eq!(at(&[3, 2, 1]).to_string(), "3.2.1");
        assert_eq!(at(&[]).to_string(), "0");
        assert_eq!(Address::new(&[1; 7]), None);
    }

    /// Headings with several levels, split anywhere, including between a
    /// heading's `#`s and its space.
    fn text() -> impl Strategy<Value = String> {
        let line = prop_oneof![
            3 => (1usize..=7, "[ a-z]{0,6}").prop_map(|(n, rest)| format!("{}{rest}", "#".repeat(n))),
            2 => "[ a-z#]{0,8}",
        ];
        prop::collection::vec(line, 0..30).prop_map(|lines| lines.join("\n"))
    }

    proptest! {
        /// Measuring the pieces of any split and combining gives the measure
        /// of the whole.
        #[test]
        fn sections_combine_across_any_split(
            text in text(),
            cuts in prop::collection::vec(any::<u16>(), 0..8),
        ) {
            let mut sum = Sections::empty();
            for range in crate::measure::tests::pieces(&text, &cuts) {
                sum.concat(&Sections::measure(&text[range]));
            }
            prop_assert_eq!(sum, Sections::measure(&text));
        }

        /// The count matches the slow ranges.
        #[test]
        fn counts_match_the_slow_ranges(text in text()) {
            prop_assert_eq!(Sections::measure(&text).count(), Unit::Section.ranges(&text).len());
        }
    }
}
