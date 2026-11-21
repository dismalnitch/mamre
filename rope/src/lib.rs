//! A text rope on mamre's sum tree, built only from its public API.
//!
//! The text is cut into chunks of up to `N` bytes (never inside a `char`),
//! held as items of a `SumTree`. Every node knows its subtree's
//! [`TextSummary`]: bytes, chars and lines, plus whatever the rope's
//! [`Measure`] counts. `Rope<Plain>` counts nothing more, `Rope<Prose>` adds
//! words and paragraphs, and `Rope<Markdown>` adds sections.
//!
//! Positions are byte offsets, as for `str`, and must fall on `char`
//! boundaries. Lines are split on `\n` only; a `\r` is ordinary text, so
//! turning `\r\n` into `\n` is up to whoever loads the file.
//!
//! Chunks stay full: no chunk is empty, and no two neighbours would fit in
//! one. Every cut and join merges small pieces into their neighbour to keep
//! it that way, so a rope of `n` bytes has fewer than `2n / N + 1` chunks.

mod measure;
mod summary;

use std::fmt;
use std::marker::PhantomData;
use std::ops::Range;

use arrayvec::ArrayString;
use mamre::{Item, SumTree, cursor::Bias};

pub use measure::{Markdown, Measure, Plain, Prose, Scan, Step};
pub use summary::{ByteOffset, CharOffset, Point, TextSummary};

/// The default chunk size, in bytes.
pub const CHUNK: usize = 128;

/// Up to `N` bytes of text: one item of the tree.
pub struct Chunk<M, const N: usize> {
    text: ArrayString<N>,
    measure: PhantomData<fn() -> M>,
}

impl<M, const N: usize> Chunk<M, N> {
    fn new(text: &str) -> Self {
        Chunk {
            text: ArrayString::from(text).expect("a chunk's text fits in a chunk"),
            measure: PhantomData,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl<M, const N: usize> Clone for Chunk<M, N> {
    fn clone(&self) -> Self {
        Chunk {
            text: self.text,
            measure: PhantomData,
        }
    }
}

impl<M: Measure, const N: usize> Item for Chunk<M, N> {
    type Summary = TextSummary<M>;
    fn summary(&self) -> TextSummary<M> {
        TextSummary::of(&self.text)
    }
}

/// Text in a sum tree. `M` is what it counts beyond bytes, chars and lines;
/// `N` is the chunk size in bytes, at least 4 so any `char` fits.
pub struct Rope<M: Measure = Plain, const N: usize = CHUNK> {
    tree: SumTree<Chunk<M, N>>,
}

impl<M: Measure, const N: usize> Clone for Rope<M, N> {
    /// O(1): the copy shares every node until one of them changes.
    fn clone(&self) -> Self {
        Rope {
            tree: self.tree.clone(),
        }
    }
}

impl<M: Measure, const N: usize> Default for Rope<M, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Measure, const N: usize> Rope<M, N> {
    pub fn new() -> Self {
        const { assert!(N >= 4, "a chunk must hold any char: 4 bytes or more") };
        Rope {
            tree: SumTree::new(),
        }
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.summary().len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// What the whole text adds up to.
    pub fn summary(&self) -> &TextSummary<M> {
        self.tree.summary()
    }

    /// The chunks, in order.
    pub fn chunks(&self) -> impl Iterator<Item = &str> {
        self.tree.iter().map(Chunk::as_str)
    }

    pub fn chars(&self) -> impl Iterator<Item = char> {
        self.chunks().flat_map(str::chars)
    }

    /// Adds `text` at the end: tops up the last chunk, then adds full ones.
    pub fn push_str(&mut self, text: &str) {
        let mut rest = text;
        self.tree.update_last(|last| {
            let take = floor_boundary(rest, N - last.text.len());
            last.text.push_str(&rest[..take]);
            rest = &rest[take..];
        });
        while !rest.is_empty() {
            let take = floor_boundary(rest, N);
            self.tree.push(Chunk::new(&rest[..take]));
            rest = &rest[take..];
        }
    }

    /// Adds all of `other` at the end, merging its first chunks into our
    /// last one while they fit.
    pub fn append(&mut self, other: Rope<M, N>) {
        let mut cursor = other.tree.cursor::<()>();
        cursor.next();
        while let Some(chunk) = cursor.item() {
            if !self.merge_into_last(chunk.as_str()) {
                break;
            }
            cursor.next();
        }
        self.tree.append(cursor.suffix());
    }

    /// The text in `range`, as a new rope sharing every whole chunk.
    ///
    /// Panics if `range` is out of bounds or doesn't fall on `char`
    /// boundaries.
    pub fn slice(&self, range: Range<usize>) -> Rope<M, N> {
        self.check_range(&range);
        self.suffix_from(range.start)
            .prefix(range.end - range.start)
    }

    /// Replaces the text in `range` with `text`.
    ///
    /// Panics if `range` is out of bounds or doesn't fall on `char`
    /// boundaries.
    pub fn replace(&mut self, range: Range<usize>, text: &str) {
        self.check_range(&range);
        let mut out = self.prefix(range.start);
        out.push_str(text);
        out.append(self.suffix_from(range.end));
        *self = out;
    }

    /// Puts `text` in at byte `at`.
    pub fn insert(&mut self, at: usize, text: &str) {
        self.replace(at..at, text);
    }

    /// Removes the text in `range`.
    pub fn remove(&mut self, range: Range<usize>) {
        self.replace(range, "");
    }

    /// The row and column of byte `offset`.
    pub fn offset_to_point(&self, offset: usize) -> Point {
        self.check_range(&(offset..offset));
        let mut cursor = self.tree.cursor::<(ByteOffset, Point)>();
        cursor.seek(&ByteOffset(offset), Bias::Right);
        let (ByteOffset(start), point) = *cursor.start();
        match cursor.item() {
            Some(chunk) => point.then(Point::of(&chunk.as_str()[..offset - start])),
            None => point,
        }
    }

    /// The byte offset of `point`. A column past the end of its row lands
    /// at the end of the row, and one inside a `char` on the next `char`; a
    /// row past the last lands at the end.
    pub fn point_to_offset(&self, point: Point) -> usize {
        let mut cursor = self.tree.cursor::<(Point, ByteOffset)>();
        cursor.seek(&point, Bias::Right);
        let (mut at, ByteOffset(start)) = *cursor.start();
        let Some(chunk) = cursor.item() else {
            return self.len();
        };
        for (i, c) in chunk.as_str().char_indices() {
            if at >= point {
                return start + i;
            }
            if c == '\n' {
                if at.row == point.row {
                    return start + i;
                }
                at = Point::new(at.row + 1, 0);
            } else {
                at.column += c.len_utf8();
            }
        }
        start + chunk.as_str().len()
    }

    /// Checks the tree's own rules, then the chunks': none empty, and no two
    /// neighbours that would fit in one.
    pub fn check(&self) -> Result<(), String> {
        self.tree.check()?;
        let mut prev: Option<usize> = None;
        for (i, chunk) in self.chunks().enumerate() {
            let len = chunk.len();
            if len == 0 {
                return Err(format!("chunk {i} is empty"));
            }
            if let Some(prev) = prev
                && prev + len <= N
            {
                return Err(format!(
                    "chunks {} and {i} ({prev} + {len} bytes) would fit in one of {N}",
                    i - 1
                ));
            }
            prev = Some(len);
        }
        Ok(())
    }

    /// Everything before byte `end`.
    fn prefix(&self, end: usize) -> Rope<M, N> {
        let mut cursor = self.tree.cursor::<ByteOffset>();
        // Whole chunks that end at or before `end`; the cursor stops on the
        // one holding `end`, if any.
        let mut out = Rope {
            tree: cursor.slice(&ByteOffset(end), Bias::Right),
        };
        if let Some(chunk) = cursor.item() {
            let k = end - cursor.start().0;
            out.push_piece(&chunk.as_str()[..k]);
        }
        out
    }

    /// Everything from byte `start` on.
    fn suffix_from(&self, start: usize) -> Rope<M, N> {
        let mut cursor = self.tree.cursor::<ByteOffset>();
        cursor.seek(&ByteOffset(start), Bias::Right);
        let mut out = Rope::new();
        if let Some(chunk) = cursor.item() {
            let k = start - cursor.start().0;
            out.push_piece(&chunk.as_str()[k..]);
            cursor.next();
        }
        out.append(Rope {
            tree: cursor.suffix(),
        });
        out
    }

    /// Adds a piece of an existing chunk at the end: into the last chunk if
    /// it fits whole, else as a chunk of its own.
    fn push_piece(&mut self, piece: &str) {
        if !piece.is_empty() && !self.merge_into_last(piece) {
            self.tree.push(Chunk::new(piece));
        }
    }

    /// Adds `piece` to the last chunk if it fits whole; says whether it did.
    fn merge_into_last(&mut self, piece: &str) -> bool {
        let fits = self
            .tree
            .last()
            .is_some_and(|last| last.text.len() + piece.len() <= N);
        if fits {
            self.tree.update_last(|last| last.text.push_str(piece));
        }
        fits
    }

    fn check_range(&self, range: &Range<usize>) {
        assert!(
            range.start <= range.end && range.end <= self.len(),
            "range {range:?} is out of bounds for a rope of {} bytes",
            self.len()
        );
        for at in [range.start, range.end] {
            assert!(
                self.is_char_boundary(at),
                "byte {at} is not a char boundary"
            );
        }
    }

    fn is_char_boundary(&self, at: usize) -> bool {
        let mut cursor = self.tree.cursor::<ByteOffset>();
        cursor.seek(&ByteOffset(at), Bias::Right);
        match cursor.item() {
            Some(chunk) => chunk.as_str().is_char_boundary(at - cursor.start().0),
            None => true,
        }
    }
}

/// The largest `char` boundary in `text` at or below `at`.
fn floor_boundary(text: &str, at: usize) -> usize {
    if at >= text.len() {
        return text.len();
    }
    let mut at = at;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

impl<M: Measure, const N: usize> From<&str> for Rope<M, N> {
    fn from(text: &str) -> Self {
        let mut rope = Rope::new();
        rope.push_str(text);
        rope
    }
}

impl<M: Measure, const N: usize> fmt::Display for Rope<M, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

impl<M: Measure, const N: usize> fmt::Debug for Rope<M, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Rope").field(&self.to_string()).finish()
    }
}
