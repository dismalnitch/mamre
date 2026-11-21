//! What a rope counts beyond bytes, chars and lines: a [`Measure`], chosen
//! with the rope's type, so `Rope<Plain>`, `Rope<Prose>` and
//! `Rope<Markdown>` each say what they keep track of.
//!
//! Counts of things that can straddle a chunk edge (a word split across two
//! chunks is still one word) are kept as a [`Scan`]: a tiny state machine's
//! results for every state it could start in, which compose.

use mamre::Summary;

/// Something counted over text, built from one chunk and combined like any
/// summary: measuring `a` and `b` and combining must equal measuring `a + b`.
pub trait Measure: Summary + Copy + PartialEq + std::fmt::Debug {
    fn measure(text: &str) -> Self;
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

/// Words and paragraphs, for any prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prose {
    words: Scan<2>,
    paragraphs: Scan<4>,
}

impl Prose {
    /// Runs of non-whitespace.
    pub fn words(&self) -> usize {
        self.words.count_from(words::START)
    }

    /// Runs of non-blank lines, where a blank line holds only whitespace.
    pub fn paragraphs(&self) -> usize {
        self.paragraphs.count_from(paragraphs::START)
    }
}

impl Summary for Prose {
    fn empty() -> Self {
        Prose {
            words: Scan::identity(),
            paragraphs: Scan::identity(),
        }
    }
    fn concat(&mut self, next: &Self) {
        self.words = self.words.then(&next.words);
        self.paragraphs = self.paragraphs.then(&next.paragraphs);
    }
}

impl Measure for Prose {
    fn measure(text: &str) -> Self {
        Prose {
            words: Scan::run(text, words::step),
            paragraphs: Scan::run(text, paragraphs::step),
        }
    }
}

/// Prose, plus markdown sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Markdown {
    prose: Prose,
    sections: Scan<3>,
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

    /// Headings, each of which starts a section. See [`sections`] for what
    /// counts as one.
    pub fn sections(&self) -> usize {
        let open = self.sections.end_from(sections::START) == sections::HASHES;
        self.sections.count_from(sections::START) + open as usize
    }
}

impl Summary for Markdown {
    fn empty() -> Self {
        Markdown {
            prose: Prose::empty(),
            sections: Scan::identity(),
        }
    }
    fn concat(&mut self, next: &Self) {
        self.prose.concat(&next.prose);
        self.sections = self.sections.then(&next.sections);
    }
}

impl Measure for Markdown {
    fn measure(text: &str) -> Self {
        Markdown {
            prose: Prose::measure(text),
            sections: Scan::run(text, sections::step),
        }
    }
}

/// A state machine with `S` states, run over some text, kept as a table:
/// for each state it could start in, the state it ends in and how many times
/// it counted. The tables for `a` and for `b` give the table for `a` then
/// `b` without looking at the text again, which is what makes it a summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scan<const S: usize> {
    end: [u8; S],
    count: [usize; S],
}

/// One step of a machine: the next state, and whether this step counts.
pub type Step = fn(u8, char) -> (u8, bool);

impl<const S: usize> Scan<S> {
    /// The table for no text: every state stays put, nothing counted.
    pub fn identity() -> Self {
        let mut end = [0; S];
        for (state, e) in end.iter_mut().enumerate() {
            *e = state as u8;
        }
        Scan { end, count: [0; S] }
    }

    /// Runs `step` over `text` from every state.
    pub fn run(text: &str, step: Step) -> Self {
        let mut out = Self::identity();
        for start in 0..S {
            let mut state = start as u8;
            for c in text.chars() {
                let (next, counts) = step(state, c);
                out.count[start] += counts as usize;
                state = next;
            }
            out.end[start] = state;
        }
        out
    }

    /// This, then `next`.
    pub fn then(&self, next: &Self) -> Self {
        let mut out = *self;
        for start in 0..S {
            let mid = self.end[start] as usize;
            out.end[start] = next.end[mid];
            out.count[start] = self.count[start] + next.count[mid];
        }
        out
    }

    /// How many times it counted, starting in `state`.
    pub fn count_from(&self, state: u8) -> usize {
        self.count[state as usize]
    }

    /// The state it ends in, starting in `state`.
    pub fn end_from(&self, state: u8) -> u8 {
        self.end[state as usize]
    }
}

/// Words: maximal runs of non-whitespace. Counts on entering one.
mod words {
    pub const START: u8 = GAP;
    const GAP: u8 = 0;
    const IN_WORD: u8 = 1;

    pub fn step(state: u8, c: char) -> (u8, bool) {
        if c.is_whitespace() {
            (GAP, false)
        } else {
            (IN_WORD, state == GAP)
        }
    }
}

/// Paragraphs: maximal runs of non-blank lines, where a blank line holds
/// only whitespace (so a lone `\r` is blank too). Counts on the first
/// non-blank character of a line that follows a blank one, or the start.
///
/// The state is two bits: whether the previous line was blank, and whether
/// the current line is blank so far.
mod paragraphs {
    const PREV_BLANK: u8 = 0b10;
    const CUR_BLANK: u8 = 0b01;
    /// The start of the text counts as following a blank line.
    pub const START: u8 = PREV_BLANK | CUR_BLANK;

    pub fn step(state: u8, c: char) -> (u8, bool) {
        if c == '\n' {
            let prev = if state & CUR_BLANK != 0 {
                PREV_BLANK
            } else {
                0
            };
            (prev | CUR_BLANK, false)
        } else if c.is_whitespace() {
            (state, false)
        } else {
            let starts = state & CUR_BLANK != 0 && state & PREV_BLANK != 0;
            (state & PREV_BLANK, starts)
        }
    }
}

/// Markdown sections: ATX headings, simplified to a line that starts with
/// one or more `#` followed by a space, a tab or the end of the line. No
/// leading indentation, and no limit of six. Counts when the heading is
/// confirmed; one still open at the end of the text is added by
/// [`Markdown::sections`].
pub mod sections {
    pub const START: u8 = LINE_START;
    const LINE_START: u8 = 0;
    pub const HASHES: u8 = 1;
    const OTHER: u8 = 2;

    pub fn step(state: u8, c: char) -> (u8, bool) {
        match (state, c) {
            (_, '\n') => (LINE_START, state == HASHES),
            (LINE_START | HASHES, '#') => (HASHES, false),
            // A `\r` before the `\n` of a heading line changes nothing.
            (HASHES, '\r') => (HASHES, false),
            (HASHES, ' ' | '\t') => (OTHER, true),
            _ => (OTHER, false),
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
    fn words() {
        assert_eq!(md("").words(), 0);
        assert_eq!(md("   ").words(), 0);
        assert_eq!(md("hello").words(), 1);
        assert_eq!(md("  hello  world\n").words(), 2);
        assert_eq!(md("a\tb\nc").words(), 3);
        assert_eq!(md("日本 語").words(), 2);
    }

    #[test]
    fn paragraphs() {
        assert_eq!(md("").paragraphs(), 0);
        assert_eq!(md("\n\n").paragraphs(), 0);
        assert_eq!(md("one line").paragraphs(), 1);
        assert_eq!(md("a\nb\nc").paragraphs(), 1, "no blank line between");
        assert_eq!(md("a\n\nb\n c\n\n\n d").paragraphs(), 3);
        assert_eq!(md("a\n  \t\nb").paragraphs(), 2, "whitespace-only is blank");
        assert_eq!(md("a\r\n\r\nb").paragraphs(), 2, "so is a lone \\r");
    }

    #[test]
    fn sections() {
        assert_eq!(md("").sections(), 0);
        assert_eq!(md("# A\ntext\n## B\n#tag\n#").sections(), 3);
        assert_eq!(md("#\n").sections(), 1, "an empty heading");
        assert_eq!(md("a # b\n").sections(), 0, "not at a line start");
        assert_eq!(md(" # indented").sections(), 0);
        assert_eq!(md("# A\r\n## B").sections(), 2);
    }

    pub fn text() -> impl Strategy<Value = String> {
        let pool = vec![
            'a', 'b', ' ', ' ', '\n', '\n', '\t', '\r', '#', '#', 'é', '日', '🦀',
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
    }
}
