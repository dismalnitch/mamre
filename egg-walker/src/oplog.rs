//! The operation log: every edit any replica has made, as events in a graph.
//!
//! An event is one insert or delete of one item, at a position in the
//! document *as it was at the event's parents*: plain positions, no IDs in
//! the document. Its parents are the events it came after, so concurrent
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

use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::ops::Range;

/// A local version: an event's number in this log. Parents always have
/// smaller numbers than their children.
pub type Lv = usize;

/// An agent, numbered in this log. Agents are named; names are what other
/// logs know them by, and what concurrent inserts are ordered by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentId(u32);

/// What an event does, to one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op<T> {
    /// Puts `item` in at `pos`.
    Insert { pos: usize, item: T },
    /// Removes the item at `pos`.
    Delete { pos: usize },
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
struct Run<T> {
    agent: AgentId,
    seq: u64,
    start: Lv,
    parents: Vec<Lv>,
    op: RunOp<T>,
}

#[derive(Clone, Debug)]
enum RunOp<T> {
    /// Item `k` goes in at `pos + k`: typing forward.
    Insert { pos: usize, items: Vec<T> },
    /// `len` items each removed at `pos`: deleting forward.
    Delete { pos: usize, len: usize },
}

impl<T> Run<T> {
    fn len(&self) -> usize {
        match &self.op {
            RunOp::Insert { items, .. } => items.len(),
            RunOp::Delete { len, .. } => *len,
        }
    }

    fn end(&self) -> Lv {
        self.start + self.len()
    }
}

/// Every event a replica knows, in a graph.
#[derive(Clone, Debug)]
pub struct OpLog<T> {
    agents: Vec<String>,
    by_name: HashMap<String, AgentId>,
    next_seq: Vec<u64>,
    /// In local-version order, covering every event.
    runs: Vec<Run<T>>,
    /// Per agent, by the seq each stretch starts at: where in this log the
    /// agent's events are, as `(first local version, how many)`.
    spans: Vec<BTreeMap<u64, (Lv, usize)>>,
    /// The events nothing else came after: the version of everything.
    frontier: Vec<Lv>,
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
            op: match &run.op {
                RunOp::Insert { pos, items } => Op::Insert {
                    pos: pos + k,
                    item: items[k].clone(),
                },
                RunOp::Delete { pos, .. } => Op::Delete { pos: *pos },
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
            self.push(agent, event.seq, parents, event.op);
            added += 1;
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
                    _ => false,
                }
        });
        if extends {
            let run = self.runs.last_mut().expect("checked above");
            match (&mut run.op, op) {
                (RunOp::Insert { items, .. }, Op::Insert { item, .. }) => items.push(item),
                (RunOp::Delete { len, .. }, Op::Delete { .. }) => *len += 1,
                _ => unreachable!("checked above"),
            }
            let span = self.spans[agent.0 as usize]
                .get_mut(&run.seq)
                .expect("a run's agent has a span for it");
            span.1 += 1;
        } else {
            self.runs.push(Run {
                agent,
                seq,
                start: lv,
                parents,
                op: match op {
                    Op::Insert { pos, item } => RunOp::Insert {
                        pos,
                        items: vec![item],
                    },
                    Op::Delete { pos } => RunOp::Delete { pos, len: 1 },
                },
            });
            self.spans[agent.0 as usize].insert(seq, (lv, 1));
        }
        lv
    }
}

impl<T> OpLog<T> {
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

    fn run_of(&self, lv: Lv) -> &Run<T> {
        let i = self.runs.partition_point(|r| r.start <= lv);
        let run = &self.runs[i - 1];
        debug_assert!(lv < run.end(), "event {lv} isn't in the log");
        run
    }

    /// This log's number for the event `agent` numbered `seq`.
    fn find(&self, agent: AgentId, seq: u64) -> Option<Lv> {
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
    /// merged).
    pub(crate) fn conflict(&self, from: &[Lv], to: &[Lv]) -> (Vec<Lv>, Vec<Lv>, Vec<Lv>) {
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
                if self.all_include(&replayed, &base) {
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
                    return (old, new, base);
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
            if !seen[lv] {
                seen[lv] = true;
                stack.extend(self.parents(lv));
            }
        }
        seen
    }

    /// Whether `version` includes event `lv`.
    pub fn contains(&self, version: &[Lv], lv: Lv) -> bool {
        self.closure(version)[lv]
    }

    /// Both versions together, as a frontier: only entries no other entry
    /// already includes.
    pub fn union(&self, a: &[Lv], b: &[Lv]) -> Vec<Lv> {
        let all = normalize(a.iter().chain(b).copied().collect());
        let keep: Vec<Lv> = all
            .iter()
            .copied()
            .filter(|&lv| {
                let others: Vec<Lv> = all.iter().copied().filter(|&o| o != lv).collect();
                !self.closure(&others)[lv]
            })
            .collect();
        keep
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

fn normalize(mut version: Vec<Lv>) -> Vec<Lv> {
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
