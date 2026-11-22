//! Chunks: a stretch of a log's history as bytes. One chunk is what a
//! save appends to a file, and what a replica sends another to sync, so a
//! chunk stands alone: it names its own agents, and refers to events before
//! it by agent and sequence number.
//!
//! ```text
//! chunk     = compression (byte, 0: none), agents, runs
//! agents    = count, then each name (length, UTF-8)
//! runs      = count, then each run
//! run       = agent (index into agents)
//!             seq   (zigzag: this run's first seq minus where the agent's
//!                    last run in the chunk ended, or 0)
//!             parents (count, then each a ref)
//!             op
//! op        = 0 insert:   pos, count, items (as a run: text is one UTF-8 string)
//!             1 delete:   pos, count          (each removes the item at pos)
//!             2 undelete: count, then each target delete, a ref
//!             (other tags are reserved for other kinds of event)
//! ref       = varint v:
//!             v even: an event in this chunk, v / 2 events before this
//!                     run's first
//!             v odd:  an event from before: agent v / 2 (into agents),
//!                     then its seq
//! ```
//!
//! Every number is a LEB128 varint. A run's events after the first each
//! have the one before as their only parent, inserts move one position
//! along, deletes stay put: the same runs the log keeps.
//!
//! Decoding checks everything before adding anything: every reference
//! names an event this log has or the chunk brings, earlier than the event
//! referring to it; an undelete's target is a delete. Events the log
//! already has are skipped, so a chunk can be applied twice. Checking that
//! an event's position fits its document is left to the walk (adversarial
//! peers are M4's later work).

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use mamre::codec::{
    Codec, DecodeError, get_str, get_usize, get_varint, put_str, put_usize, put_varint,
};

use crate::oplog::{AgentId, Kind, Lv, Op, OpLog, RunOp};

/// A new event a chunk brings: its number here, its agent (into the
/// chunk's names), seq, parents and op.
type Planned<T> = (Lv, usize, u64, Vec<Lv>, Op<T>);

const INSERT: u64 = 0;
const DELETE: u64 = 1;
const UNDELETE: u64 = 2;

/// A reference to an event, as read: in this chunk, or from before it.
#[derive(Clone, Copy)]
enum Ref {
    Back(usize),
    Before { agent: usize, seq: u64 },
}

enum RawOp<T> {
    Insert { pos: usize, items: Vec<T> },
    Delete { pos: usize, len: usize },
    Undelete { targets: Vec<Ref> },
}

struct RawRun<T> {
    agent: usize,
    seq: u64,
    parents: Vec<Ref>,
    op: RawOp<T>,
}

impl<T> RawRun<T> {
    fn len(&self) -> usize {
        match &self.op {
            RawOp::Insert { items, .. } => items.len(),
            RawOp::Delete { len, .. } => *len,
            RawOp::Undelete { targets } => targets.len(),
        }
    }
}

impl<T: Clone + Codec> OpLog<T> {
    /// Appends the events in `events` (in this log's order) as a chunk.
    /// Events before `events.start` are referred to by agent and sequence
    /// number, so whoever reads it needs to have them already.
    pub fn encode_chunk(&self, events: Range<Lv>, out: &mut Vec<u8>) {
        assert!(
            events.end <= self.len(),
            "events {events:?} aren't all in the log"
        );
        // The stretches of runs in range.
        let mut pieces = Vec::new();
        if !events.is_empty() {
            let mut r = self.run_index(events.start);
            while let Some(run) = self.runs.get(r)
                && run.start < events.end
            {
                pieces.push((r, run.start.max(events.start)..run.end().min(events.end)));
                r += 1;
            }
        }
        // The agents it names: its own, and those of events it refers to
        // from before.
        let mut agents: Vec<AgentId> = Vec::new();
        let mut index: HashMap<AgentId, usize> = HashMap::new();
        let mut name = |agent: AgentId, agents: &mut Vec<AgentId>| {
            *index.entry(agent).or_insert_with(|| {
                agents.push(agent);
                agents.len() - 1
            })
        };
        for (r, range) in &pieces {
            let run = &self.runs[*r];
            name(run.agent, &mut agents);
            for p in self.piece_parents(*r, range.start) {
                if p < events.start {
                    name(self.run_of(p).agent, &mut agents);
                }
            }
            if let RunOp::Undelete { target, .. } = run.op {
                for t in target + (range.start - run.start)..target + (range.end - run.start) {
                    if t < events.start {
                        name(self.run_of(t).agent, &mut agents);
                    }
                }
            }
        }
        let index: HashMap<AgentId, usize> =
            agents.iter().enumerate().map(|(i, &a)| (a, i)).collect();

        out.push(0); // No compression.
        put_usize(out, agents.len());
        for &agent in &agents {
            put_str(out, self.agent_name(agent));
        }
        put_usize(out, pieces.len());
        let mut next_seq: HashMap<AgentId, u64> = HashMap::new();
        for (r, range) in &pieces {
            let run = &self.runs[*r];
            let k = range.start - run.start;
            let first = range.start;
            put_usize(out, index[&run.agent]);
            let seq = run.seq + k as u64;
            let expected = next_seq.get(&run.agent).copied().unwrap_or(0);
            (seq as i64 - expected as i64).encode(out);
            next_seq.insert(run.agent, seq + range.len() as u64);
            let parents = self.piece_parents(*r, first);
            put_usize(out, parents.len());
            for p in parents {
                self.put_ref(out, p, first, events.start, &index);
            }
            match &run.op {
                RunOp::Insert { pos, items } => {
                    put_varint(out, INSERT);
                    put_usize(out, pos + k);
                    put_usize(out, range.len());
                    T::encode_run(&items[k..k + range.len()], out);
                }
                RunOp::Delete { pos, .. } => {
                    put_varint(out, DELETE);
                    put_usize(out, *pos);
                    put_usize(out, range.len());
                }
                RunOp::Undelete { target, .. } => {
                    put_varint(out, UNDELETE);
                    put_usize(out, range.len());
                    for i in 0..range.len() {
                        self.put_ref(out, target + k + i, first, events.start, &index);
                    }
                }
            }
        }
    }

    /// The parents of event `first`, the first of a piece of run `r`.
    fn piece_parents(&self, r: usize, first: Lv) -> Vec<Lv> {
        let run = &self.runs[r];
        if first == run.start {
            run.parents.clone()
        } else {
            vec![first - 1]
        }
    }

    fn put_ref(
        &self,
        out: &mut Vec<u8>,
        lv: Lv,
        first: Lv,
        start: Lv,
        index: &HashMap<AgentId, usize>,
    ) {
        if lv >= start {
            debug_assert!(lv < first, "a reference is to an earlier event");
            put_usize(out, (first - lv) << 1);
        } else {
            let run = self.run_of(lv);
            put_usize(out, (index[&run.agent] << 1) | 1);
            put_varint(out, run.seq + (lv - run.start) as u64);
        }
    }

    /// Adds the events of a chunk (one [`encode_chunk`](Self::encode_chunk)
    /// wrote) that this log doesn't have yet. Returns how many were new.
    ///
    /// Everything is checked first: if the chunk is malformed, or refers
    /// to events this log doesn't have, nothing is added.
    pub fn decode_chunk(&mut self, mut bytes: &[u8]) -> Result<usize, DecodeError> {
        let input = &mut bytes;
        let runs = Self::parse_chunk(input)?;
        if !input.is_empty() {
            return Err(DecodeError::Invalid("bytes after the chunk"));
        }
        let (names, runs) = runs;

        // Plan: each event's number here, found or new, and its parents.
        let agents: Vec<Option<AgentId>> = names.iter().map(|n| self.agent_by_name(n)).collect();
        let mut seen: HashSet<(usize, u64)> = HashSet::new();
        let mut lvs: Vec<Lv> = Vec::new();
        let mut kinds_new: Vec<Kind> = Vec::new();
        let mut next = self.len();
        let resolve = |r: Ref, at: usize, lvs: &[Lv]| -> Result<Lv, DecodeError> {
            match r {
                Ref::Back(d) => {
                    let i = at
                        .checked_sub(d)
                        .filter(|_| d > 0)
                        .ok_or(DecodeError::Invalid("a reference past the chunk's start"))?;
                    Ok(lvs[i])
                }
                Ref::Before { agent, seq } => {
                    let a = agents
                        .get(agent)
                        .ok_or(DecodeError::Invalid("an agent the chunk doesn't name"))?;
                    a.and_then(|a| self.find(a, seq))
                        .ok_or(DecodeError::Invalid("an event this log doesn't have"))
                }
            }
        };
        // New events: number, agent (into names), seq, parents, op.
        let mut events: Vec<Planned<T>> = Vec::new();
        let mut index = 0;
        for run in &runs {
            if run.agent >= names.len() {
                return Err(DecodeError::Invalid("an agent the chunk doesn't name"));
            }
            let first = index;
            let parents: Vec<Lv> = run
                .parents
                .iter()
                .map(|&p| resolve(p, first, &lvs))
                .collect::<Result<_, _>>()?;
            for k in 0..run.len() {
                let seq = run.seq.checked_add(k as u64).ok_or(DecodeError::Overflow)?;
                if !seen.insert((run.agent, seq)) {
                    return Err(DecodeError::Invalid("an event twice in one chunk"));
                }
                let known = agents[run.agent].and_then(|a| self.find(a, seq));
                let lv = known.unwrap_or(next);
                let event_parents = if k == 0 {
                    parents.clone()
                } else {
                    vec![lvs[index - 1]]
                };
                let op = match &run.op {
                    RawOp::Insert { pos, items } => Op::Insert {
                        pos: pos.checked_add(k).ok_or(DecodeError::Overflow)?,
                        item: items[k].clone(),
                    },
                    RawOp::Delete { pos, .. } => Op::Delete { pos: *pos },
                    RawOp::Undelete { targets } => {
                        let t = resolve(targets[k], first, &lvs)?;
                        let is_delete = if t < self.len() {
                            self.kind(t) == Kind::Delete
                        } else {
                            kinds_new.get(t - self.len()) == Some(&Kind::Delete)
                        };
                        if !is_delete {
                            return Err(DecodeError::Invalid(
                                "an undelete of something not a delete",
                            ));
                        }
                        Op::Undelete { target: t }
                    }
                };
                if known.is_none() {
                    if event_parents.iter().any(|&p| p >= lv) {
                        return Err(DecodeError::Invalid("a parent after its child"));
                    }
                    kinds_new.push(match op {
                        Op::Insert { .. } => Kind::Insert,
                        Op::Delete { .. } => Kind::Delete,
                        Op::Undelete { .. } => Kind::Undelete,
                    });
                    events.push((lv, run.agent, seq, event_parents, op));
                    next += 1;
                }
                lvs.push(lv);
                index += 1;
            }
        }

        // Apply: everything checked.
        let ids: Vec<AgentId> = names.iter().map(|n| self.agent(n)).collect();
        let added = events.len();
        for (lv, agent, seq, parents, op) in events {
            let got = self.add_event(ids[agent], seq, &parents, op);
            debug_assert_eq!(got, lv, "planned numbering");
        }
        Ok(added)
    }

    /// Reads a chunk's agents and runs.
    #[allow(clippy::type_complexity)]
    fn parse_chunk(input: &mut &[u8]) -> Result<(Vec<String>, Vec<RawRun<T>>), DecodeError> {
        match u8::decode(input)? {
            0 => {}
            _ => return Err(DecodeError::Invalid("an unknown compression")),
        }
        let n = get_usize(input)?;
        let mut names = Vec::with_capacity(n.min(input.len()));
        for _ in 0..n {
            names.push(get_str(input)?.to_string());
        }
        let n = get_usize(input)?;
        let mut runs = Vec::with_capacity(n.min(input.len()));
        let mut next_seq: HashMap<usize, u64> = HashMap::new();
        let get_ref = |input: &mut &[u8]| -> Result<Ref, DecodeError> {
            let v = get_usize(input)?;
            if v & 1 == 0 {
                Ok(Ref::Back(v >> 1))
            } else {
                Ok(Ref::Before {
                    agent: v >> 1,
                    seq: get_varint(input)?,
                })
            }
        };
        for _ in 0..n {
            let agent = get_usize(input)?;
            let delta = i64::decode(input)?;
            let expected = next_seq.get(&agent).copied().unwrap_or(0);
            let seq = u64::try_from(expected as i64 + delta).map_err(|_| DecodeError::Overflow)?;
            let n_parents = get_usize(input)?;
            let mut parents = Vec::with_capacity(n_parents.min(input.len()));
            for _ in 0..n_parents {
                parents.push(get_ref(input)?);
            }
            let op = match get_varint(input)? {
                INSERT => {
                    let pos = get_usize(input)?;
                    let len = get_usize(input)?;
                    RawOp::Insert {
                        pos,
                        items: T::decode_run(len, input)?,
                    }
                }
                DELETE => RawOp::Delete {
                    pos: get_usize(input)?,
                    len: get_usize(input)?,
                },
                UNDELETE => {
                    let len = get_usize(input)?;
                    let mut targets = Vec::with_capacity(len.min(input.len()));
                    for _ in 0..len {
                        targets.push(get_ref(input)?);
                    }
                    RawOp::Undelete { targets }
                }
                _ => {
                    return Err(DecodeError::Invalid(
                        "an event kind this version doesn't know",
                    ));
                }
            };
            let run = RawRun {
                agent,
                seq,
                parents,
                op,
            };
            if run.len() == 0 {
                return Err(DecodeError::Invalid("an empty run"));
            }
            next_seq.insert(
                agent,
                seq.checked_add(run.len() as u64)
                    .ok_or(DecodeError::Overflow)?,
            );
            runs.push(run);
        }
        Ok((names, runs))
    }
}
