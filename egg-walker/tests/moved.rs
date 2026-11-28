//! A move (#68) is a delete and a paste of what it deleted: the paste's
//! lineage (#36) says which characters the copies carry on. It round-trips
//! through chunks and files as a paste does, it costs references rather
//! than its text, and what's watched on moved characters follows them.

use mamre_egg_walker::{Accountant, Copied, Follow, Lv, OpLog};

/// "abcdef", then "bc" moved to the end as a delete and a paste: the log,
/// and the version after.
fn moved() -> (OpLog<char>, Vec<Lv>) {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, "abcdef".chars());
    let v = log.add_delete(a, &v, 1..3);
    let v = log.add_paste(a, &v, 4, &[1, 2]);
    (log, v)
}

fn round_trip(log: &OpLog<char>) -> OpLog<char> {
    let mut bytes = Vec::new();
    log.encode_chunk(0..log.len(), &mut bytes);
    let mut back = OpLog::<char>::new();
    assert_eq!(back.decode_chunk(&bytes), Ok(log.len()));
    back
}

#[test]
fn it_round_trips_in_a_chunk() {
    let (log, _) = moved();
    let back = round_trip(&log);
    let copies = Copied {
        copy: 8,
        source: 1,
        len: 2,
    };
    assert_eq!(log.copied(0..log.len()), [copies]);
    assert_eq!(back.copied(0..back.len()), [copies]);
}

/// Sources in two writers' runs, one after another, come back as they
/// went.
#[test]
fn sources_in_runs_round_trip() {
    let mut log = OpLog::new();
    let (a, b) = (log.agent("a"), log.agent("b"));
    let v = log.add_insert(a, &[], 0, "ab".chars());
    let v = log.add_insert(b, &v, 2, "cd".chars());
    let v = log.add_insert(a, &v, 4, "ef".chars());
    // "bcde" moved to the front: sources 1, 2..4 (b's), 4.
    let v = log.add_delete(a, &v, 1..5);
    log.add_paste(a, &v, 0, &[1, 2, 3, 4]);
    let back = round_trip(&log);
    assert_eq!(back.copied(0..back.len()), log.copied(0..log.len()));
    let pairs: Vec<(Lv, Lv)> = log.copied(0..log.len()).into_iter().flat_map(Copied::pairs).collect();
    assert_eq!(pairs, [(10, 1), (11, 2), (12, 3), (13, 4)]);
}

#[test]
fn its_sources_can_be_another_agents() {
    let mut log = OpLog::new();
    let (a, b) = (log.agent("a"), log.agent("b"));
    let v = log.add_insert(a, &[], 0, "ab".chars());
    let v = log.add_insert(b, &v, 2, "cd".chars());
    // b moves "bc" (a's "b", its own "c") to the front.
    let v = log.add_delete(b, &v, 1..3);
    log.add_paste(b, &v, 0, &[1, 2]);
    let back = round_trip(&log);
    assert_eq!(back.copied(0..back.len()), log.copied(0..log.len()));
}

/// A moved chapter costs a few references, not its text again: the copy's
/// characters are its sources', so a chunk names the sources and that's
/// all.
#[test]
fn a_move_costs_references_not_its_text() {
    let n = 200_000;
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, std::iter::repeat_n('x', n + 1));
    let typed = log.len();
    let v = log.add_delete(a, &v, 0..n);
    log.add_paste(a, &v, 1, &(0..n).collect::<Vec<_>>());
    let mut all = Vec::new();
    log.encode_chunk(0..log.len(), &mut all);
    let mut text = Vec::new();
    log.encode_chunk(0..typed, &mut text);
    assert!(text.len() > n, "the typing is its text: {}", text.len());
    // The move's delete and paste, beside it.
    let moved = all.len() - text.len();
    assert!(moved < 32, "a move of {n} characters takes {moved} bytes");
}

fn text(books: &mut Accountant, log: &OpLog<char>, to: &[Lv], doc: &mut Vec<char>) -> String {
    for c in books.merge(log, to) {
        c.apply(doc);
    }
    doc.iter().collect()
}

#[test]
fn a_watched_character_follows_its_copy() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let mut books = Accountant::new();
    let mut doc = Vec::new();
    let v = log.add_insert(a, &[], 0, "abcdef".chars());
    text(&mut books, &log, &v, &mut doc);
    let c = books.watch(2);
    assert_eq!(books.follow(c), Follow::Here(2));
    let v = log.add_delete(a, &v, 1..3);
    text(&mut books, &log, &v, &mut doc);
    assert_eq!(books.follow(c), Follow::Gone, "deleted, and not pasted yet");
    let first = log.len();
    let v = log.add_paste(a, &v, 4, &[1, 2]);
    assert_eq!(text(&mut books, &log, &v, &mut doc), "adefbc");
    assert_eq!(books.follow(c), Follow::Moved(5));
    assert_eq!(books.moved_from(first + 1), Some(c));
    books.unwatch(c);
    assert_eq!(books.follow(c), Follow::Gone, "no longer watched");
}

#[test]
fn two_copies_shown_are_ambiguous() {
    let mut log = OpLog::new();
    let (a, b) = (log.agent("a"), log.agent("b"));
    let mut books = Accountant::new();
    let mut doc = Vec::new();
    let base = log.add_insert(a, &[], 0, "xyz".chars());
    text(&mut books, &log, &base, &mut doc);
    let y = books.watch(1);
    // a moves "y" to the front, b to the end, at once.
    let mut move_y = |w, to: usize| {
        let v = log.add_delete(w, &base, 1..2);
        log.add_paste(w, &v, to, &[1])
    };
    let va = move_y(a, 0);
    let vb = move_y(b, 2);
    let all = log.union(&va, &vb);
    assert_eq!(text(&mut books, &log, &all, &mut doc), "yxzy");
    assert_eq!(books.follow(y), Follow::Ambiguous(vec![0, 3]));
}

/// After a load, the moves are read again from the log.
#[test]
fn it_follows_after_a_load() {
    let (log, v) = moved();
    let mut books = Accountant::new();
    let mut doc = Vec::new();
    text(&mut books, &log, &v, &mut doc);
    let mut file = mamre_egg_walker::file::create();
    let doc_now: Vec<char> = doc.clone();
    mamre_egg_walker::file::save(&mut file, &log, 0, doc_now, &books);
    let loaded = mamre_egg_walker::file::load::<char>(&file).unwrap();
    let snapshot = loaded.snapshot.expect("a snapshot");
    let mut books = snapshot.accountant;
    let mut doc = snapshot.doc;
    let version = loaded.log.version().to_vec();
    text(&mut books, &loaded.log, &version, &mut doc);
    assert_eq!(doc.iter().collect::<String>(), "adefbc");
    assert_eq!(books.moved_from(9), Some(2));
}

/// A big move costs a run, not a character (#76): the lineage of a
/// chapter's worth of moved text is one entry, and lookups into its middle
/// work.
#[test]
fn a_big_move_is_one_run() {
    let n = 200_000;
    let mut log = OpLog::new();
    let a = log.agent("a");
    let mut books = Accountant::new();
    let mut doc = Vec::new();
    let v = log.add_insert(a, &[], 0, std::iter::repeat_n('x', n + 1));
    text(&mut books, &log, &v, &mut doc);
    let watched = books.watch(n / 2);
    let v = log.add_delete(a, &v, 0..n);
    let first = log.len();
    let v = log.add_paste(a, &v, 1, &(0..n).collect::<Vec<_>>());
    assert_eq!(log.copied(first..first + n).len(), 1);
    text(&mut books, &log, &v, &mut doc);
    assert_eq!(books.follow(watched), Follow::Moved(1 + n / 2));
    assert_eq!(books.moved_from(first + n - 1), Some(n - 1));
    assert_eq!(books.copies(0), [first]);
    assert_eq!(books.moved_from(first + n), None, "past the run");
}
