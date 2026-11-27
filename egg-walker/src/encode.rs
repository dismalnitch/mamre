//! Chunks: a stretch of a log's history as bytes. One chunk is what a
//! save appends to a file, and what a replica sends another to sync, so a
//! chunk stands alone: it names its own agents, and refers to events before
//! it by agent and sequence number.
//!
//! ```text
//! chunk     = compression (byte, 0: none), agents, runs, [extension]
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
//!             2 undo:     count, then each target, a ref: a delete
//!                         (undone for good), a move, or an undo of a
//!                         move (a redo) (#30), that its event saw (#9)
//!             3 move:     pos, count, to   (one event: moves don't run)
//!             4 mark:     pos, level       (one event)
//!             5 copy:     pos, count, then sources until their counts
//!                         add up to count, each a ref and a count: a
//!                         paste (#36), inserts at pos like op 0, of copies
//!                         of the source inserts' items. A source is the
//!                         ref's event and the next count - 1 of its
//!                         agent's (by seq), all earlier inserts. Pasted
//!                         item k copies the k-th source event: that's
//!                         its lineage, which the merge never reads.
//!             6 aside:    an extension (see `ext.rs`): the event as JSON,
//!                         one event per run, the events it names by
//!                         agent name and seq. Ranges (#32): {"range_add":
//!                         {"start", "end", "tag"}} or {"range_set":
//!                         {"range", "tag"}}. Comments (#33):
//!                         {"comment_add": {"start", "end"}},
//!                         {"comment_set": {"comment", "state"}}, and a
//!                         body's edits, a character each:
//!                         {"body_insert": {"comment", "pos", "item"}},
//!                         {"body_delete": {"comment", "pos"}}
//!             (other kinds are reserved for other kinds of event)
//! extension = what the chunk carries beside its runs, as JSON (see
//!             `ext.rs`); only when there's something in it. So far:
//!             "steps" (#14): for each agent, by name, the seqs where its
//!             steps end, in order. The ends listed are those of the
//!             agent's events in the chunk, and the one just before them
//!             (a step left open by the last save, closed since). An
//!             agent's events after its last end are an open step.
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
//! referring to it; an undo's target is a delete, a move, or an undo of a
//! move. (Older readers refuse an undo of anything but a delete, so they
//! refuse chunks that undo moves cleanly.) Events the log
//! already has are skipped, so a chunk can be applied twice. Checking that
//! an event's position fits its document is left to the walk (adversarial
//! peers are M4's later work).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use mamre::codec::{
    Codec, DecodeError, get_str, get_usize, get_varint, put_str, put_usize, put_varint,
};

use crate::ext::{self, AsideJson, ChunkExt, Named};
use crate::oplog::{AgentId, Aside, Kind, Lv, Op, OpLog, RunOp, normalize};

const INSERT: u64 = 0;
const DELETE: u64 = 1;
const UNDO: u64 = 2;
const MOVE: u64 = 3;
const MARK: u64 = 4;
const COPY: u64 = 5;
const ASIDE: u64 = 6;

// Encoding and decoding mirror each other, step by step:
//
// | step                    | writes              | reads               |
// |-------------------------|---------------------|---------------------|
// | the whole chunk         | `encode_events`     | `parse_chunk`       |
// | compression byte        | (a 0)               | `get_compression`   |
// | the agent table         | `put_agents`        | `get_agents`        |
// | one run                 | `Writer::put_run`   | `get_run`           |
// | a run's seq             | `Seqs::put`         | `Seqs::get`         |
// | one reference           | `Writer::put_ref`   | `get_ref`           |
// | one run's op, by kind   | `Writer::put_op`    | `get_op`            |
// | the target              | (none: text only)   | `get_target`        |
//
// `decode_chunk` then checks the parsed runs against this log
// (`Planner`) before it adds anything (`apply`).

/// A reference to an event, as read: in this chunk, or from before it.
#[derive(Clone, Copy)]
enum Ref {
    Back(usize),
    Before { agent: usize, seq: u64 },
}

enum RawOp<T> {
    Insert { pos: usize, items: Vec<T> },
    Delete { pos: usize, len: usize },
    Undo { targets: Vec<Ref> },
    Move { pos: usize, len: usize, to: usize },
    Mark { pos: usize, level: u8 },
    /// Sources as (the first, how many of its agent's from there).
    Copy { pos: usize, len: usize, sources: Vec<(Ref, usize)> },
    Aside(AsideJson),
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
            RawOp::Delete { len, .. } | RawOp::Copy { len, .. } => *len,
            RawOp::Undo { targets } => targets.len(),
            RawOp::Move { .. } | RawOp::Mark { .. } | RawOp::Aside(_) => 1,
        }
    }
}

/// The events a chunk carries: spans of the log (sorted, apart, none
/// empty), and where each starts in the chunk's own order.
struct Spans {
    spans: Vec<Range<Lv>>,
    offsets: Vec<usize>,
}

impl Spans {
    fn new(spans: &[Range<Lv>], log_len: usize) -> Self {
        let spans: Vec<Range<Lv>> = spans.iter().filter(|s| !s.is_empty()).cloned().collect();
        assert!(
            spans.windows(2).all(|w| w[0].end <= w[1].start)
                && spans.last().is_none_or(|s| s.end <= log_len),
            "spans {spans:?} aren't sorted, apart and in the log"
        );
        let offsets = spans
            .iter()
            .scan(0, |n, s| {
                let at = *n;
                *n += s.len();
                Some(at)
            })
            .collect();
        Spans { spans, offsets }
    }

    /// Where `lv` is in the chunk, if the chunk carries it.
    fn index(&self, lv: Lv) -> Option<usize> {
        let k = self.spans.partition_point(|s| s.end <= lv);
        self.spans
            .get(k)
            .filter(|s| s.start <= lv)
            .map(|s| self.offsets[k] + lv - s.start)
    }
}

/// A stretch of one of the log's runs that a chunk carries: the run's
/// index, and the events.
type Piece = (usize, Range<Lv>);

/// The agents a chunk names, in the order it names them.
#[derive(Default)]
struct AgentTable {
    agents: Vec<AgentId>,
    index: HashMap<AgentId, usize>,
}

impl AgentTable {
    /// `agent`'s index in the table, adding it if it's new.
    fn name(&mut self, agent: AgentId) -> usize {
        *self.index.entry(agent).or_insert_with(|| {
            self.agents.push(agent);
            self.agents.len() - 1
        })
    }
}

/// Each agent's seq where its last run so far in the chunk ended (0 before
/// its first): a run's seq is written as its difference from that.
#[derive(Default)]
struct Seqs(HashMap<usize, u64>);

impl Seqs {
    fn expected(&self, agent: usize) -> u64 {
        self.0.get(&agent).copied().unwrap_or(0)
    }

    fn put(&self, out: &mut Vec<u8>, agent: usize, seq: u64) {
        (seq as i64 - self.expected(agent) as i64).encode(out);
    }

    fn get(&self, input: &mut &[u8], agent: usize) -> Result<u64, DecodeError> {
        let delta = i64::decode(input)?;
        (self.expected(agent) as i64)
            .checked_add(delta)
            .and_then(|s| u64::try_from(s).ok())
            .ok_or(DecodeError::Overflow)
    }

    /// Notes that `agent`'s run ended just before `end`.
    fn ran(&mut self, agent: usize, end: u64) {
        self.0.insert(agent, end);
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
        let spans = Spans::new(spans, self.len());
        let pieces = self.pieces(&spans);
        let agents = self.chunk_agents(&pieces, &spans);
        out.push(0); // No compression.
        self.put_agents(out, &agents.agents);
        put_usize(out, pieces.len());
        let writer = Writer {
            log: self,
            spans: &spans,
            agents: &agents,
        };
        let mut seqs = Seqs::default();
        for piece in &pieces {
            writer.put_run(out, piece, &mut seqs);
        }
        let steps = self.chunk_steps(&pieces, &agents);
        let side = ChunkExt { steps };
        if !side.is_empty() {
            ext::put_json(out, &side);
        }
    }

    /// The step ends of the chunk's events (and the one just before each
    /// agent's first, a step left open by the last save), by agent name:
    /// what the chunk's extension carries for #14. Mirrors [`get_steps`].
    fn chunk_steps(&self, pieces: &[Piece], agents: &AgentTable) -> BTreeMap<String, Vec<u64>> {
        let mut steps: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
        for (r, range) in pieces {
            let run = &self.runs[*r];
            let first = run.seq + (range.start - run.start) as u64;
            let last = first + range.len() as u64 - 1;
            let ends = &self.step_ends[run.agent.index()];
            let found: Vec<u64> = ends.range(first.saturating_sub(1)..=last).copied().collect();
            if !found.is_empty() {
                steps
                    .entry(self.agent_name(agents.agents[agents.index[&run.agent]]).to_string())
                    .or_default()
                    .extend(found);
            }
        }
        steps.into_iter().map(|(name, ends)| (name, ends.into_iter().collect())).collect()
    }

    /// The stretches of runs in `spans`, in order.
    fn pieces(&self, spans: &Spans) -> Vec<Piece> {
        let mut pieces = Vec::new();
        for span in &spans.spans {
            let mut r = self.run_index(span.start);
            while let Some(run) = self.runs.get(r)
                && run.start < span.end
            {
                let range = run.start.max(span.start)..run.end().min(span.end);
                if matches!(run.op, RunOp::Insert { .. }) {
                    pieces.extend(self.split_pasted(range).into_iter().map(|p| (r, p)));
                } else {
                    pieces.push((r, range));
                }
                r += 1;
            }
        }
        pieces
    }

    /// Inserts `range`, one run's, split where pastes start and end: each
    /// stretch all typed or all pasted, to be written as an insert or a
    /// copy. A pasted stretch's sources are all before its first event,
    /// since a chunk names them from there (a paste of what was pasted just
    /// before it, in the same run, starts a stretch of its own).
    fn split_pasted(&self, range: Range<Lv>) -> Vec<Range<Lv>> {
        let mut out = Vec::new();
        let mut at = range.start;
        for (first, source, len) in self.sources_in(range.clone()) {
            if first > at {
                out.push(at..first);
            }
            match out.last_mut() {
                // Pasted right after pasted, of what's before both: one copy.
                Some(last)
                    if last.end == first
                        && self.source(last.start).is_some()
                        && source + len <= last.start =>
                {
                    last.end = first + len
                }
                _ => out.push(first..first + len),
            }
            at = first + len;
        }
        if at < range.end {
            out.push(at..range.end);
        }
        out
    }

    /// The sources of pasted inserts `range`, as a chunk names them: each
    /// the first source event and how many of its agent's follow it by
    /// seq. Mirrors the decoder's reading of them in [`Planner::plan_op`].
    fn copy_sources(&self, range: Range<Lv>) -> Vec<(Lv, usize)> {
        let mut out: Vec<(Lv, usize)> = Vec::new();
        let mut end: Option<(AgentId, u64)> = None;
        for (_, source, len) in self.sources_in(range) {
            let run = self.run_of(source);
            let seq = run.seq + (source - run.start) as u64;
            match out.last_mut() {
                Some(last) if end == Some((run.agent, seq)) => last.1 += len,
                _ => out.push((source, len)),
            }
            end = Some((run.agent, seq + len as u64));
        }
        out
    }

    /// The agents a chunk names: those of its own events, and those of
    /// events it refers to from outside, in order of first mention.
    fn chunk_agents(&self, pieces: &[Piece], spans: &Spans) -> AgentTable {
        let mut agents = AgentTable::default();
        for (r, range) in pieces {
            let run = &self.runs[*r];
            agents.name(run.agent);
            let mut outside = |lv: Lv| {
                if spans.index(lv).is_none() {
                    agents.name(self.run_of(lv).agent);
                }
            };
            for p in self.piece_parents(*r, range.start) {
                outside(p);
            }
            match run.op {
                RunOp::Undelete { target, .. } => {
                    for t in target + (range.start - run.start)..target + (range.end - run.start) {
                        outside(t);
                    }
                }
                RunOp::Undo { target } => outside(target),
                RunOp::Insert { .. } => {
                    for (source, _) in self.copy_sources(range.clone()) {
                        outside(source);
                    }
                }
                _ => {}
            }
        }
        agents
    }

    /// Writes the agent table: a count, then each name.
    fn put_agents(&self, out: &mut Vec<u8>, agents: &[AgentId]) {
        put_usize(out, agents.len());
        for &agent in agents {
            put_str(out, self.agent_name(agent));
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

    /// Adds the events of a chunk (one [`encode_chunk`](Self::encode_chunk)
    /// wrote) that this log doesn't have yet. Returns how many were new.
    ///
    /// Everything is checked first: if the chunk is malformed, or refers
    /// to events this log doesn't have, nothing is added.
    pub fn decode_chunk(&mut self, mut bytes: &[u8]) -> Result<usize, DecodeError> {
        let input = &mut bytes;
        let (names, runs, side) = parse_chunk::<T>(input)?;
        let steps = get_steps(&side, &names)?;
        let mut planner = Planner::new(self, &names);
        let mut planned = Vec::new();
        for run in runs {
            planned.extend(planner.plan_run(run)?);
        }
        // A step ends at an event this log has, or the chunk brings.
        for (agent, ends) in &steps {
            if ends.iter().any(|&e| e >= planner.next_seq[*agent]) {
                return Err(DecodeError::Invalid("a step end past the agent's events"));
            }
        }
        let next = planner.next;
        let added = next - self.len();
        self.apply(&names, planned);
        debug_assert_eq!(self.len(), next, "planned numbering");
        for (agent, ends) in steps {
            let id = self.agent(&names[agent]);
            self.step_ends[id.index()].extend(ends);
        }
        Ok(added)
    }

    /// Adds a chunk's planned runs, everything checked.
    fn apply(&mut self, names: &[String], planned: Vec<PlannedRun<T>>) {
        let ids: Vec<AgentId> = names.iter().map(|n| self.agent(n)).collect();
        for PlannedRun {
            agent,
            seq,
            parents,
            op,
        } in planned
        {
            match op {
                PlannedOp::Run(op) => {
                    self.push_run(ids[agent], seq, normalize(parents), op);
                }
                PlannedOp::Undos(ops) => {
                    let mut parents = parents;
                    for (k, op) in ops.into_iter().enumerate() {
                        let lv = self.add_event(ids[agent], seq + k as u64, &parents, op);
                        parents = vec![lv];
                    }
                }
                PlannedOp::Copy { pos, sources } => {
                    // The sources are earlier, so here by now.
                    let items = sources.iter().map(|&s| self.item(s)).collect();
                    let op = RunOp::Insert { pos, items };
                    let first = self.push_run(ids[agent], seq, normalize(parents), op);
                    for (k, s) in sources.into_iter().enumerate() {
                        self.note_source(first + k, s);
                    }
                }
            }
        }
    }
}

/// Writes a chunk's runs: the log, the events the chunk carries, and the
/// agents it names.
struct Writer<'a, T> {
    log: &'a OpLog<T>,
    spans: &'a Spans,
    agents: &'a AgentTable,
}

impl<T: Clone + Codec> Writer<'_, T> {
    /// Writes one piece: agent, seq, parents, then the op. Mirrors
    /// [`get_run`].
    fn put_run(&self, out: &mut Vec<u8>, (r, range): &Piece, seqs: &mut Seqs) {
        let run = &self.log.runs[*r];
        let k = range.start - run.start;
        let first = self
            .spans
            .index(range.start)
            .expect("a piece is in the chunk");
        let agent = self.agents.index[&run.agent];
        put_usize(out, agent);
        let seq = run.seq + k as u64;
        seqs.put(out, agent, seq);
        seqs.ran(agent, seq + range.len() as u64);
        let parents = self.log.piece_parents(*r, range.start);
        put_usize(out, parents.len());
        for p in parents {
            self.put_ref(out, p, first);
        }
        if self.log.source(range.start).is_some() {
            self.put_copy(out, &run.op, k, range.clone(), first);
        } else {
            self.put_op(out, &run.op, k..k + range.len(), first);
        }
    }

    /// Writes pasted inserts `range`, from `k` in their run: a copy.
    /// Mirrors the copy in [`get_op`].
    fn put_copy(&self, out: &mut Vec<u8>, op: &RunOp<T>, k: usize, range: Range<Lv>, first: usize) {
        let RunOp::Insert { pos, .. } = op else {
            unreachable!("only inserts are pasted")
        };
        put_varint(out, COPY << 1);
        put_usize(out, pos + k);
        put_usize(out, range.len());
        for (source, len) in self.log.copy_sources(range) {
            self.put_ref(out, source, first);
            put_usize(out, len);
        }
    }

    /// Writes events `ks` (counted from the run's first) of a run's op: its
    /// tag, then its fields. Mirrors [`get_op`].
    fn put_op(&self, out: &mut Vec<u8>, op: &RunOp<T>, ks: Range<usize>, first: usize) {
        match op {
            RunOp::Insert { pos, items } => {
                put_varint(out, INSERT << 1);
                put_usize(out, pos + ks.start);
                put_usize(out, ks.len());
                T::encode_run(&items[ks], out);
            }
            RunOp::Delete { pos, .. } => {
                put_varint(out, DELETE << 1);
                put_usize(out, *pos);
                put_usize(out, ks.len());
            }
            RunOp::Undelete { target, .. } => {
                put_varint(out, UNDO << 1);
                put_usize(out, ks.len());
                for k in ks {
                    self.put_ref(out, target + k, first);
                }
            }
            RunOp::Undo { target } => {
                put_varint(out, UNDO << 1);
                put_usize(out, 1);
                self.put_ref(out, *target, first);
            }
            RunOp::Aside(aside) => {
                put_varint(out, ASIDE << 1);
                ext::put_json(out, &self.aside_json(aside));
            }
            RunOp::Move { pos, len, to } => {
                put_varint(out, MOVE << 1);
                put_usize(out, *pos);
                put_usize(out, *len);
                put_usize(out, *to);
            }
            RunOp::Mark { pos, level } => {
                put_varint(out, MARK << 1);
                put_usize(out, *pos);
                put_usize(out, *level as usize);
            }
        }
    }

    /// `lv` named for side data: its agent's name and seq.
    fn named(&self, lv: Lv) -> Named {
        let run = self.log.run_of(lv);
        Named {
            agent: self.log.agent_name(run.agent).to_string(),
            seq: run.seq + (lv - run.start) as u64,
        }
    }

    /// An aside as its run carries it. Mirrors [`Planner::aside`].
    fn aside_json(&self, aside: &Aside) -> AsideJson {
        match aside {
            Aside::RangeAdd { start, end, tag } => AsideJson::RangeAdd {
                start: self.named(*start),
                end: self.named(*end),
                tag: tag.clone(),
            },
            Aside::RangeSet { range, tag } => AsideJson::RangeSet {
                range: self.named(*range),
                tag: tag.clone(),
            },
            Aside::CommentAdd { start, end } => AsideJson::CommentAdd {
                start: self.named(*start),
                end: self.named(*end),
            },
            Aside::CommentSet { comment, state } => AsideJson::CommentSet {
                comment: self.named(*comment),
                state: *state,
            },
            Aside::BodyInsert { comment, pos, item } => AsideJson::BodyInsert {
                comment: self.named(*comment),
                pos: *pos,
                item: *item,
            },
            Aside::BodyDelete { comment, pos } => AsideJson::BodyDelete {
                comment: self.named(*comment),
                pos: *pos,
            },
        }
    }

    /// Writes a reference to `lv` from a run whose first event is `first`
    /// in the chunk: by distance if `lv` is in the chunk too, by agent and
    /// seq if not. Mirrors [`get_ref`].
    fn put_ref(&self, out: &mut Vec<u8>, lv: Lv, first: usize) {
        match self.spans.index(lv) {
            Some(at) => {
                debug_assert!(at < first, "a reference is to an earlier event");
                put_usize(out, (first - at) << 1);
            }
            None => {
                let run = self.log.run_of(lv);
                put_usize(out, (self.agents.index[&run.agent] << 1) | 1);
                put_varint(out, run.seq + (lv - run.start) as u64);
            }
        }
    }
}

/// What a new run of a chunk adds: a run as the log keeps it, or undos,
/// one target each (they needn't be side by side here, nor of one kind):
/// undeletes and undos of moves.
enum PlannedOp<T> {
    Run(RunOp<T>),
    Undos(Vec<Op<T>>),
    /// A paste: inserts at `pos`, each copying its source.
    Copy { pos: usize, sources: Vec<Lv> },
}

/// A run of a chunk with the events this log has already left out: the
/// agent (into the chunk's names), its first new seq, its parents, its op.
struct PlannedRun<T> {
    agent: usize,
    seq: u64,
    parents: Vec<Lv>,
    op: PlannedOp<T>,
}

/// Checks a chunk's runs against a log, run by run: which events the log
/// has already (a prefix of each agent's), the numbers the new ones get,
/// and their parents.
struct Planner<'a, T> {
    log: &'a OpLog<T>,
    /// The log's agent for each name, if it has one.
    agents: Vec<Option<AgentId>>,
    /// Each agent's next seq in the log before this chunk.
    have: Vec<u64>,
    /// Each agent's next new seq: what the log has, then what the chunk
    /// adds. A new event must be exactly that: every event includes the
    /// one before, so a replica has a prefix of each agent's events, and a
    /// summary can say so.
    next_seq: Vec<u64>,
    /// Each of the chunk's events so far, as numbered in the log.
    lvs: Vec<Lv>,
    /// Each of the chunk's events so far, as (agent, seq).
    ids: Vec<(usize, u64)>,
    /// The kinds of the new events so far.
    new_kinds: Vec<Kind>,
    /// The parents of each new event so far.
    new_parents: Vec<Vec<Lv>>,
    /// The number the next new event gets.
    next: Lv,
    /// The chunk's agents' names, for events side data names.
    names: &'a [String],
    /// Each agent's new runs so far: first seq, first number, length.
    planned: Vec<Vec<(u64, Lv, usize)>>,
    /// The new events that add a range, for a range set to name.
    new_ranges: std::collections::HashSet<Lv>,
    /// The new events that add a comment, for its later events to name.
    new_comments: std::collections::HashSet<Lv>,
}

impl<'a, T: Clone> Planner<'a, T> {
    fn new(log: &'a OpLog<T>, names: &'a [String]) -> Self {
        let agents: Vec<Option<AgentId>> = names.iter().map(|n| log.agent_by_name(n)).collect();
        let have: Vec<u64> = agents
            .iter()
            .map(|a| a.map_or(0, |a| log.next_seq[a.index()]))
            .collect();
        Planner {
            log,
            agents,
            next_seq: have.clone(),
            have,
            lvs: Vec::new(),
            ids: Vec::new(),
            new_kinds: Vec::new(),
            new_parents: Vec::new(),
            next: log.len(),
            names,
            planned: vec![Vec::new(); names.len()],
            new_ranges: Default::default(),
            new_comments: Default::default(),
        }
    }

    /// Plans one run: `None` if the log has all of it already.
    fn plan_run(&mut self, run: RawRun<T>) -> Result<Option<PlannedRun<T>>, DecodeError> {
        if run.agent >= self.agents.len() {
            return Err(DecodeError::Invalid("an agent the chunk doesn't name"));
        }
        let len = run.len();
        run.seq
            .checked_add(len as u64)
            .ok_or(DecodeError::Overflow)?;
        let first = self.lvs.len();
        self.ids.extend((0..len as u64).map(|k| (run.agent, run.seq + k)));
        let known = self.known(&run);
        if known == len {
            return Ok(None);
        }
        let seq = run.seq + known as u64;
        if seq != self.next_seq[run.agent] {
            return Err(DecodeError::Invalid("a gap in an agent's events"));
        }
        let n = len - known;
        self.next_seq[run.agent] += n as u64;
        let parents = if known == 0 {
            run.parents
                .iter()
                .map(|&p| self.resolve(p, first))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            vec![self.lvs[first + known - 1]]
        };
        let start = self.next;
        self.lvs.extend(start..start + n);
        self.next += n;
        self.planned[run.agent].push((seq, start, n));
        self.new_parents.push(parents.clone());
        self.new_parents.extend((start..start + n - 1).map(|p| vec![p]));
        let op = self.plan_op(run.op, known, n, first)?;
        // An undo names something its event saw (mamre#9): a peer could
        // send one that doesn't, and the walk would take it back where it
        // never was.
        if let PlannedOp::Undos(ops) = &op {
            for (k, op) in ops.iter().enumerate() {
                let (Op::Undelete { target } | Op::Undo { target }) = *op else {
                    unreachable!("an undo's op")
                };
                if !self.saw(&self.new_parents[start + k - self.log.len()], target) {
                    return Err(DecodeError::Invalid("an undo of something its event didn't see"));
                }
            }
        }
        Ok(Some(PlannedRun {
            agent: run.agent,
            seq,
            parents,
            op,
        }))
    }

    /// Whether an event after `parents` saw `target`: from the chunk's new
    /// events back into the log. Parents come before their events, so
    /// nothing before `target` can lead to it.
    fn saw(&self, parents: &[Lv], target: Lv) -> bool {
        let base = self.log.len();
        let mut todo: Vec<Lv> = parents.to_vec();
        let mut seen: std::collections::HashSet<Lv> = Default::default();
        while let Some(v) = todo.pop() {
            if v == target {
                return true;
            }
            if v < target || !seen.insert(v) {
                continue;
            }
            if v >= base {
                todo.extend(&self.new_parents[v - base]);
            } else {
                todo.extend(self.log.event(v).parents.iter());
            }
        }
        false
    }

    /// How many of `run`'s events, from its first, the log has already.
    /// Numbers them.
    fn known(&mut self, run: &RawRun<T>) -> usize {
        let known = self.have[run.agent]
            .saturating_sub(run.seq)
            .min(run.len() as u64) as usize;
        for k in 0..known {
            let a = self.agents[run.agent].expect("a known event's agent is known");
            self.lvs.push(
                self.log
                    .find(a, run.seq + k as u64)
                    .expect("a prefix of the agent's events"),
            );
        }
        known
    }

    /// The op for the `n` new events of a run, after the `known` the log
    /// has. The run's first event is `first` in the chunk.
    fn plan_op(
        &mut self,
        op: RawOp<T>,
        known: usize,
        n: usize,
        first: usize,
    ) -> Result<PlannedOp<T>, DecodeError> {
        Ok(match op {
            RawOp::Insert { pos, mut items } => {
                let pos = pos.checked_add(known).ok_or(DecodeError::Overflow)?;
                self.new_kinds.extend(std::iter::repeat_n(Kind::Insert, n));
                PlannedOp::Run(RunOp::Insert {
                    pos,
                    items: items.split_off(known),
                })
            }
            RawOp::Delete { pos, .. } => {
                self.new_kinds.extend(std::iter::repeat_n(Kind::Delete, n));
                PlannedOp::Run(RunOp::Delete { pos, len: n })
            }
            RawOp::Move { pos, len, to } => {
                self.new_kinds.push(Kind::Move);
                PlannedOp::Run(RunOp::Move { pos, len, to })
            }
            RawOp::Mark { pos, level } => {
                self.new_kinds.push(Kind::Mark);
                PlannedOp::Run(RunOp::Mark { pos, level })
            }
            RawOp::Copy { pos, sources, .. } => {
                let pos = pos.checked_add(known).ok_or(DecodeError::Overflow)?;
                // The run's first event, which every source is before.
                let before = self.lvs[first];
                let mut each = Vec::with_capacity(n);
                for (r, len) in sources {
                    let (agent, seq) = match r {
                        Ref::Back(d) => *first
                            .checked_sub(d)
                            .filter(|_| d > 0)
                            .and_then(|i| self.ids.get(i))
                            .ok_or(DecodeError::Invalid("a reference past the chunk's start"))?,
                        Ref::Before { agent, seq } => (agent, seq),
                    };
                    for j in 0..len as u64 {
                        let seq = seq.checked_add(j).ok_or(DecodeError::Overflow)?;
                        let at = self.lv_of(agent, seq)?;
                        if at >= before || self.kind(at) != Some(Kind::Insert) {
                            return Err(DecodeError::Invalid(
                                "a copy of something not an earlier insert",
                            ));
                        }
                        each.push(at);
                    }
                }
                self.new_kinds.extend(std::iter::repeat_n(Kind::Insert, n));
                PlannedOp::Copy {
                    pos,
                    sources: each.split_off(known),
                }
            }
            RawOp::Aside(json) => {
                let lv = self.next - 1;
                let aside = self.aside(json, lv)?;
                match aside {
                    Aside::RangeAdd { .. } => {
                        self.new_ranges.insert(lv);
                    }
                    Aside::CommentAdd { .. } => {
                        self.new_comments.insert(lv);
                    }
                    _ => {}
                }
                self.new_kinds.push(Kind::Aside);
                PlannedOp::Run(RunOp::Aside(aside))
            }
            RawOp::Undo { targets } => {
                let mut ops = Vec::with_capacity(n);
                for &t in &targets[known..] {
                    let target = self.resolve(t, first)?;
                    let (op, kind) = match self.kind(target) {
                        Some(Kind::Delete) => (Op::Undelete { target }, Kind::Undelete),
                        Some(Kind::Move | Kind::Undo) => (Op::Undo { target }, Kind::Undo),
                        _ => {
                            return Err(DecodeError::Invalid(
                                "an undo of something not a delete, a move or an undo of a move",
                            ));
                        }
                    };
                    self.new_kinds.push(kind);
                    ops.push(op);
                }
                PlannedOp::Undos(ops)
            }
        })
    }

    /// The event `r` names, from a run whose first event is `at` in the
    /// chunk.
    fn resolve(&self, r: Ref, at: usize) -> Result<Lv, DecodeError> {
        match r {
            Ref::Back(d) => {
                let i = at
                    .checked_sub(d)
                    .filter(|_| d > 0)
                    .ok_or(DecodeError::Invalid("a reference past the chunk's start"))?;
                Ok(self.lvs[i])
            }
            Ref::Before { agent, seq } => {
                let a = self
                    .agents
                    .get(agent)
                    .ok_or(DecodeError::Invalid("an agent the chunk doesn't name"))?;
                a.and_then(|a| self.log.find(a, seq))
                    .ok_or(DecodeError::Invalid("an event this log doesn't have"))
            }
        }
    }

    /// The event side data names: in the log, or new in this chunk.
    fn named(&self, named: &Named) -> Result<Lv, DecodeError> {
        let Some(i) = self.names.iter().position(|n| *n == named.agent) else {
            // An agent the chunk doesn't name: one the log has.
            return self
                .log
                .agent_by_name(&named.agent)
                .and_then(|a| self.log.find(a, named.seq))
                .ok_or(DecodeError::Invalid("an event this log doesn't have"));
        };
        self.lv_of(i, named.seq)
    }

    /// Event `seq` of agent `i` (into the chunk's names): in the log, or
    /// new in this chunk so far.
    fn lv_of(&self, i: usize, seq: u64) -> Result<Lv, DecodeError> {
        let unknown = DecodeError::Invalid("an event this log doesn't have");
        let Some(&have) = self.have.get(i) else {
            return Err(DecodeError::Invalid("an agent the chunk doesn't name"));
        };
        if seq < have {
            let a = self.agents[i].expect("a known event's agent is known");
            return self.log.find(a, seq).ok_or(unknown);
        }
        let runs = &self.planned[i];
        let k = runs.partition_point(|&(s, _, _)| s <= seq);
        k.checked_sub(1)
            .map(|k| runs[k])
            .filter(|&(s, _, n)| seq < s + n as u64)
            .map(|(s, lv, _)| lv + (seq - s) as usize)
            .ok_or(unknown)
    }

    /// An aside as event `lv`, from its JSON: what it names must be
    /// earlier, a range's ends inserts, a range set's range a range.
    /// Mirrors [`Writer::aside_json`].
    fn aside(&self, json: AsideJson, lv: Lv) -> Result<Aside, DecodeError> {
        let earlier = |named: &Named, ok: &dyn Fn(Lv) -> bool, what| {
            let at = self.named(named)?;
            if at < lv && ok(at) { Ok(at) } else { Err(DecodeError::Invalid(what)) }
        };
        let insert = |at: Lv| self.kind(at) == Some(Kind::Insert);
        let range = |at: Lv| {
            if at < self.log.len() {
                matches!(self.log.run_of(at).op, RunOp::Aside(Aside::RangeAdd { .. }))
            } else {
                self.new_ranges.contains(&at)
            }
        };
        let comment = |at: Lv| {
            if at < self.log.len() {
                matches!(self.log.run_of(at).op, RunOp::Aside(Aside::CommentAdd { .. }))
            } else {
                self.new_comments.contains(&at)
            }
        };
        let not_comment = "an event about a comment that isn't an earlier comment";
        Ok(match json {
            AsideJson::RangeAdd { start, end, tag } => Aside::RangeAdd {
                start: earlier(&start, &insert, "a range's end that isn't an earlier insert")?,
                end: earlier(&end, &insert, "a range's end that isn't an earlier insert")?,
                tag,
            },
            AsideJson::RangeSet { range: r, tag } => Aside::RangeSet {
                range: earlier(&r, &range, "a range set of something not an earlier range")?,
                tag,
            },
            AsideJson::CommentAdd { start, end } => Aside::CommentAdd {
                start: earlier(&start, &insert, "a comment's end that isn't an earlier insert")?,
                end: earlier(&end, &insert, "a comment's end that isn't an earlier insert")?,
            },
            AsideJson::CommentSet { comment: c, state } => Aside::CommentSet {
                comment: earlier(&c, &comment, not_comment)?,
                state,
            },
            AsideJson::BodyInsert { comment: c, pos, item } => Aside::BodyInsert {
                comment: earlier(&c, &comment, not_comment)?,
                pos,
                item,
            },
            AsideJson::BodyDelete { comment: c, pos } => Aside::BodyDelete {
                comment: earlier(&c, &comment, not_comment)?,
                pos,
            },
        })
    }

    /// The kind of event `lv`, in the log or planned.
    fn kind(&self, lv: Lv) -> Option<Kind> {
        if lv < self.log.len() {
            Some(self.log.kind(lv))
        } else {
            self.new_kinds.get(lv - self.log.len()).copied()
        }
    }
}

/// A chunk as read: its agents' names, its runs, and what it carries
/// beside them.
type Parsed<T> = (Vec<String>, Vec<RawRun<T>>, ChunkExt);

/// Reads a chunk's agents and runs. Mirrors
/// [`encode_events`](OpLog::encode_events).
fn parse_chunk<T: Codec>(input: &mut &[u8]) -> Result<Parsed<T>, DecodeError> {
    get_compression(input)?;
    let names = get_agents(input)?;
    let n = get_usize(input)?;
    let mut runs = Vec::with_capacity(n.min(input.len()));
    let mut seqs = Seqs::default();
    for _ in 0..n {
        runs.push(get_run(input, &mut seqs)?);
    }
    let side = ext::get_trailing(input, "bytes after the chunk")?;
    Ok((names, runs, side))
}

/// A chunk as JSON, for reading by people: each run with its agent, seq,
/// parents and op, references spelled out as agent and seq. Mirrors
/// [`parse_chunk`]; nothing is checked against a log.
pub(crate) fn dump_chunk<T: Codec + serde::Serialize>(
    mut body: &[u8],
) -> Result<serde_json::Value, DecodeError> {
    use serde_json::{Value, json};
    let (names, runs, side) = parse_chunk::<T>(&mut body)?;
    let name = |agent: usize| {
        names
            .get(agent)
            .cloned()
            .ok_or(DecodeError::Invalid("an agent the chunk doesn't name"))
    };
    // Each event of the chunk so far, as (agent, seq), for back references.
    let mut events: Vec<(usize, u64)> = Vec::new();
    let mut out = Vec::new();
    for run in &runs {
        let first = events.len();
        let spell = |r: &Ref| -> Result<Value, DecodeError> {
            let (agent, seq) = match *r {
                Ref::Back(d) => *first
                    .checked_sub(d)
                    .and_then(|k| events.get(k))
                    .ok_or(DecodeError::Invalid("a reference before the chunk's start"))?,
                Ref::Before { agent, seq } => (agent, seq),
            };
            Ok(json!({"agent": name(agent)?, "seq": seq}))
        };
        let parents = run.parents.iter().map(spell).collect::<Result<Vec<_>, _>>()?;
        let op = match &run.op {
            RawOp::Insert { pos, items } => json!({"insert": {"pos": pos, "items": items_json(items)}}),
            RawOp::Delete { pos, len } => json!({"delete": {"pos": pos, "count": len}}),
            RawOp::Undo { targets } => json!({"undo": {
                "targets": targets.iter().map(spell).collect::<Result<Vec<_>, _>>()?
            }}),
            RawOp::Move { pos, len, to } => json!({"move": {"pos": pos, "count": len, "to": to}}),
            RawOp::Mark { pos, level } => json!({"mark": {"pos": pos, "level": level}}),
            RawOp::Copy { pos, len, sources } => {
                let sources = sources
                    .iter()
                    .map(|(r, n)| {
                        let mut v = spell(r)?;
                        v["count"] = json!(n);
                        Ok(v)
                    })
                    .collect::<Result<Vec<_>, DecodeError>>()?;
                json!({"copy": {"pos": pos, "count": len, "sources": sources}})
            }
            RawOp::Aside(aside) => json!({"aside": aside}),
        };
        out.push(json!({"agent": name(run.agent)?, "seq": run.seq, "parents": parents, "op": op}));
        events.extend((0..run.len() as u64).map(|k| (run.agent, run.seq + k)));
    }
    let mut chunk = json!({"agents": names, "runs": out});
    if !side.is_empty() {
        chunk["extension"] = serde_json::to_value(&side).expect("side data is JSON");
    }
    Ok(chunk)
}

/// Items as JSON: text as one string, anything else as a list.
pub(crate) fn items_json<T: serde::Serialize>(items: &[T]) -> serde_json::Value {
    let value = serde_json::to_value(items).expect("items serialize");
    let text: Option<String> = value.as_array().and_then(|all| {
        all.iter()
            .map(|v| v.as_str().filter(|s| s.chars().count() == 1))
            .collect()
    });
    match text {
        Some(text) if !items.is_empty() => serde_json::Value::String(text),
        _ => value,
    }
}

/// The step ends a chunk carries, checked against its agents: each named
/// in the chunk, in order. Mirrors [`OpLog::chunk_steps`].
fn get_steps(side: &ChunkExt, names: &[String]) -> Result<Vec<(usize, Vec<u64>)>, DecodeError> {
    side.steps
        .iter()
        .map(|(name, ends)| {
            let agent = names
                .iter()
                .position(|n| n == name)
                .ok_or(DecodeError::Invalid("step ends for an agent the chunk doesn't name"))?;
            if !ends.windows(2).all(|w| w[0] < w[1]) {
                return Err(DecodeError::Invalid("step ends out of order"));
            }
            Ok((agent, ends.clone()))
        })
        .collect()
}

/// Reads the compression byte: only 0, none, so far.
fn get_compression(input: &mut &[u8]) -> Result<(), DecodeError> {
    match u8::decode(input)? {
        0 => Ok(()),
        _ => Err(DecodeError::Invalid("an unknown compression")),
    }
}

/// Reads the agent table. Mirrors [`OpLog::put_agents`].
fn get_agents(input: &mut &[u8]) -> Result<Vec<String>, DecodeError> {
    let n = get_usize(input)?;
    let mut names = Vec::with_capacity(n.min(input.len()));
    for _ in 0..n {
        names.push(get_str(input)?.to_string());
    }
    Ok(names)
}

/// Reads one run. Mirrors [`Writer::put_run`].
fn get_run<T: Codec>(input: &mut &[u8], seqs: &mut Seqs) -> Result<RawRun<T>, DecodeError> {
    let agent = get_usize(input)?;
    let seq = seqs.get(input, agent)?;
    let parents = get_refs(input)?;
    let op = get_op(input)?;
    let run = RawRun {
        agent,
        seq,
        parents,
        op,
    };
    if run.len() == 0 {
        return Err(DecodeError::Invalid("an empty run"));
    }
    let end = seq
        .checked_add(run.len() as u64)
        .ok_or(DecodeError::Overflow)?;
    seqs.ran(agent, end);
    Ok(run)
}

/// Reads one run's op: its tag, its target if any, then its fields.
/// Mirrors [`Writer::put_op`].
fn get_op<T: Codec>(input: &mut &[u8]) -> Result<RawOp<T>, DecodeError> {
    let tag = get_varint(input)?;
    if tag & 1 == 1 {
        get_target(input)?;
    }
    Ok(match tag >> 1 {
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
        UNDO => RawOp::Undo {
            targets: get_refs(input)?,
        },
        MOVE => RawOp::Move {
            pos: get_usize(input)?,
            len: get_usize(input)?,
            to: get_usize(input)?,
        },
        MARK => RawOp::Mark {
            pos: get_usize(input)?,
            level: u8::try_from(get_usize(input)?)
                .map_err(|_| DecodeError::Invalid("a section level past 255"))?,
        },
        COPY => {
            let pos = get_usize(input)?;
            let len = get_usize(input)?;
            let mut sources = Vec::new();
            let mut got = 0usize;
            while got < len {
                let r = get_ref(input)?;
                let n = get_usize(input)?;
                got = got.checked_add(n).filter(|&g| n > 0 && g <= len).ok_or(
                    DecodeError::Invalid("a copy's sources that don't add up to its count"),
                )?;
                sources.push((r, n));
            }
            RawOp::Copy { pos, len, sources }
        }
        ASIDE => RawOp::Aside(ext::get_json(input)?),
        _ => {
            return Err(DecodeError::Invalid(
                "an event kind this version doesn't know",
            ));
        }
    })
}

/// Reads a run's target. This version knows only the text, which is
/// written as no target, so every target is refused.
fn get_target(input: &mut &[u8]) -> Result<(), DecodeError> {
    Err(match get_varint(input)? {
        0 => DecodeError::Invalid("a target of 0, which is written as none"),
        _ => DecodeError::Invalid("a structure this version doesn't know"),
    })
}

/// Reads a count, then that many references.
fn get_refs(input: &mut &[u8]) -> Result<Vec<Ref>, DecodeError> {
    let n = get_usize(input)?;
    let mut refs = Vec::with_capacity(n.min(input.len()));
    for _ in 0..n {
        refs.push(get_ref(input)?);
    }
    Ok(refs)
}

/// Reads one reference. Mirrors [`Writer::put_ref`].
fn get_ref(input: &mut &[u8]) -> Result<Ref, DecodeError> {
    let v = get_usize(input)?;
    if v & 1 == 0 {
        Ok(Ref::Back(v >> 1))
    } else {
        Ok(Ref::Before {
            agent: v >> 1,
            seq: get_varint(input)?,
        })
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

    /// A chunk carrying side data a later version wrote still reads.
    #[test]
    fn a_chunks_unknown_side_data_is_ignored() {
        let mut bytes = one_insert();
        ext::put_json(&mut bytes, &serde_json::json!({"later": true}));
        let mut log = OpLog::<char>::new();
        assert_eq!(log.decode_chunk(&bytes), Ok(1));
    }

    #[test]
    fn bytes_after_a_chunks_extension_are_refused() {
        let mut bytes = one_insert();
        ext::put_json(&mut bytes, &serde_json::json!({}));
        bytes.push(0);
        let mut log = OpLog::<char>::new();
        assert_eq!(
            log.decode_chunk(&bytes),
            Err(DecodeError::Invalid("bytes after the chunk"))
        );
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

    /// a types "x" and deletes it; b undeletes it, after `parents` (refs).
    fn an_undelete_by_b(parents: &[u8]) -> Vec<u8> {
        let mut c = vec![0, 2, 1, b'a', 1, b'b', 3];
        // a: insert "x" at 0.
        c.extend([0, 0, 0, 0, 0, 1, 1, b'x']);
        // a: delete at 0, after the insert (back 1).
        c.extend([0, 0, 1, 2, 2, 0, 1]);
        // b: an undo of the delete (back 1).
        c.extend([1, 0, parents.len() as u8]);
        c.extend(parents);
        c.extend([4, 1, 2]);
        c
    }

    #[test]
    fn an_undelete_after_its_delete_reads() {
        let mut log = OpLog::<char>::new();
        assert_eq!(log.decode_chunk(&an_undelete_by_b(&[2])), Ok(3));
    }

    /// A peer could send an undo of something its event never saw (#9):
    /// refused, as nothing there could have undone it.
    #[test]
    fn an_undelete_of_a_delete_it_didnt_see_is_refused() {
        let refused = Err(DecodeError::Invalid(
            "an undo of something its event didn't see",
        ));
        let mut log = OpLog::<char>::new();
        assert_eq!(log.decode_chunk(&an_undelete_by_b(&[])), refused);
        assert_eq!(log.len(), 0);
        // After only the insert (back 2).
        assert_eq!(log.decode_chunk(&an_undelete_by_b(&[4])), refused);
        assert_eq!(log.len(), 0);
    }
}

#[cfg(test)]
mod move_tests {
    use crate::oplog::OpLog;

    /// A chunk holding a move reads back as the same events.
    #[test]
    fn a_move_round_trips() {
        let mut log = OpLog::<char>::new();
        let a = log.agent("a");
        let v = log.add_insert(a, &[], 0, "abcdef".chars());
        let v = log.add_move(a, &v, 1..3, 5);
        log.add_insert(a, &v, 0, "x".chars());
        let mut bytes = Vec::new();
        log.encode_chunk(0..log.len(), &mut bytes);
        let mut back = OpLog::<char>::new();
        assert_eq!(back.decode_chunk(&bytes), Ok(log.len()));
        for lv in 0..log.len() {
            assert_eq!(
                format!("{:?}", back.event(lv)),
                format!("{:?}", log.event(lv))
            );
        }
        assert_eq!(back.checkout(back.version()), log.checkout(log.version()));
    }
}

#[cfg(test)]
mod golden {
    use crate::oplog::{Lv, OpLog};

    /// Two agents, concurrent, every kind of event, and runs split by
    /// spans: the chunks for all of it, and for spans with gaps in.
    fn log() -> OpLog<char> {
        let mut log = OpLog::<char>::new();
        let a = log.agent("alice");
        let b = log.agent("bob");
        let v = log.add_insert(a, &[], 0, "hello world".chars());
        let va = log.add_delete(a, &v, 2..5);
        let vb = log.add_insert(b, &v, 11, "!?".chars());
        let vb = log.add_mark(b, &vb, 0, 2);
        let both = log.union(&va, &vb);
        let v = log.add_move(a, &both, 0..3, 7);
        let dels: Vec<Lv> = (11..14).collect();
        let v = log.add_undelete(b, &v, dels);
        let v = log.add_insert(b, &v, 1, "é".chars());
        log.add_delete(a, &v, 0..1);
        log
    }

    fn chunk(log: &OpLog<char>, spans: &[std::ops::Range<Lv>]) -> Vec<u8> {
        let mut out = Vec::new();
        log.encode_events(spans, &mut out);
        out
    }

    /// The bytes, as the encoder wrote them before #24 split it into
    /// steps: the format didn't change.
    #[test]
    fn the_bytes_are_pinned() {
        let log = log();
        let n = log.len();
        let mut all = Vec::new();
        log.encode_chunk(0..n, &mut all);
        let gaps = chunk(&log, &[3..9, 12..15, n - 2..n]);
        assert_eq!(all, ALL);
        assert_eq!(gaps, GAPS);
        // And they read back as the same events.
        let mut back = OpLog::<char>::new();
        assert_eq!(back.decode_chunk(&all), Ok(n));
        assert_eq!(back.checkout(back.version()), log.checkout(log.version()));
    }

    #[rustfmt::skip]
    const ALL: &[u8] = &[0, 2, 5, 97, 108, 105, 99, 101, 3, 98, 111, 98, 8, 0, 0, 0, 0, 0, 11, 11, 104, 101, 108, 108, 111, 32, 119, 111, 114, 108, 100, 0, 0, 1, 2, 2, 2, 3, 1, 0, 1, 8, 0, 11, 2, 2, 33, 63, 1, 0, 1, 2, 8, 0, 2, 0, 0, 2, 8, 2, 6, 0, 3, 7, 1, 0, 1, 2, 4, 3, 14, 12, 10, 1, 0, 1, 2, 0, 1, 1, 2, 195, 169, 0, 0, 1, 2, 2, 0, 1];
    #[rustfmt::skip]
    const GAPS: &[u8] = &[0, 2, 5, 97, 108, 105, 99, 101, 3, 98, 111, 98, 5, 0, 6, 1, 1, 2, 0, 3, 6, 6, 108, 111, 32, 119, 111, 114, 0, 6, 1, 1, 11, 2, 2, 2, 1, 0, 1, 1, 10, 0, 11, 1, 1, 33, 1, 10, 1, 3, 5, 0, 1, 1, 2, 195, 169, 0, 2, 1, 2, 2, 0, 1];
}
