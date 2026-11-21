//! What a rope keeps count of beyond bytes, chars and lines: its
//! [`Measure`], chosen with its type. Measures combine as tuples, so a rope
//! pays for exactly what it counts:
//!
//! - `Rope<Plain>`: nothing more.
//! - `Rope<Prose>`: words and paragraphs.
//! - `Rope<Sections>`: markdown headings, for an outline.
//! - `Rope<Markdown>`, which is `Rope<(Prose, Sections)>`: both.
//!
//! What's counted is what can be found by number, far away, in O(log n):
//! "paragraph 4,213", "which section is this". Every [`Unit`] can be found
//! by neighbour, near the cursor, whether it's counted or not; see
//! [`Rope::next_boundary`](crate::Rope::next_boundary).

use crate::units::{Scan, Unit, UnitState};

/// Something counted over text, built from one chunk and combined in order:
/// measuring `a` and `b` and combining must equal measuring `a + b`.
pub trait Measure: Copy + PartialEq + std::fmt::Debug {
    /// The measure of no text.
    fn empty() -> Self;

    fn measure(text: &str) -> Self;

    /// This, then `next`.
    fn concat(&mut self, next: &Self);

    /// Where `unit`'s machine stands after this text, if this measure
    /// counts it. This is what lets a rope seek by `unit`.
    fn unit(&self, unit: Unit) -> Option<UnitState> {
        let _ = unit;
        None
    }
}

/// Both measures; a unit comes from whichever counts it.
impl<A: Measure, B: Measure> Measure for (A, B) {
    fn empty() -> Self {
        (A::empty(), B::empty())
    }

    fn measure(text: &str) -> Self {
        (A::measure(text), B::measure(text))
    }

    fn concat(&mut self, next: &Self) {
        self.0.concat(&next.0);
        self.1.concat(&next.1);
    }

    fn unit(&self, unit: Unit) -> Option<UnitState> {
        self.0.unit(unit).or_else(|| self.1.unit(unit))
    }
}

/// Counts nothing extra: bytes, chars and lines only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plain;

impl Measure for Plain {
    fn empty() -> Self {
        Plain
    }
    fn measure(_: &str) -> Self {
        Plain
    }
    fn concat(&mut self, _: &Self) {}
}

/// Words, for word counts, and paragraphs (the blank-line rule), which
/// anchor things like ink and comments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prose {
    word: Scan<2>,
    paragraph: Scan<4>,
}

impl Prose {
    /// Runs of non-whitespace.
    pub fn words(&self) -> usize {
        self.word.from(Unit::Word.start_state()).starts
    }

    /// Runs of non-blank lines, where a blank line holds only whitespace.
    pub fn paragraphs(&self) -> usize {
        self.paragraph.from(Unit::Paragraph.start_state()).starts
    }
}

impl Measure for Prose {
    fn empty() -> Self {
        Prose {
            word: Scan::identity(),
            paragraph: Scan::identity(),
        }
    }

    fn measure(text: &str) -> Self {
        Prose {
            word: Scan::of(text, Unit::Word),
            paragraph: Scan::of(text, Unit::Paragraph),
        }
    }

    fn concat(&mut self, next: &Self) {
        self.word = self.word.then(&next.word);
        self.paragraph = self.paragraph.then(&next.paragraph);
    }

    fn unit(&self, unit: Unit) -> Option<UnitState> {
        let start = unit.start_state();
        match unit {
            Unit::Word => Some(self.word.from(start)),
            Unit::Paragraph => Some(self.paragraph.from(start)),
            _ => None,
        }
    }
}

/// Markdown headings, each starting a section; see [`Unit::Section`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sections {
    section: Scan<3>,
}

impl Sections {
    /// Headings, each of which starts a section.
    pub fn sections(&self) -> usize {
        let s = self.section.from(Unit::Section.start_state());
        s.starts + Unit::Section.pending(s.state) as usize
    }
}

impl Measure for Sections {
    fn empty() -> Self {
        Sections {
            section: Scan::identity(),
        }
    }

    fn measure(text: &str) -> Self {
        Sections {
            section: Scan::of(text, Unit::Section),
        }
    }

    fn concat(&mut self, next: &Self) {
        self.section = self.section.then(&next.section);
    }

    fn unit(&self, unit: Unit) -> Option<UnitState> {
        (unit == Unit::Section).then(|| self.section.from(unit.start_state()))
    }
}

/// Prose and sections: the counts a markdown document's editor wants.
pub type Markdown = (Prose, Sections);

#[cfg(test)]
pub(crate) mod tests {
    use proptest::prelude::*;

    use super::*;

    fn md(text: &str) -> Markdown {
        Markdown::measure(text)
    }

    #[test]
    fn counts() {
        assert_eq!(md("").0.words(), 0);
        assert_eq!(md("  hello  world\n").0.words(), 2);
        assert_eq!(md("日本 語").0.words(), 2);
        assert_eq!(md("a\n\nb\n c\n\n\n d").0.paragraphs(), 3);
        assert_eq!(md("a\r\n\r\nb").0.paragraphs(), 2, "a lone \\r is blank");
        assert_eq!(md("# A\ntext\n## B\n#tag\n#").1.sections(), 3);
        assert_eq!(md("# A\r\n## B").1.sections(), 2);
    }

    #[test]
    fn a_unit_comes_from_whichever_half_counts_it() {
        let m = md("# A\nsome words\n");
        assert_eq!(m.unit(Unit::Word).map(|s| s.starts), Some(4));
        assert_eq!(m.unit(Unit::Section).map(|s| s.starts), Some(1));
        assert_eq!(m.unit(Unit::Sentence), None, "neither counts sentences");
    }

    pub fn text() -> impl Strategy<Value = String> {
        let pool = vec![
            'a', 'b', '_', ' ', ' ', '\n', '\n', '\t', '\r', '#', '#', '.', '!', '"', ')', 'é',
            '日', '🦀',
        ];
        prop::collection::vec(prop::sample::select(pool), 0..120)
            .prop_map(|cs| cs.into_iter().collect())
    }

    /// Cuts `text` at char boundaries near these fractions of its length.
    pub fn pieces(text: &str, cuts: &[u16]) -> Vec<std::ops::Range<usize>> {
        let mut at: Vec<usize> = cuts
            .iter()
            .map(|&c| {
                let mut i = text.len() * c as usize / u16::MAX as usize;
                while !text.is_char_boundary(i) {
                    i -= 1;
                }
                i
            })
            .collect();
        at.push(0);
        at.push(text.len());
        at.sort();
        at.windows(2).map(|w| w[0]..w[1]).collect()
    }

    proptest! {
        /// Measuring the pieces of any split and combining gives the measure
        /// of the whole: the hard part for the scans.
        #[test]
        fn measures_combine_across_any_split(text in text(), cuts in prop::collection::vec(any::<u16>(), 0..6)) {
            let mut sum = Markdown::empty();
            for range in pieces(&text, &cuts) {
                sum.concat(&md(&text[range]));
            }
            prop_assert_eq!(sum, md(&text));
        }

        /// The counts agree with the slow, one-pass ranges.
        #[test]
        fn counts_match_the_slow_ranges(text in text()) {
            let m = md(&text);
            for unit in [Unit::Word, Unit::Paragraph] {
                prop_assert_eq!(m.unit(unit).unwrap().starts, unit.ranges(&text).len(), "{:?}", unit);
            }
            prop_assert_eq!(m.1.sections(), Unit::Section.ranges(&text).len());
        }
    }
}
