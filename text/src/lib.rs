//! Collaborative text: a [`Rope`] that many writers edit at once, kept by
//! mamre's CRDT.
//!
//! A [`Text`] is one writer's replica of a document. The editor edits it in
//! UTF-8 byte offsets, as it would a `str`: [`insert`](Text::insert),
//! [`delete`](Text::delete), [`replace`](Text::replace),
//! [`move_text`](Text::move_text), and sections with
//! [`mark_section`](Text::mark_section). Every edit is recorded in the
//! history as it's made, and returns what it did to the text as
//! [`Edit`]s.
//!
//! Replicas talk in messages: plain bytes, carried however the editor
//! likes (a socket, a relay, a file). [`hello`](Text::hello) says what a
//! replica has; [`receive`](Text::receive) takes a message from another,
//! brings the text up to date, and hands back what to send in answer and
//! what changed, as [`Edit`]s, so the editor can update its layout,
//! carets and undo stack. Messages can arrive late, twice, out of order or
//! not at all: saying hello again now and then is all it takes to catch
//! up. Every replica that has seen the same edits shows the same text.
//!
//! What stays put through other writers' edits: [`Anchor`]s (carets,
//! selections, bookmarks), and sections, by [`SectionId`]. A section is an
//! explicit mark on a character, not syntax in the text, so `#` in code or
//! `\section` in LaTeX mean nothing here.
//!
//! Text can be set in a direction of its own, apart from what's round it:
//! an [`Isolate`], marked with [`mark_isolate`](Text::mark_isolate). It's
//! in the history as the text is, and it isn't text: see `isolates.rs`.
//!
//! [`save`](Text::save) appends to a file; [`open`](Text::open) reads one
//! back, history and all.
//!
//! # What's not here yet
//!
//! - Undo (#14): undo groups come with the editor's needs in hand.
//! - [`Edit::Lost`]: the interface has it, but nothing sends it until
//!   moves become copies (#20). Until then a move made at the same time as
//!   another that crosses it is dropped without a word, and a move of a
//!   range that cuts into a moved block does nothing.
//! - Peers aren't checked for honesty (#9): only talk to ones you trust.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

use mamre_egg_walker::sync::Message;
use mamre_egg_walker::{Accountant, AgentId, Change, Lv, OpLog, Sequence, file};
use mamre_rope::Measure;
pub use mamre_rope::{Plain, Rope, Unit, Units};

mod isolates;
mod piece;
mod units;

mod reconcile;

pub use isolates::{Direction, Isolate, IsolateId};
use piece::Piece;

/// One writer's replica of a document: the text, its history, and the
/// books that keep merges cheap.
///
/// What the text counts beyond bytes, chars and lines is its measure `M`,
/// chosen with its type: a plain `Text` counts nothing more, a
/// `Text<Units>` counts words, sentences and paragraphs (#31), so it can
/// count and seek by them (see [`count`](Text::count)) in O(log n). A
/// document that doesn't use them doesn't pay for bigger summaries.
pub struct Text<M: Measure = Plain> {
    rope: Rope<M>,
    log: OpLog<Piece>,
    books: Accountant,
    me: AgentId,
    /// How many of the history's events the file holds.
    saved: usize,
    /// How many anchors watch each character.
    watchers: HashMap<Lv, usize>,
    /// The sequence's markers, which the rope leaves out: where the
    /// isolates open and close.
    markers: isolates::Markers,
}

/// What an edit did to the text, in byte offsets, in order: each as the
/// text was just before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// `text` went in at `at`.
    Insert { at: usize, text: String },
    /// The bytes in `range` went.
    Delete { range: Range<usize> },
    /// The bytes in `range` moved to `to`, an offset before the move,
    /// outside the range. The same characters: anchors in them go along.
    Move { range: Range<usize>, to: usize },
    /// The character `section` names now starts a section at `level` (0:
    /// none). It's at `at`, if it shows. The text doesn't change.
    Section {
        section: SectionId,
        at: Option<usize>,
        level: u8,
    },
    /// The isolates changed: one was made, removed, or moved by what
    /// another writer did. The text doesn't change. Ask
    /// [`Text::isolates`] what they are now.
    Isolates,
    /// A move or section mark that was discarded: it lost to something
    /// done at the same time. Where its text is now. (Not sent yet: see
    /// "What's not here yet" above.)
    Lost { ranges: Vec<Range<usize>> },
}

/// A place in the text that stays put through every edit, merge and
/// undo: a caret, the ends of a selection, a bookmark. Get one with
/// [`Text::anchor`]; let it go with [`Text::release`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Anchor {
    /// The character just before it, and just after.
    before: Option<Lv>,
    after: Option<Lv>,
}

/// Which character starts a section: its identity, through moves, merges
/// and edits to the heading. Only meaningful to the [`Text`] it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SectionId(Lv);

/// A section, as the text shows it now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    pub id: SectionId,
    /// The byte offset of the character that starts it, or (its heading
    /// deleted) of its place.
    pub at: usize,
    pub level: u8,
    /// Whether its heading shows, and if not, how it's shown (#19).
    pub start: mamre_egg_walker::Start,
}

/// What came of a message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Received {
    /// What to send back to the replica it came from, in order.
    pub reply: Vec<Vec<u8>>,
    /// What changed in the text, in order.
    pub edits: Vec<Edit>,
}

/// Why an edit couldn't be made. Nothing changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextError {
    /// `at` is past the end of the text.
    OutOfBounds { at: usize, len: usize },
    /// `start..end` runs backwards or past the end.
    RangeOutOfBounds {
        start: usize,
        end: usize,
        len: usize,
    },
    /// `at` falls inside a `char`'s UTF-8 encoding.
    NotCharBoundary { at: usize },
    /// There's no section with that id now.
    NoSuchSection(SectionId),
    /// There's no isolate with that id now, or it doesn't reach there.
    NoSuchIsolate(IsolateId),
    /// The range takes part of an isolate and not the rest.
    CrossesIsolate,
}

/// Why a message couldn't be read: damaged, or not one of ours. Nothing
/// changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadMessage;

/// Why a file couldn't be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// It doesn't start like a mamre file.
    NotAFile,
    /// It's a format version this one can't read.
    Version(u8),
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TextError::OutOfBounds { at, len } => {
                write!(f, "byte {at} is past the end of {len} bytes")
            }
            TextError::RangeOutOfBounds { start, end, len } => {
                write!(f, "range {start}..{end} is out of bounds for {len} bytes")
            }
            TextError::NotCharBoundary { at } => write!(f, "byte {at} is not a char boundary"),
            TextError::NoSuchSection(id) => write!(f, "no section {}", id.0),
            TextError::NoSuchIsolate(id) => write!(f, "no isolate {:x} there", id.0),
            TextError::CrossesIsolate => write!(f, "the range takes part of an isolate"),
        }
    }
}

impl std::error::Error for TextError {}

impl fmt::Display for BadMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a damaged or foreign message")
    }
}

impl std::error::Error for BadMessage {}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::NotAFile => write!(f, "not a mamre file"),
            OpenError::Version(v) => write!(f, "format version {v}, which this can't read"),
        }
    }
}

impl std::error::Error for OpenError {}

/// The rope and its markers, edited by the CRDT's changes, noting each
/// in bytes. Positions count items: characters and markers.
struct Applying<'a, M: Measure> {
    rope: &'a mut Rope<M>,
    markers: &'a mut isolates::Markers,
    edits: &'a mut Vec<Edit>,
    /// Whether a marker came, went or moved.
    isolates: bool,
}

impl<M: Measure> Applying<'_, M> {
    /// The byte offset of item position `pos`.
    fn byte(&self, pos: usize) -> usize {
        self.rope.char_to_offset(self.markers.to_char(pos))
    }

    /// Puts `pieces` in at `pos`. Returns the text among them.
    fn put(&mut self, pos: usize, pieces: &[Piece]) -> String {
        let at = self.byte(pos);
        let mut text = String::new();
        for (i, piece) in pieces.iter().enumerate() {
            match *piece {
                Piece::Char(c) => {
                    self.markers.inserted(pos + i, 1);
                    text.push(c);
                }
                marker => {
                    self.markers.insert(pos + i, marker);
                    self.isolates = true;
                }
            }
        }
        self.rope.insert(at, &text);
        text
    }

    /// Takes the items in `range` out. Returns them.
    fn take(&mut self, range: Range<usize>) -> Vec<Piece> {
        let bytes = self.byte(range.start)..self.byte(range.end);
        let mut chars = String::with_capacity(bytes.len());
        for chunk in self.rope.slice(bytes.clone()).chunks() {
            chars.push_str(chunk);
        }
        self.rope.remove(bytes);
        let gone = self.markers.deleted(range.clone());
        self.isolates |= !gone.is_empty();
        let mut chars = chars.chars();
        let mut gone = gone.into_iter().peekable();
        (0..range.len())
            .map(|i| match gone.next_if(|&(at, _)| at == i) {
                Some((_, marker)) => marker,
                None => Piece::Char(chars.next().expect("a character for every other item")),
            })
            .collect()
    }
}

impl<M: Measure> Sequence<Piece> for Applying<'_, M> {
    fn insert(&mut self, pos: usize, items: &[Piece]) {
        let at = self.byte(pos);
        let text = self.put(pos, items);
        if !text.is_empty() {
            self.edits.push(Edit::Insert { at, text });
        }
    }

    fn delete(&mut self, range: Range<usize>) {
        let bytes = self.byte(range.start)..self.byte(range.end);
        self.take(range);
        if !bytes.is_empty() {
            self.edits.push(Edit::Delete { range: bytes });
        }
    }

    fn move_range(&mut self, range: Range<usize>, to: usize) {
        let bytes = self.byte(range.start)..self.byte(range.end);
        let to_byte = self.byte(to);
        let moved = self.take(range.clone());
        let at = if to > range.start {
            to - range.len()
        } else {
            to
        };
        self.put(at, &moved);
        // Markers alone moving, or text moving only past markers, leaves
        // the text as it was.
        if bytes.is_empty() || (bytes.start..=bytes.end).contains(&to_byte) {
            self.isolates = true;
        } else {
            self.edits.push(Edit::Move {
                range: bytes,
                to: to_byte,
            });
        }
    }
}

impl Text {
    /// An empty text, edited by `writer`. Writers must be unique: two
    /// replicas (two devices, two windows) never share a name. A user on a
    /// device, say, `"zach@aperol"`.
    pub fn new(writer: &str) -> Self {
        Text::with_measure(writer)
    }

    /// Opens a file `save` wrote, to be edited by `writer` (see
    /// [`new`](Self::new)). A save cut off partway is read up to where
    /// it's whole.
    pub fn open(writer: &str, bytes: &[u8]) -> Result<Text, OpenError> {
        Text::open_with_measure(writer, bytes)
    }
}

impl<M: Measure> Text<M> {
    /// An empty text counting what `M` counts, edited by `writer`: as
    /// [`Text::new`], say `Text::<Units>::with_measure("me")`.
    pub fn with_measure(writer: &str) -> Self {
        let mut log = OpLog::new();
        let me = log.agent(writer);
        Text {
            rope: Rope::new(),
            log,
            books: Accountant::new(),
            me,
            saved: 0,
            watchers: HashMap::new(),
            markers: isolates::Markers::default(),
        }
    }

    /// The text.
    pub fn rope(&self) -> &Rope<M> {
        &self.rope
    }

    /// Its length in bytes.
    pub fn len(&self) -> usize {
        self.rope.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rope.is_empty()
    }

    /// The text in `range`: a rope of its own, sharing this one's chunks.
    pub fn slice(&self, range: Range<usize>) -> Result<Rope<M>, TextError> {
        self.check_range(&range)?;
        Ok(self.rope.slice(range))
    }

    fn check_at(&self, at: usize) -> Result<(), TextError> {
        if at > self.len() {
            return Err(TextError::OutOfBounds {
                at,
                len: self.len(),
            });
        }
        if !self.rope.is_char_boundary(at) {
            return Err(TextError::NotCharBoundary { at });
        }
        Ok(())
    }

    fn check_range(&self, range: &Range<usize>) -> Result<(), TextError> {
        if range.start > range.end || range.end > self.len() {
            return Err(TextError::RangeOutOfBounds {
                start: range.start,
                end: range.end,
                len: self.len(),
            });
        }
        self.check_at(range.start)?;
        self.check_at(range.end)
    }

    fn chars(&self, range: &Range<usize>) -> Range<usize> {
        self.rope.offset_to_char(range.start)..self.rope.offset_to_char(range.end)
    }

    /// Brings the text and books up to `to` (everything in the history,
    /// after a local event the CRDT decides the effect of), noting edits.
    fn catch_up(&mut self, to: &[Lv]) -> Vec<Edit> {
        let changes = self.books.merge(&self.log, to);
        let mut edits = Vec::new();
        let mut changed = false;
        for change in &changes {
            if let Change::Mark { item, pos, level } = *change {
                let at = pos.map(|p| self.rope.char_to_offset(self.markers.to_char(p)));
                edits.push(Edit::Section {
                    section: SectionId(item),
                    at,
                    level,
                });
                continue;
            }
            let mut doc = Applying {
                rope: &mut self.rope,
                markers: &mut self.markers,
                edits: &mut edits,
                isolates: false,
            };
            change.apply(&mut doc);
            changed |= doc.isolates;
        }
        if changed {
            edits.push(Edit::Isolates);
        }
        edits
    }

    /// Puts `text` in at byte `at`. Where an isolate starts or ends
    /// there, the text goes outside it: [`insert_in`](Self::insert_in)
    /// puts it inside.
    pub fn insert(&mut self, at: usize, text: &str) -> Result<Vec<Edit>, TextError> {
        self.check_at(at)?;
        let c = self.rope.offset_to_char(at);
        let pos = self
            .markers
            .place(c, None)
            .expect("somewhere at every offset");
        Ok(self.insert_at(pos, at, text))
    }

    /// Puts `text` in at item position `pos`, which is byte `at`.
    fn insert_at(&mut self, pos: usize, at: usize, text: &str) -> Vec<Edit> {
        if text.is_empty() {
            return Vec::new();
        }
        self.rope.insert(at, text);
        self.markers.inserted(pos, text.chars().count());
        let v = self.log.add_insert(
            self.me,
            self.books.version(),
            pos,
            text.chars().map(Piece::Char),
        );
        self.books.record(&self.log, &v);
        vec![Edit::Insert {
            at,
            text: text.to_owned(),
        }]
    }

    /// Takes out the text in `range`. An isolate goes with it if all of
    /// the isolate does; one that only starts or ends in the range keeps
    /// what's left of it.
    pub fn delete(&mut self, range: Range<usize>) -> Result<Vec<Edit>, TextError> {
        self.check_range(&range)?;
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let chars = self.chars(&range);
        // Every item from the first marker at the range's start to the
        // last at its end, but for the markers of isolates that reach
        // outside it: runs of items, last first.
        let span = self.markers.first_at(chars.start)..self.markers.last_at(chars.end);
        let within = |pos: usize| span.start <= pos && pos < span.end;
        let mut stay: Vec<usize> = self
            .markers
            .unpaired(&span)
            .into_iter()
            .chain(
                self.markers
                    .shown()
                    .into_iter()
                    .filter(|s| within(s.open) != within(s.close))
                    .map(|s| if within(s.open) { s.open } else { s.close }),
            )
            .collect();
        stay.sort_unstable();
        stay.dedup();
        let mut runs = Vec::new();
        let mut from = span.start;
        for &pos in &stay {
            if pos > from {
                runs.push(from..pos);
            }
            from = pos + 1;
        }
        if span.end > from {
            runs.push(from..span.end);
        }
        let mut isolates = false;
        for run in runs.into_iter().rev() {
            let v = self
                .log
                .add_delete(self.me, self.books.version(), run.clone());
            self.books.record(&self.log, &v);
            isolates |= !self.markers.deleted(run).is_empty();
        }
        self.rope.remove(range.clone());
        let mut edits = vec![Edit::Delete { range }];
        if isolates {
            edits.push(Edit::Isolates);
        }
        Ok(edits)
    }

    /// Replaces the text in `range` with `text`.
    pub fn replace(&mut self, range: Range<usize>, text: &str) -> Result<Vec<Edit>, TextError> {
        self.check_range(&range)?;
        let mut edits = self.delete(range.clone())?;
        edits.extend(self.insert(range.start, text)?);
        Ok(edits)
    }

    /// Moves the text in `range` to byte `to` (an offset before the move):
    /// the same characters, somewhere else, so anchors and sections in
    /// them go along, and someone typing in them at the same time types
    /// where they went. A move to inside itself, or to where it is, does
    /// nothing. The edits say what it did.
    ///
    /// An isolate goes along if all of it moves. Text that leaves its
    /// isolate behind keeps the direction it was set in: it's made an
    /// isolate of that direction where it lands, unless it lands in one.
    pub fn move_text(&mut self, range: Range<usize>, to: usize) -> Result<Vec<Edit>, TextError> {
        self.check_range(&range)?;
        self.check_at(to)?;
        if range.is_empty() || (range.start..=range.end).contains(&to) {
            return Ok(Vec::new());
        }
        let chars = self.chars(&range);
        let stretches = self.markers.stretches(&chars);
        let to = self.rope.offset_to_char(to);
        // Text all from one isolate, put at that isolate's edge, stays
        // in it.
        let home = match stretches.first() {
            Some(&(_, Some((id, _))))
                if stretches
                    .iter()
                    .all(|(_, from)| from.map(|f| f.0) == Some(id)) =>
            {
                Some(id)
            }
            _ => None,
        };
        let to = home
            .and_then(|id| self.markers.place(to, Some(id)))
            .or_else(|| self.markers.place(to, None))
            .expect("somewhere at every offset");
        let there = self.markers.direction_at(to);
        let earlier = stretches.first().is_some_and(|(s, _)| to <= s.start);
        // The last stretch first, each to just before the one moved
        // before it, so they land in order. A marker between two of them
        // stays where it is.
        let mut edits = Vec::new();
        let mut at = to;
        let mut moved = 0;
        for (items, _) in stretches.iter().rev() {
            // Moved earlier, what's gone ahead has pushed this one along.
            let shift = if earlier { moved } else { 0 };
            let items = items.start + shift..items.end + shift;
            let v = self
                .log
                .add_move(self.me, self.books.version(), items.clone(), at);
            edits.extend(self.catch_up(&v));
            moved += items.len();
            if !earlier {
                at -= items.len();
            }
        }
        // They're together now, from `at`. Each that left an isolate
        // behind is made one of its direction, unless it's landed in one:
        // last first again, so the places of the ones before hold.
        let mut end = at + moved;
        let mut wrapped = false;
        for (items, direction) in stretches.iter().rev() {
            let landed = end - items.len()..end;
            end = landed.start;
            let Some((_, direction)) = *direction else {
                continue;
            };
            let text = self.markers.to_char(landed.start) < self.markers.to_char(landed.end);
            if there != Some(direction) && text {
                self.wrap(landed, direction);
                wrapped = true;
            }
        }
        if wrapped && edits.last() != Some(&Edit::Isolates) {
            edits.push(Edit::Isolates);
        }
        Ok(edits)
    }

    /// Makes the text `incoming`, edited outside mamre (#35): its content
    /// is matched against the text (see `reconcile.rs`), what matches keeps
    /// its characters, so their anchors, sections and history, and the
    /// rest is ordinary edits, made here and recorded. A paragraph moved
    /// elsewhere is moved, not deleted and retyped. The edits say what it
    /// did.
    pub fn reconcile(&mut self, incoming: &str) -> Vec<Edit> {
        use reconcile::Piece;
        let old = self.to_string();
        let pieces = reconcile::plan(&old, incoming);
        let mut edits = Vec::new();
        // What goes: what no piece keeps, from the end.
        let mut kept: Vec<Range<usize>> = pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Old(r) => Some(r.clone()),
                Piece::New(_) => None,
            })
            .collect();
        kept.sort_unstable_by_key(|r| r.start);
        let mut gone = Vec::new();
        let mut at = 0;
        for r in &kept {
            if r.start > at {
                gone.push(at..r.start);
            }
            at = r.end;
        }
        if at < old.len() {
            gone.push(at..old.len());
        }
        for r in gone.iter().rev() {
            edits.extend(self.delete(r.clone()).expect("in the text"));
        }
        // The kept pieces as they are now, in the text's order, and as
        // they should be. Those in the longest run in both orders stay;
        // each other goes after the one that should come before it.
        let shift = |p: usize| {
            p - gone
                .iter()
                .filter(|g| g.end <= p)
                .map(|g| g.len())
                .sum::<usize>()
        };
        let now: Vec<(usize, usize)> = kept.iter().map(|r| (shift(r.start), r.len())).collect();
        let want: Vec<usize> = pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Old(r) => Some(
                    kept.binary_search_by_key(&r.start, |k| k.start)
                        .expect("kept"),
                ),
                Piece::New(_) => None,
            })
            .collect();
        let order: Vec<(usize, usize)> = want.iter().enumerate().map(|(t, &k)| (k, t)).collect();
        let stay: std::collections::HashSet<usize> =
            lis_by_first(&order).into_iter().map(|t| want[t]).collect();
        // Where each kept piece is in `now` (by its index in `kept`).
        let mut seq: Vec<usize> = (0..kept.len()).collect();
        for t in 0..want.len() {
            let k = want[t];
            if stay.contains(&k) {
                continue;
            }
            let from = seq.iter().position(|&x| x == k).expect("kept");
            let start: usize = seq[..from].iter().map(|&x| now[x].1).sum();
            let range = start..start + now[k].1;
            // After the piece that should come before it, or first.
            let to = match t.checked_sub(1).map(|p| want[p]) {
                Some(prev) => {
                    let i = seq.iter().position(|&x| x == prev).expect("kept");
                    seq[..=i].iter().map(|&x| now[x].1).sum::<usize>()
                }
                None => 0,
            };
            if to != range.start && to != range.end {
                edits.extend(self.move_text(range.clone(), to).expect("in the text"));
            }
            seq.remove(from);
            let at = match t.checked_sub(1).map(|p| want[p]) {
                Some(prev) => seq.iter().position(|&x| x == prev).expect("kept") + 1,
                None => 0,
            };
            seq.insert(at, k);
        }
        // What comes: the new text, where it goes.
        let mut at = 0;
        for p in &pieces {
            match p {
                Piece::Old(r) => at += r.len(),
                Piece::New(r) => {
                    edits.extend(self.insert(at, &incoming[r.clone()]).expect("in the text"));
                    at += r.len();
                }
            }
        }
        debug_assert_eq!(self.to_string(), incoming);
        edits
    }

    /// Makes the character at byte `at` start a section at `level` (0: no
    /// section). Returns its id, the same through every edit and merge.
    pub fn mark_section(
        &mut self,
        at: usize,
        level: u8,
    ) -> Result<(SectionId, Vec<Edit>), TextError> {
        self.check_at(at)?;
        if at == self.len() {
            return Err(TextError::OutOfBounds {
                at,
                len: self.len(),
            });
        }
        let pos = self.markers.last_at(self.rope.offset_to_char(at));
        let item = self.books.identities().item(pos);
        let v = self.log.add_mark(self.me, self.books.version(), pos, level);
        Ok((SectionId(item), self.catch_up(&v)))
    }

    /// The sections the text shows, in order.
    pub fn sections(&self) -> Vec<Section> {
        self.books
            .sections()
            .into_iter()
            .map(|s| Section {
                id: SectionId(s.item),
                at: self.rope.char_to_offset(self.markers.to_char(s.pos)),
                level: s.level,
                start: s.start,
            })
            .collect()
    }

    /// Where section `id` runs: from its first character to the next
    /// section at its level or above, or the end. `None` if it isn't a
    /// section now: unmarked, or its first character deleted and the
    /// outline's policy folds it (by default a section whose heading is
    /// deleted stays, headless, at its place).
    pub fn section_range(&self, id: SectionId) -> Option<Range<usize>> {
        let r = self.books.section_range(id.0)?;
        let byte = |pos: usize| self.rope.char_to_offset(self.markers.to_char(pos));
        Some(byte(r.start)..byte(r.end))
    }

    /// Moves section `id`, subsections and all, to byte `to`.
    pub fn move_section(&mut self, id: SectionId, to: usize) -> Result<Vec<Edit>, TextError> {
        let range = self.section_range(id).ok_or(TextError::NoSuchSection(id))?;
        self.move_text(range, to)
    }

    fn watch(&mut self, pos: usize) -> Lv {
        let item = self.books.watch(pos);
        *self.watchers.entry(item).or_default() += 1;
        item
    }

    fn unwatch(&mut self, item: Lv) {
        if let Some(n) = self.watchers.get_mut(&item) {
            *n -= 1;
            if *n == 0 {
                self.watchers.remove(&item);
                self.books.unwatch(item);
            }
        }
    }

    /// An anchor at byte `at`: between the characters either side of it,
    /// wherever they go.
    pub fn anchor(&mut self, at: usize) -> Result<Anchor, TextError> {
        self.check_at(at)?;
        let pos = self.rope.offset_to_char(at);
        let chars = self.books.len() - self.markers.len();
        // The characters either side, wherever they are among the items.
        let before = pos
            .checked_sub(1)
            .map(|p| self.watch(self.markers.last_at(p)));
        let after = (pos < chars).then(|| self.watch(self.markers.last_at(pos)));
        Ok(Anchor { before, after })
    }

    /// Where `anchor` is now, in bytes: just before the character that was
    /// after it, or if that's gone, just after the one before; at the start
    /// or the end if it was there and its one neighbour is gone. `None` if
    /// both its characters are deleted (an undo can bring them back).
    pub fn position(&self, anchor: &Anchor) -> Option<usize> {
        let at = |item: Option<Lv>| item.and_then(|i| self.books.anchors().position(i));
        let pos = if let Some(p) = at(anchor.after) {
            self.markers.to_char(p)
        } else if let Some(p) = at(anchor.before) {
            self.markers.to_char(p) + 1
        } else {
            match (anchor.before, anchor.after) {
                // It was at the start: it still is.
                (None, _) => 0,
                // At the end.
                (Some(_), None) => self.books.len() - self.markers.len(),
                (Some(_), Some(_)) => return None,
            }
        };
        Some(self.rope.char_to_offset(pos))
    }

    /// Lets `anchor` go.
    pub fn release(&mut self, anchor: Anchor) {
        for item in [anchor.before, anchor.after].into_iter().flatten() {
            self.unwatch(item);
        }
    }

    /// A message saying what this replica has: send it to another, and it
    /// answers with what this one is missing (and asks for what it is).
    pub fn hello(&self) -> Vec<u8> {
        Message::Summary(self.log.summary()).encode()
    }

    /// Takes a message from another replica: brings the text up to date
    /// with what it has, and says what to send back and what changed. A
    /// message that comes too early (before something it builds on)
    /// changes nothing, and asks again.
    pub fn receive(&mut self, message: &[u8]) -> Result<Received, BadMessage> {
        let message = Message::decode(message).map_err(|_| BadMessage)?;
        let mut out = Received::default();
        match message {
            Message::Summary(theirs) => {
                let missing = self.log.missing(&theirs);
                if !missing.is_empty() {
                    let mut chunk = Vec::new();
                    self.log.encode_events(&missing, &mut chunk);
                    out.reply.push(Message::Events(chunk).encode());
                }
                if !self.log.summary().covers(&theirs) {
                    out.reply.push(self.hello());
                }
            }
            Message::Events(chunk) => {
                if self.log.decode_chunk(&chunk).is_err() {
                    out.reply.push(self.hello());
                    return Ok(out);
                }
                let to = self.log.version().to_vec();
                out.edits = self.catch_up(&to);
            }
        }
        Ok(out)
    }

    /// Appends a save to `file` (begin with an empty `Vec`): whatever
    /// history it doesn't hold yet, and the text as it is now.
    pub fn save(&mut self, file: &mut Vec<u8>) {
        if file.is_empty() {
            *file = file::create();
            self.saved = 0;
        }
        let doc = self.markers.pieces(self.rope.chars());
        self.saved = file::save(file, &self.log, self.saved, doc, &self.books);
    }

    /// Opens a file `save` wrote, counting what `M` counts: as
    /// [`Text::open`]. The measure isn't in the file: open it however the
    /// document wants it counted.
    pub fn open_with_measure(writer: &str, bytes: &[u8]) -> Result<Self, OpenError> {
        let loaded = file::load::<Piece>(bytes).map_err(|e| match e {
            file::LoadError::NotAFile => OpenError::NotAFile,
            file::LoadError::Version(v) => OpenError::Version(v),
        })?;
        let mut log = loaded.log;
        let me = log.agent(writer);
        let mut text = Text {
            rope: Rope::new(),
            log,
            books: Accountant::new(),
            me,
            saved: 0,
            watchers: HashMap::new(),
            markers: isolates::Markers::default(),
        };
        if let Some(snapshot) = loaded.snapshot {
            let mut doc = String::new();
            for (pos, piece) in snapshot.doc.into_iter().enumerate() {
                match piece {
                    Piece::Char(c) => doc.push(c),
                    marker => text.markers.insert(pos, marker),
                }
            }
            text.rope.push_str(&doc);
            text.books = snapshot.accountant;
        }
        let to = text.log.version().to_vec();
        text.catch_up(&to);
        text.saved = text.log.len();
        Ok(text)
    }
}

impl<M: Measure> fmt::Display for Text<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.rope.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

/// The longest run of `pairs` in order by their first: the seconds of its
/// pairs, which must be in order already.
fn lis_by_first(pairs: &[(usize, usize)]) -> Vec<usize> {
    let mut tops: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; pairs.len()];
    for (k, &(i, _)) in pairs.iter().enumerate() {
        let p = tops.partition_point(|&t| pairs[t].0 < i);
        prev[k] = p.checked_sub(1).map(|q| tops[q]);
        if p == tops.len() {
            tops.push(k);
        } else {
            tops[p] = k;
        }
    }
    let mut out = Vec::new();
    let mut at = tops.last().copied();
    while let Some(k) = at {
        out.push(pairs[k].1);
        at = prev[k];
    }
    out.reverse();
    out
}
