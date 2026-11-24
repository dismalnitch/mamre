//! A document's file: one container holding its whole history, and the
//! state to open it without replaying any.
//!
//! ```text
//! file     = "MAMRE" 0x00, format version (byte, 1), records
//! record   = kind (byte), body length, body, CRC-32C of all before it (4 bytes, LE)
//! kind     = 1 chunk:    events since the last save (see `encode.rs`)
//!            2 snapshot: the state at a version, to open without replaying
//!            (other kinds are skipped, so they can be added)
//! snapshot = version (count, then events), document (count, items as a run),
//!            identities, anchors
//! ```
//!
//! Saving appends: a chunk of the events since the last save, then a
//! snapshot. Nothing is rewritten, so a save costs what changed, and a
//! crash during one leaves every earlier record whole: loading stops at
//! the first record that's cut off or fails its checksum, and says how far
//! the file was good, so the next save can carry on from there.
//!
//! A file is written by one log, so its events are numbered as that log
//! numbers them, and a snapshot refers to them by number. Loading reads the
//! chunks in order, which numbers them the same.

use mamre::codec::{Codec, DecodeError, get_frame, get_usize, put_frame, put_usize};

use crate::anchors::Anchors;
use crate::ids::Identities;
use crate::oplog::{Lv, OpLog};

const MAGIC: &[u8; 6] = b"MAMRE\0";
const VERSION: u8 = 1;
const CHUNK: u8 = 1;
const SNAPSHOT: u8 = 2;

/// The state of a document at a version: what opening it needs, beside
/// the history.
#[derive(Clone)]
pub struct Snapshot<T> {
    pub version: Vec<Lv>,
    pub doc: Vec<T>,
    pub ids: Identities,
    pub anchors: Anchors,
}

/// What loading a file found.
pub struct Loaded<T> {
    pub log: OpLog<T>,
    /// The last snapshot, if there was one. It may be of an older version
    /// than the log, if the last save was cut off after its chunk: bring it
    /// up to date with [`OpLog::changes_with`].
    pub snapshot: Option<Snapshot<T>>,
    /// How many bytes were good. A save after a crash appends from here.
    pub good: usize,
    /// Whether anything after `good` was cut off or damaged.
    pub damaged: bool,
}

/// Why a file couldn't be loaded at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// It doesn't start like a mamre file.
    NotAFile,
    /// It's a format version this one can't read.
    Version(u8),
}

/// A new, empty file.
pub fn create() -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.push(VERSION);
    out
}

/// Appends a save to `file`: the log's events from `saved` on (the first
/// save of a file passes 0), then the snapshot. Returns how many events the
/// file now holds: the next save's `saved`.
pub fn save<T: Clone + Codec>(
    file: &mut Vec<u8>,
    log: &OpLog<T>,
    saved: usize,
    version: &[Lv],
    doc: &[T],
    ids: &Identities,
    anchors: &Anchors,
) -> usize {
    if saved < log.len() {
        let mut body = Vec::new();
        log.encode_chunk(saved..log.len(), &mut body);
        record(file, CHUNK, &body);
    }
    let mut body = Vec::new();
    put_usize(&mut body, version.len());
    for &lv in version {
        put_usize(&mut body, lv);
    }
    put_usize(&mut body, doc.len());
    T::encode_run(doc, &mut body);
    ids.encode(&mut body);
    anchors.encode(&mut body);
    record(file, SNAPSHOT, &body);
    log.len()
}

fn record(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    put_frame(out, kind, body);
}

/// Loads a file: its history, and its last snapshot.
pub fn load<T: Clone + Codec>(bytes: &[u8]) -> Result<Loaded<T>, LoadError> {
    if bytes.len() < MAGIC.len() + 1 || &bytes[..MAGIC.len()] != MAGIC {
        return Err(LoadError::NotAFile);
    }
    let version = bytes[MAGIC.len()];
    if version != VERSION {
        return Err(LoadError::Version(version));
    }
    let mut loaded = Loaded {
        log: OpLog::new(),
        snapshot: None,
        good: MAGIC.len() + 1,
        damaged: false,
    };
    while loaded.good < bytes.len() {
        match read_record(&bytes[loaded.good..], &mut loaded.log) {
            Ok((len, snapshot)) => {
                if let Some(snapshot) = snapshot {
                    loaded.snapshot = Some(snapshot);
                }
                loaded.good += len;
            }
            Err(_) => {
                loaded.damaged = true;
                break;
            }
        }
    }
    Ok(loaded)
}

/// Reads one record from the front of `bytes`, applying a chunk to `log`.
/// Returns its length, and the snapshot if it was one.
#[allow(clippy::type_complexity)]
fn read_record<T: Clone + Codec>(
    bytes: &[u8],
    log: &mut OpLog<T>,
) -> Result<(usize, Option<Snapshot<T>>), DecodeError> {
    let mut input = bytes;
    let (kind, body) = get_frame(&mut input)?;
    let total = bytes.len() - input.len();
    match kind {
        CHUNK => {
            log.decode_chunk(body)?;
            Ok((total, None))
        }
        SNAPSHOT => {
            let mut input = body;
            let events = log.len();
            let n = get_usize(&mut input)?;
            let mut version = Vec::with_capacity(n.min(input.len()));
            for _ in 0..n {
                let lv = get_usize(&mut input)?;
                if lv >= events {
                    return Err(DecodeError::Invalid("a version this log doesn't have"));
                }
                version.push(lv);
            }
            let n = get_usize(&mut input)?;
            let doc = T::decode_run(n, &mut input)?;
            let ids = Identities::decode(&mut input, events)?;
            let anchors = Anchors::decode(&mut input, events)?;
            if !input.is_empty() {
                return Err(DecodeError::Invalid("bytes after the snapshot"));
            }
            Ok((
                total,
                Some(Snapshot {
                    version,
                    doc,
                    ids,
                    anchors,
                }),
            ))
        }
        // A kind a later version added: skipped whole.
        _ => Ok((total, None)),
    }
}
