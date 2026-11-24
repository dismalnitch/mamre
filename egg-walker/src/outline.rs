//! The outline: which items start sections, at what level, and where they
//! are. Kept by the [`Accountant`](crate::Accountant) from the changes it
//! hands out.
//!
//! A section is an explicit mark on an item (see `Op::Mark`), not anything
//! in the text, so headings need no syntax: `#` in code or `\section` in
//! LaTeX mean nothing here. The section's identity is its item's, through
//! moves, merges, edits to the heading and undo. It runs from its item to
//! the next marked item at the same level or above, or to the end.

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
    /// Where that item is.
    pub pos: usize,
    pub level: u8,
}

#[derive(Clone, Default)]
pub(crate) struct Outline {
    /// Every marked item's level, shown or not: a deleted one keeps its
    /// mark, for when an undo brings it back.
    levels: BTreeMap<Lv, u8>,
    /// Where the marked items are. Its own anchors, so nothing else
    /// watching or unwatching an item touches them.
    anchors: Anchors,
}

impl Outline {
    pub(crate) fn apply<T>(&mut self, change: &Change<'_, T>) {
        match *change {
            Change::Mark { item, pos, level } => {
                if level == 0 {
                    self.levels.remove(&item);
                    self.anchors.unwatch(item);
                    return;
                }
                self.levels.insert(item, level);
                match pos {
                    Some(pos) => self.anchors.watch(pos, item),
                    None => self.anchors.watch_deleted(item),
                }
            }
            _ => self.anchors.apply(change),
        }
    }

    /// An item's level: 0 if it starts no section.
    pub(crate) fn level(&self, item: Lv) -> u8 {
        self.levels.get(&item).copied().unwrap_or(0)
    }

    /// The sections the document shows, in order.
    pub(crate) fn sections(&self) -> Vec<Section> {
        let mut sections: Vec<Section> = self
            .levels
            .iter()
            .filter_map(|(&item, &level)| {
                let pos = self.anchors.position(item)?;
                Some(Section { item, pos, level })
            })
            .collect();
        sections.sort_by_key(|s| s.pos);
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
        let anchors = Anchors::decode(input, events)?;
        Ok(Outline { levels, anchors })
    }
}
