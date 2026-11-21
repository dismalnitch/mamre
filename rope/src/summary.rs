//! What a run of text adds up to, and the dimensions to seek it by.
//!
//! Every rope counts bytes, chars and lines; what else it counts is its
//! [`Measure`](crate::Measure), kept alongside in [`TextSummary::units`].

use mamre::{Dimension, Summary};

use crate::measure::Measure;

/// A position as a row (newlines before it) and a column (bytes since the
/// last newline). Ordered row first, so it's a dimension you can seek by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

impl Point {
    pub fn new(row: usize, column: usize) -> Self {
        Point { row, column }
    }

    /// The extent of `text`: its newlines, and the bytes after the last one.
    pub fn of(text: &str) -> Self {
        match text.rfind('\n') {
            Some(i) => Point {
                row: text.bytes().filter(|&b| b == b'\n').count(),
                column: text.len() - i - 1,
            },
            None => Point {
                row: 0,
                column: text.len(),
            },
        }
    }

    /// Where you end up after this extent, then `next`'s. Not commutative,
    /// and can't be undone: a newline in `next` resets the column.
    pub fn then(self, next: Point) -> Point {
        if next.row > 0 {
            Point {
                row: self.row + next.row,
                column: next.column,
            }
        } else {
            Point {
                row: self.row,
                column: self.column + next.column,
            }
        }
    }
}

/// What a run of text adds up to: the counts every rope keeps, and the
/// extra `units` its measure `M` keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextSummary<M> {
    /// Length in bytes.
    pub len: usize,
    /// Length in `char`s.
    pub chars: usize,
    /// Newlines, and the bytes after the last one.
    pub lines: Point,
    /// Whatever else `M` counts: words, paragraphs, sections, ...
    pub units: M,
}

impl<M: Measure> TextSummary<M> {
    pub fn of(text: &str) -> Self {
        TextSummary {
            len: text.len(),
            chars: text.chars().count(),
            lines: Point::of(text),
            units: M::measure(text),
        }
    }
}

impl<M: Measure> Summary for TextSummary<M> {
    fn empty() -> Self {
        TextSummary {
            len: 0,
            chars: 0,
            lines: Point::default(),
            units: M::empty(),
        }
    }

    fn concat(&mut self, next: &Self) {
        self.len += next.len;
        self.chars += next.chars;
        self.lines = self.lines.then(next.lines);
        self.units.concat(&next.units);
    }
}

/// A position in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteOffset(pub usize);

/// A position in `char`s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CharOffset(pub usize);

impl<M: Measure> Dimension<TextSummary<M>> for ByteOffset {
    fn zero() -> Self {
        ByteOffset(0)
    }
    fn add_summary(&mut self, summary: &TextSummary<M>) {
        self.0 += summary.len;
    }
}

impl<M: Measure> Dimension<TextSummary<M>> for CharOffset {
    fn zero() -> Self {
        CharOffset(0)
    }
    fn add_summary(&mut self, summary: &TextSummary<M>) {
        self.0 += summary.chars;
    }
}

impl<M: Measure> Dimension<TextSummary<M>> for Point {
    fn zero() -> Self {
        Point::default()
    }
    fn add_summary(&mut self, summary: &TextSummary<M>) {
        *self = self.then(summary.lines);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::measure::{Markdown, Plain};

    #[test]
    fn points() {
        assert_eq!(Point::of(""), Point::new(0, 0));
        assert_eq!(Point::of("ab"), Point::new(0, 2));
        assert_eq!(Point::of("ab\ncde\nf"), Point::new(2, 1));
        assert_eq!(Point::of("ab\n"), Point::new(1, 0));
        assert_eq!(Point::of("日"), Point::new(0, 3), "columns are bytes");
    }

    proptest! {
        /// Summing the pieces of any split gives the summary of the whole,
        /// for every measure.
        #[test]
        fn summaries_combine_across_any_split(
            text in crate::measure::tests::text(),
            cuts in prop::collection::vec(any::<u16>(), 0..6),
        ) {
            let mut md = TextSummary::<Markdown>::empty();
            let mut plain = TextSummary::<Plain>::empty();
            for range in crate::measure::tests::pieces(&text, &cuts) {
                md.concat(&TextSummary::of(&text[range.clone()]));
                plain.concat(&TextSummary::of(&text[range]));
            }
            prop_assert_eq!(md, TextSummary::of(&text));
            prop_assert_eq!(plain, TextSummary::of(&text));
        }
    }
}
