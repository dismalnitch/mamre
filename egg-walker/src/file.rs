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
//!            outline, moved blocks; then [extension]: what it carries
//!            beside the books, as JSON (see `ext.rs`), only when there's
//!            something in it
//! ```
//!
//! [`dump`] shows a whole file as JSON, for reading by people.
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
use crate::encode::dump_chunk;
use crate::ext::{self, SnapshotExt};
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
    let side = accountant.side();
    if !side.is_empty() {
        ext::put_json(&mut body, &side);
    }
    record(file, SNAPSHOT, &body);
    log.len()
}

fn record(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    put_frame(out, kind, body);
}

/// Loads a file: its history, and its last snapshot.
pub fn load<T: Clone + Codec>(bytes: &[u8]) -> Result<Loaded<T>, LoadError> {
    header(bytes)?;
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

/// Checks a file's magic and version, and returns the version.
fn header(bytes: &[u8]) -> Result<u8, LoadError> {
    if bytes.len() < MAGIC.len() + 1 || &bytes[..MAGIC.len()] != MAGIC {
        return Err(LoadError::NotAFile);
    }
    match bytes[MAGIC.len()] {
        VERSION => Ok(VERSION),
        version => Err(LoadError::Version(version)),
    }
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
            let mut accountant = Accountant::decode(&mut input, log.len())?;
            if accountant.len() != doc.len() {
                return Err(DecodeError::Invalid("books for another document"));
            }
            let side: SnapshotExt = ext::get_trailing(&mut input, "bytes after the snapshot")?;
            accountant.apply_side(&side)?;
            Ok((total, Some(Snapshot { doc, accountant })))
        }
        // A kind a later version added: skipped whole.
        _ => Ok((total, None)),
    }
}

/// A whole file as JSON, for reading by people: each record in order,
/// chunks with their runs spelled out, snapshots with their document and
/// what they carry beside the books (the books themselves are binary, and
/// shown by size). A record that's cut off or damaged ends the list, as it
/// ends loading, with a note saying so.
pub fn dump<T: Clone + Codec + serde::Serialize>(
    bytes: &[u8],
) -> Result<serde_json::Value, LoadError> {
    use serde_json::json;
    let version = header(bytes)?;
    let mut log = OpLog::<T>::new();
    let mut records = Vec::new();
    let mut at = MAGIC.len() + 1;
    while at < bytes.len() {
        let shown = (|| {
            let mut input = &bytes[at..];
            let (kind, body) = get_frame(&mut input)?;
            let total = bytes.len() - at - input.len();
            let shown = match kind {
                CHUNK => {
                    let mut chunk = dump_chunk::<T>(body)?;
                    log.decode_chunk(body)?;
                    chunk["record"] = json!("chunk");
                    chunk
                }
                SNAPSHOT => {
                    let mut input = body;
                    let n = get_usize(&mut input)?;
                    let doc = T::decode_run(n, &mut input)?;
                    let before = input.len();
                    Accountant::decode(&mut input, log.len())?;
                    let books = before - input.len();
                    let side: serde_json::Value =
                        ext::get_trailing(&mut input, "bytes after the snapshot")?;
                    let mut snap = json!({
                        "record": "snapshot",
                        "document": crate::encode::items_json(&doc),
                        "books": format!("{books} bytes"),
                    });
                    if !side.is_null() {
                        snap["extension"] = side;
                    }
                    snap
                }
                other => json!({"record": other, "skipped": format!("{} bytes", body.len())}),
            };
            Ok::<_, DecodeError>((total, shown))
        })();
        match shown {
            Ok((total, shown)) => {
                records.push(shown);
                at += total;
            }
            Err(e) => {
                records.push(json!({"damaged": e.to_string(), "at": at}));
                break;
            }
        }
    }
    Ok(json!({"version": version, "records": records}))
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

    /// A saved file reads as JSON: its chunk's runs spelled out, its
    /// snapshot's document as text.
    #[test]
    fn a_file_dumps_as_json() {
        let mut log = OpLog::new();
        let a = log.agent("alice");
        let mut books = Accountant::new();
        let v = log.add_insert(a, &[], 0, "hello".chars());
        let v = log.add_delete(a, &v, 1..2);
        books.merge(&log, &v);
        let mut file = create();
        save(&mut file, &log, 0, "hllo".chars(), &books);
        let shown = dump::<char>(&file).unwrap();
        assert_eq!(shown["version"], 1);
        let records = shown["records"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["record"], "chunk");
        assert_eq!(records[0]["agents"], serde_json::json!(["alice"]));
        let runs = records[0]["runs"].as_array().unwrap();
        assert_eq!(runs[0]["op"]["insert"]["items"], "hello");
        assert_eq!(
            runs[1]["parents"],
            serde_json::json!([{"agent": "alice", "seq": 4}])
        );
        assert_eq!(
            runs[1]["op"]["delete"],
            serde_json::json!({"pos": 1, "count": 1})
        );
        assert_eq!(records[1]["record"], "snapshot");
        assert_eq!(records[1]["document"], "hllo");
    }

    /// A paste (#36) dumps as a copy, its sources by agent and seq.
    #[test]
    fn a_paste_dumps_as_a_copy() {
        let mut log = OpLog::new();
        let a = log.agent("alice");
        let mut books = Accountant::new();
        let v = log.add_insert(a, &[], 0, "hi ".chars());
        let v = log.add_paste(a, &v, 3, &[0, 1]);
        books.merge(&log, &v);
        let mut file = create();
        save(&mut file, &log, 0, "hi hi".chars(), &books);
        let shown = dump::<char>(&file).unwrap();
        let runs = shown["records"][0]["runs"].as_array().unwrap().clone();
        assert_eq!(runs[0]["op"]["insert"]["items"], "hi ");
        assert_eq!(
            runs[1]["op"],
            serde_json::json!({"copy": {"pos": 3, "count": 2, "sources": [
                {"agent": "alice", "seq": 0, "count": 2}
            ]}})
        );
        let loaded = load::<char>(&file).unwrap();
        assert_eq!(loaded.log.source(4), Some(1));
    }

    /// A damaged record ends the dump, as it ends loading, and says so.
    #[test]
    fn a_damaged_file_dumps_up_to_the_damage() {
        let mut log = OpLog::new();
        let a = log.agent("a");
        let mut books = Accountant::new();
        let v = log.add_insert(a, &[], 0, "hi".chars());
        books.merge(&log, &v);
        let mut file = create();
        save(&mut file, &log, 0, "hi".chars(), &books);
        let last = file.len() - 1;
        file[last] ^= 1;
        let shown = dump::<char>(&file).unwrap();
        let records = shown["records"].as_array().unwrap();
        assert_eq!(records[0]["record"], "chunk");
        assert!(records[1]["damaged"].is_string());
    }

    /// A snapshot carrying side data a later version wrote still loads.
    #[test]
    fn a_snapshots_unknown_side_data_is_ignored() {
        let mut log = OpLog::new();
        let a = log.agent("a");
        let mut books = Accountant::new();
        let v = log.add_insert(a, &[], 0, "hi".chars());
        books.merge(&log, &v);
        let mut file = create();
        let mut body = Vec::new();
        log.encode_chunk(0..log.len(), &mut body);
        record(&mut file, CHUNK, &body);
        let mut body = Vec::new();
        put_usize(&mut body, 2);
        char::encode_run(&['h', 'i'], &mut body);
        books.encode(&mut body);
        ext::put_json(&mut body, &serde_json::json!({"later": [1, 2]}));
        record(&mut file, SNAPSHOT, &body);
        let loaded = load::<char>(&file).unwrap();
        assert!(!loaded.damaged);
        assert_eq!(loaded.snapshot.unwrap().doc, ['h', 'i']);
        assert_eq!(
            dump::<char>(&file).unwrap()["records"][1]["extension"],
            serde_json::json!({"later": [1, 2]})
        );
    }

    /// A headless section survives a save and a load, its ghost written
    /// beside the books as JSON (#19).
    #[test]
    fn a_headless_section_saves_and_loads() {
        let mut log = OpLog::new();
        let a = log.agent("a");
        let v = log.add_insert(a, &[], 0, "abcdef".chars());
        let v = log.add_mark(a, &v, 4, 1);
        let v = log.add_delete(a, &v, 4..5);
        let mut books = Accountant::new();
        books.merge(&log, &v);
        let mut file = create();
        save(&mut file, &log, 0, "abcdf".chars(), &books);
        let loaded = load::<char>(&file).unwrap();
        assert!(!loaded.damaged);
        let back = loaded.snapshot.unwrap().accountant;
        assert_eq!(back.sections(), books.sections());
        assert_eq!(back.headless(), vec![4]);
        assert_eq!(
            dump::<char>(&file).unwrap()["records"][1]["extension"],
            serde_json::json!({"ghosts": [{"item": 4, "place": 4}]})
        );
    }

    /// A ghost that can't be (here, on an item that isn't parked) is
    /// refused: the snapshot is damaged, not wrongly read.
    #[test]
    fn a_ghost_that_cant_be_is_refused() {
        let mut log = OpLog::new();
        let a = log.agent("a");
        let mut books = Accountant::new();
        let v = log.add_insert(a, &[], 0, "hi".chars());
        books.merge(&log, &v);
        let mut file = create();
        let mut body = Vec::new();
        log.encode_chunk(0..log.len(), &mut body);
        record(&mut file, CHUNK, &body);
        let mut body = Vec::new();
        put_usize(&mut body, 2);
        char::encode_run(&['h', 'i'], &mut body);
        books.encode(&mut body);
        ext::put_json(
            &mut body,
            &serde_json::json!({"ghosts": [{"item": 0, "place": 0}]}),
        );
        record(&mut file, SNAPSHOT, &body);
        let loaded = load::<char>(&file).unwrap();
        assert!(loaded.damaged);
        assert!(loaded.snapshot.is_none());
    }
}
