//! The moved blocks a document shows: each active move with something
//! visible in it, and the visible positions it covers. They nest.
//!
//! A merge starting from a document needs them (see `simple.rs`): typing
//! at a block's edge, and whether a move cuts into one, depend on where
//! blocks are, and the walk's base is otherwise a flat run of items. A
//! replica keeps them beside its identities, up to date as changes are
//! made where nothing is concurrent; a merge recomputes them.
//!
//! Where nothing is concurrent, an edit's place among blocks follows from
//! its position, by what shows: an insert at `pos` is inside the blocks
//! that hold items on both sides of it, `start < pos < end`. So typing
//! just after a block's last item, or just before its first, is outside.

use std::ops::Range;

use mamre::codec::{DecodeError, get_usize, put_usize};

use crate::oplog::Lv;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Blocks(pub(crate) Vec<(Lv, Range<usize>)>);

impl Blocks {
    /// `n` items go in at `pos`.
    pub(crate) fn insert(&mut self, pos: usize, n: usize) {
        for (_, b) in &mut self.0 {
            if b.start < pos && pos < b.end {
                b.end += n;
            } else if pos <= b.start {
                b.start += n;
                b.end += n;
            }
        }
    }

    /// The items in `range` go. A block left with nothing showing goes too.
    pub(crate) fn delete(&mut self, range: Range<usize>) {
        let map = |x: usize| {
            if x <= range.start {
                x
            } else if x >= range.end {
                x - range.len()
            } else {
                range.start
            }
        };
        for (_, b) in &mut self.0 {
            *b = map(b.start)..map(b.end);
        }
        self.0.retain(|(_, b)| !b.is_empty());
    }

    /// Move `lv` of the items in `range` to `to` (a position before the
    /// move), made where nothing is concurrent. Returns whether it does
    /// anything: not if it moves nothing, or cuts into a block. A move of
    /// exactly a block takes it over.
    pub(crate) fn apply_move(&mut self, lv: Lv, range: Range<usize>, to: usize) -> bool {
        let r = range;
        if r.is_empty() || (r.start..=r.end).contains(&to) {
            return false;
        }
        let crosses = |b: &Range<usize>| {
            (b.start < r.start && r.start < b.end && b.end < r.end)
                || (r.start < b.start && b.start < r.end && r.end < b.end)
        };
        if self.0.iter().any(|(_, b)| crosses(b)) {
            return false;
        }
        self.0.retain(|(_, b)| *b != r);
        let n = r.len();
        let dest = if to > r.start { to - n } else { to };
        for (_, b) in &mut self.0 {
            if r.start <= b.start && b.end <= r.end {
                // Inside what moves: it goes along.
                *b = b.start - r.start + dest..b.end - r.start + dest;
                continue;
            }
            let was = b.clone();
            // The moved items leave...
            if b.start <= r.start && r.end <= b.end {
                b.end -= n;
            } else if b.start >= r.end {
                b.start -= n;
                b.end -= n;
            }
            // ...and land: inside it if it held items either side of where
            // they went, before it if they went at or before its start.
            if was.start < to && to < was.end {
                b.end += n;
            } else if dest <= b.start {
                b.start += n;
                b.end += n;
            }
        }
        self.0.push((lv, dest..dest + n));
        true
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_usize(out, self.0.len());
        for (lv, b) in &self.0 {
            put_usize(out, *lv);
            put_usize(out, b.start);
            put_usize(out, b.len());
        }
    }

    /// Reads blocks [`encode`](Self::encode) wrote, for a log of `events`
    /// events and a document of `len` items.
    pub(crate) fn decode(
        input: &mut &[u8],
        events: usize,
        len: usize,
    ) -> Result<Self, DecodeError> {
        let n = get_usize(input)?;
        let mut blocks = Vec::with_capacity(n.min(input.len()));
        for _ in 0..n {
            let lv = get_usize(input)?;
            let start = get_usize(input)?;
            let end = start
                .checked_add(get_usize(input)?)
                .ok_or(DecodeError::Overflow)?;
            if lv >= events || start >= end || end > len {
                return Err(DecodeError::Invalid("a block this document can't have"));
            }
            blocks.push((lv, start..end));
        }
        Ok(Blocks(blocks))
    }
}
