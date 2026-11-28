//! Pastes (#36): an insert of copies, each remembering its source. The
//! engines see only inserts; chunks carry the source by identity.

use mamre_egg_walker::{Lv, OpLog, copies};

fn text(log: &OpLog<char>) -> String {
    copies::checkout(log, log.version()).into_iter().collect()
}

/// `log`'s events re-read from one chunk into a fresh log.
fn round_trip(log: &OpLog<char>) -> OpLog<char> {
    let mut chunk = Vec::new();
    log.encode_chunk(0..log.len(), &mut chunk);
    let mut other = OpLog::new();
    other
        .decode_chunk(&chunk)
        .expect("a chunk we wrote reads back");
    other
}

/// Each pasted event and its source, by agent and seq.
fn lineage(log: &OpLog<char>) -> Vec<((String, u64), (String, u64))> {
    let name = |lv: Lv| {
        let e = log.event(lv);
        (log.agent_name(e.agent).to_string(), e.seq)
    };
    let mut out: Vec<_> = (0..log.len())
        .filter_map(|lv| log.source(lv).map(|s| (name(lv), name(s))))
        .collect();
    out.sort();
    out
}

#[test]
fn a_paste_right_after_what_it_copies_round_trips() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, ['x']);
    let v = log.add_paste(a, &v, 1, &[0]);
    let v = log.add_paste(a, &v, 1, &[0, 1]);
    assert_eq!(text(&log), "xxxx");
    let _ = v;
    let other = round_trip(&log);
    assert_eq!(text(&other), "xxxx");
    assert_eq!(lineage(&other), lineage(&log));
}

/// Found by `three_peers_on_a_bad_network`: a paste at the end of a run,
/// of what was pasted just before it in that run. Its source is inside
/// the run, so a chunk can't name it from the run's start: it's written
/// as a copy of its own.
#[test]
fn a_paste_of_the_paste_just_before_it_in_one_run_round_trips() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, ['x']);
    let v = log.add_paste(a, &v, 1, &[0]);
    log.add_paste(a, &v, 2, &[0, 1]);
    assert_eq!(log.runs(), 1, "typing and both pastes are one run");
    let other = round_trip(&log);
    assert_eq!(text(&other), "xxxx");
    assert_eq!(lineage(&other), lineage(&log));
}

/// A paste is inserts: new ids, each pointing at its source; typed text
/// points nowhere.
#[test]
fn a_paste_is_inserts_that_remember_their_sources() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, "hello ".chars());
    let v = log.add_paste(a, &v, 6, &[0, 1, 2, 3, 4]);
    assert_eq!(text(&log), "hello hello");
    assert_eq!(v, [10]);
    assert_eq!(
        (0..6).map(|lv| log.source(lv)).collect::<Vec<_>>(),
        [None; 6]
    );
    assert_eq!(
        (6..11).map(|lv| log.source(lv)).collect::<Vec<_>>(),
        [Some(0), Some(1), Some(2), Some(3), Some(4)]
    );
}

/// The chunk names the source; the text isn't written twice.
#[test]
fn a_chunk_carries_a_paste_by_its_source() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, "a long sentence ".chars());
    let sources: Vec<Lv> = (0..16).collect();
    log.add_paste(a, &v, 16, &sources);
    let mut chunk = Vec::new();
    log.encode_chunk(0..log.len(), &mut chunk);
    let once = chunk
        .windows(15)
        .filter(|w| *w == b"a long sentence")
        .count();
    assert_eq!(once, 1, "{chunk:?}");
    let other = round_trip(&log);
    assert_eq!(text(&other), "a long sentence a long sentence ");
    assert_eq!(lineage(&other), lineage(&log));
}

/// Sources from two writers, in an order neither typed: several source
/// runs, read back the same.
#[test]
fn a_paste_of_two_writers_text_round_trips() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let b = log.agent("b");
    let v = log.add_insert(a, &[], 0, "abc".chars());
    let v = log.add_insert(b, &v, 3, "XYZ".chars());
    // "cXa": a's third, b's first, a's first.
    let v = log.add_paste(a, &v, 6, &[2, 3, 0]);
    assert_eq!(text(&log), "abcXYZcXa");
    let other = round_trip(&log);
    assert_eq!(text(&other), "abcXYZcXa");
    assert_eq!(lineage(&other), lineage(&log));
    let _ = v;
}

/// A source can be text deleted since: a paste from an older version.
/// It reads back, and so does a pull.
#[test]
fn a_paste_of_deleted_text_round_trips() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, "gone".chars());
    let v = log.add_delete(a, &v, 0..4);
    log.add_paste(a, &v, 0, &[0, 1, 2, 3]);
    assert_eq!(text(&log), "gone");
    let other = round_trip(&log);
    assert_eq!(text(&other), "gone");
    assert_eq!(lineage(&other), lineage(&log));
    let mut pulled = OpLog::new();
    pulled.pull(&log);
    assert_eq!(lineage(&pulled), lineage(&log));
}

/// A paste's source in an earlier chunk is named by agent and seq.
#[test]
fn a_paste_of_what_an_earlier_chunk_brought_round_trips() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let b = log.agent("b");
    let v = log.add_insert(a, &[], 0, "abc".chars());
    let typed = log.len();
    log.add_paste(b, &v, 0, &[1, 2]);
    let (mut first, mut second) = (Vec::new(), Vec::new());
    log.encode_chunk(0..typed, &mut first);
    log.encode_chunk(typed..log.len(), &mut second);
    let mut other = OpLog::new();
    assert!(
        other.decode_chunk(&second).is_err(),
        "its source isn't here yet"
    );
    other.decode_chunk(&first).unwrap();
    other.decode_chunk(&second).unwrap();
    assert_eq!(text(&other), "bcabc");
    assert_eq!(lineage(&other), lineage(&log));
}

/// Typing inside the source at the same time as the paste stays in the
/// source: the copy is the text as the paster saw it.
#[test]
fn typing_in_the_source_meanwhile_stays_out_of_the_copy() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let b = log.agent("b");
    let v = log.add_insert(a, &[], 0, "ab".chars());
    let typed = log.add_insert(a, &v, 1, ['-']);
    let pasted = log.add_paste(b, &v, 2, &[0, 1]);
    let both = log.union(&typed, &pasted);
    assert_eq!(
        copies::checkout(&log, &both)
            .into_iter()
            .collect::<String>(),
        "a-bab"
    );
}

// ---- Hand-made chunks: what's refused ----

/// A chunk of `a` typing "x", deleting it, then a copy of `sources`
/// (each a ref and a count) with `count` items.
fn copy_chunk(count: u8, sources: &[(u8, u8)]) -> Vec<u8> {
    let mut c = vec![0, 1, 1, b'a', 3];
    // Insert "x" at 0.
    c.extend([0, 0, 0, 0, 0, 1, 1, b'x']);
    // Delete at 0, after the insert.
    c.extend([0, 0, 1, 2, 2, 0, 1]);
    // Copy at 0, after the delete.
    c.extend([0, 0, 1, 2, 10, 0, count]);
    for &(r, n) in sources {
        c.extend([r, n]);
    }
    c
}

#[test]
fn a_hand_made_copy_of_deleted_text_reads() {
    // Back 2: the insert.
    let mut log = OpLog::new();
    assert_eq!(log.decode_chunk(&copy_chunk(1, &[(4, 1)])), Ok(3));
    assert_eq!(text(&log), "x");
    assert_eq!(log.source(2), Some(0));
}

#[test]
fn a_copy_of_what_isnt_an_insert_is_refused() {
    // Back 1: the delete.
    let mut log = OpLog::<char>::new();
    assert!(log.decode_chunk(&copy_chunk(1, &[(2, 1)])).is_err());
    assert!(log.is_empty());
}

#[test]
fn a_copy_whose_sources_dont_add_up_is_refused() {
    let mut log = OpLog::<char>::new();
    assert!(
        log.decode_chunk(&copy_chunk(1, &[(4, 2)])).is_err(),
        "too many"
    );
    assert!(
        log.decode_chunk(&copy_chunk(1, &[(4, 0), (4, 1)])).is_err(),
        "a source of none"
    );
    assert!(
        log.decode_chunk(&copy_chunk(2, &[(4, 1)])).is_err(),
        "too few"
    );
    // Back 0: the copy itself.
    assert!(
        log.decode_chunk(&copy_chunk(1, &[(0, 1)])).is_err(),
        "itself"
    );
    assert!(log.is_empty());
}
