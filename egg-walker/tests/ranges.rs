//! Ranges the author defines (#32): they follow their text through edits,
//! moves and merges, converge across peers, and match a recomputation from
//! where their ends are.

use std::collections::BTreeSet;

use mamre_egg_walker::{Accountant, AgentId, AuthorRange, Change, Lv, OpLog, copies, ranges};
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
}

fn typed(log: &mut OpLog<char>, w: &mut Writer, text: &str) {
    let v = log.add_insert(w.me, w.books.version(), w.doc.len(), text.chars());
    w.merge(log, &v);
}

fn tagged(r: &AuthorRange) -> (&str, std::ops::Range<usize>) {
    (r.tag.as_str(), r.span.clone())
}

#[test]
fn a_range_follows_its_text() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    typed(&mut log, &mut w, "the big dog ran");
    let (s, e) = (w.item(4), w.item(10));
    let v = log.add_range(w.me, w.books.version(), s, e, "pov");
    let id = v[0];
    w.merge(&log, &v);
    assert_eq!(w.books.range(id).map(|r| r.span), Some(4..11));
    // Typing before it, and inside it.
    let v = log.add_insert(w.me, w.books.version(), 0, "so ".chars());
    w.merge(&log, &v);
    let v = log.add_insert(w.me, w.books.version(), 11, "brown ".chars());
    w.merge(&log, &v);
    assert_eq!(w.doc.iter().collect::<String>(), "so the big brown dog ran");
    assert_eq!(w.books.range(id).map(|r| r.span), Some(7..20));
    assert_eq!(w.books.ranges_at(9).len(), 1);
    assert!(w.books.ranges_at(3).is_empty());
    assert_eq!(w.books.ranges_overlapping(0..8).len(), 1);
    assert!(w.books.ranges_overlapping(20..24).is_empty());
    // Retagged, removed.
    let v = log.set_range(w.me, w.books.version(), id, Some("flashback"));
    w.merge(&log, &v);
    assert_eq!(
        w.books.range(id).as_ref().map(tagged),
        Some(("flashback", 7..20))
    );
    let v = log.set_range(w.me, w.books.version(), id, None);
    w.merge(&log, &v);
    assert_eq!(w.books.range(id), None);
}

#[test]
fn a_range_follows_a_moved_section() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    typed(&mut log, &mut w, "AaaBbbCcc");
    let v = log.add_range(w.me, w.books.version(), w.item(4), w.item(5), "needs work");
    let id = v[0];
    w.merge(&log, &v);
    let v = log.add_move(w.me, w.books.version(), 3..6, 9);
    w.merge(&log, &v);
    assert_eq!(w.doc.iter().collect::<String>(), "AaaCccBbb");
    assert_eq!(w.books.range(id).map(|r| r.span), Some(7..9));
}

#[test]
fn a_deleted_end_falls_back_inside() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    typed(&mut log, &mut w, "abcdefgh");
    let v = log.add_range(w.me, w.books.version(), w.item(2), w.item(5), "t");
    let id = v[0];
    w.merge(&log, &v);
    assert_eq!(w.books.range(id).map(|r| r.span), Some(2..6));
    // Its last item, and its first, deleted: the nearest survivors inside.
    let v = log.add_delete(w.me, w.books.version(), 5..6);
    w.merge(&log, &v);
    assert_eq!(w.books.range(id).map(|r| r.span), Some(2..5));
    let v = log.add_delete(w.me, w.books.version(), 2..3);
    w.merge(&log, &v);
    assert_eq!(w.books.range(id).map(|r| r.span), Some(2..4));
    assert_eq!(w.doc[2..4].iter().collect::<String>(), "de");
}

#[test]
fn the_latest_set_wins_everywhere() {
    let mut log = OpLog::new();
    let mut a = Writer::new(&mut log, "a");
    let mut b = Writer::new(&mut log, "b");
    typed(&mut log, &mut a, "hello world");
    let v = log.add_range(a.me, a.books.version(), a.item(0), a.item(4), "x");
    let id = v[0];
    a.merge(&log, &v);
    b.merge(&log, a.books.version());
    let va = log.set_range(a.me, a.books.version(), id, Some("from a"));
    let vb = log.set_range(b.me, b.books.version(), id, Some("from b"));
    a.merge(&log, &va);
    b.merge(&log, &vb);
    let all = log.union(&va, &vb);
    a.merge(&log, &all);
    b.merge(&log, &all);
    assert_eq!(a.books.ranges(), b.books.ranges());
    assert_eq!(a.books.ranges().len(), 1);
}

#[test]
fn ranges_undo_with_their_group() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    log.begin_group(w.me);
    typed(&mut log, &mut w, "some text here");
    log.end_group(w.me);
    log.begin_group(w.me);
    let v = log.add_range(w.me, w.books.version(), w.item(5), w.item(8), "scene");
    let id = v[0];
    w.merge(&log, &v);
    log.end_group(w.me);
    log.begin_group(w.me);
    let v = log.set_range(w.me, w.books.version(), id, Some("pov"));
    w.merge(&log, &v);
    log.end_group(w.me);
    let undo = |w: &mut Writer, log: &mut OpLog<char>| {
        let changes = w.books.undo_group(log, w.me).unwrap();
        for c in changes {
            c.apply(&mut w.doc);
        }
    };
    undo(&mut w, &mut log);
    assert_eq!(
        w.books.range(id).as_ref().map(tagged),
        Some(("scene", 5..9))
    );
    undo(&mut w, &mut log);
    assert_eq!(w.books.range(id), None);
    let redo = w.books.redo_group(&mut log, w.me).unwrap();
    w.apply(redo);
    assert_eq!(
        w.books.range(id).as_ref().map(tagged),
        Some(("scene", 5..9))
    );
}

#[test]
#[should_panic(expected = "until their encoding is decided")]
fn a_chunk_with_a_range_is_refused_until_its_encoding_is_decided() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, "ab".chars());
    log.add_range(a, &v, 0, 1, "t");
    let mut out = Vec::new();
    log.encode_chunk(0..log.len(), &mut out);
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
    Add(usize, prop::sample::Index, prop::sample::Index, u8),
    Set(usize, prop::sample::Index, Option<u8>),
    Undo(usize),
    Sync(usize, usize),
}

fn acts() -> impl Strategy<Value = Vec<Act>> {
    let i = || any::<prop::sample::Index>();
    let act = prop_oneof![
        4 => (0..3usize, i(), prop::char::range('a', 'e')).prop_map(|(w, p, c)| Act::Type(w, p, c)),
        2 => (0..3usize, i()).prop_map(|(w, p)| Act::Delete(w, p)),
        1 => (0..3usize, i(), i(), i()).prop_map(|(w, a, b, t)| Act::Move(w, a, b, t)),
        2 => (0..3usize, i(), i(), 0u8..3).prop_map(|(w, a, b, t)| Act::Add(w, a, b, t)),
        2 => (0..3usize, i(), prop::option::of(0u8..3)).prop_map(|(w, k, t)| Act::Set(w, k, t)),
        1 => (0..3usize).prop_map(Act::Undo),
        2 => (0..3usize, 0..3usize).prop_map(|(a, b)| Act::Sync(a, b)),
    ];
    prop::collection::vec(act, 1..40)
}

/// What the ranges at `version` should show, from where their ends are:
/// those whose ends both show, by id.
/// A range by id, and what it shows: its tag and span, or nothing.
type Shown = (Lv, Option<(String, std::ops::Range<usize>)>);

fn naive(log: &OpLog<char>, version: &[Lv]) -> Vec<Shown> {
    let ids = copies::identities(log, version);
    let pos = |item: Lv| ids.iter().position(|&i| i == item);
    ranges::reference(log, version)
        .into_iter()
        .filter_map(|(id, s, e, tag)| {
            let (s, e) = (pos(s)?, pos(e)?);
            Some((id, tag.map(|t| (t, s..(e + 1).max(s)))))
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn ranges_follow_their_text_and_converge(acts in acts()) {
        let mut log = OpLog::new();
        let mut ws: Vec<Writer> = ["a", "b", "c"].iter().map(|n| Writer::new(&mut log, n)).collect();
        for act in acts {
            let w = match act {
                Act::Type(w, ..) | Act::Delete(w, ..) | Act::Move(w, ..) | Act::Add(w, ..) | Act::Set(w, ..) | Act::Undo(w) => w,
                Act::Sync(_, b) => b,
            };
            let me = ws[w].me;
            let len = ws[w].doc.len();
            let v = ws[w].books.version().to_vec();
            if !matches!(act, Act::Undo(_) | Act::Sync(..)) {
                log.begin_group(me);
            }
            match act {
                Act::Type(_, p, c) => {
                    let v = log.add_insert(me, &v, p.index(len + 1), [c]);
                    ws[w].merge(&log, &v);
                }
                Act::Delete(_, p) if len > 0 => {
                    let at = p.index(len);
                    let v = log.add_delete(me, &v, at..at + 1);
                    ws[w].merge(&log, &v);
                }
                Act::Move(_, a, b, t) if len > 0 => {
                    let (a, b) = (a.index(len), b.index(len));
                    let (a, b) = (a.min(b), a.max(b) + 1);
                    let to = t.index(len + 1);
                    if to <= a || to >= b {
                        let v = log.add_move(me, &v, a..b, to);
                        ws[w].merge(&log, &v);
                    }
                }
                Act::Add(_, a, b, t) if len > 0 => {
                    let (a, b) = (a.index(len), b.index(len));
                    let (s, e) = (ws[w].item(a.min(b)), ws[w].item(a.max(b)));
                    let v = log.add_range(me, &v, s, e, &format!("t{t}"));
                    ws[w].merge(&log, &v);
                }
                Act::Set(_, k, t) => {
                    let known: Vec<Lv> = ranges::reference(&log, &v).into_iter().map(|r| r.0).collect();
                    if !known.is_empty() {
                        let id = known[k.index(known.len())];
                        let tag = t.map(|t| format!("t{t}"));
                        let v = log.set_range(me, &v, id, tag.as_deref());
                        ws[w].merge(&log, &v);
                    }
                }
                Act::Undo(_) => {
                    let writer = &mut ws[w];
                    if let Some(changes) = writer.books.undo_group(&mut log, me) {
                        for c in changes {
                            c.apply(&mut writer.doc);
                        }
                    }
                }
                Act::Sync(a, b) => {
                    let to = ws[a].books.version().to_vec();
                    ws[b].merge(&log, &to);
                }
                _ => {}
            }
            if !matches!(act, Act::Undo(_) | Act::Sync(..)) {
                log.end_group(me);
            }
            // The replica that acted, against where its ranges' ends are.
            let at = ws[w].books.version().to_vec();
            prop_assert_eq!(&ws[w].doc, &copies::checkout(&log, &at));
            for (id, want) in naive(&log, &at) {
                let got = ws[w].books.range(id).map(|r| (r.tag, r.span));
                prop_assert_eq!(got, want, "range {} at {:?}", id, at);
            }
            for r in ws[w].books.ranges() {
                prop_assert!(r.span.end <= ws[w].doc.len());
            }
        }
        // Everyone with everything: the same ranges, wherever both ends
        // show.
        let all = log.version().to_vec();
        for w in &mut ws {
            w.merge(&log, &all);
        }
        let shown: BTreeSet<Lv> = naive(&log, &all).into_iter().map(|r| r.0).collect();
        let pick = |w: &Writer| -> Vec<AuthorRange> {
            w.books.ranges().into_iter().filter(|r| shown.contains(&r.id)).collect()
        };
        prop_assert_eq!(pick(&ws[0]), pick(&ws[1]));
        prop_assert_eq!(pick(&ws[1]), pick(&ws[2]));
    }
}
