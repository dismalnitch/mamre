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
//!             tag   (varint: the op's kind times 2, plus 1 if a target follows)
//!             target (only if the tag says so: which structure the run edits)
//!             op
//! op        = 0 insert:   pos, count, items (as a run: text is one UTF-8 string)
//!             1 delete:   pos, count          (each removes the item at pos)
//!             2 undelete: count, then each target delete, a ref
//!             3 move:     pos, count, to   (one event: moves don't run)
//!             (other kinds are reserved for other kinds of event)
//! target    = varint, never 0. With no target, a run edits the document's
//!             text, so text costs nothing for this. Other structures (ink,
//!             comments) share the log, so they share its one order and one
//!             undo history; this version knows none, and refuses a chunk
//!             that edits one, since a later event could depend on it.
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

use std::collections::HashMap;
use std::ops::Range;

use mamre::codec::{
    Codec, DecodeError, get_str, get_usize, get_varint, put_str, put_usize, put_varint,
};

use crate::oplog::{AgentId, Kind, Lv, Op, OpLog, RunOp, normalize};

/// What a new run of a chunk adds: a run as the log keeps it, or
/// undeletes, one target each (they needn't be side by side here).
enum PlannedOp<T> {
    Run(RunOp<T>),
    Undeletes(Vec<Lv>),
}

const INSERT: u64 = 0;
const DELETE: u64 = 1;
const UNDELETE: u64 = 2;
const MOVE: u64 = 3;

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
    Move { pos: usize, len: usize, to: usize },
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
            RawOp::Move { .. } => 1,
        }
    }
}

impl<T: Clone + Codec> OpLog<T> {
    /// Appends the events in `events` (in this log's order) as a chunk.
    /// Events before `events.start` are referred to by agent and sequence
    /// number, so whoever reads it needs to have them already.
    pub fn encode_chunk(&self, events: Range<Lv>, out: &mut Vec<u8>) {
        self.encode_events(&[events], out);
    }

    /// Appends the events in `spans` (sorted, apart, in this log's order)
    /// as one chunk: what a replica that has everything else is missing,
    /// say. Events outside the spans are referred to by agent and sequence
    /// number. For one span, the same bytes as
    /// [`encode_chunk`](Self::encode_chunk).
    pub fn encode_events(&self, spans: &[Range<Lv>], out: &mut Vec<u8>) {
        let spans: Vec<Range<Lv>> = spans.iter().filter(|s| !s.is_empty()).cloned().collect();
        assert!(
            spans.windows(2).all(|w| w[0].end <= w[1].start)
                && spans.last().is_none_or(|s| s.end <= self.len()),
            "spans {spans:?} aren't sorted, apart and in the log"
        );
        // Where each span starts in the chunk's own order.
        let offsets: Vec<usize> = spans
            .iter()
            .scan(0, |n, s| {
                let at = *n;
                *n += s.len();
                Some(at)
            })
            .collect();
        let chunk_index = |lv: Lv| -> Option<usize> {
            let k = spans.partition_point(|s| s.end <= lv);
            spans
                .get(k)
                .filter(|s| s.start <= lv)
                .map(|s| offsets[k] + lv - s.start)
        };
        // The stretches of runs in the spans.
        let mut pieces = Vec::new();
        for span in &spans {
            let mut r = self.run_index(span.start);
            while let Some(run) = self.runs.get(r)
                && run.start < span.end
            {
                pieces.push((r, run.start.max(span.start)..run.end().min(span.end)));
                r += 1;
            }
        }
        // The agents it names: its own, and those of events it refers to
        // from outside.
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
                if chunk_index(p).is_none() {
                    name(self.run_of(p).agent, &mut agents);
                }
            }
            if let RunOp::Undelete { target, .. } = run.op {
                for t in target + (range.start - run.start)..target + (range.end - run.start) {
                    if chunk_index(t).is_none() {
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
            let first = chunk_index(range.start).expect("a piece is in the chunk");
            put_usize(out, index[&run.agent]);
            let seq = run.seq + k as u64;
            let expected = next_seq.get(&run.agent).copied().unwrap_or(0);
            (seq as i64 - expected as i64).encode(out);
            next_seq.insert(run.agent, seq + range.len() as u64);
            let parents = self.piece_parents(*r, range.start);
            put_usize(out, parents.len());
            for p in parents {
                self.put_ref(out, p, first, chunk_index(p), &index);
            }
            match &run.op {
                RunOp::Insert { pos, items } => {
                    put_varint(out, INSERT << 1);
                    put_usize(out, pos + k);
                    put_usize(out, range.len());
                    T::encode_run(&items[k..k + range.len()], out);
                }
                RunOp::Delete { pos, .. } => {
                    put_varint(out, DELETE << 1);
                    put_usize(out, *pos);
                    put_usize(out, range.len());
                }
                RunOp::Undelete { target, .. } => {
                    put_varint(out, UNDELETE << 1);
                    put_usize(out, range.len());
                    for i in 0..range.len() {
                        let t = target + k + i;
                        self.put_ref(out, t, first, chunk_index(t), &index);
                    }
                }
                RunOp::Move { pos, len, to } => {
                    put_varint(out, MOVE << 1);
                    put_usize(out, *pos);
                    put_usize(out, *len);
                    put_usize(out, *to);
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

    /// Writes a reference to `lv` from a run whose first event is `first`
    /// in the chunk: by distance if `lv` is in the chunk too (at `inside`),
    /// by agent and seq if not.
    fn put_ref(
        &self,
        out: &mut Vec<u8>,
        lv: Lv,
        first: usize,
        inside: Option<usize>,
        index: &HashMap<AgentId, usize>,
    ) {
        match inside {
            Some(at) => {
                debug_assert!(at < first, "a reference is to an earlier event");
                put_usize(out, (first - at) << 1);
            }
            None => {
                let run = self.run_of(lv);
                put_usize(out, (index[&run.agent] << 1) | 1);
                put_varint(out, run.seq + (lv - run.start) as u64);
            }
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

        // Plan, run by run: which events this log has already (a prefix of
        // each agent's), the numbers the new ones get, and their parents.
        let agents: Vec<Option<AgentId>> = names.iter().map(|n| self.agent_by_name(n)).collect();
        // Each agent's next new seq: what this log has, then what the
        // chunk adds. A new event must be exactly that: every event
        // includes the one before, so a replica has a prefix of each
        // agent's events, and a summary can say so.
        let mut next_seq: Vec<u64> = agents
            .iter()
            .map(|a| a.map_or(0, |a| self.next_seq[a.index()]))
            .collect();
        let have: Vec<u64> = next_seq.clone();
        let mut lvs: Vec<Lv> = Vec::new();
        let mut new_kinds: Vec<Kind> = Vec::new();
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
        // New runs: agent (into names), seq, parents, and the op, as a run
        // or (undeletes) one target per event.
        let mut planned: Vec<(usize, u64, Vec<Lv>, PlannedOp<T>)> = Vec::new();
        for run in runs {
            if run.agent >= names.len() {
                return Err(DecodeError::Invalid("an agent the chunk doesn't name"));
            }
            let len = run.len();
            run.seq
                .checked_add(len as u64)
                .ok_or(DecodeError::Overflow)?;
            let first = lvs.len();
            // The prefix this log has already.
            let known = have[run.agent].saturating_sub(run.seq).min(len as u64) as usize;
            let agent = agents[run.agent];
            for k in 0..known {
                let a = agent.expect("a known event's agent is known");
                lvs.push(
                    self.find(a, run.seq + k as u64)
                        .expect("a prefix of the agent's events"),
                );
            }
            if known == len {
                continue;
            }
            let seq = run.seq + known as u64;
            if seq != next_seq[run.agent] {
                return Err(DecodeError::Invalid("a gap in an agent's events"));
            }
            let n = len - known;
            next_seq[run.agent] += n as u64;
            let parents = if known == 0 {
                run.parents
                    .iter()
                    .map(|&p| resolve(p, first, &lvs))
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                vec![lvs[first + known - 1]]
            };
            let start = next;
            lvs.extend(start..start + n);
            next += n;
            let op = match run.op {
                RawOp::Insert { pos, mut items } => {
                    let pos = pos.checked_add(known).ok_or(DecodeError::Overflow)?;
                    new_kinds.extend(std::iter::repeat_n(Kind::Insert, n));
                    PlannedOp::Run(RunOp::Insert {
                        pos,
                        items: items.split_off(known),
                    })
                }
                RawOp::Delete { pos, .. } => {
                    new_kinds.extend(std::iter::repeat_n(Kind::Delete, n));
                    PlannedOp::Run(RunOp::Delete { pos, len: n })
                }
                RawOp::Move { pos, len, to } => {
                    new_kinds.push(Kind::Move);
                    PlannedOp::Run(RunOp::Move { pos, len, to })
                }
                RawOp::Undelete { targets } => {
                    let mut resolved = Vec::with_capacity(n);
                    for &t in &targets[known..] {
                        let t = resolve(t, first, &lvs)?;
                        let is_delete = if t < self.len() {
                            self.kind(t) == Kind::Delete
                        } else {
                            new_kinds.get(t - self.len()) == Some(&Kind::Delete)
                        };
                        if !is_delete {
                            return Err(DecodeError::Invalid(
                                "an undelete of something not a delete",
                            ));
                        }
                        resolved.push(t);
                    }
                    new_kinds.extend(std::iter::repeat_n(Kind::Undelete, n));
                    PlannedOp::Undeletes(resolved)
                }
            };
            planned.push((run.agent, seq, parents, op));
        }

        // Apply: everything checked.
        let ids: Vec<AgentId> = names.iter().map(|n| self.agent(n)).collect();
        let added = next - self.len();
        for (agent, seq, parents, op) in planned {
            match op {
                PlannedOp::Run(op) => {
                    self.push_run(ids[agent], seq, normalize(parents), op);
                }
                PlannedOp::Undeletes(targets) => {
                    let mut parents = parents;
                    for (k, target) in targets.into_iter().enumerate() {
                        let lv = self.add_event(
                            ids[agent],
                            seq + k as u64,
                            &parents,
                            Op::Undelete { target },
                        );
                        parents = vec![lv];
                    }
                }
            }
        }
        debug_assert_eq!(self.len(), next, "planned numbering");
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
            let tag = get_varint(input)?;
            if tag & 1 == 1 {
                return Err(match get_varint(input)? {
                    0 => DecodeError::Invalid("a target of 0, which is written as none"),
                    _ => DecodeError::Invalid("a structure this version doesn't know"),
                });
            }
            let op = match tag >> 1 {
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
                MOVE => RawOp::Move {
                    pos: get_usize(input)?,
                    len: get_usize(input)?,
                    to: get_usize(input)?,
                },
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One insert of "x" by "a": the chunk byte for byte, so the tests below
    /// can change exactly the tag.
    fn one_insert() -> Vec<u8> {
        let mut log = OpLog::<char>::new();
        let a = log.agent("a");
        log.add_insert(a, &[], 0, "x".chars());
        let mut out = Vec::new();
        log.encode_chunk(0..1, &mut out);
        // compression, agents, "a", runs, agent, seq, parents, tag, pos, count, "x"
        assert_eq!(out, [0, 1, 1, b'a', 1, 0, 0, 0, 0, 0, 1, 1, b'x']);
        out
    }

    const TAG: usize = 8;

    #[test]
    fn text_runs_carry_no_target() {
        let bytes = one_insert();
        let mut log = OpLog::<char>::new();
        assert_eq!(log.decode_chunk(&bytes), Ok(1));
    }

    #[test]
    fn a_run_editing_another_structure_is_refused() {
        let mut bytes = one_insert();
        bytes[TAG] = 1; // insert, with a target
        bytes.insert(TAG + 1, 5);
        let mut log = OpLog::<char>::new();
        assert_eq!(
            log.decode_chunk(&bytes),
            Err(DecodeError::Invalid(
                "a structure this version doesn't know"
            ))
        );
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn a_target_of_zero_is_refused() {
        let mut bytes = one_insert();
        bytes[TAG] = 1;
        bytes.insert(TAG + 1, 0);
        let mut log = OpLog::<char>::new();
        assert!(log.decode_chunk(&bytes).is_err());
        assert_eq!(log.len(), 0);
    }
}
