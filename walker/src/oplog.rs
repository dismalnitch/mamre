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
        // A frontier entry is a parent of this event or still a frontier
        // entry: it has nothing after it, so it can't be a deeper ancestor.
        self.frontier.retain(|f| !parents.contains(f));
        self.frontier.push(lv);
        self.frontier.sort_unstable();
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
