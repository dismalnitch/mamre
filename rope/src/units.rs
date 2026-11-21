//! The units a rope can count and seek by: words, sentences, paragraphs,
//! sections, in the variants editors disagree about.
//!
//! Each unit is a tiny state machine run over the text. A step says where
//! the machine goes next, and whether a unit starts or ends at that `char`.
//! A [`Scan`] keeps the machine's results for every state it could start
//! in, so the scans of two runs of text combine into the scan of both: a
//! summary, which the tree keeps for every subtree. That makes "the 4,000th
//! paragraph" and "how many sentences before here" O(log n) seeks.
//!
//! Every variant is counted at once, and the choice between them is made
//! when asking, so settings like "does `}` stop at whitespace-only lines"
//! cost nothing to change.

use std::ops::Range;

/// Something text is made of, that a rope can count and seek by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unit {
    /// A run of non-whitespace: vim's WORD (`W`), and a prose word count.
    Word,
    /// Vim's word (`w`): a run of letters, digits and `_`, or a run of
    /// other non-blank characters. Vim also counts an empty line as a word;
    /// that's for a motion layer to add, since it's a rule about motions.
    VimWord,
    /// Vim's sentence (`(` `)`): from the first non-blank character, to a
    /// `.`, `!` or `?` (followed by any `)`, `]`, `"` or `'`) that's
    /// followed by whitespace, or to an empty line. One ended by an empty
    /// line includes the newline before it.
    Sentence,
    /// A run of non-blank lines, where a blank line holds only whitespace.
    /// From the start of its first line to the start of the blank line
    /// after it.
    Paragraph,
    /// Vim's paragraph (`{` `}`): a run of non-empty lines. A line of only
    /// whitespace is not a boundary.
    VimParagraph,
    /// A markdown section: from a heading line to the next heading. A
    /// heading is CommonMark's ATX heading, simplified: a line starting with
    /// one to six `#`, its level, followed by a space, a tab or the end of
    /// the line. No leading indentation.
    Section,
}

/// Where a unit's machine stands after some text: its state, and how many
/// units have started and ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnitState {
    pub state: u8,
    pub starts: usize,
    pub ends: usize,
}

/// One step of a machine: the next state, whether a unit starts at this
/// `char`, and whether one ends here (so this `char` is just past it).
pub type Step = fn(u8, char) -> (u8, bool, bool);

/// Where a unit's boundary lands, given the `char` its machine noticed it
/// at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Anchor {
    /// Right there.
    Here,
    /// At the start of that line: a paragraph is only known on its first
    /// non-blank character, and a heading on the space after its `#`s.
    LineStart,
}

/// How a unit's end is found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// Its machine notices it.
    Event(Anchor),
    /// It ends where the next one starts, or at the end of the text.
    NextStart,
}

impl Unit {
    pub(crate) fn step(self) -> Step {
        match self {
            Unit::Word => word::step,
            Unit::VimWord => vim_word::step,
            Unit::Sentence => sentence::step,
            Unit::Paragraph => paragraph::step,
            Unit::VimParagraph => vim_paragraph::step,
            Unit::Section => section::step,
        }
    }

    /// The state at the start of the text.
    pub(crate) fn start_state(self) -> u8 {
        match self {
            Unit::Word => word::START,
            Unit::VimWord => vim_word::START,
            Unit::Sentence => sentence::START,
            Unit::Paragraph => paragraph::START,
            Unit::VimParagraph => vim_paragraph::START,
            Unit::Section => section::START,
        }
    }

    pub(crate) fn start_anchor(self) -> Anchor {
        match self {
            Unit::Paragraph | Unit::VimParagraph | Unit::Section => Anchor::LineStart,
            Unit::Word | Unit::VimWord | Unit::Sentence => Anchor::Here,
        }
    }

    pub(crate) fn end(self) -> End {
        match self {
            Unit::Word | Unit::VimWord | Unit::Sentence => End::Event(Anchor::Here),
            Unit::Paragraph | Unit::VimParagraph => End::Event(Anchor::LineStart),
            Unit::Section => End::NextStart,
        }
    }

    /// Whether a unit is still waiting to be recognized when the text ends
    /// in this state: a heading line with nothing after its `#`s yet.
    pub(crate) fn pending(self, state: u8) -> bool {
        self == Unit::Section && section::pending(state).is_some()
    }

    /// Whether the machine is inside a unit in this state, so one is open
    /// if the text ends here. (Sections end at the next start instead.)
    pub(crate) fn inside(self, state: u8) -> bool {
        match self {
            Unit::Word | Unit::VimWord | Unit::Sentence => state != self.start_state(),
            Unit::Paragraph | Unit::VimParagraph => state != self.start_state(),
            Unit::Section => true,
        }
    }

    /// A point at or before `pos` where this unit's machine state is known
    /// without reading further back, and that state. `before` gives the
    /// `char`s before `pos`, nearest first.
    ///
    /// - Words: just after the previous whitespace.
    /// - Sentences: just after the previous `.`, `!` or `?` (and closers)
    ///   followed by whitespace, or the previous empty line.
    /// - Paragraphs and sections: the start of the line, with the line
    ///   above it read to see if it's blank.
    ///
    /// Each falls back to the start of the text.
    pub(crate) fn sync(self, mut before: impl Iterator<Item = (usize, char)>) -> (usize, u8) {
        let start = (0, self.start_state());
        match self {
            Unit::Word | Unit::VimWord => before
                .find(|&(_, c)| c.is_whitespace())
                .map_or(start, |(i, c)| (i + c.len_utf8(), self.start_state())),
            Unit::Sentence => sentence::sync(before).unwrap_or(start),
            Unit::Paragraph | Unit::VimParagraph | Unit::Section => {
                let Some((newline, _)) = before.find(|&(_, c)| c == '\n') else {
                    return start;
                };
                let line_start = newline + 1;
                let state = match self {
                    Unit::Paragraph => {
                        let blank = before
                            .take_while(|&(_, c)| c != '\n')
                            .all(|(_, c)| c.is_whitespace());
                        paragraph::at_line_start(blank)
                    }
                    Unit::VimParagraph => {
                        let empty = before.next().is_none_or(|(_, c)| c == '\n');
                        vim_paragraph::at_line_start(empty)
                    }
                    _ => section::START,
                };
                (line_start, state)
            }
        }
    }

    /// Every unit of this kind in `text`, the slow way: one pass, no tree.
    /// For short strings, and as the reference the rope is tested against.
    pub fn ranges(self, text: &str) -> Vec<Range<usize>> {
        let step = self.step();
        let mut state = self.start_state();
        let (mut starts, mut ends) = (Vec::new(), Vec::new());
        for (i, c) in text.char_indices() {
            let (next, start, end) = step(state, c);
            if end {
                ends.push(i);
            }
            if start {
                starts.push(i);
            }
            state = next;
        }
        if self.pending(state) {
            starts.push(text.len());
        }

        let line_start = |at: usize| text[..at].rfind('\n').map_or(0, |i| i + 1);
        let place = |at: usize, anchor: Anchor| match anchor {
            Anchor::Here => at,
            Anchor::LineStart => line_start(at),
        };
        let starts: Vec<usize> = starts
            .into_iter()
            .map(|at| place(at, self.start_anchor()))
            .collect();
        let ends: Vec<usize> = match self.end() {
            End::NextStart => starts.iter().skip(1).copied().chain([text.len()]).collect(),
            End::Event(anchor) => {
                let mut ends: Vec<usize> = ends.into_iter().map(|at| place(at, anchor)).collect();
                // One still open at the end of the text ends there.
                ends.resize(starts.len(), text.len());
                ends
            }
        };
        starts.into_iter().zip(ends).map(|(s, e)| s..e).collect()
    }
}

/// A state machine with `S` states, run over some text, kept as a table:
/// for each state it could start in, the state it ends in and how many
/// units started and ended. The tables for `a` and for `b` give the table
/// for `a` then `b` without looking at the text again, which is what makes
/// it a summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scan<const S: usize> {
    end: [u8; S],
    starts: [u32; S],
    ends: [u32; S],
}

impl<const S: usize> Scan<S> {
    /// The table for no text: every state stays put, nothing counted.
    pub fn identity() -> Self {
        let mut end = [0; S];
        for (state, e) in end.iter_mut().enumerate() {
            *e = state as u8;
        }
        Scan {
            end,
            starts: [0; S],
            ends: [0; S],
        }
    }

    /// Runs `unit`'s machine over `text` from every state.
    pub fn of(text: &str, unit: Unit) -> Self {
        let step = unit.step();
        let mut out = Self::identity();
        for start in 0..S {
            let mut state = start as u8;
            for c in text.chars() {
                let (next, starts, ends) = step(state, c);
                out.starts[start] += starts as u32;
                out.ends[start] += ends as u32;
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
            out.starts[start] = self.starts[start] + next.starts[mid];
            out.ends[start] = self.ends[start] + next.ends[mid];
        }
        out
    }

    /// Where the machine stands, starting in `state`.
    pub fn from(&self, state: u8) -> UnitState {
        let s = state as usize;
        UnitState {
            state: self.end[s],
            starts: self.starts[s] as usize,
            ends: self.ends[s] as usize,
        }
    }
}

/// Word (vim's WORD): runs of non-whitespace.
mod word {
    pub const START: u8 = GAP;
    const GAP: u8 = 0;
    const IN: u8 = 1;

    pub fn step(state: u8, c: char) -> (u8, bool, bool) {
        if c.is_whitespace() {
            (GAP, false, state == IN)
        } else {
            (IN, state == GAP, false)
        }
    }
}

/// Vim's word: runs of keyword characters, or runs of other non-blanks.
mod vim_word {
    pub const START: u8 = GAP;
    const GAP: u8 = 0;
    const KEYWORD: u8 = 1;
    const OTHER: u8 = 2;

    pub fn step(state: u8, c: char) -> (u8, bool, bool) {
        let to = if c.is_whitespace() {
            GAP
        } else if c.is_alphanumeric() || c == '_' {
            KEYWORD
        } else {
            OTHER
        };
        if to == state {
            (state, false, false)
        } else {
            // Leaving a word ends it; entering one starts it. Going from
            // keyword to punctuation does both at once.
            (to, to != GAP, state != GAP)
        }
    }
}

/// Vim's sentence.
mod sentence {
    pub const START: u8 = GAP;
    /// Between sentences.
    const GAP: u8 = 0;
    /// In a sentence.
    const IN: u8 = 1;
    /// Just past a `.`, `!` or `?`, and any closers after it.
    const TERM: u8 = 2;
    /// In a sentence, at the start of a line: another newline here makes
    /// an empty line, which ends it.
    const LINE: u8 = 3;

    fn ends_sentence(c: char) -> bool {
        matches!(c, '.' | '!' | '?')
    }

    fn closes(c: char) -> bool {
        matches!(c, ')' | ']' | '"' | '\'')
    }

    pub fn step(state: u8, c: char) -> (u8, bool, bool) {
        match state {
            GAP if c.is_whitespace() => (GAP, false, false),
            GAP if ends_sentence(c) => (TERM, true, false),
            GAP => (IN, true, false),
            TERM if closes(c) || ends_sentence(c) => (TERM, false, false),
            TERM if c.is_whitespace() => (GAP, false, true),
            LINE if c == '\n' => (GAP, false, true),
            _ if ends_sentence(c) => (TERM, false, false),
            _ if c == '\n' => (LINE, false, false),
            _ => (IN, false, false),
        }
    }

    /// Reading back (nearest first), the nearest point where the state is
    /// certainly `GAP`: just after whitespace that follows a `.`, `!` or
    /// `?` (and any closers), since every state goes to `TERM` on those and
    /// `TERM` goes to `GAP` on whitespace; or just after two newlines in a
    /// row, which take every state to `GAP`.
    pub fn sync(before: impl Iterator<Item = (usize, char)>) -> Option<(usize, u8)> {
        // The latest whitespace seen, and whether only closers follow it
        // back to here.
        let mut space: Option<(usize, char)> = None;
        let mut closers = false;
        for (i, c) in before {
            if let Some((j, w)) = space {
                if ends_sentence(c) {
                    return Some((j + w.len_utf8(), GAP));
                }
                if closes(c) {
                    closers = true;
                    continue;
                }
                if c == '\n' && w == '\n' && !closers {
                    return Some((j + 1, GAP));
                }
            }
            if c.is_whitespace() {
                space = Some((i, c));
                closers = false;
            } else {
                space = None;
            }
        }
        None
    }
}

/// Paragraphs, where a blank line holds only whitespace.
///
/// Two bits: whether the previous line was blank, and whether the current
/// one is blank so far.
mod paragraph {
    const PREV_BLANK: u8 = 0b10;
    const CUR_BLANK: u8 = 0b01;
    /// The start of the text counts as following a blank line.
    pub const START: u8 = PREV_BLANK | CUR_BLANK;

    pub fn step(state: u8, c: char) -> (u8, bool, bool) {
        let prev_blank = state & PREV_BLANK != 0;
        let cur_blank = state & CUR_BLANK != 0;
        if c == '\n' {
            if cur_blank {
                // A blank line just finished: it ends a paragraph above it.
                (PREV_BLANK | CUR_BLANK, false, !prev_blank)
            } else {
                (CUR_BLANK, false, false)
            }
        } else if c.is_whitespace() || !cur_blank {
            (state, false, false)
        } else {
            // The first non-blank of a line: a paragraph starts if the line
            // above was blank.
            (state & PREV_BLANK, prev_blank, false)
        }
    }

    /// The state at the start of a line, given whether the line above it
    /// was blank.
    pub fn at_line_start(above_blank: bool) -> u8 {
        if above_blank { START } else { CUR_BLANK }
    }
}

/// Vim's paragraphs, where only an empty line is a boundary.
///
/// Two bits: whether the previous line was empty, and whether the current
/// one is empty so far.
mod vim_paragraph {
    const PREV_EMPTY: u8 = 0b10;
    const CUR_EMPTY: u8 = 0b01;
    pub const START: u8 = PREV_EMPTY | CUR_EMPTY;

    pub fn step(state: u8, c: char) -> (u8, bool, bool) {
        let prev_empty = state & PREV_EMPTY != 0;
        let cur_empty = state & CUR_EMPTY != 0;
        if c == '\n' {
            if cur_empty {
                (PREV_EMPTY | CUR_EMPTY, false, !prev_empty)
            } else {
                (CUR_EMPTY, false, false)
            }
        } else if !cur_empty {
            (state, false, false)
        } else {
            (state & PREV_EMPTY, prev_empty, false)
        }
    }

    /// The state at the start of a line, given whether the line above it
    /// was empty.
    pub fn at_line_start(above_empty: bool) -> u8 {
        if above_empty { START } else { CUR_EMPTY }
    }
}

/// Markdown headings: confirmed by the space, tab or newline after one to
/// six `#` at the start of a line; a section runs to the next heading.
///
/// States: `LINE_START`, then 1 to 6 for that many `#` so far, then
/// `OTHER` for the rest of a line that isn't (or is past) a heading's
/// marker.
pub(crate) mod section {
    pub const START: u8 = LINE_START;
    pub const LINE_START: u8 = 0;
    const OTHER: u8 = 7;
    /// How many states there are.
    pub const STATES: usize = 8;

    /// One step: the next state, and the level of the heading this `char`
    /// confirms, or 0.
    pub fn heading(state: u8, c: char) -> (u8, u8) {
        let hashes = (1..=6).contains(&state);
        match c {
            '\n' => (LINE_START, if hashes { state } else { 0 }),
            '#' if state == LINE_START => (1, 0),
            '#' if hashes && state < 6 => (state + 1, 0),
            // A `\r` ends the line's marker like a space does (it's a line
            // ending in CommonMark), so `#\r\n` is an empty heading and
            // `#\r#` is no deeper than one `#`.
            ' ' | '\t' | '\r' if hashes => (OTHER, state),
            // Anything else, including a seventh `#`: not a heading.
            _ => (OTHER, 0),
        }
    }

    pub fn step(state: u8, c: char) -> (u8, bool, bool) {
        let (next, level) = heading(state, c);
        (next, level > 0, false)
    }

    /// The level of a heading still waiting for its space or newline when
    /// the text ends in this state; at the very end, it counts.
    pub fn pending(state: u8) -> Option<u8> {
        (1..=6).contains(&state).then_some(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of each unit.
    fn pieces(unit: Unit, text: &str) -> Vec<&str> {
        unit.ranges(text).into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn words() {
        assert_eq!(
            pieces(Unit::Word, "  foo.bar(x)  baz\n"),
            ["foo.bar(x)", "baz"]
        );
        assert_eq!(pieces(Unit::Word, ""), Vec::<&str>::new());
    }

    #[test]
    fn vim_words() {
        assert_eq!(
            pieces(Unit::VimWord, "  foo.bar(x)  baz_2\n"),
            ["foo", ".", "bar", "(", "x", ")", "baz_2"]
        );
        assert_eq!(pieces(Unit::VimWord, "é日本 -> ok"), ["é日本", "->", "ok"]);
    }

    #[test]
    fn sentences() {
        assert_eq!(
            pieces(Unit::Sentence, "Hi there. How are you?  Fine!"),
            ["Hi there.", "How are you?", "Fine!"]
        );
        assert_eq!(
            pieces(Unit::Sentence, "He said \"go.\" Then left."),
            ["He said \"go.\"", "Then left."],
            "closers stay with the sentence"
        );
        assert_eq!(
            pieces(Unit::Sentence, "Pi is 3.14 or so. Yes"),
            ["Pi is 3.14 or so.", "Yes"],
            "a full stop needs whitespace after it"
        );
        assert_eq!(
            pieces(Unit::Sentence, "no stop\n\nnext"),
            ["no stop\n", "next"],
            "an empty line ends one"
        );
        assert_eq!(
            pieces(Unit::Sentence, "still\n  \none"),
            ["still\n  \none"],
            "a whitespace-only line doesn't"
        );
    }

    #[test]
    fn paragraphs() {
        let text = "a\nb\n\n  \nc\n \t\nd";
        assert_eq!(pieces(Unit::Paragraph, text), ["a\nb\n", "c\n", "d"]);
        // Vim: only the empty line is a boundary; the whitespace-only lines
        // belong to paragraphs.
        assert_eq!(
            pieces(Unit::VimParagraph, text),
            ["a\nb\n", "  \nc\n \t\nd"]
        );
        assert_eq!(pieces(Unit::Paragraph, "\n\n"), Vec::<&str>::new());
        assert_eq!(
            pieces(Unit::Paragraph, "  indented\nline\n\nx"),
            ["  indented\nline\n", "x"],
            "a paragraph starts at its line, not its first non-blank"
        );
    }

    #[test]
    fn sections() {
        let text = "intro\n# One\ntext\n## Two\n#tag\n#";
        assert_eq!(
            pieces(Unit::Section, text),
            ["# One\ntext\n", "## Two\n#tag\n", "#"],
            "a heading still open at the end counts"
        );
        assert_eq!(pieces(Unit::Section, "a # b\n"), Vec::<&str>::new());
        assert_eq!(
            pieces(Unit::Section, "###### Six\n####### Seven\n"),
            ["###### Six\n####### Seven\n"],
            "seven `#` isn't a heading"
        );
    }
}
