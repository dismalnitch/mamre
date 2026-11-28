//! A move's came-from record (#68): it round-trips through chunks and
//! files, a malformed one is refused, and what's watched on moved
//! characters follows them.

use mamre_egg_walker::{Accountant, Aside, Follow, Lv, Op, OpLog};

/// "abcdef", then "bc" moved to the end as a delete, an insert and its
/// record: the log, and the version after.
fn moved() -> (OpLog<char>, Vec<Lv>) {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, "abcdef".chars());
    let v = log.add_delete(a, &v, 1..3);
    let first = log.len();
    let v = log.add_insert(a, &v, 4, "bc".chars());
    let v = log.add_came_from(a, &v, vec![first, first + 1], vec![1, 2]);
    (log, v)
}

fn text(books: &mut Accountant, log: &OpLog<char>, to: &[Lv], doc: &mut Vec<char>) -> String {
    for c in books.merge(log, to) {
        c.apply(doc);
    }
    doc.iter().collect()
}

#[test]
fn it_round_trips_in_a_chunk() {
    let (log, _) = moved();
    let mut bytes = Vec::new();
    log.encode_chunk(0..log.len(), &mut bytes);
    let mut back = OpLog::<char>::new();
    assert_eq!(back.decode_chunk(&bytes), Ok(log.len()));
    let last = log.len() - 1;
    assert_eq!(
        back.event(last).op,
        Op::Aside(Aside::CameFrom {
            copies: vec![8, 9],
            sources: vec![1, 2]
        })
    );
}

#[test]
fn its_sources_can_be_another_agents() {
    let mut log = OpLog::new();
    let (a, b) = (log.agent("a"), log.agent("b"));
    let v = log.add_insert(a, &[], 0, "ab".chars());
    let v = log.add_insert(b, &v, 2, "cd".chars());
    // b moves "bc" (a's "b", its own "c") to the front.
    let v = log.add_delete(b, &v, 1..3);
    let first = log.len();
    let v = log.add_insert(b, &v, 0, "bc".chars());
    log.add_came_from(b, &v, vec![first, first + 1], vec![1, 2]);
    let mut bytes = Vec::new();
    log.encode_chunk(0..log.len(), &mut bytes);
    let mut back = OpLog::<char>::new();
    assert_eq!(back.decode_chunk(&bytes), Ok(log.len()));
    assert_eq!(back.event(log.len() - 1).op, log.event(log.len() - 1).op);
}

/// The record is JSON in the chunk; one that doesn't pair up, or names
/// something that isn't an insert, is refused (#9).
#[test]
fn a_bad_one_is_refused() {
    let (log, _) = moved();
    let mut bytes = Vec::new();
    log.encode_chunk(0..log.len(), &mut bytes);
    let json = String::from_utf8_lossy(&bytes).into_owned();
    assert!(json.contains("\"len\":2"), "{json}");
    for (from, to, why) in [
        (
            "\"len\":2,",
            "\"len\":3,",
            "a came-from whose copies and sources don't pair up",
        ),
        // The copies start at a's delete, not its insert.
        (
            "\"seq\":8",
            "\"seq\":6",
            "a came-from naming something that isn't an earlier insert",
        ),
    ] {
        let bad = json.replacen(from, to, 1);
        assert_ne!(bad, json, "{from} is in {json}");
        let bad = bytes_with(&bytes, &json, &bad);
        let mut back = OpLog::<char>::new();
        assert_eq!(
            back.decode_chunk(&bad),
            Err(mamre::codec::DecodeError::Invalid(why)),
            "{from} → {to}"
        );
    }
}

/// `bytes` with its text `json` (lossily read) changed to `bad`, the same
/// length.
fn bytes_with(bytes: &[u8], json: &str, bad: &str) -> Vec<u8> {
    assert_eq!(json.len(), bad.len());
    let k = json
        .bytes()
        .zip(bad.bytes())
        .position(|(x, y)| x != y)
        .unwrap();
    let mut out = bytes.to_vec();
    out[k] = bad.as_bytes()[k];
    out
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
    let first = log.len();
    let v = log.add_insert(a, &v, 4, "bc".chars());
    assert_eq!(text(&mut books, &log, &v, &mut doc), "adefbc");
    assert_eq!(books.follow(c), Follow::Gone, "not yet: no record");
    let v = log.add_came_from(a, &v, vec![first, first + 1], vec![1, 2]);
    text(&mut books, &log, &v, &mut doc);
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
        let first = log.len();
        let v = log.add_insert(w, &v, to, ['y']);
        log.add_came_from(w, &v, vec![first], vec![1])
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
