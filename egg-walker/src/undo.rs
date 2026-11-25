//! Undo groups (#14): a writer's steps, undone and redone one at a time.
//!
//! The editor decides what a step is, with
//! [`OpLog::begin_group`]/[`OpLog::end_group`] (a typed word, a paste, a
//! section move's two events); groups never span writers. Undoing a step
//! makes a step of its own that reverses only the step's own effects:
//! - its inserts are deleted, where they show;
//! - its deletes are undone, and its moves (or undos of moves) undone in
//!   turn (#30): one kind-2 run;
//! - an undelete in it is reversed by a new delete (deletes stay undone
//!   for good);
//! - its marks go back to the levels their items had before it, where the
//!   step's mark is still the latest.
//!
//! What cancels out inside the step (a delete it also undid, a move it
//! also undid) is left alone. Concurrent edits by others inside what it
//! touched survive, as they do with undelete. A redo reverses the undo's
//! step the same way, so a move comes back with the same copies.
//!
//! The undo and redo stacks are this replica's, kept in its
//! [`Accountant`] for each local writer, and not saved: after a load, undo
//! starts from nothing (steps themselves are in the log).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;

use crate::accountant::Accountant;
use crate::oplog::{AgentId, Aside, CommentState, Kind, Lv, Op, OpLog};
use crate::walk::Change;

/// A local writer's undo and redo stacks, by its steps' first seqs.
#[derive(Clone, Default)]
pub(crate) struct History {
    /// Steps undone and not redone, and the step that undid each.
    undone: BTreeMap<u64, Range<u64>>,
    /// Steps an undo made: never undone themselves (a redo reverses them).
    undos: HashSet<u64>,
    /// Steps a redo brought back: the redo's step stands for them now.
    retired: HashSet<u64>,
    /// What redo brings back, latest last.
    redo: Vec<u64>,
    /// Where the steps an undo or redo made end: a step from here on that
    /// they didn't make is a new edit, and clears `redo`.
    seen: u64,
}

impl Accountant {
    /// Undoes `agent`'s latest step that isn't undone, as a step of its
    /// own, made at [`version`](Self::version) and merged: returns the
    /// changes that bring the document there. `None` if there's nothing to
    /// undo. Its open step, if any, is ended first.
    pub fn undo_group<'a, T: Clone>(
        &mut self,
        log: &'a mut OpLog<T>,
        agent: AgentId,
    ) -> Option<Vec<Change<'a, T>>> {
        assert!(!log.in_group(agent), "an undo inside a group");
        log.end_step(agent);
        self.note_new_edits(log, agent);
        let step = {
            let h = self.history.entry(agent).or_default();
            log.steps(agent).into_iter().rev().find(|s| {
                !h.undone.contains_key(&s.start)
                    && !h.undos.contains(&s.start)
                    && !h.retired.contains(&s.start)
            })?
        };
        let (made, version) = self.reverse(log, agent, step.clone());
        let h = self.history.entry(agent).or_default();
        h.undone.insert(step.start, made.clone());
        h.undos.insert(made.start);
        h.redo.push(step.start);
        h.seen = made.end;
        let log: &'a OpLog<T> = log;
        Some(self.merge(log, &version))
    }

    /// Redoes the step `agent` last undid, if nothing new came since: the
    /// undo's step reversed, as a step of its own. `None` if there's
    /// nothing to redo.
    pub fn redo_group<'a, T: Clone>(
        &mut self,
        log: &'a mut OpLog<T>,
        agent: AgentId,
    ) -> Option<Vec<Change<'a, T>>> {
        assert!(!log.in_group(agent), "a redo inside a group");
        log.end_step(agent);
        self.note_new_edits(log, agent);
        let (step, undo) = {
            let h = self.history.entry(agent).or_default();
            let step = h.redo.pop()?;
            (
                step,
                h.undone
                    .remove(&step)
                    .expect("a step on the redo stack is undone"),
            )
        };
        let (made, version) = self.reverse(log, agent, undo);
        let h = self.history.entry(agent).or_default();
        h.retired.insert(step);
        h.seen = made.end;
        let log: &'a OpLog<T> = log;
        Some(self.merge(log, &version))
    }

    /// Clears `agent`'s redo stack if it made a step of its own since the
    /// last undo or redo.
    fn note_new_edits<T: Clone>(&mut self, log: &OpLog<T>, agent: AgentId) {
        let h = self.history.entry(agent).or_default();
        if log.steps(agent).iter().any(|s| s.start >= h.seen) {
            h.redo.clear();
            h.seen = log.next_seq[agent.index()];
        }
    }

    /// Makes the step that reverses `agent`'s step `seqs`, at this
    /// version: its seqs, and the version after it.
    fn reverse<T: Clone>(
        &mut self,
        log: &mut OpLog<T>,
        agent: AgentId,
        seqs: Range<u64>,
    ) -> (Range<u64>, Vec<Lv>) {
        let events = log.events_of(agent, seqs);
        let in_step: HashSet<Lv> = events.iter().copied().collect();
        // What the step undid itself, inside it: left alone, and so is
        // what undid it.
        let mut cancelled: HashSet<Lv> = HashSet::new();
        for &e in &events {
            let target = match log.event(e).op {
                Op::Undelete { target } | Op::Undo { target } => target,
                _ => continue,
            };
            if in_step.contains(&target) {
                cancelled.insert(target);
                // Deletes are undone for good: the pair cancels.
                if log.kind(target) == Kind::Delete {
                    cancelled.insert(e);
                }
            }
        }
        let mut to_delete: BTreeSet<Lv> = BTreeSet::new();
        let mut to_undo: Vec<Lv> = Vec::new();
        let mut marks: Vec<(Lv, Vec<Lv>)> = Vec::new();
        // Ranges: the step's own removed; its sets put back, where they're
        // still the latest.
        let mut ranges: Vec<(Lv, Option<String>)> = Vec::new();
        let added: HashSet<Lv> = events
            .iter()
            .copied()
            .filter(|&e| matches!(log.event(e).op, Op::Aside(Aside::RangeAdd { .. })))
            .collect();
        let mut sets: Vec<(Lv, Lv)> = Vec::new();
        // Comments: the step's own taken back; its states put back.
        let mut comments: Vec<(Lv, CommentState)> = Vec::new();
        let mut states: Vec<Lv> = Vec::new();
        let commented: HashSet<Lv> = events
            .iter()
            .copied()
            .filter(|&e| matches!(log.event(e).op, Op::Aside(Aside::CommentAdd { .. })))
            .collect();
        for &e in &events {
            if cancelled.contains(&e) {
                continue;
            }
            match log.event(e).op {
                Op::Insert { .. } => {
                    to_delete.insert(e);
                }
                Op::Delete { .. } => to_undo.push(e),
                // The latest of a chain of undos in the step (or a move
                // nothing in it undid): undone, so the chain's state goes
                // back to what it was. But a chain from a move the step
                // made, left cancelled, is as if never made: nothing to do.
                Op::Move { .. } | Op::Undo { .. } => {
                    let mut x = e;
                    let mut hops = 0;
                    while let Op::Undo { target } = log.event(x).op
                        && in_step.contains(&target)
                    {
                        x = target;
                        hops += 1;
                    }
                    if !(log.kind(x) == Kind::Move && hops % 2 == 1) {
                        to_undo.push(e);
                    }
                }
                Op::Undelete { target } => {
                    to_delete.insert(self.deleted_by(log, target));
                }
                Op::Mark { .. } => marks.push((e, log.event(e).parents)),
                Op::Aside(Aside::RangeAdd { .. }) => {
                    if self.ranges.tag(e).is_some_and(|t| t.is_some()) {
                        ranges.push((e, None));
                    }
                }
                Op::Aside(Aside::RangeSet { range, .. }) => {
                    if !added.contains(&range) && self.ranges.set_by(range) == Some(e) {
                        sets.push((range, e));
                    }
                }
                Op::Aside(Aside::CommentAdd { .. }) => {
                    if self
                        .comments
                        .state(e)
                        .is_some_and(|s| s != CommentState::Removed)
                    {
                        comments.push((e, CommentState::Removed));
                    }
                }
                Op::Aside(Aside::CommentSet { comment, .. }) => {
                    if !commented.contains(&comment) && self.comments.set_by(comment) == Some(e) {
                        states.push(comment);
                    }
                }
                // A body's edits aren't undone with the step yet: its
                // undo needs undeletes of body deletes (a follow-up).
                Op::Aside(Aside::BodyInsert { .. } | Aside::BodyDelete { .. }) => {}
            }
        }
        let start = log.next_seq[agent.index()];
        let mut v = self.version().to_vec();
        log.begin_group(agent);
        // Marks back where they were before the step: positions don't move.
        for (item, level) in self.old_levels(log, &events, &marks) {
            if let Some(&(_, pos)) = self.identities().positions(&[item].into()).first() {
                v = log.add_mark(agent, &v, pos, level);
            }
        }
        // Its items out, from the end, so the positions stay put.
        let mut at: Vec<usize> = self
            .identities()
            .positions(&to_delete)
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        at.sort_unstable();
        while let Some(&last) = at.last() {
            let mut first = last;
            while at.len() > 1 && at[at.len() - 2] + 1 == first {
                at.pop();
                first -= 1;
            }
            at.pop();
            v = log.add_delete(agent, &v, first..last + 1);
        }
        // Deletes and moves undone, by identity: one kind-2 run.
        if !to_undo.is_empty() {
            v = log.add_undo(agent, &v, to_undo);
        }
        // Sets and states put back to what they were before the step: the
        // books there.
        if !sets.is_empty() || !states.is_empty() {
            let mut before = Accountant::new();
            before.merge(log, &log.event(events[0]).parents);
            for (range, _) in sets {
                ranges.push((range, before.ranges.tag(range).flatten()));
            }
            for comment in states {
                let was = before
                    .comments
                    .state(comment)
                    .unwrap_or(CommentState::Removed);
                comments.push((comment, was));
            }
        }
        for (comment, state) in comments {
            v = log.set_comment(agent, &v, comment, state);
        }
        for (range, tag) in ranges {
            v = log.set_range(agent, &v, range, tag.as_deref());
        }
        log.end_group(agent);
        (start..log.next_seq[agent.index()], v)
    }

    /// The item delete `d` removed.
    fn deleted_by<T: Clone>(&self, log: &OpLog<T>, d: Lv) -> Lv {
        if let Some(item) = self.identities().removed(d) {
            return item;
        }
        let event = log.event(d);
        let Op::Delete { pos } = event.op else {
            unreachable!("{d} is a delete")
        };
        let mut books = Accountant::new();
        books.merge(log, &event.parents);
        books.identities().item(pos)
    }

    /// For the step's marks (each with its parents): each item they marked
    /// whose level is still the step's last for it, and the level it had
    /// before the step.
    fn old_levels<T: Clone>(
        &self,
        log: &OpLog<T>,
        events: &[Lv],
        marks: &[(Lv, Vec<Lv>)],
    ) -> Vec<(Lv, u8)> {
        if marks.is_empty() {
            return Vec::new();
        }
        let mut before = Accountant::new();
        before.merge(log, &log.event(events[0]).parents);
        // Each item's last level in the step.
        let mut last: HashMap<Lv, u8> = HashMap::new();
        let mut order: Vec<Lv> = Vec::new();
        for (m, parents) in marks {
            let Op::Mark { pos, level } = log.event(*m).op else {
                unreachable!("a mark")
            };
            let mut at = Accountant::new();
            at.merge(log, parents);
            let item = at.identities().item(pos);
            if last.insert(item, level).is_none() {
                order.push(item);
            }
        }
        order
            .into_iter()
            .filter(|item| self.level(*item) == last[item])
            .map(|item| (item, before.level(item)))
            .filter(|&(item, old)| old != last[&item])
            .collect()
    }
}
