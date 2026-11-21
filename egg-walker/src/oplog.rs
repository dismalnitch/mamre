//! The operation log: every edit any replica has made, as events in a graph.
//!
//! An event is one insert or delete of one item, at a position in the
//! document *as it was at the event's parents*: plain positions, no IDs in
//! the document. Its parents are the events it came after, so concurrent
//! edits are siblings. Events are numbered locally (`Lv`) in the order this
//! log learned them, which is always a topological order: parents first.
//!
//! Across replicas an event is named by its agent and sequence number.

use std::collections::HashMap;
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

#[derive(Clone, Debug)]
pub struct Event<T> {
    pub agent: AgentId,
    /// This agent's count of events before this one.
    pub seq: u64,
    /// The events this one came after: the version its position refers to.
    pub parents: Vec<Lv>,
    pub op: Op<T>,
}

/// Every event a replica knows, in a graph.
#[derive(Clone, Debug)]
pub struct OpLog<T> {
    agents: Vec<String>,
    by_name: HashMap<String, AgentId>,
    next_seq: Vec<u64>,
    events: Vec<Event<T>>,
    /// `(agent, seq)` to local version.
    ids: HashMap<(AgentId, u64), Lv>,
    /// The events nothing else came after: the version of everything.
    frontier: Vec<Lv>,
    /// Critical events, in order: each has every earlier event among its
    /// ancestors and every later one among its descendants, so the history
    /// narrows to it. A merge can start from the latest one it holds.
    critical: Vec<Lv>,
}

impl<T> Default for OpLog<T> {
    fn default() -> Self {
        OpLog {
            agents: Vec::new(),
            by_name: HashMap::new(),
            next_seq: Vec::new(),
            events: Vec::new(),
            ids: HashMap::new(),
            frontier: Vec::new(),
            critical: Vec::new(),
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
        id
    }

    pub fn agent_name(&self, agent: AgentId) -> &str {
        &self.agents[agent.0 as usize]
    }

    /// How many events this log knows.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn events(&self) -> &[Event<T>] {
        &self.events
    }

    /// The version holding every event this log knows.
    pub fn version(&self) -> &[Lv] {
        &self.frontier
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
        for event in &other.events {
            let agent = self.agent(other.agent_name(event.agent));
            if self.ids.contains_key(&(agent, event.seq)) {
                continue;
            }
            let parents = event
                .parents
                .iter()
                .map(|&p| {
                    self.local(other, p)
                        .expect("parents arrive before children")
                })
                .collect();
            self.push(agent, event.seq, parents, event.op.clone());
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
        let event = &other.events[lv];
        let agent = *self.by_name.get(other.agent_name(event.agent))?;
        self.ids.get(&(agent, event.seq)).copied()
    }

    fn push(&mut self, agent: AgentId, seq: u64, parents: Vec<Lv>, op: Op<T>) -> Lv {
        let lv = self.events.len();
        assert!(
            parents.iter().all(|&p| p < lv),
            "an event's parents must already be known"
        );
        let previous = self.ids.insert((agent, seq), lv);
        assert!(
            previous.is_none(),
            "two events can't share an agent and seq"
        );
        let next = &mut self.next_seq[agent.0 as usize];
        *next = (*next).max(seq + 1);
        // A critical event stays critical while every event after it
        // descends from it: a new event does if any parent is it or after
        // it. Critical events are a chain, so the ones this breaks are the
        // latest.
        let latest_parent = parents.iter().max().copied();
        while let Some(&c) = self.critical.last()
            && latest_parent.is_none_or(|p| p < c)
        {
            self.critical.pop();
        }
        // This event has every earlier one as an ancestor if its parents
        // are the whole frontier.
        let mut sorted = parents.clone();
        sorted.sort_unstable();
        sorted.dedup();
        let has_everything = sorted == self.frontier;
        // A frontier entry is a parent of this event or still a frontier
        // entry: it has nothing after it, so it can't be a deeper ancestor.
        self.frontier.retain(|f| !parents.contains(f));
        self.frontier.push(lv);
        self.frontier.sort_unstable();
        if has_everything {
            self.critical.push(lv);
        }
        self.events.push(Event {
            agent,
            seq,
            parents,
            op,
        });
        lv
    }
}

impl<T> OpLog<T> {
    /// The latest critical event in `version`, if any: everything up to it
    /// is in `version`, and everything after it descends from it.
    pub(crate) fn critical_in(&self, version: &[Lv]) -> Option<Lv> {
        // Everything after a critical event descends from it, so `version`
        // holds it if any of its events is it or after it.
        let latest = *version.iter().max()?;
        let i = self.critical.partition_point(|&c| c <= latest);
        i.checked_sub(1).map(|i| self.critical[i])
    }

    /// The events in `version` and before it, but after `floor`, in order.
    /// The search stops at `floor`, which all of them descend from.
    pub(crate) fn events_after(&self, version: &[Lv], floor: Option<Lv>) -> Vec<Lv> {
        let above = |lv: Lv| floor.is_none_or(|f| lv > f);
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<Lv> = version.iter().copied().filter(|&lv| above(lv)).collect();
        while let Some(lv) = stack.pop() {
            if seen.insert(lv) {
                stack.extend(
                    self.events[lv]
                        .parents
                        .iter()
                        .copied()
                        .filter(|&p| above(p)),
                );
            }
        }
        let mut out: Vec<Lv> = seen.into_iter().collect();
        out.sort_unstable();
        out
    }

    /// What's in `a` but not `b`, and in `b` but not `a`, latest first.
    ///
    /// Walks back from both at once, latest event first, marking which side
    /// reached each, and stops once everything still to visit was reached
    /// by both: so it costs what the two differ by, not the whole history.
    pub(crate) fn diff(&self, a: &[Lv], b: &[Lv]) -> (Vec<Lv>, Vec<Lv>) {
        const A: u8 = 1;
        const B: u8 = 2;
        let mut queue: std::collections::BinaryHeap<(Lv, u8)> = a
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
            for &p in &self.events[lv].parents {
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
        let mut seen = vec![false; self.events.len()];
        let mut stack: Vec<Lv> = version.to_vec();
        while let Some(lv) = stack.pop() {
            if !seen[lv] {
                seen[lv] = true;
                stack.extend(&self.events[lv].parents);
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
        let event = &self.events[lv];
        (&self.agents[event.agent.0 as usize], event.seq)
    }
}

fn normalize(mut version: Vec<Lv>) -> Vec<Lv> {
    version.sort_unstable();
    version.dedup();
    version
}
