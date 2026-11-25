//! The outline: which items start sections, at what level, and where they
//! are. Kept by the [`Accountant`](crate::Accountant) from the changes it
//! hands out.
//!
//! A section is an explicit mark on an item (see `Op::Mark`), not anything
//! in the text, so headings need no syntax: `#` in code or `\section` in
//! LaTeX mean nothing here. The section's identity is its item's, through
//! moves, merges, edits to the heading and undo. It runs from its item to
//! the next marked item at the same level or above, or to the end.
//!
//! # A deleted heading (#19)
//!
//! When a section's heading item is deleted, what the section becomes is
//! the author's call, so the outline makes the choice it must and says so.
//! By default the section stays, **headless**, at the deleted heading's
//! place (its anchor falls back: see `anchors.rs`), with its own level. A
//! [`Policy`] per case ([`Next`]: what follows the heading) can instead carry
//! the section on from the next text that shows, or fold it into the
//! section above. Either way an [`Ambiguity::HeadingDeleted`] is reported,
//! once, saying which choice was made; when it's resolved (the heading
//! restored, the section unmarked, or a new heading marked at its place),
//! an [`Ambiguity::Resolved`] says how.
//!
//! The outline is derived from the books, not document state: replicas
//! can hold different policies, and text and marks still converge. A
//! heading's place is this replica's: one deleted before this replica ever
//! showed it has no place, and isn't in the outline until it's restored.
//! Policies are the replica's settings, so they aren't saved with the books.

use std::collections::BTreeMap;
use std::ops::Range;

use mamre::codec::{DecodeError, get_usize, put_usize};

use crate::anchors::Anchors;
use crate::oplog::Lv;
use crate::walk::Change;

/// A section, as the document shows it now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    /// The item that starts it: its identity.
    pub item: Lv,
    /// Where that item is, or (its heading deleted) where it was.
    pub pos: usize,
    pub level: u8,
    /// Whether its heading shows, and if not, how it's shown.
    pub start: Start,
}

/// How a section starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Start {
    /// At its heading item, which shows.
    Heading,
    /// Its heading item is deleted: it starts at its place, headless.
    Headless,
    /// Its heading item is deleted: it carries on from the next text that
    /// shows, which stands as its heading.
    CarriedOn,
}

/// What follows a deleted heading: the first item that still shows after
/// its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Next {
    /// Plain text, in no section of its own.
    Text,
    /// A heading at the section's level or above: the section is empty.
    Heading,
    /// A heading below the section's level: a subsection.
    Subsection,
    /// Nothing: the heading was the document's last item.
    End,
}

/// What the outline makes of a section whose heading is deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Policy {
    /// Keep it, headless, at the heading's place, with its own level: the
    /// forced default, since it's deterministic and hides nothing.
    #[default]
    Headless,
    /// Carry it on from the next text that still shows.
    CarryOn,
    /// Fold it into the section above: it leaves the outline.
    Fold,
}

/// A condition the outline reports: something a merge or a local edit did
/// that the author may want to decide on. Not an error: a merge can't fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ambiguity {
    /// Section `section`'s heading item was deleted. `next` is what
    /// followed it then, and `chose` the policy that applied.
    HeadingDeleted {
        section: Lv,
        level: u8,
        next: Next,
        chose: Policy,
    },
    /// The ambiguity about `section` is over.
    Resolved { section: Lv, by: Resolution },
}

/// How a deleted heading's ambiguity ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The heading shows again: an undo, here or elsewhere.
    Restored,
    /// The section was unmarked.
    Unmarked,
    /// A new heading was marked at its place: `by` starts a section there
    /// now, and the headless one leaves the outline (it comes back with
    /// its heading, if that's ever restored).
    Replaced { by: Lv },
}

/// The policy for each case of [`Next`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Policies {
    pub text: Policy,
    pub heading: Policy,
    pub subsection: Policy,
    pub end: Policy,
}

impl Policies {
    pub fn get(&self, next: Next) -> Policy {
        match next {
            Next::Text => self.text,
            Next::Heading => self.heading,
            Next::Subsection => self.subsection,
            Next::End => self.end,
        }
    }

    pub fn set(&mut self, next: Next, policy: Policy) {
        match next {
            Next::Text => self.text = policy,
            Next::Heading => self.heading = policy,
            Next::Subsection => self.subsection = policy,
            Next::End => self.end = policy,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Outline {
    /// Every marked item's level, shown or not: a deleted one keeps its
    /// mark, for when an undo brings it back.
    levels: BTreeMap<Lv, u8>,
    /// Where the marked items are, or were. Its own anchors, so nothing
    /// else watching or unwatching an item touches them; they fall back,
    /// so a deleted heading keeps its place.
    anchors: Anchors,
    pub(crate) policies: Policies,
    /// Reported, not yet taken.
    reports: Vec<Ambiguity>,
}

impl Default for Outline {
    fn default() -> Self {
        Outline {
            levels: BTreeMap::new(),
            anchors: Anchors::falling_back(0),
            policies: Policies::default(),
            reports: Vec::new(),
        }
    }
}

impl Outline {
    pub(crate) fn apply<T>(&mut self, change: &Change<'_, T>) {
        match *change {
            Change::Mark { item, pos, level } => {
                let was = self.levels.get(&item).copied();
                if level == 0 {
                    if was.is_some() && self.anchors.is_ghost(item) {
                        self.resolved(item, Resolution::Unmarked);
                    }
                    self.levels.remove(&item);
                    self.anchors.unwatch(item);
                    return;
                }
                self.levels.insert(item, level);
                match pos {
                    Some(pos) => {
                        self.anchors.watch(pos, item);
                        // A new heading where a headless section is.
                        if was.is_none() {
                            for ghost in self.anchors.ghosts_at(pos) {
                                if self.levels.contains_key(&ghost) {
                                    self.resolved(ghost, Resolution::Replaced { by: item });
                                    self.anchors.forget_place(ghost);
                                }
                            }
                        }
                    }
                    None => self.anchors.watch_deleted(item),
                }
            }
            _ => {
                let (deleted, back) = self.anchors.apply_noting(change);
                for item in back {
                    if self.levels.contains_key(&item) {
                        self.resolved(item, Resolution::Restored);
                    }
                }
                for item in deleted {
                    if let Some(&level) = self.levels.get(&item) {
                        let next = self.next(item, level);
                        self.reports.push(Ambiguity::HeadingDeleted {
                            section: item,
                            level,
                            next,
                            chose: self.policies.get(next),
                        });
                    }
                }
            }
        }
    }

    fn resolved(&mut self, section: Lv, by: Resolution) {
        self.reports.push(Ambiguity::Resolved { section, by });
    }

    /// Takes what's been reported since the last time.
    pub(crate) fn take_reports(&mut self) -> Vec<Ambiguity> {
        std::mem::take(&mut self.reports)
    }

    /// What follows deleted heading `item` (at `level`) now.
    fn next(&self, item: Lv, level: u8) -> Next {
        let place = self.anchors.place_of(item).expect("a ghost has a place");
        if place >= self.anchors.len() {
            return Next::End;
        }
        match self
            .anchors
            .item_at(place)
            .and_then(|i| self.levels.get(&i))
        {
            None => Next::Text,
            Some(&l) if l <= level => Next::Heading,
            Some(_) => Next::Subsection,
        }
    }

    /// An item's level: 0 if it starts no section.
    pub(crate) fn level(&self, item: Lv) -> u8 {
        self.levels.get(&item).copied().unwrap_or(0)
    }

    /// The sections whose heading is deleted but which keep its place,
    /// folded or not: each an open [`Ambiguity::HeadingDeleted`].
    pub(crate) fn headless(&self) -> Vec<Lv> {
        self.levels
            .keys()
            .copied()
            .filter(|&item| self.anchors.is_ghost(item))
            .collect()
    }

    /// The sections the document shows, in order.
    pub(crate) fn sections(&self) -> Vec<Section> {
        let mut sections: Vec<Section> = self
            .levels
            .iter()
            .filter_map(|(&item, &level)| {
                let pos = self.anchors.place_of(item)?;
                let start = if !self.anchors.is_ghost(item) {
                    Start::Heading
                } else {
                    match self.policies.get(self.next(item, level)) {
                        Policy::Headless => Start::Headless,
                        Policy::CarryOn => Start::CarriedOn,
                        Policy::Fold => return None,
                    }
                };
                Some(Section {
                    item,
                    pos,
                    level,
                    start,
                })
            })
            .collect();
        // A deleted heading's place is before the item there.
        sections.sort_by_key(|s| (s.pos, s.start == Start::Heading));
        sections
    }

    /// Where the section `item` starts runs to: the next section at its
    /// level or above, or the end. `None` if it isn't a section now.
    pub(crate) fn range(&self, item: Lv) -> Option<Range<usize>> {
        let sections = self.sections();
        let k = sections.iter().position(|s| s.item == item)?;
        let me = sections[k];
        let end = sections[k + 1..]
            .iter()
            .find(|s| s.level <= me.level)
            .map_or(self.anchors.len(), |s| s.pos);
        Some(me.pos..end)
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_usize(out, self.levels.len());
        let mut prev = 0;
        for (&item, &level) in &self.levels {
            put_usize(out, item - prev);
            put_usize(out, level as usize);
            prev = item;
        }
        self.anchors.encode(out);
    }

    pub(crate) fn decode(input: &mut &[u8], events: usize) -> Result<Self, DecodeError> {
        let n = get_usize(input)?;
        let mut levels = BTreeMap::new();
        let mut prev: usize = 0;
        for _ in 0..n {
            let item = prev
                .checked_add(get_usize(input)?)
                .ok_or(DecodeError::Overflow)?;
            let level = get_usize(input)?;
            if item >= events || level == 0 || level > u8::MAX as usize {
                return Err(DecodeError::Invalid("a mark this log can't have"));
            }
            levels.insert(item, level as u8);
            prev = item;
        }
        let mut anchors = Anchors::decode(input, events)?;
        anchors.fall_back();
        Ok(Outline {
            levels,
            anchors,
            ..Outline::default()
        })
    }
}
