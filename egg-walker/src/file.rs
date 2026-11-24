//! A document's file: one container holding its whole history, and the
//! state to open it without replaying any.
//!
//! ```text
//! file     = "MAMRE" 0x00, format version (byte, 1), records
//! record   = kind (byte), body length, body, CRC-32C of all before it (4 bytes, LE)
//! kind     = 1 chunk:    events since the last save (see `encode.rs`)
//!            2 snapshot: the state at a version, to open without replaying
//!            (other kinds are skipped, so they can be added)
//! snapshot = document (count, items as a run), then its accountant's
//!            books: version (count, then events), identities, anchors,
//!            outline, moved blocks
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

use crate::accountant::Accountant;
use crate::oplog::OpLog;

const MAGIC: &[u8; 6] = b"MAMRE\0";
const VERSION: u8 = 1;
const CHUNK: u8 = 1;
const SNAPSHOT: u8 = 2;

/// The state of a document at a version: what opening it needs, beside
/// the history. The version is the accountant's.
#[derive(Clone)]
pub struct Snapshot<T> {
    pub doc: Vec<T>,
    pub accountant: Accountant,
}

/// What loading a file found.
pub struct Loaded<T> {
    pub log: OpLog<T>,
    /// The last snapshot, if there was one. It may be of an older version
    /// than the log, if the last save was cut off after its chunk: bring it
    /// up to date with [`Accountant::merge`].
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
/// save of a file passes 0), then the snapshot: the document's items, at
/// its accountant's version, and the accountant. The items come as they
/// come, from a rope's iterator, say, so no document needs to be a slice.
/// Returns how many events the file now holds: the next save's `saved`.
pub fn save<T: Clone + Codec>(
    file: &mut Vec<u8>,
    log: &OpLog<T>,
    saved: usize,
    doc: impl IntoIterator<Item = T>,
    accountant: &Accountant,
) -> usize {
    if saved < log.len() {
        let mut body = Vec::new();
        log.encode_chunk(saved..log.len(), &mut body);
        record(file, CHUNK, &body);
    }
    // The items go straight from the document into the snapshot: how many
    // there are, the books know.
    let mut body = Vec::new();
    put_usize(&mut body, accountant.len());
    let n = T::encode_iter(doc, &mut body);
    debug_assert_eq!(n, accountant.len(), "the books are the document's");
    accountant.encode(&mut body);
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
            let n = get_usize(&mut input)?;
            let doc = T::decode_run(n, &mut input)?;
            let accountant = Accountant::decode(&mut input, log.len())?;
            if accountant.len() != doc.len() {
                return Err(DecodeError::Invalid("books for another document"));
            }
            if !input.is_empty() {
                return Err(DecodeError::Invalid("bytes after the snapshot"));
            }
            Ok((total, Some(Snapshot { doc, accountant })))
        }
        // A kind a later version added: skipped whole.
        _ => Ok((total, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Saving streams the document's items, and writes exactly the bytes
    /// collecting them into a `Vec` first did.
    #[test]
    fn a_saved_document_is_the_same_bytes_streamed() {
        let long: String = "a moment's ñ, then 𝄞; ".repeat(300);
        for text in ["", "hello", "héllo wörld ✓ 𝄞", long.as_str()] {
            let mut log = OpLog::new();
            let me = log.agent("me");
            let mut books = Accountant::new();
            let v = log.add_insert(me, &[], 0, text.chars());
            books.merge(&log, &v);
            let doc: Vec<char> = text.chars().collect();

            let mut streamed = create();
            save(&mut streamed, &log, 0, text.chars(), &books);

            // As it was: the items in a Vec, written as a slice.
            let mut collected = create();
            if log.len() > 0 {
                let mut body = Vec::new();
                log.encode_chunk(0..log.len(), &mut body);
                record(&mut collected, CHUNK, &body);
            }
            let mut body = Vec::new();
            put_usize(&mut body, doc.len());
            char::encode_run(&doc, &mut body);
            books.encode(&mut body);
            record(&mut collected, SNAPSHOT, &body);

            assert_eq!(streamed, collected, "saving {} chars", doc.len());
        }
    }
}
