//! Comments (#33): they converge, survive moves, follow a word through a
//! delete-and-retype replacement, and come back exactly with an undo.

use std::collections::BTreeSet;

use mamre_egg_walker::{Accountant, AgentId, Change, Comment, CommentState, Lv, OpLog, copies};
use proptest::prelude::*;

struct Writer {
    me: AgentId,
    books: Accountant,
    doc: Vec<char>,
}

impl Writer {
    fn new(log: &mut OpLog<char>, name: &str) -> Self {
        Writer {
            me: log.agent(name),
            books: Accountant::new(),
            doc: Vec::new(),
        }
    }

    fn apply(&mut self, changes: Vec<Change<'_, char>>) {
        for c in changes {
            c.apply(&mut self.doc);
        }
    }

    fn merge(&mut self, log: &OpLog<char>, to: &[Lv]) {
        let changes = self.books.merge(log, to);
        self.apply(changes);
    }

    fn item(&self, pos: usize) -> Lv {
        self.books.identities().item(pos)
    }

    fn text(&self, span: std::ops::Range<usize>) -> String {
        self.doc[span].iter().collect()
    }
}

/// "the big dog", with a comment on "big": the log, the writer, the comment.
fn big() -> (OpLog<char>, Writer, Lv) {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    let v = log.add_insert(w.me, &[], 0, "the big dog".chars());
    w.merge(&log, &v);
    let v = log.add_comment(w.me, w.books.version(), w.item(4), w.item(6));
    let c = v[0];
    w.merge(&log, &v);
    assert_eq!(w.books.comment(c).map(|c| c.span), Some(4..7));
    (log, w, c)
}

#[test]
fn a_comment_on_big_follows_it_to_large() {
    let (mut log, mut w, c) = big();
    let v = log.add_delete(w.me, w.books.version(), 4..7);
    w.merge(&log, &v);
    // Every item of it gone: it falls back to where they were.
    assert_eq!(w.books.comment(c).map(|c| c.span), Some(4..4));
    let v = log.add_insert(w.me, w.books.version(), 4, "large".chars());
    w.merge(&log, &v);
    let span = w.books.comment(c).unwrap().span;
    assert_eq!(w.text(span), "large");
    assert_eq!(w.doc.iter().collect::<String>(), "the large dog");
}

#[test]
fn an_undo_of_the_delete_brings_it_back_exactly() {
    let (mut log, mut w, c) = big();
    let ids_before: Vec<Lv> = (0..w.doc.len()).map(|p| w.item(p)).collect();
    let v = log.add_delete(w.me, w.books.version(), 4..7);
    let deletes: Vec<Lv> = (v[0] - 2..=v[0]).collect();
    w.merge(&log, &v);
    let v = log.add_undo(w.me, w.books.version(), deletes);
    w.merge(&log, &v);
    assert_eq!(w.books.comment(c).map(|c| c.span), Some(4..7));
    let ids_after: Vec<Lv> = (0..w.doc.len()).map(|p| w.item(p)).collect();
    assert_eq!(ids_after, ids_before, "the same items, as themselves");
}

#[test]
fn a_comment_goes_with_a_moved_section() {
    let (mut log, mut w, c) = big();
    // "big " to the end.
    let v = log.add_move(w.me, w.books.version(), 4..8, 11);
    w.merge(&log, &v);
    assert_eq!(w.doc.iter().collect::<String>(), "the dogbig ");
    let span = w.books.comment(c).unwrap().span;
    assert_eq!(w.text(span), "big");
}

#[test]
fn a_body_is_a_small_text_of_its_own() {
    let (mut log, mut a, c) = big();
    let mut b = Writer::new(&mut log, "b");
    b.merge(&log, a.books.version());
    let va = log.add_body_insert(a.me, a.books.version(), c, 0, "too big?");
    a.merge(&log, &va);
    assert_eq!(a.books.comment_body(&log, c), "too big?");
    // B, at the same time, starts the body too.
    let vb = log.add_body_insert(b.me, b.books.version(), c, 0, "hm. ");
    b.merge(&log, &vb);
    let all = log.union(&va, &vb);
    a.merge(&log, &all);
    b.merge(&log, &all);
    let body = a.books.comment_body(&log, c);
    assert_eq!(body, b.books.comment_body(&log, c));
    assert_eq!(body.len(), "too big?hm. ".len());
    // Edited: a delete in the body.
    let v = log.add_body_delete(a.me, a.books.version(), c, 0..1);
    a.merge(&log, &v);
    assert_eq!(a.books.comment_body(&log, c).len(), body.len() - 1);
    // None of it touched the text.
    assert_eq!(a.doc.iter().collect::<String>(), "the big dog");
}

/// Comment events go in chunks as JSON in their runs: adding, state, and
/// a body's edits, a character each. They read back as the same events.
#[test]
fn comments_round_trip_in_chunks() {
    let (mut log, mut a, c) = big();
    let v = log.add_body_insert(a.me, a.books.version(), c, 0, "hi");
    a.merge(&log, &v);
    let v = log.add_body_delete(a.me, a.books.version(), c, 1..2);
    a.merge(&log, &v);
    let v = log.set_comment(a.me, a.books.version(), c, CommentState::Resolved);
    a.merge(&log, &v);
    let mut bytes = Vec::new();
    log.encode_chunk(0..log.len(), &mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    for want in [r#"{"comment_add":{"start":"#, r#""item":"h"}}"#, r#""state":"resolved"}}"#, r#"{"body_delete":"#] {
        assert!(text.contains(want), "{want} in {text}");
    }
    let mut back = OpLog::<char>::new();
    assert_eq!(back.decode_chunk(&bytes), Ok(log.len()));
    for lv in 0..log.len() {
        assert_eq!(event(&back, lv), event(&log, lv));
    }
    assert_eq!(back.comment_body(c, log.version()), "h");
    // A body edit naming something that isn't a comment is refused.
    let bad = String::from_utf8_lossy(&bytes).replacen(r#"{"body_insert":{"comment":{"agent":"w","seq":11}"#, r#"{"body_insert":{"comment":{"agent":"w","seq":10}"#, 1);
    assert_ne!(bad.as_bytes(), &bytes[..], "the test changed something");
    assert!(OpLog::<char>::new().decode_chunk(bad.as_bytes()).is_err());
}

#[test]
fn resolving_and_undoing_by_step() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    log.begin_group(w.me);
    let v = log.add_insert(w.me, &[], 0, "the big dog".chars());
    w.merge(&log, &v);
    log.end_group(w.me);
    log.begin_group(w.me);
    let v = log.add_comment(w.me, w.books.version(), w.item(4), w.item(6));
    let c = v[0];
    w.merge(&log, &v);
    log.end_group(w.me);
    log.begin_group(w.me);
    let v = log.set_comment(w.me, w.books.version(), c, CommentState::Resolved);
    w.merge(&log, &v);
    log.end_group(w.me);
    assert_eq!(
        w.books.comment(c).map(|c| c.state),
        Some(CommentState::Resolved)
    );
    let undo = |w: &mut Writer, log: &mut OpLog<char>| {
        let changes = w.books.undo_group(log, w.me).unwrap();
        for ch in changes {
            ch.apply(&mut w.doc);
        }
    };
    undo(&mut w, &mut log);
    assert_eq!(
        w.books.comment(c).map(|c| c.state),
        Some(CommentState::Open)
    );
    undo(&mut w, &mut log);
    assert_eq!(w.books.comment(c), None, "taken back");
    let redo = w.books.redo_group(&mut log, w.me).unwrap();
    w.apply(redo);
    assert_eq!(w.books.comment(c).map(|c| c.span), Some(4..7));
}

// ---- Many writers ----

#[derive(Clone, Debug)]
enum Act {
    Type(usize, prop::sample::Index, char),
    Delete(usize, prop::sample::Index),
    Move(
        usize,
        prop::sample::Index,
        prop::sample::Index,
        prop::sample::Index,
    ),
    Comment(usize, prop::sample::Index, prop::sample::Index),
    /// Deletes a comment's whole span, and types in its place.
    Replace(usize, prop::sample::Index, u8),
    Body(usize, prop::sample::Index, char),
    Undo(usize),
    Sync(usize, usize),
}

fn acts() -> impl Strategy<Value = Vec<Act>> {
    let i = || any::<prop::sample::Index>();
    let act = prop_oneof![
        4 => (0..3usize, i(), prop::char::range('a', 'e')).prop_map(|(w, p, c)| Act::Type(w, p, c)),
        2 => (0..3usize, i()).prop_map(|(w, p)| Act::Delete(w, p)),
        1 => (0..3usize, i(), i(), i()).prop_map(|(w, a, b, t)| Act::Move(w, a, b, t)),
        2 => (0..3usize, i(), i()).prop_map(|(w, a, b)| Act::Comment(w, a, b)),
        2 => (0..3usize, i(), 1u8..4).prop_map(|(w, k, n)| Act::Replace(w, k, n)),
        1 => (0..3usize, i(), prop::char::range('x', 'z')).prop_map(|(w, k, c)| Act::Body(w, k, c)),
        1 => (0..3usize).prop_map(Act::Undo),
        2 => (0..3usize, 0..3usize).prop_map(|(a, b)| Act::Sync(a, b)),
    ];
    prop::collection::vec(act, 1..40)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn comments_converge_and_follow_replacements(acts in acts()) {
        let mut log = OpLog::new();
        let mut ws: Vec<Writer> = ["a", "b", "c"].iter().map(|n| Writer::new(&mut log, n)).collect();
        let mut made: Vec<Lv> = Vec::new();
        for act in acts {
            let w = match act {
                Act::Sync(_, b) => b,
                Act::Type(w, ..) | Act::Delete(w, ..) | Act::Move(w, ..) | Act::Comment(w, ..)
                | Act::Replace(w, ..) | Act::Body(w, ..) | Act::Undo(w) => w,
            };
            let me = ws[w].me;
            let len = ws[w].doc.len();
            let grouped = !matches!(act, Act::Undo(_) | Act::Sync(..));
            if grouped {
                log.begin_group(me);
            }
            match act {
                Act::Type(_, p, c) => {
                    let v = log.add_insert(me, ws[w].books.version(), p.index(len + 1), [c]);
                    ws[w].merge(&log, &v);
                }
                Act::Delete(_, p) if len > 0 => {
                    let at = p.index(len);
                    let v = log.add_delete(me, ws[w].books.version(), at..at + 1);
                    ws[w].merge(&log, &v);
                }
                Act::Move(_, a, b, t) if len > 0 => {
                    let (a, b) = (a.index(len), b.index(len));
                    let (a, b) = (a.min(b), a.max(b) + 1);
                    let to = t.index(len + 1);
                    if to <= a || to >= b {
                        let v = log.add_move(me, ws[w].books.version(), a..b, to);
                        ws[w].merge(&log, &v);
                    }
                }
                Act::Comment(_, a, b) if len > 0 => {
                    let (a, b) = (a.index(len), b.index(len));
                    let (s, e) = (ws[w].item(a.min(b)), ws[w].item(a.max(b)));
                    let v = log.add_comment(me, ws[w].books.version(), s, e);
                    made.push(v[0]);
                    ws[w].merge(&log, &v);
                }
                Act::Replace(_, k, n) => {
                    let shown = ws[w].books.comments();
                    if !shown.is_empty() {
                        let c = shown[k.index(shown.len())].clone();
                        if !c.span.is_empty() {
                            let v = log.add_delete(me, ws[w].books.version(), c.span.clone());
                            ws[w].merge(&log, &v);
                        }
                        let new: String = std::iter::repeat_n('R', n as usize).collect();
                        let v = log.add_insert(me, ws[w].books.version(), c.span.start, new.chars());
                        ws[w].merge(&log, &v);
                        // A comment that covered something holds just the new
                        // word. (One whose end a move put before its start
                        // covers nothing, and stays so.)
                        if !c.span.is_empty() {
                            let got = ws[w].books.comment(c.id).map(|c| c.span);
                            prop_assert_eq!(got, Some(c.span.start..c.span.start + n as usize));
                        }
                    }
                }
                Act::Body(_, k, ch) => {
                    let mine: Vec<Lv> = made.iter().copied().filter(|&c| log.contains(ws[w].books.version(), c)).collect();
                    if !mine.is_empty() {
                        let c = mine[k.index(mine.len())];
                        let v = log.add_body_insert(me, ws[w].books.version(), c, 0, &ch.to_string());
                        ws[w].merge(&log, &v);
                    }
                }
                Act::Undo(_) => {
                    let writer = &mut ws[w];
                    if let Some(changes) = writer.books.undo_group(&mut log, me) {
                        for ch in changes {
                            ch.apply(&mut writer.doc);
                        }
                    }
                }
                Act::Sync(a, b) => {
                    let to = ws[a].books.version().to_vec();
                    ws[b].merge(&log, &to);
                }
                _ => {}
            }
            if grouped {
                log.end_group(me);
            }
            let at = ws[w].books.version().to_vec();
            prop_assert_eq!(&ws[w].doc, &copies::checkout(&log, &at));
            for c in ws[w].books.comments() {
                prop_assert!(c.span.end <= ws[w].doc.len());
            }
        }
        // Everyone with everything: the same comments where both ends show,
        // and the same bodies.
        let all = log.version().to_vec();
        for w in &mut ws {
            w.merge(&log, &all);
        }
        let ids: BTreeSet<Lv> = copies::identities(&log, &all).into_iter().collect();
        let shown = |w: &Writer| -> Vec<Comment> {
            w.books
                .comments()
                .into_iter()
                .filter(|c| ends_live(&log, c.id, &ids))
                .collect()
        };
        prop_assert_eq!(shown(&ws[0]), shown(&ws[1]));
        prop_assert_eq!(shown(&ws[1]), shown(&ws[2]));
        for &c in &made {
            let body = ws[0].books.comment_body(&log, c);
            prop_assert_eq!(&body, &ws[1].books.comment_body(&log, c));
            prop_assert_eq!(&body, &ws[2].books.comment_body(&log, c));
        }
        // And the whole history through a chunk: the same events, so the
        // same comments and bodies.
        let mut bytes = Vec::new();
        log.encode_chunk(0..log.len(), &mut bytes);
        let mut back = OpLog::<char>::new();
        prop_assert_eq!(back.decode_chunk(&bytes), Ok(log.len()));
        for lv in 0..log.len() {
            prop_assert_eq!(event(&back, lv), event(&log, lv));
        }
        for &c in &made {
            prop_assert_eq!(back.comment_body(c, &all), log.comment_body(c, &all));
        }
    }
}

/// Whether both of comment `c`'s end items show.
fn ends_live(log: &OpLog<char>, c: Lv, ids: &BTreeSet<Lv>) -> bool {
    match log.event(c).op {
        mamre_egg_walker::Op::Aside(mamre_egg_walker::Aside::CommentAdd { start, end }) => {
            ids.contains(&start) && ids.contains(&end)
        }
        _ => false,
    }
}

/// Event `lv`, with its agent by name: a log read from a chunk numbers
/// agents in the order the chunk names them.
fn event(log: &OpLog<char>, lv: Lv) -> String {
    let e = log.event(lv);
    format!("{} {} {:?} {:?}", log.agent_name(e.agent), e.seq, e.parents, e.op)
}
