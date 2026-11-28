//! Isolates: stretches of text set in a direction of their own, apart
//! from the text round them. A Hebrew phrase in an English paragraph is
//! one; so can be the English word inside the phrase, nested in it.
//!
//! An isolate is in the history, so it's saved, sent and merged as the
//! text is. It's two markers in the sequence the CRDT keeps: one where it
//! opens, one where it closes (see `piece.rs`). The markers aren't text,
//! and the rope never holds them. What's between them is in the isolate,
//! whatever its script; nothing re-reads the characters to decide.
//!
//! Where a marker sits, one byte offset is more than one place in the
//! sequence: just inside the isolate, or just outside. [`Text::insert`]
//! puts text outside every isolate that only starts or ends there;
//! [`Text::insert_in`] says which one it goes in. That choice costs
//! nothing in the history: it's which side of a marker the text went.
//!
//! mamre has no opinion about which stretches should be isolates. An
//! editor finds them (by Unicode's bidirectional algorithm, say, or by
//! what the writer does) and marks them with [`Text::mark_isolate`].
//!
//! # What's rough
//!
//! - Markers are kept in a list in order, and isolates are worked out
//!   from it each time they're asked for: fine for a page, not for a
//!   book. They want a tree of their own.
//! - Deleting text takes an isolate with it only if all of the isolate
//!   goes. One left with nothing in it by another writer's delete isn't
//!   shown, and its markers stay.
//! - Two isolates made at the same time over part of the same text cross.
//!   The one that opens first is shown; the other is lost
//!   ([`Text::lost_isolates`]) until an editor does something about it.
//! - Text moved out of no isolate into one takes that one's direction:
//!   mamre doesn't know what direction a paragraph gives its text.
//! - Nothing stops an isolate running over a line end.
//! - Two directions. Top to bottom is a direction too.

use std::collections::HashMap;
use std::ops::Range;

use mamre_rope::Measure;

use crate::piece::Piece;
use crate::{Edit, Text, TextError};

/// Which way text runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    LeftToRight,
    RightToLeft,
}

/// Which isolate: the same on every replica, through every edit, merge
/// and save.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IsolateId(pub(crate) u64);

/// An isolate, as the text shows it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Isolate {
    pub id: IsolateId,
    /// The bytes it covers.
    pub range: Range<usize>,
    pub direction: Direction,
    /// The isolate it's nested in; `None` if it's straight in the text.
    pub parent: Option<IsolateId>,
}

/// A marker, and where it is among the sequence's items.
#[derive(Clone, Copy, Debug)]
struct Marker {
    pos: usize,
    piece: Piece,
}

/// The isolate a stretch of items is in, and its direction: none if it's
/// in none.
pub(crate) type Home = Option<(u64, Direction)>;

/// An isolate among the items: its markers' positions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Shown {
    pub(crate) id: u64,
    pub(crate) open: usize,
    pub(crate) close: usize,
    direction: Direction,
    parent: Option<u64>,
}

impl Shown {
    /// Whether an item put in at `pos` would be inside it.
    fn holds(&self, pos: usize) -> bool {
        self.open < pos && pos <= self.close
    }
}

/// The sequence's markers, in order: what the rope leaves out. With it, a
/// position among the items (characters and markers) and a position among
/// the characters turn into each other.
#[derive(Clone, Debug, Default)]
pub(crate) struct Markers {
    all: Vec<Marker>,
}

impl Markers {
    pub(crate) fn len(&self) -> usize {
        self.all.len()
    }

    /// How many markers come before item position `pos`.
    fn before(&self, pos: usize) -> usize {
        self.all.partition_point(|m| m.pos < pos)
    }

    /// The character position of item position `pos`: the characters
    /// before it.
    pub(crate) fn to_char(&self, pos: usize) -> usize {
        pos - self.before(pos)
    }

    /// How many markers sit between characters at a place `at` says yes
    /// to: marker `k` sits at `pos - k`, and those only grow.
    fn count(&self, at: impl Fn(usize) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.all.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if at(self.all[mid].pos - mid) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// The first item position at character position `c`: before every
    /// marker there.
    pub(crate) fn first_at(&self, c: usize) -> usize {
        c + self.count(|at| at < c)
    }

    /// The last item position at character position `c`: after every
    /// marker there, so where character `c` itself is.
    pub(crate) fn last_at(&self, c: usize) -> usize {
        c + self.count(|at| at <= c)
    }

    /// `n` items went in at `pos`.
    pub(crate) fn inserted(&mut self, pos: usize, n: usize) {
        let k = self.before(pos);
        for m in &mut self.all[k..] {
            m.pos += n;
        }
    }

    /// Marker `piece` went in at `pos`.
    pub(crate) fn insert(&mut self, pos: usize, piece: Piece) {
        self.inserted(pos, 1);
        let k = self.before(pos);
        self.all.insert(k, Marker { pos, piece });
    }

    /// The items in `range` went. Returns the markers among them, each
    /// with how far into the range it was.
    pub(crate) fn deleted(&mut self, range: Range<usize>) -> Vec<(usize, Piece)> {
        let (a, b) = (self.before(range.start), self.before(range.end));
        let gone = self
            .all
            .drain(a..b)
            .map(|m| (m.pos - range.start, m.piece))
            .collect();
        for m in &mut self.all[a..] {
            m.pos -= range.len();
        }
        gone
    }

    /// The items that characters `chars` and the isolates wholly among
    /// them come to, as the stretches to move: from the first character to
    /// the last, widened over the markers of each isolate that opens and
    /// closes within, and cut at every other marker, which stays where it
    /// is. With each stretch, the isolate it's in and leaving behind, if
    /// it's in one, and that isolate's direction.
    pub(crate) fn stretches(&self, chars: &Range<usize>) -> Vec<(Range<usize>, Home)> {
        let shown = self.shown();
        let (mut lo, mut hi) = (self.last_at(chars.start), self.first_at(chars.end));
        let (first, last) = (self.first_at(chars.start), self.last_at(chars.end));
        loop {
            let opens = (lo > first)
                .then(|| shown.iter().find(|s| s.open == lo - 1))
                .flatten();
            let closes = (hi < last)
                .then(|| shown.iter().find(|s| s.close == hi))
                .flatten();
            match (opens, closes) {
                (Some(s), _) if s.close < hi => lo -= 1,
                (_, Some(s)) if s.open >= lo => hi += 1,
                (Some(a), Some(b)) if a.id == b.id => {
                    lo -= 1;
                    hi += 1;
                }
                _ => break,
            }
        }
        let within = |pos: usize| lo <= pos && pos < hi;
        let goes = |s: &Shown| within(s.open) && within(s.close);
        // A marker in the range stays unless its whole isolate goes.
        let cuts: Vec<usize> = self.all[self.before(lo)..self.before(hi)]
            .iter()
            .filter(|m| {
                !shown
                    .iter()
                    .any(|s| goes(s) && (s.open == m.pos || s.close == m.pos))
            })
            .map(|m| m.pos)
            .collect();
        let mut out = Vec::new();
        let mut from = lo;
        for cut in cuts.into_iter().chain([hi]) {
            if cut > from {
                // The innermost isolate holding the stretch that isn't
                // going with it.
                let left = shown
                    .iter()
                    .filter(|s| !goes(s) && s.open < from && cut <= s.close)
                    .min_by_key(|s| s.close - s.open);
                out.push((from..cut, left.map(|s| (s.id, s.direction))));
            }
            from = cut + 1;
        }
        out
    }

    /// The direction of the innermost isolate an item put in at `pos`
    /// would be inside.
    pub(crate) fn direction_at(&self, pos: usize) -> Option<Direction> {
        self.shown()
            .iter()
            .filter(|s| s.holds(pos))
            .min_by_key(|s| s.close - s.open)
            .map(|s| s.direction)
    }

    /// The positions in `span` of markers that make no isolate: halves
    /// without the other, or of one that isn't shown.
    pub(crate) fn unpaired(&self, span: &Range<usize>) -> Vec<usize> {
        let shown = self.shown();
        self.all[self.before(span.start)..self.before(span.end)]
            .iter()
            .filter(|m| !shown.iter().any(|s| s.open == m.pos || s.close == m.pos))
            .map(|m| m.pos)
            .collect()
    }

    /// Every piece of the sequence, in order, given its characters.
    pub(crate) fn pieces<'a>(
        &'a self,
        chars: impl Iterator<Item = char> + 'a,
    ) -> impl Iterator<Item = Piece> + 'a {
        let mut chars = chars;
        let mut next = 0;
        let mut pos = 0;
        std::iter::from_fn(move || {
            let piece = match self.all.get(next) {
                Some(m) if m.pos == pos => {
                    next += 1;
                    m.piece
                }
                _ => Piece::Char(chars.next()?),
            };
            pos += 1;
            Some(piece)
        })
    }

    /// The pieces at item positions `items`, given `chars`, the characters
    /// among them, in order.
    pub(crate) fn pieces_in(
        &self,
        items: Range<usize>,
        chars: impl Iterator<Item = char>,
    ) -> Vec<Piece> {
        let mut chars = chars;
        let mut next = self.before(items.start);
        items
            .map(|pos| match self.all.get(next) {
                Some(m) if m.pos == pos => {
                    next += 1;
                    m.piece
                }
                _ => Piece::Char(chars.next().expect("a character at every other position")),
            })
            .collect()
    }

    /// The isolates the markers make, in order of where they open, each
    /// before the ones nested in it; and the ones that lost. A marker
    /// without its other half, or a pair in the wrong order, makes none.
    /// An isolate that crosses one before it (half in, half out) isn't
    /// shown: it's lost.
    fn resolve(&self) -> (Vec<Shown>, Vec<Shown>) {
        let mut closes: HashMap<u64, usize> = HashMap::new();
        for m in &self.all {
            if let Piece::Close(id) = m.piece {
                closes.entry(id).or_insert(m.pos);
            }
        }
        let mut out: Vec<Shown> = Vec::new();
        let mut lost = Vec::new();
        // The isolates still open, innermost last: indices into `out`.
        let mut open: Vec<usize> = Vec::new();
        for m in &self.all {
            let Piece::Open(direction, id) = m.piece else {
                continue;
            };
            let Some(&close) = closes.get(&id) else {
                continue;
            };
            if close < m.pos || out.iter().chain(&lost).any(|s| s.id == id) {
                continue;
            }
            while open.last().is_some_and(|&i| out[i].close < m.pos) {
                open.pop();
            }
            let parent = open.last().map(|&i| out[i]);
            let this = Shown {
                id,
                open: m.pos,
                close,
                direction,
                parent: parent.map(|p| p.id),
            };
            if parent.is_some_and(|p| close > p.close) {
                lost.push(this);
                continue;
            }
            open.push(out.len());
            out.push(this);
        }
        (out, lost)
    }

    pub(crate) fn shown(&self) -> Vec<Shown> {
        self.resolve().0
    }

    /// The isolates that are whole but cross one before them, so aren't
    /// shown: each with the one it crosses as its parent.
    pub(crate) fn lost(&self) -> Vec<Shown> {
        self.resolve().1
    }

    /// Where among the items text goes at character position `c`, to be
    /// in isolate `inside` and no deeper; or with `None`, outside every
    /// isolate that only starts or ends there. `None` if `inside` doesn't
    /// reach `c`.
    pub(crate) fn place(&self, c: usize, inside: Option<u64>) -> Option<usize> {
        let (first, last) = (self.first_at(c), self.last_at(c));
        if first == last && inside.is_none() {
            return Some(first);
        }
        let shown = self.shown();
        let depth = |pos: usize| shown.iter().filter(|s| s.holds(pos)).count();
        let within = |pos: usize| match inside {
            Some(id) => shown.iter().any(|s| s.id == id && s.holds(pos)),
            None => true,
        };
        (first..=last)
            .filter(|&pos| within(pos))
            .min_by_key(|&pos| depth(pos))
    }
}

/// An id for a new isolate: this writer's name and how much history it
/// had, hashed (FNV-1a). No two of a writer's isolates share the second,
/// and writers don't share names.
fn new_id(writer: &str, events: usize) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in writer.bytes().chain([0xff]).chain(events.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

impl<M: Measure> Text<M> {
    /// The isolates the text shows, in order of where they start, each
    /// before the ones nested in it.
    pub fn isolates(&self) -> Vec<Isolate> {
        self.as_isolates(self.markers.shown())
    }

    /// The isolates that aren't shown because they cross another: made
    /// at the same time as one that takes part of the same text, and
    /// opening after it. Each is where its markers are, with the isolate
    /// it crosses as its `parent`. An editor that hears
    /// [`Edit::Isolates`] can look here, and tell its writer, mark the
    /// text again, or [`remove_isolate`](Self::remove_isolate) what's
    /// left.
    pub fn lost_isolates(&self) -> Vec<Isolate> {
        self.as_isolates(self.markers.lost())
    }

    fn as_isolates(&self, shown: Vec<Shown>) -> Vec<Isolate> {
        let byte = |pos: usize| self.rope.char_to_offset(self.markers.to_char(pos));
        shown
            .into_iter()
            .map(|s| Isolate {
                id: IsolateId(s.id),
                range: byte(s.open)..byte(s.close),
                direction: s.direction,
                parent: s.parent.map(IsolateId),
            })
            .filter(|i| !i.range.is_empty())
            .collect()
    }

    /// Makes the text in `range` an isolate set in `direction`: the same
    /// one on every replica from now on. If the range is exactly an
    /// isolate's already, that isolate is set in `direction` instead, and
    /// keeps its id.
    ///
    /// An empty range is out of bounds for one, and a range that takes
    /// part of an isolate and not the rest is
    /// [`TextError::CrossesIsolate`].
    pub fn mark_isolate(
        &mut self,
        range: Range<usize>,
        direction: Direction,
    ) -> Result<(IsolateId, Vec<Edit>), TextError> {
        self.stepped(|t| t.mark_isolate_step(range, direction))
    }

    fn mark_isolate_step(
        &mut self,
        range: Range<usize>,
        direction: Direction,
    ) -> Result<(IsolateId, Vec<Edit>), TextError> {
        self.check_range(&range)?;
        if range.is_empty() {
            return Err(TextError::RangeOutOfBounds {
                start: range.start,
                end: range.end,
                len: self.len(),
            });
        }
        let chars = self.chars(&range);
        // Just before its first character, and just after its last.
        let open = self.markers.last_at(chars.start);
        let close = self.markers.first_at(chars.end);
        let taken = |pos: usize| open <= pos && pos < close;
        let shown = self.markers.shown();
        if shown.iter().any(|s| taken(s.open) != taken(s.close)) {
            return Err(TextError::CrossesIsolate);
        }
        // The isolate wrapped tightly round these very characters.
        if let Some(same) = shown
            .iter()
            .find(|s| s.open + 1 == open && s.close == close)
        {
            if same.direction == direction {
                return Ok((IsolateId(same.id), Vec::new()));
            }
            // A marker says its direction for good: a new one goes in its
            // place, with its id.
            self.take_marker(same.open);
            self.put_marker(same.open, Piece::Open(direction, same.id));
            return Ok((IsolateId(same.id), vec![Edit::Isolates]));
        }
        Ok((
            IsolateId(self.wrap(open..close, direction)),
            vec![Edit::Isolates],
        ))
    }

    /// Makes isolate `id` no isolate: its text stays, in whatever it was
    /// nested in. A lost one ([`lost_isolates`](Self::lost_isolates)) can
    /// be removed too.
    pub fn remove_isolate(&mut self, id: IsolateId) -> Result<Vec<Edit>, TextError> {
        self.stepped(|t| t.remove_isolate_step(id))
    }

    fn remove_isolate_step(&mut self, id: IsolateId) -> Result<Vec<Edit>, TextError> {
        let (shown, lost) = (self.markers.shown(), self.markers.lost());
        let s = shown
            .iter()
            .chain(&lost)
            .find(|s| s.id == id.0)
            .ok_or(TextError::NoSuchIsolate(id))?;
        self.take_marker(s.close);
        self.take_marker(s.open);
        Ok(vec![Edit::Isolates])
    }

    /// Puts `text` in at byte `at`, inside isolate `isolate` and no
    /// deeper: at its very start or end, where [`insert`](Self::insert)
    /// would put it outside. [`TextError::NoSuchIsolate`] if the isolate
    /// doesn't reach `at`.
    pub fn insert_in(
        &mut self,
        at: usize,
        text: &str,
        isolate: IsolateId,
    ) -> Result<Vec<Edit>, TextError> {
        self.stepped(|t| t.insert_in_step(at, text, isolate))
    }

    fn insert_in_step(
        &mut self,
        at: usize,
        text: &str,
        isolate: IsolateId,
    ) -> Result<Vec<Edit>, TextError> {
        self.check_at(at)?;
        let c = self.rope.offset_to_char(at);
        let pos = self
            .markers
            .place(c, Some(isolate.0))
            .ok_or(TextError::NoSuchIsolate(isolate))?;
        Ok(self.insert_at(pos, at, text))
    }

    /// Makes the items in `items` a new isolate. Returns its id.
    pub(crate) fn wrap(&mut self, items: Range<usize>, direction: Direction) -> u64 {
        let id = new_id(self.log.agent_name(self.me), self.log.len());
        // The close first, so the open's position is still good.
        self.put_marker(items.end, Piece::Close(id));
        self.put_marker(items.start, Piece::Open(direction, id));
        id
    }

    fn take_marker(&mut self, pos: usize) {
        let v = self
            .log
            .add_delete(self.me, self.books.version(), pos..pos + 1);
        self.books.record(&self.log, &v);
        self.markers.deleted(pos..pos + 1);
    }

    fn put_marker(&mut self, pos: usize, piece: Piece) {
        let v = self
            .log
            .add_insert(self.me, self.books.version(), pos, [piece]);
        self.books.record(&self.log, &v);
        self.markers.insert(pos, piece);
    }
}
