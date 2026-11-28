//! Following moved text (#68): what's watched on a character goes with it
//! when a move carries it somewhere else.
//!
//! A move is a delete and a paste of what it deleted, in one step, and the
//! paste's lineage (#36) pairs each copy with the character it carries on.
//! The merge never reads lineage; this does.
//! A watched character that's deleted follows its copies: if exactly one
//! of them shows (or one of theirs, for text moved again), that's where it
//! is now. Every replica works it out from the log alone, so none can
//! disagree, and it needs nothing written per anchor: an anchor made in
//! the range at the same moment as the move follows too.
//!
//! Undoing the move brings the originals back as themselves (undo revives
//! deleted items) and deletes the copies, so what's watched is found on
//! the originals again, exactly. A paste on its own moves nothing: the
//! originals still show, so nothing follows it. Cut and pasted later, in
//! two steps, it's a move like any other.
//!
//! To know where a copy is, it's watched too, for as long as anything
//! watched has it among its copies. The record of copies is kept in runs,
//! as the log keeps it: a moved chapter costs a few entries, not one a
//! character. It's rebuilt from the log after a load, as ranges are.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::Range;

use crate::anchors::Anchors;
use crate::ids::Identities;
use crate::oplog::{Copied, Lv, OpLog};

/// Where a watched item is, following moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Follow {
    /// It shows, at this position.
    Here(usize),
    /// It's deleted, and a move carried it here: its one copy that shows.
    Moved(usize),
    /// It's deleted, and more than one of its copies shows (two writers
    /// moved it at once, say): these positions, in order. It goes with
    /// none of them until it's put somewhere.
    Ambiguous(Vec<usize>),
    /// It's deleted, and no copy of it shows.
    Gone,
}

/// Something pinned to text in the log (#85).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Pin {
    /// A comment, by the event that added it.
    Comment(Lv),
    /// A range the author defined, by the event that added it.
    Range(Lv),
}

/// How a move touched something pinned to text (#85): what the editor
/// asks about (#79). mamre's own answer, until the editor re-pins it, is to
/// follow: one wholly inside goes with the text, the others stay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Touch {
    /// All of it was in the moved text.
    Inside,
    /// Some of it was, and some wasn't: it overlaps the moved text, or
    /// holds it.
    Partly,
    /// None of it was, but it ends where the moved text began or begins
    /// where it ended.
    Edge,
}

/// Something pinned to text, and how a move touched it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Touched {
    pub pin: Pin,
    pub touch: Touch,
}

/// How moving `stretches` (positions, sorted and apart) touches `span`.
pub(crate) fn touch(span: &Range<usize>, stretches: &[Range<usize>]) -> Option<Touch> {
    if span.is_empty() {
        // Nothing of its own: at most at an edge.
        let at = span.start;
        return stretches
            .iter()
            .any(|s| s.start <= at && at <= s.end)
            .then_some(Touch::Edge);
    }
    if stretches.iter().any(|s| s.start <= span.start && span.end <= s.end) {
        Some(Touch::Inside)
    } else if stretches.iter().any(|s| s.start < span.end && span.start < s.end) {
        Some(Touch::Partly)
    } else if stretches.iter().any(|s| s.end == span.start || span.end == s.start) {
        Some(Touch::Edge)
    } else {
        None
    }
}

/// Where a moved item is now: its one copy that shows, if a move carried
/// it (#68). `None` if it shows itself, or no single copy does.
pub(crate) type Moved<'a> = &'a dyn Fn(Lv) -> Option<usize>;

/// Moves' copies, in runs, and why each item is watched.
#[derive(Clone, Default)]
pub(crate) struct Moves {
    /// Each run of copies, by its first copy.
    by_copy: BTreeMap<Lv, Copied>,
    /// Each run of copies, by its first source. Two moves made at once can
    /// copy the same sources, in runs that overlap.
    by_source: BTreeMap<Lv, Vec<Copied>>,
    /// The longest run: how far back a run holding a source can start.
    longest: usize,
    /// Items watched for their own sake.
    direct: HashSet<Lv>,
    /// Ends of ranges and comments: watched for as long as the books are,
    /// apart from what's watched for its own sake (#85).
    pinned: HashSet<Lv>,
    /// How many reasons each watched item has: being watched for its own
    /// sake, and each watched item it's a copy of.
    reasons: BTreeMap<Lv, usize>,
    /// Read from a file: rebuilt from the log before it's next used.
    pub(crate) stale: bool,
}

impl Moves {
    pub(crate) fn stale() -> Self {
        Moves {
            stale: true,
            ..Moves::default()
        }
    }

    fn add_run(&mut self, run: Copied) {
        self.by_copy.insert(run.copy, run);
        self.by_source.entry(run.source).or_default().push(run);
        self.longest = self.longest.max(run.len);
    }

    /// The runs holding `source` among their sources.
    fn runs_from(&self, source: Lv) -> impl Iterator<Item = &Copied> {
        let from = (source + 1).saturating_sub(self.longest);
        self.by_source
            .range(from..=source)
            .flat_map(|(_, runs)| runs)
            .filter(move |r| source < r.source + r.len)
    }

    /// The source copy `item` came from, if a move made it.
    pub(crate) fn source(&self, item: Lv) -> Option<Lv> {
        let (_, r) = self.by_copy.range(..=item).next_back()?;
        (item < r.copy + r.len).then(|| r.source + (item - r.copy))
    }

    /// The copies moves made of `item`.
    pub(crate) fn copies_of(&self, item: Lv) -> Vec<Lv> {
        self.runs_from(item).map(|r| r.copy + (item - r.source)).collect()
    }

    /// Watches `item`, the one at `pos` now, for its own sake.
    pub(crate) fn watch(&mut self, anchors: &mut Anchors, ids: &Identities, pos: usize, item: Lv) {
        if self.direct.insert(item) {
            self.add_reason(anchors, ids, item, Some(pos));
        }
    }

    /// Watches `item`, an end of a range or a comment, for as long as the
    /// books are kept: wherever it is, or deleted.
    pub(crate) fn pin(&mut self, anchors: &mut Anchors, ids: &Identities, item: Lv) {
        if self.pinned.insert(item) {
            let pos = ids.positions(&BTreeSet::from([item])).first().map(|&(_, p)| p);
            self.add_reason(anchors, ids, item, pos);
        }
    }

    /// Stops watching `item` for its own sake.
    pub(crate) fn unwatch(&mut self, anchors: &mut Anchors, item: Lv) {
        if self.direct.remove(&item) {
            self.remove_reason(anchors, item);
        }
    }

    /// One more reason to watch `item`, at `pos` (`None`: deleted), and so
    /// its copies.
    fn add_reason(&mut self, anchors: &mut Anchors, ids: &Identities, item: Lv, pos: Option<usize>) {
        let n = self.reasons.entry(item).or_insert(0);
        *n += 1;
        if *n > 1 {
            return;
        }
        match pos {
            Some(pos) => anchors.watch(pos, item),
            None => anchors.watch_deleted(item),
        }
        // Its copies, theirs, and so on: newly watched, placed in one pass
        // (text that was never moved has none, and costs nothing).
        let mut new = BTreeSet::new();
        let mut todo = self.copies_of(item);
        while let Some(x) = todo.pop() {
            let n = self.reasons.entry(x).or_insert(0);
            *n += 1;
            if *n == 1 {
                new.insert(x);
                todo.extend(self.copies_of(x));
            }
        }
        place(anchors, ids, &new);
    }

    fn remove_reason(&mut self, anchors: &mut Anchors, item: Lv) {
        let mut todo = vec![item];
        while let Some(x) = todo.pop() {
            let n = self
                .reasons
                .get_mut(&x)
                .expect("an item that's watched has a reason");
            *n -= 1;
            if *n == 0 {
                self.reasons.remove(&x);
                anchors.unwatch(x);
                todo.extend(self.copies_of(x));
            }
        }
    }

    /// Takes in newly merged copies, `runs`: they're watched wherever
    /// their sources are.
    pub(crate) fn take_in(&mut self, runs: &[Copied], anchors: &mut Anchors, ids: &Identities) {
        let mut new = BTreeSet::new();
        for &run in runs {
            self.add_run(run);
            // The watched among its sources: each gives its copy a reason,
            // from it.
            let watched: Vec<Lv> = self
                .reasons
                .range(run.source..run.source + run.len)
                .map(|(&s, _)| s)
                .collect();
            for s in watched {
                let c = run.copy + (s - run.source);
                let n = self.reasons.entry(c).or_insert(0);
                *n += 1;
                if *n == 1 {
                    new.insert(c);
                }
            }
        }
        place(anchors, ids, &new);
    }

    /// Rebuilds the copies from every move in the log up to `version`.
    /// Nothing's watched for its own sake after a load, and nothing's
    /// pinned until the books pin ranges' and comments' ends again.
    pub(crate) fn rebuild<T: Clone>(&mut self, log: &OpLog<T>, version: &[Lv]) {
        *self = Moves::default();
        let holds = log.closure(version);
        for run in log.copied(0..log.len()) {
            for run in Copied::runs(run.pairs().filter(|&(c, _)| holds[c])) {
                self.add_run(run);
            }
        }
    }

    /// Where watched `item` is, following moves.
    pub(crate) fn follow(&self, anchors: &Anchors, item: Lv) -> Follow {
        if let Some(pos) = anchors.position(item) {
            return Follow::Here(pos);
        }
        let mut found = Vec::new();
        let mut todo = self.copies_of(item);
        while let Some(x) = todo.pop() {
            match anchors.position(x) {
                Some(pos) => found.push(pos),
                None => todo.extend(self.copies_of(x)),
            }
        }
        found.sort_unstable();
        found.dedup();
        match found.len() {
            0 => Follow::Gone,
            1 => Follow::Moved(found[0]),
            _ => Follow::Ambiguous(found),
        }
    }
}

/// Watches each of `items`: where it shows, or deleted (it comes back where
/// an undo or redo puts it).
fn place(anchors: &mut Anchors, ids: &Identities, items: &BTreeSet<Lv>) {
    if items.is_empty() {
        return;
    }
    let shown = ids.positions(items);
    let mut left: BTreeSet<Lv> = items.clone();
    for (item, pos) in shown {
        anchors.watch(pos, item);
        left.remove(&item);
    }
    for item in left {
        anchors.watch_deleted(item);
    }
}
