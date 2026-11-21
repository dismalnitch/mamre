//! Grapheme clusters: what a reader takes for one character (a letter and
//! its accents, an emoji and its modifiers, a flag, `\r\n`), by Unicode's
//! extended grapheme cluster rules (UAX #29), in byte offsets.
//!
//! Found a chunk at a time, never flattening the rope: a
//! `unicode_segmentation::GraphemeCursor` is handed the chunk it's in and
//! asks for the one before or after, or for text before it, as a cluster
//! spans chunk seams. "Next" is just a higher offset: which way the text
//! runs on the page is the layout's business, not the rope's.

use std::borrow::Cow;

use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

use crate::{ByteOffset, Measure, Rope};
use mamre::cursor::Bias;

impl<M: Measure, const N: usize> Rope<M, N> {
    /// The chunk holding byte `offset`, and the offset it starts at: the
    /// one starting at `offset` if it's between two, the last one at the
    /// very end. `None` only for an empty rope.
    ///
    /// Panics if `offset` is past the end.
    pub fn chunk_at(&self, offset: usize) -> Option<(usize, &str)> {
        assert!(
            offset <= self.len(),
            "byte {offset} is out of bounds for a rope of {} bytes",
            self.len()
        );
        let mut cursor = self.tree.cursor::<ByteOffset>();
        let bias = if offset == self.len() {
            Bias::Left
        } else {
            Bias::Right
        };
        cursor.seek(&ByteOffset(offset), bias);
        let chunk = cursor.item()?;
        Some((cursor.start().0, chunk.as_str()))
    }

    /// Whether byte `offset` is between two grapheme clusters (or at either
    /// end).
    ///
    /// Panics if `offset` is out of bounds or not on a `char` boundary.
    pub fn is_grapheme_boundary(&self, offset: usize) -> bool {
        self.check_range(&(offset..offset));
        if offset == 0 || offset == self.len() {
            return true;
        }
        self.drive(offset, |cursor, chunk, start| {
            cursor.is_boundary(chunk, start)
        })
    }

    /// The first grapheme cluster boundary after byte `offset`: where the
    /// cluster there ends. `None` at the end.
    ///
    /// Panics if `offset` is out of bounds or not on a `char` boundary.
    pub fn next_grapheme_boundary(&self, offset: usize) -> Option<usize> {
        self.check_range(&(offset..offset));
        if offset == self.len() {
            return None;
        }
        self.drive(offset, |cursor, chunk, start| {
            cursor.next_boundary(chunk, start)
        })
    }

    /// The last grapheme cluster boundary before byte `offset`: where the
    /// cluster before it starts. `None` at the start.
    ///
    /// Panics if `offset` is out of bounds or not on a `char` boundary.
    pub fn prev_grapheme_boundary(&self, offset: usize) -> Option<usize> {
        self.check_range(&(offset..offset));
        if offset == 0 {
            return None;
        }
        self.drive(offset, |cursor, chunk, start| {
            cursor.prev_boundary(chunk, start)
        })
    }

    /// Runs `step` on a grapheme cursor at `offset`, feeding it text and
    /// context until it has its answer.
    ///
    /// The cursor is always given a window holding the `char` on each side
    /// of where it is: a chunk, or two joined. unicode-segmentation 1.13's
    /// cursor goes wrong in two cases when a seam falls in the wrong place
    /// (dismalnitch/mamre#4): `is_boundary` between a Prepend and a control
    /// (`\u{600}\n`) says no; and `next_boundary` from a Prepend before a
    /// flag (`\u{600}🇺🇸`) stops between the flag's two indicators. Neither
    /// happens when it reads the `char`s either side of its offset from the
    /// text it's given.
    fn drive<R>(
        &self,
        offset: usize,
        mut step: impl FnMut(&mut GraphemeCursor, &str, usize) -> Result<R, GraphemeIncomplete>,
    ) -> R {
        let mut cursor = GraphemeCursor::new(offset, self.len(), true);
        // Where the cursor is: each window straddles it.
        let mut at = offset;
        loop {
            let (start, window) = self.window(at);
            match step(&mut cursor, &window, start) {
                Ok(answer) => return answer,
                Err(GraphemeIncomplete::PreContext(end)) => {
                    // Text that ends at `end`, from the chunk before it.
                    let (s, c) = self.chunk_at(end - 1).expect("there's text before");
                    cursor.provide_context(&c[..end - s], s);
                }
                // It read to the end of the window, or back to its start.
                Err(GraphemeIncomplete::NextChunk) => at = start + window.len(),
                Err(GraphemeIncomplete::PrevChunk) => at = start,
                Err(GraphemeIncomplete::InvalidOffset) => {
                    unreachable!("every window holds the cursor's offset")
                }
            }
        }
    }

    /// Text around byte `at`: the chunk holding the `char` before it
    /// through the chunk holding the one after, and where that starts.
    fn window(&self, at: usize) -> (usize, Cow<'_, str>) {
        let (start, before) = self
            .chunk_at(at.saturating_sub(1))
            .expect("the rope isn't empty");
        let end = start + before.len();
        if at < end || at == self.len() {
            return (start, Cow::Borrowed(before));
        }
        let (_, after) = self.chunk_at(at).expect("there's text after");
        (start, Cow::Owned([before, after].concat()))
    }
}
