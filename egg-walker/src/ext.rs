//! Side data: what isn't the text's core log, written as JSON.
//!
//! The core log (text inserts and deletes, moves, marks, undos, with their
//! parents and agents) is our own bytes, in `encode.rs`: it's nearly all of
//! a file, and it's settled. What sits beside the text (ranges, comments,
//! undo steps, ghosts' places, moves' came-from records, the history
//! index) is rare, so its size
//! hardly matters, and it's what's still changing, so being able to add a
//! field matters more. It's written with serde:
//!
//! ```text
//! extension = format (byte, 1: JSON), length, the bytes
//! ```
//!
//! The format byte leaves room for a compact binary format later without
//! breaking files: the same derives work with either. Fields a reader
//! doesn't know are ignored, so later versions can add them; a field whose
//! meaning a reader can't do without belongs in the core log, or behind a
//! new event kind, which older readers refuse.
//!
//! Characters are named as `{"agent": "…", "seq": n}` in side data, not by
//! the core log's back references: side data stands alone.

use std::collections::BTreeMap;

use mamre::codec::{DecodeError, get_str, put_str};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The one format so far.
const JSON: u8 = 1;

/// Writes `value` as an extension: the format byte, then its JSON.
pub(crate) fn put_json<V: Serialize>(out: &mut Vec<u8>, value: &V) {
    out.push(JSON);
    let json = serde_json::to_string(value).expect("side data is always valid JSON");
    put_str(out, &json);
}

/// Reads an extension [`put_json`] wrote.
pub(crate) fn get_json<V: DeserializeOwned>(input: &mut &[u8]) -> Result<V, DecodeError> {
    let (&format, rest) = input.split_first().ok_or(DecodeError::UnexpectedEnd)?;
    *input = rest;
    if format != JSON {
        return Err(DecodeError::Invalid("an extension in a format this version doesn't know"));
    }
    serde_json::from_str(get_str(input)?)
        .map_err(|_| DecodeError::Invalid("an extension that isn't the JSON it should be"))
}

/// Reads the extension that ends a body, if there is one: an empty rest
/// is nothing beside, so the default. `after` is the complaint for bytes
/// past it.
pub(crate) fn get_trailing<V: DeserializeOwned + Default>(
    input: &mut &[u8],
    after: &'static str,
) -> Result<V, DecodeError> {
    if input.is_empty() {
        return Ok(V::default());
    }
    let side = get_json(input)?;
    if !input.is_empty() {
        return Err(DecodeError::Invalid(after));
    }
    Ok(side)
}

/// What a chunk carries beside its runs. Written only when there's
/// something in it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ChunkExt {
    /// Where undo steps end (#14): for each agent, by name, the seqs of
    /// the events that end its steps, in order.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) steps: BTreeMap<String, Vec<u64>>,
}

impl ChunkExt {
    pub(crate) fn is_empty(&self) -> bool {
        *self == ChunkExt::default()
    }
}

/// What a snapshot carries beside the books. Written only when there's
/// something in it. Events are named by number, as the rest of the
/// snapshot names them: a file is written by one log (see `file.rs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct SnapshotExt {
    /// The outline's ghosts (#19): each deleted heading that keeps its
    /// place, and the place, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) ghosts: Vec<Ghost>,
}

/// A ghost: a deleted item that keeps its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Ghost {
    pub(crate) item: usize,
    pub(crate) place: usize,
}

impl SnapshotExt {
    pub(crate) fn is_empty(&self) -> bool {
        *self == SnapshotExt::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Later {
        later: Vec<u64>,
    }

    #[test]
    fn json_round_trips() {
        let mut out = Vec::new();
        put_json(&mut out, &Later { later: vec![3, 9] });
        assert_eq!(out[0], JSON);
        let mut input = &out[..];
        assert_eq!(get_json::<Later>(&mut input), Ok(Later { later: vec![3, 9] }));
        assert!(input.is_empty());
    }

    /// A later version's fields are ignored, so it can add them.
    #[test]
    fn fields_a_reader_doesnt_know_are_ignored() {
        let mut out = Vec::new();
        put_json(&mut out, &Later { later: vec![1] });
        assert_eq!(get_json::<ChunkExt>(&mut &out[..]), Ok(ChunkExt::default()));
    }

    #[test]
    fn another_format_is_refused() {
        let mut out = Vec::new();
        put_json(&mut out, &ChunkExt::default());
        out[0] = 2;
        assert!(get_json::<ChunkExt>(&mut &out[..]).is_err());
    }

    #[test]
    fn bad_json_is_refused() {
        let mut out = vec![JSON];
        put_str(&mut out, "{\"steps\": [");
        assert!(get_json::<ChunkExt>(&mut &out[..]).is_err());
    }
}

/// An event, named for side data: its agent's name and seq.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Named {
    pub(crate) agent: String,
    pub(crate) seq: u64,
}

/// An aside event (see [`Aside`](crate::Aside)) as a chunk carries it:
/// JSON in its run, after the kind. Events it names are named in full.
/// A variant a reader doesn't know is refused, as an unknown event kind
/// is: a later event could depend on it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum AsideJson {
    /// A range from `start` to `end` (inserts, both included), tagged.
    RangeAdd { start: Named, end: Named, tag: String },
    /// A range's tag set, or the range removed (`null`).
    RangeSet { range: Named, tag: Option<String> },
    /// A comment on `start` to `end` (inserts, both included).
    CommentAdd { start: Named, end: Named },
    /// A comment's state: "open", "resolved" or "removed".
    CommentSet { comment: Named, state: crate::CommentState },
    /// A character typed into a comment's body at `pos`.
    BodyInsert { comment: Named, pos: usize, item: char },
    /// The character at `pos` taken out of a comment's body.
    BodyDelete { comment: Named, pos: usize },
    /// A move's copies (#68): `first` and the `len - 1` inserts its agent
    /// made after it, copies of the `from` runs in order. Each run is
    /// `start` and the `len - 1` events its agent made after it.
    CameFrom { first: Named, len: usize, from: Vec<NamedRun> },
}

/// Events in a run of one agent's: the first, and how many.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NamedRun {
    pub(crate) start: Named,
    pub(crate) len: usize,
}
