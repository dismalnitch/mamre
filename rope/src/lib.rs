//! A text rope on mamre's sum tree, built only from its public API.
//!
//! The text is cut into chunks of up to `N` bytes (never inside a `char`),
//! held as items of a `SumTree`. Every node knows its subtree's
//! [`TextSummary`]: bytes, chars and lines, plus whatever the rope's
//! [`Measure`] counts: `Rope<Plain>` nothing more, `Rope<Prose>` words and
//! paragraphs, `Rope<Sections>` markdown headings, and measures combine as
//! tuples (`Markdown` is `(Prose, Sections)`).
//!
//! Units ([`Unit`]: words, sentences, paragraphs, sections, in the variants
//! vim and prose disagree on) can be found two ways:
//!
//! - by number, far away, in O(log n), if the measure counts them:
//!   [`Rope::unit_start`] and friends;
//! - by neighbour, near a position, whatever the measure:
//!   [`Rope::next_boundary`] and [`Rope::prev_boundary`], which is what
//!   motions want.
//!
//! A rope that keeps [`Sections`] also knows its outline: the [`Address`]
//! of any position (`3.2` is chapter 3, section 2), and where any address's
//! heading is, both in O(log n). See [`Rope::address_at`] and
//! [`Rope::heading_at`].
//!
//! Grapheme clusters (what a reader takes for one character: a letter and
//! its accents, an emoji and its modifiers, a flag) are found by
//! neighbour, a chunk at a time: [`Rope::next_grapheme_boundary`],
//! [`Rope::prev_grapheme_boundary`] and [`Rope::is_grapheme_boundary`],
//! with [`Rope::chunk_at`] for anything else that reads chunk by chunk.
//!
//! Positions are byte offsets, as for `str`, and must fall on `char`
//! boundaries. Lines are split on `\n` only; a `\r` is ordinary text, so
//! turning `\r\n` into `\n` is up to whoever loads the file.
//!
//! Chunks stay full: no chunk is empty, and no two neighbours would fit in
//! one. Every cut and join merges small pieces into their neighbour to keep
//! it that way, so a rope of `n` bytes has fewer than `2n / N + 1` chunks.

mod angela;
mod graphemes;
mod measure;
mod moves;
mod outline;
mod scan;
mod seek;
mod summary;
mod units;

use std::fmt;
use std::marker::PhantomData;
use std::ops::Range;

use arrayvec::ArrayString;
use mamre::cursor::{Bias, Cursor};
use mamre::{Item, SumTree};

pub use angela::Angela;
pub use measure::{Markdown, Measure, Motions, Plain, Prose, Units};
pub use moves::{MoveError, Moved, Newline};
pub use outline::{Address, Sections};
pub use scan::Edge;
pub use summary::{ByteOffset, CharOffset, Point, TextSummary};
pub use units::{Scan, Step, Unit, UnitState};

/// The default chunk size, in bytes. Large enough that a `Markdown` rope's
/// summaries cost about a quarter of its text; an edit re-measures one
/// chunk, which at this size takes microseconds.
pub const CHUNK: usize = 512;

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
        // Char boundaries are checked where the text is to hand: on the
        // chunks an edit in place touches, and otherwise here.
        self.check_bounds(&range);
        if self.replace_in_chunk(&range, text) {
            return;
        }
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

    /// How many `char`s come before byte `offset`.
    pub fn offset_to_char(&self, offset: usize) -> usize {
        self.check_range(&(offset..offset));
        let mut cursor = self.tree.cursor::<(ByteOffset, CharOffset)>();
        cursor.seek(&ByteOffset(offset), Bias::Right);
        let (ByteOffset(start), CharOffset(chars)) = *cursor.start();
        match cursor.item() {
            Some(chunk) => chars + chunk.as_str()[..offset - start].chars().count(),
            None => chars,
        }
    }

    /// The byte offset of `char` number `chars`, or the end if it's
    /// exactly the number of chars.
    ///
    /// Panics if there are fewer `char`s than that.
    pub fn char_to_offset(&self, chars: usize) -> usize {
        let total = self.summary().chars;
        assert!(
            chars <= total,
            "char {chars} is out of bounds for a rope of {total} chars"
        );
        let mut cursor = self.tree.cursor::<(CharOffset, ByteOffset)>();
        cursor.seek(&CharOffset(chars), Bias::Right);
        let (CharOffset(before), ByteOffset(start)) = *cursor.start();
        match cursor.item() {
            Some(chunk) => {
                let (i, _) = chunk
                    .as_str()
                    .char_indices()
                    .nth(chars - before)
                    .expect("the seek lands on the chunk holding the char");
                start + i
            }
            None => self.len(),
        }
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

    /// Replaces `range` in place, when it touches only a few chunks: most
    /// edits, typing above all. Inside one chunk that still holds it, with
    /// no neighbour that would fit alongside, the chunk is just changed;
    /// otherwise the chunks it touches are rewritten (see `replace_chunks`).
    /// Either way only paths down to chunks are copied. Whether it did.
    fn replace_in_chunk(&mut self, range: &Range<usize>, text: &str) -> bool {
        let (bias, start, a, b, len, neighbours_fit) = {
            let Some((cursor, bias)) = self.seek_chunk(range.start) else {
                return false;
            };
            let chunk = cursor.item().expect("seek_chunk lands on a chunk");
            let start = cursor.start().0;
            let len = chunk.text.len();
            for at in [range.start, range.end] {
                if at - start <= len {
                    assert!(
                        chunk.text.is_char_boundary(at - start),
                        "byte {at} is not a char boundary"
                    );
                }
            }
            let new_len = (len + text.len()).saturating_sub(range.len());
            let neighbour = |forward: bool| {
                let mut around = cursor.clone();
                if forward {
                    around.next();
                } else {
                    around.prev();
                }
                around.item().map_or(0, |c| c.text.len())
            };
            let neighbours_fit = range.end - start <= len
                && new_len < len
                && (fits::<N>(neighbour(false), new_len) || fits::<N>(new_len, neighbour(true)));
            (
                bias,
                start,
                range.start - start,
                range.end - start,
                len,
                neighbours_fit,
            )
        };
        let new_len = (len + text.len()).saturating_sub(range.len());
        if b <= len && new_len > 0 && new_len <= N && !neighbours_fit {
            self.tree
                .update_at::<ByteOffset, _>(&ByteOffset(range.start), bias, |chunk| {
                    let mut edited = ArrayString::new();
                    edited.push_str(&chunk.text[..a]);
                    edited.push_str(text);
                    edited.push_str(&chunk.text[b..]);
                    chunk.text = edited;
                });
            return true;
        }
        self.replace_chunks(range, text, start)
    }

    /// The chunk holding byte `at`, and the bias that finds it: at the very
    /// end, the last chunk, so typing at the end grows it. `None` for an
    /// empty rope.
    fn seek_chunk(&self, at: usize) -> Option<(Cursor<'_, Chunk<M, N>, ByteOffset>, Bias)> {
        let bias = if at == self.len() {
            Bias::Left
        } else {
            Bias::Right
        };
        let mut cursor = self.tree.cursor::<ByteOffset>();
        cursor.seek(&ByteOffset(at), bias);
        cursor.item()?;
        Some((cursor, bias))
    }

    /// Replaces `range` by rewriting the chunks it touches (up to eight,
    /// the first starting at byte `start`) as however many even chunks the
    /// result needs (up to four): overwritten in place, extras spliced in
    /// after, leftovers removed. Then a neighbour that now fits alongside
    /// either end merges into it: once each end is enough, since a merged
    /// chunk is bigger than the old neighbour, which didn't fit with its
    /// own, and even pieces never fit two to one. Whether it did.
    fn replace_chunks(&mut self, range: &Range<usize>, text: &str, start: usize) -> bool {
        // More than these is a big edit: slicing is as good.
        const MOST_OLD: usize = 8;
        const MOST_NEW: usize = 4;
        let mut old = String::new();
        let mut k = 0;
        {
            let mut cursor = self.tree.cursor::<ByteOffset>();
            cursor.seek(&ByteOffset(start), Bias::Right);
            while let Some(chunk) = cursor.item() {
                old.push_str(&chunk.text);
                k += 1;
                if start + old.len() >= range.end {
                    break;
                }
                if k == MOST_OLD {
                    return false;
                }
                cursor.next();
            }
        }
        if k == 0 || start + old.len() < range.end {
            return false;
        }
        for at in [range.start, range.end] {
            assert!(
                old.is_char_boundary(at - start),
                "byte {at} is not a char boundary"
            );
        }
        let mut edited = String::with_capacity(old.len() + text.len());
        edited.push_str(&old[..range.start - start]);
        edited.push_str(text);
        edited.push_str(&old[range.end - start..]);
        let m = edited.len().div_ceil(N);
        if m > MOST_NEW {
            return false;
        }
        let mut pieces: Vec<Chunk<M, N>> = Vec::with_capacity(m);
        let mut rest = edited.as_str();
        for left in (1..=m).rev() {
            let take = if left == 1 {
                rest.len()
            } else {
                floor_boundary(rest, rest.len().div_ceil(left))
            };
            if take == 0 || take > N {
                return false;
            }
            pieces.push(Chunk::new(&rest[..take]));
            rest = &rest[take..];
        }
        // Even pieces of more than a chunk never fit two to one (shaving to
        // char boundaries could only matter for tiny chunks), but check.
        if pieces
            .windows(2)
            .any(|w| fits::<N>(w[0].text.len(), w[1].text.len()))
        {
            return false;
        }

        // Overwrite in order: each piece starts where the last one ended.
        let mut at = start;
        let mut pieces = pieces.into_iter();
        for _ in 0..m.min(k) {
            let piece = pieces.next().expect("m pieces");
            let piece_len = piece.text.len();
            self.tree
                .update_at::<ByteOffset, _>(&ByteOffset(at), Bias::Right, |chunk| *chunk = piece);
            at += piece_len;
        }
        if m > k {
            // The rest go after the last one overwritten.
            self.tree
                .splice_at::<ByteOffset, _, _>(&ByteOffset(at - 1), Bias::Right, |_| {
                    ((), pieces.collect::<Vec<_>>())
                });
        }
        for _ in m..k {
            // Each leftover moves up to where the new text ends.
            self.tree
                .remove_at::<ByteOffset>(&ByteOffset(at), Bias::Right);
        }
        self.merge_at(start);
        self.merge_at(start + edited.len());
        true
    }

    /// Merges the chunks either side of byte `offset`, if it's between two
    /// and they fit in one.
    fn merge_at(&mut self, offset: usize) {
        if offset == 0 || offset >= self.len() {
            return;
        }
        let right = {
            let mut cursor = self.tree.cursor::<ByteOffset>();
            cursor.seek(&ByteOffset(offset), Bias::Left);
            let Some(left) = cursor.item() else {
                return;
            };
            if cursor.start().0 + left.text.len() != offset {
                return;
            }
            let left_len = left.text.len();
            cursor.next();
            match cursor.item() {
                Some(right) if left_len + right.text.len() <= N => right.text,
                _ => return,
            }
        };
        self.tree
            .update_at::<ByteOffset, _>(&ByteOffset(offset), Bias::Left, |chunk| {
                chunk.text.push_str(&right);
            });
        self.tree
            .remove_at::<ByteOffset>(&ByteOffset(offset + right.len()), Bias::Right);
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

    fn check_bounds(&self, range: &Range<usize>) {
        assert!(
            range.start <= range.end && range.end <= self.len(),
            "range {range:?} is out of bounds for a rope of {} bytes",
            self.len()
        );
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

    /// Whether byte `at` falls between `char`s (or at either end), as
    /// [`str::is_char_boundary`]. Past the end, it doesn't.
    pub fn is_char_boundary(&self, at: usize) -> bool {
        if at > self.len() {
            return false;
        }
        let mut cursor = self.tree.cursor::<ByteOffset>();
        cursor.seek(&ByteOffset(at), Bias::Right);
        match cursor.item() {
            Some(chunk) => chunk.as_str().is_char_boundary(at - cursor.start().0),
            None => true,
        }
    }
}

/// The largest `char` boundary in `text` at or below `at`.
/// Whether two chunks of these lengths, both there, would fit in one.
fn fits<const N: usize>(a: usize, b: usize) -> bool {
    a > 0 && b > 0 && a + b <= N
}

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
