//! Sync: what a replica has, in a few bytes, and what another is missing.
//!
//! Every event of an agent includes the one before (it saw its own edits),
//! so a replica always has a prefix of each agent's events, and saying how
//! many it has of each says exactly what it has. That's a [`Summary`]. A
//! replica sends its summary; the other answers with one chunk of the
//! events it has beyond it ([`OpLog::missing`], then
//! [`OpLog::encode_events`]). Chunks check what they need before adding
//! anything, so one that arrives early just fails and comes again later.
//!
//! On the wire, each message is a frame with a checksum, like a file's
//! records ([`Message::encode`]): bytes damaged on the way are refused.

use std::collections::BTreeMap;
use std::ops::Range;

use mamre::codec::{
    Codec, DecodeError, get_frame, get_str, get_usize, get_varint, put_frame, put_str, put_usize,
    put_varint,
};

use crate::oplog::{Lv, OpLog};

/// What replicas send each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// What the sender has: answer with what it's missing.
    Summary(Summary),
    /// A chunk of events (see [`OpLog::encode_events`]).
    Events(Vec<u8>),
}

const SUMMARY: u8 = 1;
const EVENTS: u8 = 2;

impl Message {
    /// The message as a frame, checksummed.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Message::Summary(summary) => {
                let mut body = Vec::new();
                summary.encode(&mut body);
                put_frame(&mut out, SUMMARY, &body);
            }
            Message::Events(chunk) => put_frame(&mut out, EVENTS, chunk),
        }
        out
    }

    /// Reads a message: an error if it's damaged or not one.
    pub fn decode(bytes: &[u8]) -> Result<Message, DecodeError> {
        let mut input = bytes;
        let (kind, body) = get_frame(&mut input)?;
        if !input.is_empty() {
            return Err(DecodeError::Invalid("bytes after the message"));
        }
        match kind {
            SUMMARY => {
                let mut body = body;
                let summary = Summary::decode(&mut body)?;
                if !body.is_empty() {
                    return Err(DecodeError::Invalid("bytes after the summary"));
                }
                Ok(Message::Summary(summary))
            }
            EVENTS => Ok(Message::Events(body.to_vec())),
            _ => Err(DecodeError::Invalid(
                "a message kind this version doesn't know",
            )),
        }
    }
}

/// How many events of each agent a replica has.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    counts: BTreeMap<String, u64>,
}

impl Summary {
    /// How many of `agent`'s events it covers.
    pub fn count(&self, agent: &str) -> u64 {
        self.counts.get(agent).copied().unwrap_or(0)
    }

    /// Whether everything `other` covers, this does too.
    pub fn covers(&self, other: &Summary) -> bool {
        other.counts.iter().all(|(a, &n)| self.count(a) >= n)
    }
}

impl Codec for Summary {
    fn encode(&self, out: &mut Vec<u8>) {
        put_usize(out, self.counts.len());
        for (agent, &n) in &self.counts {
            put_str(out, agent);
            put_varint(out, n);
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self, DecodeError> {
        let n = get_usize(input)?;
        let mut counts = BTreeMap::new();
        for _ in 0..n {
            let agent = get_str(input)?.to_string();
            counts.insert(agent, get_varint(input)?);
        }
        Ok(Summary { counts })
    }
}

impl<T> OpLog<T> {
    /// What this log has: how many events of each agent.
    pub fn summary(&self) -> Summary {
        Summary {
            counts: self
                .agents
                .iter()
                .zip(&self.next_seq)
                .filter(|&(_, &n)| n > 0)
                .map(|(a, &n)| (a.clone(), n))
                .collect(),
        }
    }

    /// The events this log has that a replica with `theirs` doesn't, as
    /// sorted spans of this log's numbering: what to send it.
    pub fn missing(&self, theirs: &Summary) -> Vec<Range<Lv>> {
        let mut spans: Vec<Range<Lv>> = Vec::new();
        for (agent, by_seq) in self.agents.iter().zip(&self.spans) {
            let have = theirs.count(agent);
            // The stretch holding `have`, from there on; then the rest.
            let from = by_seq
                .range(..=have)
                .next_back()
                .map(|(&s, _)| s)
                .unwrap_or(0);
            for (&seq, &(lv, len)) in by_seq.range(from..) {
                let end = seq + len as u64;
                if end <= have {
                    continue;
                }
                let skip = have.saturating_sub(seq) as usize;
                spans.push(lv + skip..lv + len);
            }
        }
        spans.sort_by_key(|s| s.start);
        // Join spans that touch.
        let mut joined: Vec<Range<Lv>> = Vec::new();
        for s in spans {
            match joined.last_mut() {
                Some(last) if last.end == s.start => last.end = s.end,
                _ => joined.push(s),
            }
        }
        joined
    }
}
