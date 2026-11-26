//! What a paragraph is: a heading, a quotation, code, or plain text.
//!
//! A paragraph's kind is a mark on the line break that starts it (the
//! same mark that makes a section), not anything in the text. So:
//!
//! - an empty paragraph has a kind: its break is there to mark;
//! - a break typed at the end of a heading starts a plain paragraph;
//! - deleting a paragraph's break joins it to the one before, whose kind
//!   it takes;
//! - a section runs from its heading's break, so a moved section brings
//!   its own break and lands whole between two paragraphs.
//!
//! Text before the first break is a paragraph with no break to mark: it's
//! always plain. A document whose first paragraph needs a kind starts
//! with a break, which its editor doesn't show.

use std::ops::Range;

use mamre_rope::Measure;

use crate::{Edit, SectionId, Text, TextError};

/// Levels from here up are kinds that aren't headings. They nest inside
/// every heading's section, since a section ends only at a mark at its
/// level or above.
const KINDS: u8 = 16;

/// What a paragraph is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Plain: no mark.
    Text,
    /// A heading at a level, 1 the outermost. It starts a section.
    Heading(u8),
    Quote,
    Code,
    Equation,
    /// A kind from a newer version, kept as it is.
    Other(u8),
}

impl Kind {
    /// The mark that says it.
    pub fn level(self) -> u8 {
        match self {
            Kind::Text => 0,
            Kind::Heading(l) => l.clamp(1, KINDS - 1),
            Kind::Quote => KINDS,
            Kind::Code => KINDS + 1,
            Kind::Equation => KINDS + 2,
            Kind::Other(l) => l,
        }
    }

    /// The kind a mark says.
    pub fn of(level: u8) -> Kind {
        match level {
            0 => Kind::Text,
            l if l < KINDS => Kind::Heading(l),
            l if l == KINDS => Kind::Quote,
            l if l == KINDS + 1 => Kind::Code,
            l if l == KINDS + 2 => Kind::Equation,
            l => Kind::Other(l),
        }
    }

    pub fn is_heading(self) -> bool {
        matches!(self, Kind::Heading(_))
    }
}

/// A paragraph, as the text shows it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paragraph {
    /// The byte offset of the break that starts it; `None` for text
    /// before the first break.
    pub starts: Option<usize>,
    /// Its text, in bytes: after its break, up to the next.
    pub range: Range<usize>,
    pub kind: Kind,
    /// Its mark's id, if it has one: for a heading, its section's.
    pub id: Option<SectionId>,
}

impl<M: Measure> Text<M> {
    /// The break that starts the paragraph a cursor at byte `at` is in:
    /// the last one before `at`.
    fn break_before(&self, at: usize) -> Option<usize> {
        let mut found = None;
        let mut base = 0;
        for chunk in self.rope.chunks() {
            if base >= at {
                break;
            }
            let upto = (at - base).min(chunk.len());
            if let Some(i) = chunk.as_bytes()[..upto].iter().rposition(|&b| b == b'\n') {
                found = Some(base + i);
            }
            base += chunk.len();
        }
        found
    }

    /// Makes the paragraph a cursor at byte `at` is in a `kind`. The
    /// mark's id is the same through every edit, move and merge; when two
    /// writers set one paragraph's kind at once, one wins everywhere.
    ///
    /// Text before the first break can't be given a kind:
    /// [`TextError::NoBreak`].
    pub fn set_kind(&mut self, at: usize, kind: Kind) -> Result<(SectionId, Vec<Edit>), TextError> {
        self.check_at(at)?;
        let starts = self.break_before(at).ok_or(TextError::NoBreak { at })?;
        self.mark_section(starts, kind.level())
    }

    /// What the paragraph a cursor at byte `at` is in is.
    pub fn kind_at(&self, at: usize) -> Kind {
        let Some(starts) = self.break_before(at.min(self.len())) else {
            return Kind::Text;
        };
        self.sections()
            .into_iter()
            .find(|s| s.at == starts)
            .map_or(Kind::Text, |s| Kind::of(s.level))
    }

    /// Every paragraph, in order. They cover the text but for the breaks
    /// between them.
    pub fn paragraphs(&self) -> Vec<Paragraph> {
        // Marks on letters, from before kinds, are sections and not a
        // paragraph's kind: only a break's mark is looked up.
        let marks: std::collections::HashMap<usize, crate::Section> =
            self.sections().into_iter().map(|s| (s.at, s)).collect();
        let mut out = Vec::new();
        let mut open: (Option<usize>, usize) = (None, 0);
        let close = |open: (Option<usize>, usize), end: usize, out: &mut Vec<Paragraph>| {
            let mark = open.0.and_then(|b| marks.get(&b));
            out.push(Paragraph {
                starts: open.0,
                range: open.1..end,
                kind: mark.map_or(Kind::Text, |s| Kind::of(s.level)),
                id: mark.map(|s| s.id),
            });
        };
        let mut base = 0;
        for chunk in self.rope.chunks() {
            for (i, b) in chunk.bytes().enumerate() {
                if b == b'\n' {
                    let at = base + i;
                    close(open, at, &mut out);
                    open = (Some(at), at + 1);
                }
            }
            base += chunk.len();
        }
        close(open, self.len(), &mut out);
        out
    }
}
