//! What a rope counts beyond bytes, chars and lines: a [`Measure`], chosen
//! with the rope's type, so `Rope<Plain>`, `Rope<Prose>` and
//! `Rope<Markdown>` each say what they keep track of.

use mamre::Summary;

use crate::units::{Scan, Unit, UnitState};

/// Something counted over text, built from one chunk and combined like any
/// summary: measuring `a` and `b` and combining must equal measuring `a + b`.
pub trait Measure: Summary + Copy + PartialEq + std::fmt::Debug {
    fn measure(text: &str) -> Self;

    /// Where `unit`'s machine stands after this text, if this measure
    /// counts it. This is what lets a rope seek by `unit`.
    fn unit(&self, unit: Unit) -> Option<UnitState> {
        let _ = unit;
        None
    }
}

/// Counts nothing extra: bytes, chars and lines only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plain;

impl Summary for Plain {
    fn empty() -> Self {
        Plain
    }
    fn concat(&mut self, _: &Self) {}
}

impl Measure for Plain {
    fn measure(_: &str) -> Self {
        Plain
    }
}

/// Prose units, in every variant: words and vim words, sentences, and
/// paragraphs with either rule for blank lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prose {
    word: Scan<2>,
    vim_word: Scan<3>,
    sentence: Scan<4>,
    paragraph: Scan<4>,
    vim_paragraph: Scan<4>,
}

impl Prose {
    /// Runs of non-whitespace.
    pub fn words(&self) -> usize {
        self.count(Unit::Word)
    }

    /// Runs of non-blank lines, where a blank line holds only whitespace.
    pub fn paragraphs(&self) -> usize {
        self.count(Unit::Paragraph)
    }

    fn count(&self, unit: Unit) -> usize {
        self.unit(unit).map_or(0, |s| s.starts)
    }
}

impl Summary for Prose {
    fn empty() -> Self {
        Prose {
            word: Scan::identity(),
            vim_word: Scan::identity(),
            sentence: Scan::identity(),
            paragraph: Scan::identity(),
            vim_paragraph: Scan::identity(),
        }
    }
    fn concat(&mut self, next: &Self) {
        self.word = self.word.then(&next.word);
        self.vim_word = self.vim_word.then(&next.vim_word);
        self.sentence = self.sentence.then(&next.sentence);
        self.paragraph = self.paragraph.then(&next.paragraph);
        self.vim_paragraph = self.vim_paragraph.then(&next.vim_paragraph);
    }
}

impl Measure for Prose {
    fn measure(text: &str) -> Self {
        Prose {
            word: Scan::of(text, Unit::Word),
            vim_word: Scan::of(text, Unit::VimWord),
            sentence: Scan::of(text, Unit::Sentence),
            paragraph: Scan::of(text, Unit::Paragraph),
            vim_paragraph: Scan::of(text, Unit::VimParagraph),
        }
    }

    fn unit(&self, unit: Unit) -> Option<UnitState> {
        let start = unit.start_state();
        Some(match unit {
            Unit::Word => self.word.from(start),
            Unit::VimWord => self.vim_word.from(start),
            Unit::Sentence => self.sentence.from(start),
            Unit::Paragraph => self.paragraph.from(start),
            Unit::VimParagraph => self.vim_paragraph.from(start),
            Unit::Section => return None,
        })
    }
}

/// Prose, plus markdown sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Markdown {
    prose: Prose,
    section: Scan<3>,
}

impl Markdown {
    /// Runs of non-whitespace.
    pub fn words(&self) -> usize {
        self.prose.words()
    }

    /// Runs of non-blank lines.
    pub fn paragraphs(&self) -> usize {
        self.prose.paragraphs()
    }

    /// Headings, each of which starts a section; see [`Unit::Section`].
    pub fn sections(&self) -> usize {
        let s = self.section.from(Unit::Section.start_state());
        s.starts + Unit::Section.pending(s.state) as usize
    }
}

impl Summary for Markdown {
    fn empty() -> Self {
        Markdown {
            prose: Prose::empty(),
            section: Scan::identity(),
        }
    }
    fn concat(&mut self, next: &Self) {
        self.prose.concat(&next.prose);
        self.section = self.section.then(&next.section);
    }
}

impl Measure for Markdown {
    fn measure(text: &str) -> Self {
        Markdown {
            prose: Prose::measure(text),
            section: Scan::of(text, Unit::Section),
        }
    }

    fn unit(&self, unit: Unit) -> Option<UnitState> {
        match unit {
            Unit::Section => Some(self.section.from(unit.start_state())),
            _ => self.prose.unit(unit),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use proptest::prelude::*;

    use super::*;

    fn md(text: &str) -> Markdown {
        Markdown::measure(text)
    }

    #[test]
    fn counts() {
        assert_eq!(md("").words(), 0);
        assert_eq!(md("  hello  world\n").words(), 2);
        assert_eq!(md("日本 語").words(), 2);
        assert_eq!(md("a\n\nb\n c\n\n\n d").paragraphs(), 3);
        assert_eq!(md("a\r\n\r\nb").paragraphs(), 2, "a lone \\r is blank");
        assert_eq!(md("# A\ntext\n## B\n#tag\n#").sections(), 3);
        assert_eq!(md("# A\r\n## B").sections(), 2);
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

        /// The summary's counts agree with the slow, one-pass ranges.
        #[test]
        fn counts_match_the_slow_ranges(text in text()) {
            let m = md(&text);
            for unit in [Unit::Word, Unit::VimWord, Unit::Sentence, Unit::Paragraph, Unit::VimParagraph] {
                prop_assert_eq!(m.unit(unit).unwrap().starts, unit.ranges(&text).len(), "{:?}", unit);
            }
            prop_assert_eq!(m.sections(), Unit::Section.ranges(&text).len());
        }
    }
}
