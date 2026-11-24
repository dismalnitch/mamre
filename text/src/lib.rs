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
pub use mamre_rope::Rope;

/// One writer's replica of a document: the text, its history, and the
/// books that keep merges cheap.
pub struct Text {
    rope: Rope,
    log: OpLog<char>,
    books: Accountant,
    me: AgentId,
    /// How many of the history's events the file holds.
    saved: usize,
    /// How many anchors watch each character.
    watchers: HashMap<Lv, usize>,
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
    /// The byte offset of the character that starts it.
    pub at: usize,
    pub level: u8,
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

/// The rope, edited by the CRDT's changes, noting each in bytes.
struct Applying<'a> {
    rope: &'a mut Rope,
    edits: &'a mut Vec<Edit>,
}

impl Sequence<char> for Applying<'_> {
    fn insert(&mut self, pos: usize, items: &[char]) {
        let at = self.rope.char_to_offset(pos);
        let text: String = items.iter().collect();
        self.rope.insert(at, &text);
        self.edits.push(Edit::Insert { at, text });
    }

    fn delete(&mut self, range: Range<usize>) {
        let range = self.rope.char_to_offset(range.start)..self.rope.char_to_offset(range.end);
        self.rope.remove(range.clone());
        self.edits.push(Edit::Delete { range });
    }

    fn move_range(&mut self, range: Range<usize>, to: usize) {
        let range = self.rope.char_to_offset(range.start)..self.rope.char_to_offset(range.end);
        let to = self.rope.char_to_offset(to);
        let moved = self.rope.slice(range.clone());
        self.rope.remove(range.clone());
        let at = if to > range.start {
            to - range.len()
        } else {
            to
        };
        let mut text = String::with_capacity(moved.len());
        for chunk in moved.chunks() {
            text.push_str(chunk);
        }
        self.rope.insert(at, &text);
        self.edits.push(Edit::Move { range, to });
    }
}

impl Text {
    /// An empty text, edited by `writer`. Writers must be unique: two
    /// replicas (two devices, two windows) never share a name. A user on a
    /// device, say, `"zach@aperol"`.
    pub fn new(writer: &str) -> Self {
        let mut log = OpLog::new();
        let me = log.agent(writer);
        Text {
            rope: Rope::new(),
            log,
            books: Accountant::new(),
            me,
            saved: 0,
            watchers: HashMap::new(),
        }
    }

    /// The text.
    pub fn rope(&self) -> &Rope {
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
    pub fn slice(&self, range: Range<usize>) -> Result<Rope, TextError> {
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
        for change in &changes {
            if let Change::Mark { item, pos, level } = *change {
                let at = pos.map(|p| self.rope.char_to_offset(p));
                edits.push(Edit::Section {
                    section: SectionId(item),
                    at,
                    level,
                });
                continue;
            }
            let mut doc = Applying {
                rope: &mut self.rope,
                edits: &mut edits,
            };
            change.apply(&mut doc);
        }
        edits
    }

    /// Puts `text` in at byte `at`.
    pub fn insert(&mut self, at: usize, text: &str) -> Result<Vec<Edit>, TextError> {
        self.check_at(at)?;
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let pos = self.rope.offset_to_char(at);
        self.rope.insert(at, text);
        let v = self
            .log
            .add_insert(self.me, self.books.version(), pos, text.chars());
        self.books.record(&self.log, &v);
        Ok(vec![Edit::Insert {
            at,
            text: text.to_owned(),
        }])
    }

    /// Takes out the text in `range`.
    pub fn delete(&mut self, range: Range<usize>) -> Result<Vec<Edit>, TextError> {
        self.check_range(&range)?;
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let chars = self.chars(&range);
        self.rope.remove(range.clone());
        let v = self.log.add_delete(self.me, self.books.version(), chars);
        self.books.record(&self.log, &v);
        Ok(vec![Edit::Delete { range }])
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
    pub fn move_text(&mut self, range: Range<usize>, to: usize) -> Result<Vec<Edit>, TextError> {
        self.check_range(&range)?;
        self.check_at(to)?;
        if range.is_empty() || (range.start..=range.end).contains(&to) {
            return Ok(Vec::new());
        }
        let chars = self.chars(&range);
        let to = self.rope.offset_to_char(to);
        let v = self.log.add_move(self.me, self.books.version(), chars, to);
        Ok(self.catch_up(&v))
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
        let pos = self.rope.offset_to_char(at);
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
                at: self.rope.char_to_offset(s.pos),
                level: s.level,
            })
            .collect()
    }

    /// Where section `id` runs: from its first character to the next
    /// section at its level or above, or the end. `None` if it isn't a
    /// section now (unmarked, or its first character deleted).
    pub fn section_range(&self, id: SectionId) -> Option<Range<usize>> {
        let r = self.books.section_range(id.0)?;
        Some(self.rope.char_to_offset(r.start)..self.rope.char_to_offset(r.end))
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
        let chars = self.books.len();
        let before = pos.checked_sub(1).map(|p| self.watch(p));
        let after = (pos < chars).then(|| self.watch(pos));
        Ok(Anchor { before, after })
    }

    /// Where `anchor` is now, in bytes: just before the character that was
    /// after it, or if that's gone, just after the one before; at the start
    /// or the end if it was there and its one neighbour is gone. `None` if
    /// both its characters are deleted (an undo can bring them back).
    pub fn position(&self, anchor: &Anchor) -> Option<usize> {
        let at = |item: Option<Lv>| item.and_then(|i| self.books.anchors().position(i));
        let pos = if let Some(p) = at(anchor.after) {
            p
        } else if let Some(p) = at(anchor.before) {
            p + 1
        } else {
            match (anchor.before, anchor.after) {
                // It was at the start: it still is.
                (None, _) => 0,
                // At the end.
                (Some(_), None) => self.books.len(),
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
        self.saved = file::save(file, &self.log, self.saved, self.rope.chars(), &self.books);
    }

    /// Opens a file `save` wrote, to be edited by `writer` (see
    /// [`new`](Self::new)). A save cut off partway is read up to where
    /// it's whole.
    pub fn open(writer: &str, bytes: &[u8]) -> Result<Text, OpenError> {
        let loaded = file::load::<char>(bytes).map_err(|e| match e {
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
        };
        if let Some(snapshot) = loaded.snapshot {
            let doc: String = snapshot.doc.into_iter().collect();
            text.rope.push_str(&doc);
            text.books = snapshot.accountant;
        }
        let to = text.log.version().to_vec();
        text.catch_up(&to);
        text.saved = text.log.len();
        Ok(text)
    }
}

impl fmt::Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.rope.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}
