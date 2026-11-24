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
//!
//! # Hidden edges
//!
//! What a block shows isn't all of it. Its interval can start with records
//! that show nothing: deleted items, or items a move has carried off and
//! draws elsewhere. The base a merge starts from has none of those, so a
//! block keeps whether it has some at each end: its *head* and *tail*.
//! Precisely, a block's head is hidden when its first record isn't where
//! its first visible element starts (an element of a block: an item of
//! its own, or a block inside it), and its tail likewise.
//!
//! It matters in two places. Two nested blocks can show exactly the same
//! items, the outer one holding hidden records the inner one doesn't: they
//! are still two. And a move of exactly a block's visible items takes it
//! over only if the block has no hidden edges; otherwise the move's
//! interval is inside the block's, and nests there.
//!
//! When a merge lays out its base, each hidden edge gets a *marker*: a
//! record of no items, never shown, standing for what's hidden there, so
//! the block's interval starts (or ends) at it. Blocks showing the same
//! items keep their nesting by order: an outer block always comes before
//! any it holds.

use std::cmp::Reverse;
use std::ops::Range;

use mamre::codec::{DecodeError, get_usize, put_usize};

use crate::oplog::Lv;
use crate::walk::{BASE_ITEM, UNDERWATER};

/// The ids of a base's markers start here: past every base item.
pub(crate) const MARKER: usize = BASE_ITEM + UNDERWATER + 1;

/// Which ends of a block's interval hide records: before its first
/// visible element (`head`), after its last (`tail`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Edges {
    pub(crate) head: bool,
    pub(crate) tail: bool,
}

impl Edges {
    /// Neither end hides anything.
    pub(crate) const NONE: Edges = Edges {
        head: false,
        tail: false,
    };

    /// These ends hide something now too: an edge, once hidden, stays so.
    fn hide(&mut self, more: Edges) {
        self.head |= more.head;
        self.tail |= more.tail;
    }

    fn to_bits(self) -> usize {
        self.head as usize | (self.tail as usize) << 1
    }

    fn from_bits(bits: usize) -> Option<Self> {
        (bits <= 3).then_some(Edges {
            head: bits & 1 != 0,
            tail: bits & 2 != 0,
        })
    }
}

/// One moved block: its move, the visible positions it covers, and which
/// of its ends hide records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Block {
    pub(crate) lv: Lv,
    pub(crate) span: Range<usize>,
    pub(crate) hidden: Edges,
}

impl Block {
    pub(crate) fn new(lv: Lv, span: Range<usize>, hidden: Edges) -> Self {
        Block { lv, span, hidden }
    }
}

/// A document's blocks, each before any it holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Blocks(pub(crate) Vec<Block>);

/// A piece of a merge's base, in order: a run of the base document's
/// items, or a marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Piece {
    Items(Range<usize>),
    Marker(usize),
}

/// A merge's base, laid out: its pieces, and each block's move with the
/// first and last record of its interval (a base item, by `BASE_ITEM` plus
/// its position, or a marker), in the blocks' order.
#[derive(Clone, Debug)]
pub(crate) struct Laid {
    pub(crate) pieces: Vec<Piece>,
    pub(crate) moves: Vec<(Lv, usize, usize)>,
}

/// How the blocks nest.
struct Nesting {
    /// Each block's first and last element, if it's a block: the outermost
    /// block inside it starting where it starts, or ending where it ends.
    first_child: Vec<Option<usize>>,
    last_child: Vec<Option<usize>>,
    /// The blocks outermost first, starting at the same place.
    order: Vec<usize>,
    depth: Vec<usize>,
}

impl Blocks {
    fn nesting(&self) -> Nesting {
        let b = &self.0;
        let n = b.len();
        // By start, the longer first, and of the same span, the outer: the
        // one that comes first.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&k| (b[k].span.start, Reverse(b[k].span.end), k));
        let mut parent = vec![None; n];
        let mut depth = vec![0; n];
        let mut stack: Vec<usize> = Vec::new();
        for &k in &order {
            while stack
                .last()
                .is_some_and(|&t| b[t].span.end <= b[k].span.start)
            {
                stack.pop();
            }
            parent[k] = stack.last().copied();
            depth[k] = stack.len();
            stack.push(k);
        }
        let mut first_child = vec![None; n];
        let mut last_child = vec![None; n];
        for &k in &order {
            if let Some(p) = parent[k] {
                if b[k].span.start == b[p].span.start && first_child[p].is_none() {
                    first_child[p] = Some(k);
                }
                if b[k].span.end == b[p].span.end && last_child[p].is_none() {
                    last_child[p] = Some(k);
                }
            }
        }
        Nesting {
            first_child,
            last_child,
            order,
            depth,
        }
    }

    /// Whether block `k`'s first element (or last, `!front`) loses all it
    /// shows to `gone`: the visible positions an edit removes from where
    /// they are, deleted or moved off.
    fn element_lost(&self, nest: &Nesting, k: usize, gone: &Range<usize>, front: bool) -> bool {
        let b = &self.0[k];
        let child = if front {
            nest.first_child[k]
        } else {
            nest.last_child[k]
        };
        match child {
            Some(c) => {
                let c = &self.0[c].span;
                gone.start <= c.start && c.end <= gone.end
            }
            None => {
                let p = if front { b.span.start } else { b.span.end - 1 };
                gone.contains(&p)
            }
        }
    }

    /// The ends of each block whose first or last element loses all it
    /// shows to `gone` (see `element_lost`): they hide something now. Only
    /// blocks `gone` reaches into can lose one, so how blocks nest is
    /// worked out only if there are some.
    fn edges_lost(&self, gone: &Range<usize>) -> Vec<Edges> {
        let reached = |b: &Block| b.span.start < gone.end && gone.start < b.span.end;
        if !self.0.iter().any(reached) {
            return vec![Edges::NONE; self.0.len()];
        }
        let nest = self.nesting();
        (0..self.0.len())
            .map(|k| Edges {
                head: self.element_lost(&nest, k, gone, true),
                tail: self.element_lost(&nest, k, gone, false),
            })
            .collect()
    }

    /// `n` items go in at `pos`. Only inside a block, never at its edge,
    /// so no edge changes.
    pub(crate) fn insert(&mut self, pos: usize, n: usize) {
        for Block { span: b, .. } in &mut self.0 {
            if b.start < pos && pos < b.end {
                b.end += n;
            } else if pos <= b.start {
                b.start += n;
                b.end += n;
            }
        }
    }

    /// The items in `range` go. A block whose first (or last) element
    /// shows nothing now has a hidden edge there; a block left with
    /// nothing showing goes.
    pub(crate) fn delete(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let lost = self.edges_lost(&range);
        let map = |x: usize| {
            if x <= range.start {
                x
            } else if x >= range.end {
                x - range.len()
            } else {
                range.start
            }
        };
        for (b, lost) in self.0.iter_mut().zip(lost) {
            b.span = map(b.span.start)..map(b.span.end);
            b.hidden.hide(lost);
        }
        self.0.retain(|b| !b.span.is_empty());
    }

    /// Move `lv` of the items in `range` to `to` (a position before the
    /// move), made where nothing is concurrent. Returns whether it does
    /// anything: not if it moves nothing, or cuts into a block.
    ///
    /// A move of exactly a block's items takes it over if the block has
    /// no hidden edges; if it has, the move's interval is the block's
    /// visible elements, inside it. Either way the block shows nothing
    /// after, so it goes, and the new block takes its place. A block whose
    /// first (or last) element moves off has a hidden edge there now.
    pub(crate) fn apply_move(&mut self, lv: Lv, range: Range<usize>, to: usize) -> bool {
        let r = range;
        if r.is_empty() || (r.start..=r.end).contains(&to) {
            return false;
        }
        let crosses = |b: &Range<usize>| {
            (b.start < r.start && r.start < b.end && b.end < r.end)
                || (r.start < b.start && b.start < r.end && r.end < b.end)
        };
        if self.0.iter().any(|b| crosses(&b.span)) {
            return false;
        }
        let nest = self.nesting();
        // The innermost block of exactly these items, if any.
        let exact = nest
            .order
            .iter()
            .copied()
            .filter(|&k| self.0[k].span == r)
            .last();
        let lost = self.edges_lost(&r);
        let n = r.len();
        let dest = if to > r.start { to - n } else { to };
        // Where the new block goes: before any it holds, after any holding
        // it.
        let inside = |b: &Range<usize>| r.start <= b.start && b.end <= r.end && *b != r;
        let at = match exact {
            Some(k) => k,
            None => self
                .0
                .iter()
                .position(|b| inside(&b.span))
                .unwrap_or(self.0.len()),
        };
        for (k, b) in self.0.iter_mut().enumerate() {
            let s = &mut b.span;
            if inside(s) {
                // Inside what moves: it goes along, as it is.
                *s = s.start - r.start + dest..s.end - r.start + dest;
                continue;
            }
            let was = s.clone();
            // The moved items leave...
            if s.start <= r.start && r.end <= s.end {
                s.end -= n;
                b.hidden.hide(lost[k]);
            } else if s.start >= r.end {
                s.start -= n;
                s.end -= n;
            }
            // ...and land: inside it if it held items either side of where
            // they went, before it if they went at or before its start.
            if was.start < to && to < was.end {
                s.end += n;
            } else if dest <= s.start {
                s.start += n;
                s.end += n;
            }
        }
        // Blocks of exactly the moved items are left with nothing: in
        // their place, the new block.
        self.0
            .insert(at, Block::new(lv, dest..dest + n, Edges::NONE));
        self.0.retain(|b| !b.span.is_empty());
        true
    }

    /// The base a merge from this document starts with: the document's
    /// items, cut wherever a block starts or ends (and its last item on its
    /// own, so it can be an origin), with a marker at each hidden edge.
    /// Between two items, the markers of blocks ending there come first,
    /// innermost first, then those of blocks starting there, outermost
    /// first.
    pub(crate) fn lay(&self) -> Laid {
        let b = &self.0;
        let nest = self.nesting();
        let mut cuts: Vec<usize> = b
            .iter()
            .flat_map(|x| [x.span.start, x.span.end - 1, x.span.end])
            .collect();
        cuts.push(0);
        cuts.sort_unstable();
        cuts.dedup();
        cuts.push(UNDERWATER);
        // Each block's hidden edges' markers, by where they go.
        let mut heads: Vec<(usize, usize, usize)> = Vec::new();
        let mut tails: Vec<(usize, Reverse<usize>, usize)> = Vec::new();
        for (k, x) in b.iter().enumerate() {
            if x.hidden.head {
                heads.push((x.span.start, nest.depth[k], k));
            }
            if x.hidden.tail {
                tails.push((x.span.end, Reverse(nest.depth[k]), k));
            }
        }
        heads.sort_unstable();
        tails.sort_unstable();
        let mut head_marker = vec![None; b.len()];
        let mut tail_marker = vec![None; b.len()];
        let mut pieces = Vec::new();
        let mut next_marker = MARKER;
        let (mut h, mut t) = (0, 0);
        for w in cuts.windows(2) {
            let at = w[0];
            while t < tails.len() && tails[t].0 == at {
                tail_marker[tails[t].2] = Some(next_marker);
                pieces.push(Piece::Marker(next_marker));
                next_marker += 1;
                t += 1;
            }
            while h < heads.len() && heads[h].0 == at {
                head_marker[heads[h].2] = Some(next_marker);
                pieces.push(Piece::Marker(next_marker));
                next_marker += 1;
                h += 1;
            }
            pieces.push(Piece::Items(w[0]..w[1]));
        }
        // A block's first record: its marker, or its first element's.
        fn first(k: usize, b: &[Block], nest: &Nesting, marker: &[Option<usize>]) -> usize {
            match (marker[k], nest.first_child[k]) {
                (Some(m), _) => m,
                (None, Some(c)) => first(c, b, nest, marker),
                (None, None) => BASE_ITEM + b[k].span.start,
            }
        }
        fn last(k: usize, b: &[Block], nest: &Nesting, marker: &[Option<usize>]) -> usize {
            match (marker[k], nest.last_child[k]) {
                (Some(m), _) => m,
                (None, Some(c)) => last(c, b, nest, marker),
                (None, None) => BASE_ITEM + b[k].span.end - 1,
            }
        }
        let moves = (0..b.len())
            .map(|k| {
                (
                    b[k].lv,
                    first(k, b, &nest, &head_marker),
                    last(k, b, &nest, &tail_marker),
                )
            })
            .collect();
        Laid { pieces, moves }
    }

    /// The blocks in the order they're compared in: by start, the longer
    /// first, and of the same span, the outer first.
    pub(crate) fn sorted(&self) -> Vec<Block> {
        let nest = self.nesting();
        nest.order.iter().map(|&k| self.0[k].clone()).collect()
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_usize(out, self.0.len());
        for b in &self.0 {
            put_usize(out, b.lv);
            put_usize(out, b.span.start);
            put_usize(out, b.span.len());
            put_usize(out, b.hidden.to_bits());
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
            let hidden = Edges::from_bits(get_usize(input)?);
            let Some(hidden) = hidden.filter(|_| lv < events && start < end && end <= len) else {
                return Err(DecodeError::Invalid("a block this document can't have"));
            };
            blocks.push(Block::new(lv, start..end, hidden));
        }
        Ok(Blocks(blocks))
    }
}
