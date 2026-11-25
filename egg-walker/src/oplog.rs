//! The operation log: every edit any replica has made, as events in a graph.
//!
//! An event is one insert or delete of one item, at a position in the
//! document *as it was at the event's parents*: plain positions, no IDs in
//! the document. Or it undoes a delete: an *undelete* names the delete it
//! undoes, and an item is visible while every delete of it is undone. Undo
//! and redo never make new items, so anything keyed to an item (an anchor)
//! finds it again. Its parents are the events it came after, so concurrent
//! edits are siblings. Events are numbered locally (`Lv`) in the order this
//! log learned them, which is always a topological order: parents first.
//!
//! Across replicas an event is named by its agent and sequence number.
//!
//! Events are stored as runs: one agent's consecutive events, each right
//! after the last, typing forward or deleting forward, are one entry with
//! their items, not one event (and one list of parents) per item. That's
//! how a document's whole history stays small. Single events are rebuilt
//! when asked for; nothing about their numbering changes.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::ops::Range;

/// What an event does, without its details.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Insert,
    Delete,
    Undelete,
    Move,
    Mark,
    Undo,
    /// Adding or setting an author's range (#32): not the text's.
    Range,
}

/// A local version: an event's number in this log. Parents always have
/// smaller numbers than their children.
pub type Lv = usize;

/// An agent, numbered in this log. Agents are named; names are what other
/// logs know them by, and what concurrent inserts are ordered by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentId(u32);

impl AgentId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// What an event does, to one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op<T> {
    /// Puts `item` in at `pos`.
    Insert { pos: usize, item: T },
    /// Removes the item at `pos`.
    Delete { pos: usize },
    /// Undoes delete `target`, an earlier event this one includes. Once any
    /// undelete of a delete exists, that delete is undone for good; a redo
    /// is a new delete.
    Undelete { target: Lv },
    /// Moves the `len` items at `pos` to `to` (a position in the document
    /// as it is, outside them or at either end), keeping their identities:
    /// the same items, somewhere else. See `reference.rs` for what a move
    /// does when others edit or move at the same time.
    Move { pos: usize, len: usize, to: usize },
    /// Marks the item at `pos` as starting a section at `level`; level 0
    /// unmarks it. The mark is the item's, wherever it goes; the latest
    /// mark of an item wins (see `reference.rs`).
    Mark { pos: usize, level: u8 },
    /// Undoes `target`, an earlier event this one includes: a move, or an
    /// undo of one (#30). A move is cancelled while an undo of it is in
    /// force: its originals come back where they were, its copies go. An
    /// undo is in force unless an undo of it is: so a redo is an undo of
    /// the undo, and brings back the same copies. (Deletes are undone by
    /// [`Op::Undelete`]; in a chunk, both are kind 2.)
    Undo { target: Lv },
    /// Adds a range the author defines (#32): from item `start` to item
    /// `end` (both included; insert events this one includes), tagged
    /// `tag`. The range is this event. It sits beside the text, not in it:
    /// the walk passes over it.
    RangeAdd { start: Lv, end: Lv, tag: String },
    /// Sets range `range`'s tag (`None`: removed). The latest set of a
    /// range wins, in Lamport time, then the order concurrent inserts use.
    RangeSet { range: Lv, tag: Option<String> },
}

/// One event, as a log hands it out.
#[derive(Clone, Debug)]
pub struct Event<T> {
    pub agent: AgentId,
    /// This agent's count of events before this one.
    pub seq: u64,
    /// The events this one came after: the version its position refers to.
    pub parents: Vec<Lv>,
    pub op: Op<T>,
}

/// One agent's consecutive events, each right after the last: the first
/// has `parents`, every other has the one before it.
#[derive(Clone, Debug)]
pub(crate) struct Run<T> {
    pub(crate) agent: AgentId,
    pub(crate) seq: u64,
    pub(crate) start: Lv,
    pub(crate) parents: Vec<Lv>,
    pub(crate) op: RunOp<T>,
    /// The first event's Lamport time; each after it is one more.
    pub(crate) lamport: u64,
}

#[derive(Clone, Debug)]
pub(crate) enum RunOp<T> {
    /// Item `k` goes in at `pos + k`: typing forward.
    Insert {
        pos: usize,
        items: Vec<T>,
    },
    /// `len` items each removed at `pos`: deleting forward.
    Delete {
        pos: usize,
        len: usize,
    },
    /// Deletes `target..target + len` undone, in order: undoing a run of
    /// deletes.
    Undelete {
        target: Lv,
        len: usize,
    },
    /// One move: moves don't run together.
    Move {
        pos: usize,
        len: usize,
        to: usize,
    },
    /// One mark: marks don't run together either.
    Mark {
        pos: usize,
        level: u8,
    },
    /// One undo of a move, or of an undo of one.
    Undo {
        target: Lv,
    },
    /// One range added, or set.
    RangeAdd {
        start: Lv,
        end: Lv,
        tag: String,
    },
    RangeSet {
        range: Lv,
        tag: Option<String>,
    },
}

impl<T> Run<T> {
    pub(crate) fn len(&self) -> usize {
        match &self.op {
            RunOp::Insert { items, .. } => items.len(),
            RunOp::Delete { len, .. } | RunOp::Undelete { len, .. } => *len,
            RunOp::Move { .. }
            | RunOp::Mark { .. }
            | RunOp::Undo { .. }
            | RunOp::RangeAdd { .. }
            | RunOp::RangeSet { .. } => 1,
        }
    }

    pub(crate) fn end(&self) -> Lv {
        self.start + self.len()
    }
}

/// Every event a replica knows, in a graph.
#[derive(Clone, Debug)]
pub struct OpLog<T> {
    pub(crate) agents: Vec<String>,
    by_name: HashMap<String, AgentId>,
    pub(crate) next_seq: Vec<u64>,
    /// In local-version order, covering every event.
    pub(crate) runs: Vec<Run<T>>,
    /// Per agent, by the seq each stretch starts at: where in this log the
    /// agent's events are, as `(first local version, how many)`.
    pub(crate) spans: Vec<BTreeMap<u64, (Lv, usize)>>,
    /// The events nothing else came after: the version of everything.
    frontier: Vec<Lv>,
    /// Per agent, the seqs where a step ends (#14): the events from just
    /// after one end to the next are one step, undone as one. An agent's
    /// events after its last end are an open step.
    pub(crate) step_ends: Vec<BTreeSet<u64>>,
    /// Per agent, how deep in [`begin_group`](Self::begin_group)s it is
    /// here: this replica's, never sent.
    depth: Vec<u32>,
}

impl<T> Default for OpLog<T> {
    fn default() -> Self {
        OpLog {
            agents: Vec::new(),
            by_name: HashMap::new(),
            next_seq: Vec::new(),
            runs: Vec::new(),
            spans: Vec::new(),
            frontier: Vec::new(),
            step_ends: Vec::new(),
            depth: Vec::new(),
        }
    }
}

impl<T: Clone> OpLog<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// The agent with this name, added if new.
    pub fn agent(&mut self, name: &str) -> AgentId {
        if let Some(&id) = self.by_name.get(name) {
            return id;
        }
        let id = AgentId(self.agents.len() as u32);
        self.agents.push(name.to_string());
        self.by_name.insert(name.to_string(), id);
        self.next_seq.push(0);
        self.spans.push(BTreeMap::new());
        self.step_ends.push(BTreeSet::new());
        self.depth.push(0);
        id
    }

    /// Event `lv`.
    pub fn event(&self, lv: Lv) -> Event<T> {
        let run = self.run_of(lv);
        let k = lv - run.start;
        Event {
            agent: run.agent,
            seq: run.seq + k as u64,
            parents: self.parents(lv).collect(),
            op: self.op(lv),
        }
    }

    /// The items of inserts `first..first + len`, if they're one run here:
    /// typed one after another, so they lie side by side.
    pub(crate) fn items(&self, first: Lv, len: usize) -> Option<&[T]> {
        let run = self.run_of(first);
        let k = first - run.start;
        match &run.op {
            RunOp::Insert { items, .. } => items.get(k..k + len),
            RunOp::Delete { .. }
            | RunOp::Undelete { .. }
            | RunOp::Move { .. }
            | RunOp::Mark { .. }
            | RunOp::Undo { .. }
            | RunOp::RangeAdd { .. }
            | RunOp::RangeSet { .. } => None,
        }
    }

    /// Just the operation of event `lv`.
    pub(crate) fn op(&self, lv: Lv) -> Op<T> {
        let run = self.run_of(lv);
        let k = lv - run.start;
        match &run.op {
            RunOp::Insert { pos, items } => Op::Insert {
                pos: pos + k,
                item: items[k].clone(),
            },
            RunOp::Delete { pos, .. } => Op::Delete { pos: *pos },
            RunOp::Undelete { target, .. } => Op::Undelete { target: target + k },
            &RunOp::Move { pos, len, to } => Op::Move { pos, len, to },
            &RunOp::Mark { pos, level } => Op::Mark { pos, level },
            &RunOp::Undo { target } => Op::Undo { target },
            RunOp::RangeAdd { start, end, tag } => Op::RangeAdd {
                start: *start,
                end: *end,
                tag: tag.clone(),
            },
            RunOp::RangeSet { range, tag } => Op::RangeSet {
                range: *range,
                tag: tag.clone(),
            },
        }
    }

    /// Puts `items` in at `pos`, in a document at `parents`, as `agent`:
    /// one event per item, each after the last. Returns the version after.
    pub fn add_insert(
        &mut self,
        agent: AgentId,
        parents: &[Lv],
        pos: usize,
        items: impl IntoIterator<Item = T>,
    ) -> Vec<Lv> {
        let mut version = normalize(parents.to_vec());
        for (k, item) in items.into_iter().enumerate() {
            let seq = self.next_seq[agent.0 as usize];
            let lv = self.push(agent, seq, version, Op::Insert { pos: pos + k, item });
            version = vec![lv];
        }
        version
    }

    /// Removes the items in `range`, in a document at `parents`, as `agent`:
    /// one event per item. Returns the version after.
    pub fn add_delete(&mut self, agent: AgentId, parents: &[Lv], range: Range<usize>) -> Vec<Lv> {
        let mut version = normalize(parents.to_vec());
        for _ in range.clone() {
            let seq = self.next_seq[agent.0 as usize];
            // Each delete leaves the next item where this one was.
            let lv = self.push(agent, seq, version, Op::Delete { pos: range.start });
            version = vec![lv];
        }
        version
    }

    /// Undoes `deletes`, in a document at `parents`, as `agent`: one
    /// undelete each, in order. Each must be a delete `parents` includes.
    /// Returns the version after.
    pub fn add_undelete(
        &mut self,
        agent: AgentId,
        parents: &[Lv],
        deletes: impl IntoIterator<Item = Lv>,
    ) -> Vec<Lv> {
        let mut version = normalize(parents.to_vec());
        for target in deletes {
            debug_assert!(
                self.contains(parents, target),
                "an undelete undoes a delete it includes"
            );
            let seq = self.next_seq[agent.0 as usize];
            let lv = self.push(agent, seq, version, Op::Undelete { target });
            version = vec![lv];
        }
        version
    }

    /// Undoes `targets`, in a document at `parents`, as `agent`: one event
    /// each, in order. A delete is undone by an undelete; a move, or an
    /// undo of one (a redo), by an undo (#30). Each must be an event
    /// `parents` includes. Returns the version after.
    pub fn add_undo(
        &mut self,
        agent: AgentId,
        parents: &[Lv],
        targets: impl IntoIterator<Item = Lv>,
    ) -> Vec<Lv> {
        let mut version = normalize(parents.to_vec());
        for target in targets {
            debug_assert!(
                self.contains(parents, target),
                "an undo undoes an event it includes"
            );
            let op = match self.kind(target) {
                Kind::Delete => Op::Undelete { target },
                Kind::Move | Kind::Undo => Op::Undo { target },
                kind => panic!("event {target} is a {kind:?}: nothing to undo"),
            };
            let seq = self.next_seq[agent.0 as usize];
            let lv = self.push(agent, seq, version, op);
            version = vec![lv];
        }
        version
    }

    /// Adds a range (#32) from item `start` to item `end`, both included,
    /// tagged `tag`, in a document at `parents`, as `agent`. Its items are
    /// named by who they are (their inserts), not where, so it follows them
    /// through any edit, move or merge. Returns the version after; the range
    /// is the event that made it, the version's only entry.
    pub fn add_range(
        &mut self,
        agent: AgentId,
        parents: &[Lv],
        start: Lv,
        end: Lv,
        tag: &str,
    ) -> Vec<Lv> {
        let seq = self.next_seq[agent.0 as usize];
        let op = Op::RangeAdd {
            start,
            end,
            tag: tag.to_string(),
        };
        vec![self.push(agent, seq, normalize(parents.to_vec()), op)]
    }

    /// Sets range `range`'s tag, or removes it (`None`), in a document at
    /// `parents`, as `agent`. Returns the version after.
    pub fn set_range(
        &mut self,
        agent: AgentId,
        parents: &[Lv],
        range: Lv,
        tag: Option<&str>,
    ) -> Vec<Lv> {
        let seq = self.next_seq[agent.0 as usize];
        let op = Op::RangeSet {
            range,
            tag: tag.map(str::to_string),
        };
        vec![self.push(agent, seq, normalize(parents.to_vec()), op)]
    }

    /// Moves the items in `range` to `to` (a position in the document as
    /// it is, outside `range`), in a document at `parents`, as `agent`,
    /// keeping their identities. Returns the version after.
    pub fn add_move(
        &mut self,
        agent: AgentId,
        parents: &[Lv],
        range: Range<usize>,
        to: usize,
    ) -> Vec<Lv> {
        assert!(
            to <= range.start || to >= range.end,
            "can't move {range:?} into itself, to {to}"
        );
        let seq = self.next_seq[agent.0 as usize];
        let op = Op::Move {
            pos: range.start,
            len: range.len(),
            to,
        };
        vec![self.push(agent, seq, normalize(parents.to_vec()), op)]
    }

    /// Marks the item at `pos` as starting a section at `level` (0 to
    /// unmark), in a document at `parents`, as `agent`. Returns the version
    /// after.
    pub fn add_mark(&mut self, agent: AgentId, parents: &[Lv], pos: usize, level: u8) -> Vec<Lv> {
        let seq = self.next_seq[agent.0 as usize];
        let op = Op::Mark { pos, level };
        vec![self.push(agent, seq, normalize(parents.to_vec()), op)]
    }

    /// Adds an event made elsewhere: `agent`'s event number `seq`, after
    /// `parents`. Returns its local version. For logs decoded or synced from
    /// outside; local edits go through `add_insert` and `add_delete`.
    pub fn add_event(&mut self, agent: AgentId, seq: u64, parents: &[Lv], op: Op<T>) -> Lv {
        self.push(agent, seq, normalize(parents.to_vec()), op)
    }

    /// Starts a group (#14): `agent`'s events from here to the matching
    /// [`end_group`](Self::end_group) are one step, undone as one. Groups
    /// nest; only the outermost counts. Anything `agent` did since its last
    /// step ended is a step of its own, ended here.
    pub fn begin_group(&mut self, agent: AgentId) {
        let a = agent.index();
        if self.depth[a] == 0 {
            self.end_step(agent);
        }
        self.depth[a] += 1;
    }

    /// Ends a group [`begin_group`](Self::begin_group) started: at the
    /// outermost, `agent`'s events since are one step. Groups never span
    /// writers: each agent's are its own.
    pub fn end_group(&mut self, agent: AgentId) {
        let a = agent.index();
        assert!(self.depth[a] > 0, "an end_group without a begin_group");
        self.depth[a] -= 1;
        if self.depth[a] == 0 {
            self.end_step(agent);
        }
    }

    /// Whether `agent` is inside a group here.
    pub fn in_group(&self, agent: AgentId) -> bool {
        self.depth[agent.index()] > 0
    }

    /// Ends `agent`'s open step at its latest event, if it has one.
    pub(crate) fn end_step(&mut self, agent: AgentId) {
        let a = agent.index();
        let Some(last) = self.next_seq[a].checked_sub(1) else {
            return;
        };
        if self.step_ends[a].last().is_none_or(|&e| e < last) {
            self.step_ends[a].insert(last);
        }
    }

    /// `agent`'s whole steps, in order, as ranges of its seqs.
    pub fn steps(&self, agent: AgentId) -> Vec<Range<u64>> {
        let mut out = Vec::new();
        let mut start = 0;
        for &end in &self.step_ends[agent.index()] {
            out.push(start..end + 1);
            start = end + 1;
        }
        out
    }

    /// The events of `agent`'s seqs `seqs`, in order.
    pub fn events_of(&self, agent: AgentId, seqs: Range<u64>) -> Vec<Lv> {
        seqs.map(|s| self.find(agent, s).expect("an event this log has"))
            .collect()
    }

    /// The history a step at a time (#14): the versions after each step,
    /// in this log's order, each holding only whole steps. Where the
    /// events of a step need part of another (it saw that one half done),
    /// the timeline waits until that one is whole too. Open steps never
    /// show.
    pub fn timeline(&self) -> Vec<Vec<Lv>> {
        let n = self.len();
        // Where each event's step ends, if it has.
        let mut ends_at: Vec<Option<Lv>> = vec![None; n];
        for (a, ends) in self.step_ends.iter().enumerate() {
            let agent = AgentId(a as u32);
            let mut start = 0;
            for &end in ends {
                let end_lv = self.find(agent, end).expect("a step ends at an event");
                for s in start..=end {
                    ends_at[self.find(agent, s).expect("an event")] = Some(end_lv);
                }
                start = end + 1;
            }
        }
        let mut by_end: BTreeMap<Lv, Vec<Lv>> = BTreeMap::new();
        for (lv, e) in ends_at.iter().enumerate() {
            if let Some(e) = e {
                by_end.entry(*e).or_default().push(lv);
            }
        }
        let mut children: Vec<Vec<Lv>> = vec![Vec::new(); n];
        for lv in 0..n {
            for p in self.parents(lv) {
                children[p].push(lv);
            }
        }
        let mut inside = vec![false; n];
        // Parents of events inside that aren't.
        let mut missing = 0usize;
        let mut out: Vec<Vec<Lv>> = Vec::new();
        for (_, step) in by_end {
            for &x in &step {
                inside[x] = true;
                missing += self.parents(x).filter(|&p| !inside[p]).count();
                missing -= children[x].iter().filter(|&&c| inside[c]).count();
            }
            if missing == 0 {
                let version: Vec<Lv> = (0..n)
                    .filter(|&x| inside[x] && !children[x].iter().any(|&c| inside[c]))
                    .collect();
                if out.last() != Some(&version) {
                    out.push(version);
                }
            }
        }
        out
    }

    /// Adds every event `other` knows and this log doesn't, in `other`'s
    /// order (so parents first). Returns how many were new.
    pub fn pull(&mut self, other: &OpLog<T>) -> usize {
        let mut added = 0;
        for lv in 0..other.len() {
            if self.local(other, lv).is_some() {
                continue;
            }
            let event = other.event(lv);
            let agent = self.agent(other.agent_name(event.agent));
            let parents = event
                .parents
                .iter()
                .map(|&p| {
                    self.local(other, p)
                        .expect("parents arrive before children")
                })
                .collect();
            let op = match event.op {
                Op::Undelete { target } => Op::Undelete {
                    target: self
                        .local(other, target)
                        .expect("an undelete's target arrives before it"),
                },
                Op::Undo { target } => Op::Undo {
                    target: self
                        .local(other, target)
                        .expect("an undo's target arrives before it"),
                },
                Op::RangeAdd { start, end, tag } => Op::RangeAdd {
                    start: self
                        .local(other, start)
                        .expect("a range's ends arrive first"),
                    end: self.local(other, end).expect("a range's ends arrive first"),
                    tag,
                },
                Op::RangeSet { range, tag } => Op::RangeSet {
                    range: self
                        .local(other, range)
                        .expect("a range arrives before its sets"),
                    tag,
                },
                op => op,
            };
            self.push(agent, event.seq, parents, op);
            added += 1;
        }
        for (a, ends) in other.step_ends.iter().enumerate() {
            let agent = self.agent(&other.agents[a]);
            let have = self.next_seq[agent.index()];
            self.step_ends[agent.index()].extend(ends.iter().copied().filter(|&s| s < have));
        }
        added
    }

    /// `other`'s `version`, in this log's numbering; `None` if this log is
    /// missing any of it.
    pub fn translate(&self, other: &OpLog<T>, version: &[Lv]) -> Option<Vec<Lv>> {
        let lvs = version
            .iter()
            .map(|&lv| self.local(other, lv))
            .collect::<Option<Vec<_>>>()?;
        Some(normalize(lvs))
    }

    /// This log's number for `other`'s event `lv`.
    fn local(&self, other: &OpLog<T>, lv: Lv) -> Option<Lv> {
        let run = other.run_of(lv);
        let seq = run.seq + (lv - run.start) as u64;
        let agent = *self.by_name.get(other.agent_name(run.agent))?;
        self.find(agent, seq)
    }

    fn push(&mut self, agent: AgentId, seq: u64, parents: Vec<Lv>, op: Op<T>) -> Lv {
        let lv = self.len();
        assert!(
            parents.iter().all(|&p| p < lv),
            "an event's parents must already be known"
        );
        assert!(
            self.find(agent, seq).is_none(),
            "two events can't share an agent and seq"
        );
        if let Op::Undelete { target } = op {
            assert!(
                target < lv && matches!(self.run_of(target).op, RunOp::Delete { .. }),
                "an undelete undoes an earlier delete"
            );
        }
        if let Op::Undo { target } = op {
            assert!(
                target < lv
                    && matches!(
                        self.run_of(target).op,
                        RunOp::Move { .. } | RunOp::Undo { .. }
                    ),
                "an undo undoes an earlier move, or an undo of one"
            );
        }
        match &op {
            Op::RangeAdd { start, end, .. } => assert!(
                [start, end]
                    .iter()
                    .all(|&&i| i < lv && matches!(self.run_of(i).op, RunOp::Insert { .. })),
                "a range's ends are earlier inserts"
            ),
            Op::RangeSet { range, .. } => assert!(
                *range < lv && matches!(self.run_of(*range).op, RunOp::RangeAdd { .. }),
                "a range set sets an earlier range"
            ),
            _ => {}
        }
        let next = &mut self.next_seq[agent.0 as usize];
        *next = (*next).max(seq + 1);
        // A frontier entry is a parent of this event or still a frontier
        // entry: it has nothing after it, so it can't be a deeper ancestor.
        self.frontier.retain(|f| !parents.contains(f));
        self.frontier.push(lv);
        self.frontier.sort_unstable();

        // Carry on the last run if this is its agent's next event, right
        // after it, doing the same thing one item on.
        let extends = self.runs.last().is_some_and(|run| {
            run.agent == agent
                && run.seq + run.len() as u64 == seq
                && parents == [lv - 1]
                && match (&run.op, &op) {
                    (RunOp::Insert { pos: p, items }, Op::Insert { pos, .. }) => {
                        *pos == p + items.len()
                    }
                    (RunOp::Delete { pos: p, .. }, Op::Delete { pos }) => pos == p,
                    (RunOp::Undelete { target: t, len }, Op::Undelete { target }) => {
                        *target == t + len
                    }
                    _ => false,
                }
        });
        if extends {
            let run = self.runs.last_mut().expect("checked above");
            match (&mut run.op, op) {
                (RunOp::Insert { items, .. }, Op::Insert { item, .. }) => items.push(item),
                (RunOp::Delete { len, .. }, Op::Delete { .. })
                | (RunOp::Undelete { len, .. }, Op::Undelete { .. }) => *len += 1,
                _ => unreachable!("checked above"),
            }
            let span = self.spans[agent.0 as usize]
                .get_mut(&run.seq)
                .expect("a run's agent has a span for it");
            span.1 += 1;
        } else {
            let lamport = self.lamport_after(&parents);
            self.runs.push(Run {
                agent,
                seq,
                start: lv,
                parents,
                lamport,
                op: match op {
                    Op::Insert { pos, item } => RunOp::Insert {
                        pos,
                        items: vec![item],
                    },
                    Op::Delete { pos } => RunOp::Delete { pos, len: 1 },
                    Op::Undelete { target } => RunOp::Undelete { target, len: 1 },
                    Op::Move { pos, len, to } => RunOp::Move { pos, len, to },
                    Op::Mark { pos, level } => RunOp::Mark { pos, level },
                    Op::Undo { target } => RunOp::Undo { target },
                    Op::RangeAdd { start, end, tag } => RunOp::RangeAdd { start, end, tag },
                    Op::RangeSet { range, tag } => RunOp::RangeSet { range, tag },
                },
            });
            self.spans[agent.0 as usize].insert(seq, (lv, 1));
        }
        lv
    }
}

impl<T> OpLog<T> {
    /// Adds a run of `agent`'s events from `seq` on, each the only child of
    /// the one before (inserts typing forward, or deletes deleting
    /// forward); the first has `parents`. For a decoded chunk, which has
    /// checked them: parents known, seqs new. Returns the first's number.
    pub(crate) fn push_run(
        &mut self,
        agent: AgentId,
        seq: u64,
        parents: Vec<Lv>,
        op: RunOp<T>,
    ) -> Lv {
        let lv = self.len();
        let len = match &op {
            RunOp::Insert { items, .. } => items.len(),
            RunOp::Delete { len, .. } => *len,
            RunOp::Move { .. } | RunOp::Mark { .. } => 1,
            RunOp::Undelete { .. } | RunOp::Undo { .. } => {
                unreachable!("undos go one at a time")
            }
            RunOp::RangeAdd { .. } | RunOp::RangeSet { .. } => 1,
        };
        debug_assert!(len > 0 && parents.iter().all(|&p| p < lv));
        debug_assert!(self.find(agent, seq).is_none(), "new events");
        let next = &mut self.next_seq[agent.index()];
        *next = (*next).max(seq + len as u64);
        self.frontier.retain(|f| !parents.contains(f));
        self.frontier.push(lv + len - 1);
        self.frontier.sort_unstable();
        let extends = self.runs.last().is_some_and(|run| {
            run.agent == agent
                && run.seq + run.len() as u64 == seq
                && parents == [lv - 1]
                && match (&run.op, &op) {
                    (RunOp::Insert { pos: p, items }, RunOp::Insert { pos, .. }) => {
                        *pos == p + items.len()
                    }
                    (RunOp::Delete { pos: p, .. }, RunOp::Delete { pos, .. }) => pos == p,
                    _ => false,
                }
        });
        if extends {
            let run = self.runs.last_mut().expect("checked above");
            match (&mut run.op, op) {
                (RunOp::Insert { items, .. }, RunOp::Insert { items: more, .. }) => {
                    items.extend(more)
                }
                (RunOp::Delete { len: l, .. }, RunOp::Delete { .. }) => *l += len,
                _ => unreachable!("checked above"),
            }
            let span = self.spans[agent.index()]
                .get_mut(&run.seq)
                .expect("a run's agent has a span for it");
            span.1 += len;
        } else {
            let lamport = self.lamport_after(&parents);
            self.runs.push(Run {
                agent,
                seq,
                start: lv,
                parents,
                op,
                lamport,
            });
            self.spans[agent.index()].insert(seq, (lv, len));
        }
        lv
    }

    pub(crate) fn agent_by_name(&self, name: &str) -> Option<AgentId> {
        self.by_name.get(name).copied()
    }

    pub fn agent_name(&self, agent: AgentId) -> &str {
        &self.agents[agent.0 as usize]
    }

    /// How many events this log knows.
    pub fn len(&self) -> usize {
        self.runs.last().map_or(0, Run::end)
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// How many runs the events are stored as.
    pub fn runs(&self) -> usize {
        self.runs.len()
    }

    /// The version holding every event this log knows.
    pub fn version(&self) -> &[Lv] {
        &self.frontier
    }

    pub(crate) fn run_of(&self, lv: Lv) -> &Run<T> {
        &self.runs[self.run_index(lv)]
    }

    pub(crate) fn run_index(&self, lv: Lv) -> usize {
        let i = self.runs.partition_point(|r| r.start <= lv) - 1;
        debug_assert!(lv < self.runs[i].end(), "event {lv} isn't in the log");
        i
    }

    /// Sorted events as spans, each inside one run.
    pub(crate) fn spans_of(&self, events: &[Lv]) -> Vec<Range<Lv>> {
        let mut spans: Vec<Range<Lv>> = Vec::new();
        let mut end = 0;
        for &lv in events {
            match spans.last_mut() {
                Some(span) if span.end == lv && lv < end => span.end += 1,
                _ => {
                    end = self.run_of(lv).end();
                    spans.push(lv..lv + 1);
                }
            }
        }
        spans
    }

    /// Everything `version` includes, as spans, each inside one run. A run
    /// is included up to some event, so this costs what the runs do, not
    /// the events.
    fn closure_spans(&self, version: &[Lv]) -> Vec<Range<Lv>> {
        // How far into each run is included.
        let mut upto = vec![0; self.runs.len()];
        let mut stack: Vec<Lv> = version.to_vec();
        while let Some(lv) = stack.pop() {
            let r = self.run_index(lv);
            let run = &self.runs[r];
            let want = lv - run.start + 1;
            if want <= upto[r] {
                continue;
            }
            if upto[r] == 0 {
                stack.extend(&run.parents);
            }
            upto[r] = want;
        }
        self.runs
            .iter()
            .zip(upto)
            .filter(|&(_, n)| n > 0)
            .map(|(run, n)| run.start..run.start + n)
            .collect()
    }

    /// This log's number for the event `agent` numbered `seq`.
    pub(crate) fn find(&self, agent: AgentId, seq: u64) -> Option<Lv> {
        let (&first, &(lv, len)) = self.spans[agent.0 as usize].range(..=seq).next_back()?;
        let k = (seq - first) as usize;
        (k < len).then_some(lv + k)
    }

    /// Event `lv`'s parents.
    pub(crate) fn parents(&self, lv: Lv) -> impl Iterator<Item = Lv> + '_ {
        let run = self.run_of(lv);
        let (first, previous) = if lv == run.start {
            (&run.parents[..], None)
        } else {
            (&[][..], Some(lv - 1))
        };
        first.iter().copied().chain(previous)
    }

    /// Whether event `lv` is an insert.
    pub(crate) fn is_insert(&self, lv: Lv) -> bool {
        matches!(self.run_of(lv).op, RunOp::Insert { .. })
    }

    /// What kind of event `lv` is.
    pub(crate) fn kind(&self, lv: Lv) -> Kind {
        match self.run_of(lv).op {
            RunOp::Insert { .. } => Kind::Insert,
            RunOp::Delete { .. } => Kind::Delete,
            RunOp::Undelete { .. } => Kind::Undelete,
            RunOp::Move { .. } => Kind::Move,
            RunOp::Mark { .. } => Kind::Mark,
            RunOp::Undo { .. } => Kind::Undo,
            RunOp::RangeAdd { .. } | RunOp::RangeSet { .. } => Kind::Range,
        }
    }

    /// What undo `lv` undoes: a move, or an undo of one.
    pub(crate) fn undone(&self, lv: Lv) -> Lv {
        match self.run_of(lv).op {
            RunOp::Undo { target } => target,
            _ => panic!("event {lv} isn't an undo"),
        }
    }

    /// The move at the end of undo `lv`'s chain of targets.
    pub(crate) fn undone_move(&self, mut lv: Lv) -> Lv {
        while self.kind(lv) == Kind::Undo {
            lv = self.undone(lv);
        }
        lv
    }

    /// What an undo-like event needs replayed with it, so the merge knows
    /// what it brings back: an undelete's delete, an undo's move.
    pub(crate) fn needs_replayed(&self, lv: Lv) -> Option<Lv> {
        match self.kind(lv) {
            Kind::Undelete => Some(self.undeleted(lv)),
            Kind::Undo => Some(self.undone_move(lv)),
            _ => None,
        }
    }

    /// The delete undelete `lv` undoes.
    pub(crate) fn undeleted(&self, lv: Lv) -> Lv {
        let run = self.run_of(lv);
        match run.op {
            RunOp::Undelete { target, .. } => target + (lv - run.start),
            _ => panic!("event {lv} isn't an undelete"),
        }
    }

    /// What a merge of `to` into `from` must replay, and the base it can
    /// start from: `(old, new, base)`, where `old` is replayed events
    /// `from` already holds, `new` the ones it doesn't (both in order), and
    /// `base` a version every replayed event includes whole, so replaying
    /// never retreats anything in it.
    ///
    /// Walks back from both versions at once, latest event first, marking
    /// which side reached each. Once everything left to visit was reached
    /// by both, what's left is a candidate base: it's taken if every event
    /// visited includes all of it, and otherwise the walk goes one step
    /// further and tries again. So it costs what's happened since the two
    /// versions met, not the whole history; unmerged branches elsewhere
    /// don't count, and the base can be several events (two people's work,
    /// merged). An undelete's delete is always replayed with it, so the item
    /// it brings back is known to the walk.
    pub(crate) fn conflict(
        &self,
        from: &[Lv],
        to: &[Lv],
    ) -> (Vec<Range<Lv>>, Vec<Range<Lv>>, Vec<Lv>) {
        // From nothing: everything in `to` is new, and there's no base.
        if from.is_empty() {
            return (Vec::new(), self.closure_spans(to), Vec::new());
        }
        const FROM: u8 = 1;
        const TO: u8 = 2;
        const BOTH: u8 = FROM | TO;
        // What's left to visit, latest first, with which sides reached it:
        // an event reached by both is queued once.
        let mut queue: BTreeMap<Lv, u8> = BTreeMap::new();
        // How many queued events one side alone has reached.
        let mut one_sided = 0usize;
        fn reach(queue: &mut BTreeMap<Lv, u8>, one_sided: &mut usize, lv: Lv, side: u8) {
            let sides = queue.entry(lv).or_insert(0);
            let was = *sides;
            *sides |= side;
            match (was, *sides) {
                (0, BOTH) | (BOTH, _) => {}
                (0, _) => *one_sided += 1,
                (_, BOTH) => *one_sided -= 1,
                _ => {}
            }
        }
        for &lv in from {
            reach(&mut queue, &mut one_sided, lv, FROM);
        }
        for &lv in to {
            reach(&mut queue, &mut one_sided, lv, TO);
        }
        let mut visited: Vec<(Lv, u8)> = Vec::new();
        loop {
            if one_sided == 0 {
                let base: Vec<Lv> = queue.keys().copied().collect();
                let mut replayed: Vec<Lv> = visited.iter().map(|&(lv, _)| lv).collect();
                replayed.sort_unstable();
                // An undelete needs the delete it undoes replayed too, so
                // the item it brings back is known: the base can't hold it.
                let undoes_replayed = || {
                    replayed.iter().all(|&lv| {
                        self.needs_replayed(lv)
                            .is_none_or(|d| replayed.binary_search(&d).is_ok())
                    })
                };
                if self.all_include(&replayed, &base) && undoes_replayed() {
                    let mut old = Vec::new();
                    let mut new = Vec::new();
                    for &(lv, side) in &visited {
                        if side & FROM != 0 {
                            old.push(lv);
                        } else {
                            new.push(lv);
                        }
                    }
                    old.sort_unstable();
                    new.sort_unstable();
                    return (self.spans_of(&old), self.spans_of(&new), base);
                }
            }
            let Some((lv, side)) = queue.pop_last() else {
                unreachable!("an empty base is included by everything")
            };
            if side != BOTH {
                one_sided -= 1;
            }
            visited.push((lv, side));
            for p in self.parents(lv) {
                reach(&mut queue, &mut one_sided, p, side);
            }
        }
    }

    /// Whether every event in `events` (in order, parents first) includes
    /// every event in `base`. Each event's share of `base` is what its
    /// parents include, so one pass in order settles it.
    fn all_include(&self, events: &[Lv], base: &[Lv]) -> bool {
        if base.is_empty() {
            return true;
        }
        let words = base.len().div_ceil(64);
        let full: Vec<u64> = (0..words)
            .map(|w| {
                let bits = (base.len() - w * 64).min(64);
                if bits == 64 {
                    u64::MAX
                } else {
                    (1 << bits) - 1
                }
            })
            .collect();
        let index: HashMap<Lv, usize> = base.iter().enumerate().map(|(i, &lv)| (lv, i)).collect();
        let mut includes: HashMap<Lv, Vec<u64>> = HashMap::new();
        for &lv in events {
            let mut set = vec![0u64; words];
            for p in self.parents(lv) {
                // Every parent of a visited event was reached when it was
                // visited: it's still queued, so in the base, or visited too.
                if let Some(&i) = index.get(&p) {
                    set[i / 64] |= 1 << (i % 64);
                } else {
                    for (s, t) in set.iter_mut().zip(&includes[&p]) {
                        *s |= t;
                    }
                }
            }
            if set != full {
                return false;
            }
            includes.insert(lv, set);
        }
        true
    }

    /// What's in `a` but not `b`, and in `b` but not `a`, latest first.
    ///
    /// Walks back from both at once, latest event first, marking which side
    /// reached each, and stops once everything still to visit was reached
    /// by both: so it costs what the two differ by, not the whole history.
    pub(crate) fn diff(&self, a: &[Lv], b: &[Lv]) -> (Vec<Lv>, Vec<Lv>) {
        const A: u8 = 1;
        const B: u8 = 2;
        let mut queue: BinaryHeap<(Lv, u8)> = a
            .iter()
            .map(|&lv| (lv, A))
            .chain(b.iter().map(|&lv| (lv, B)))
            .collect();
        // How many queued entries one side alone reached.
        let mut one_sided = queue.len();
        let (mut only_a, mut only_b) = (Vec::new(), Vec::new());
        while one_sided > 0 {
            let (lv, mut side) = queue.pop().expect("one-sided entries are queued");
            if side != A | B {
                one_sided -= 1;
            }
            while let Some(&(next, more)) = queue.peek()
                && next == lv
            {
                queue.pop();
                if more != A | B {
                    one_sided -= 1;
                }
                side |= more;
            }
            match side {
                A => only_a.push(lv),
                B => only_b.push(lv),
                _ => {}
            }
            for p in self.parents(lv) {
                if side != A | B {
                    one_sided += 1;
                }
                queue.push((p, side));
            }
        }
        (only_a, only_b)
    }

    /// Every event in `version` and before it, as a set by local version.
    pub(crate) fn closure(&self, version: &[Lv]) -> Vec<bool> {
        let mut seen = vec![false; self.len()];
        let mut stack: Vec<Lv> = version.to_vec();
        while let Some(lv) = stack.pop() {
            // Down its run, each event the only parent of the next, to the
            // start or to what's already seen.
            let run = self.run_of(lv);
            let mut x = lv;
            while !seen[x] {
                seen[x] = true;
                if x == run.start {
                    stack.extend(&run.parents);
                    break;
                }
                x -= 1;
            }
        }
        seen
    }

    /// Event `lv`'s Lamport time: one more than its latest parent's. What
    /// decides which moves are active (see `reference.rs`): a time every
    /// replica gives an event alike. Kept as events are added.
    pub(crate) fn lamport(&self, lv: Lv) -> u64 {
        let run = self.run_of(lv);
        run.lamport + (lv - run.start) as u64
    }

    /// The Lamport time of an event after `parents`.
    fn lamport_after(&self, parents: &[Lv]) -> u64 {
        parents.iter().map(|&p| self.lamport(p)).max().unwrap_or(0) + 1
    }

    /// Whether `version` includes event `lv`.
    pub fn contains(&self, version: &[Lv], lv: Lv) -> bool {
        self.closure(version)[lv]
    }

    /// Both versions together, as a frontier: only entries no other entry
    /// already includes.
    pub fn union(&self, a: &[Lv], b: &[Lv]) -> Vec<Lv> {
        let all = normalize(a.iter().chain(b).copied().collect());
        if all.len() < 2 {
            return all;
        }
        // An entry another includes is before that one: in what all the
        // entries' parents include, as nothing includes itself. One walk
        // back from all the parents at once finds them, latest first, a run
        // at a time; as events come after their parents, it stops below the
        // earliest entry.
        let mut dropped = vec![false; all.len()];
        let mut next: BinaryHeap<Lv> = all.iter().flat_map(|&lv| self.parents(lv)).collect();
        let mut seen: HashSet<Lv> = HashSet::new();
        while let Some(x) = next.pop() {
            if x < all[0] {
                break;
            }
            // Latest first, so the first time a run is reached is from
            // its latest event reached at all.
            let run = self.run_of(x);
            if !seen.insert(run.start) {
                continue;
            }
            let (i, j) = (
                all.partition_point(|&e| e < run.start),
                all.partition_point(|&e| e <= x),
            );
            dropped[i..j].fill(true);
            next.extend(&run.parents);
        }
        all.into_iter()
            .zip(dropped)
            .filter_map(|(lv, gone)| (!gone).then_some(lv))
            .collect()
    }

    /// How an event's inserts are ordered against concurrent ones at the
    /// same place: by agent name, then sequence number.
    pub(crate) fn order_key(&self, lv: Lv) -> (&str, u64) {
        let run = self.run_of(lv);
        (
            &self.agents[run.agent.0 as usize],
            run.seq + (lv - run.start) as u64,
        )
    }
}

pub(crate) fn normalize(mut version: Vec<Lv>) -> Vec<Lv> {
    version.sort_unstable();
    version.dedup();
    version
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_is_one_run() {
        let mut log: OpLog<char> = OpLog::new();
        let alice = log.agent("alice");
        let mut v = Vec::new();
        for (i, c) in "hello world".chars().enumerate() {
            v = log.add_insert(alice, &v, i, [c]);
        }
        v = log.add_delete(alice, &v, 0..6);
        assert_eq!(log.len(), 17);
        assert_eq!(log.runs(), 2, "one run of typing, one of deleting");
        assert_eq!(log.event(4).op, Op::Insert { pos: 4, item: 'o' });
        assert_eq!(log.event(4).parents, [3]);
        assert_eq!(log.event(11).parents, [10]);
        assert_eq!(log.event(12).op, Op::Delete { pos: 0 });
        assert_eq!(log.checkout(&v).into_iter().collect::<String>(), "world");
    }

    #[test]
    fn a_jump_or_another_agent_starts_a_run() {
        let mut log: OpLog<char> = OpLog::new();
        let (alice, bob) = (log.agent("alice"), log.agent("bob"));
        let v = log.add_insert(alice, &[], 0, "abc".chars());
        let v = log.add_insert(alice, &v, 0, "x".chars()); // jumped back
        let v = log.add_insert(bob, &v, 4, "yz".chars());
        assert_eq!(log.runs(), 3);
        assert_eq!(log.checkout(&v).into_iter().collect::<String>(), "xabcyz");
    }

    #[test]
    fn pulling_keeps_runs() {
        let mut a: OpLog<char> = OpLog::new();
        let alice = a.agent("alice");
        let mut v = Vec::new();
        for (i, c) in "hello".chars().enumerate() {
            v = a.add_insert(alice, &v, i, [c]);
        }
        let mut b: OpLog<char> = OpLog::new();
        b.pull(&a);
        assert_eq!(b.runs(), 1);
        let v = b.translate(&a, &v).unwrap();
        assert_eq!(b.checkout(&v).into_iter().collect::<String>(), "hello");
    }
}
