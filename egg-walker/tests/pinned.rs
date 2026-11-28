//! What's pinned to text in the log, comments and ranges, follows a move
//! of it by default (#85, as #79 decided), on every replica. Undo puts it
//! back, and one only partly moved stays where it was.

use mamre_egg_walker::{Accountant, AgentId, Change, Lv, OpLog};

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

    fn text(&self) -> String {
        self.doc.iter().collect()
    }

    fn at(&self, span: std::ops::Range<usize>) -> String {
        self.doc[span].iter().collect()
    }

    /// Moves `range` to position `to` (outside it, before the move), as
    /// `mamre-text` does: a delete and a paste of what it deleted, in one
    /// step.
    fn move_items(&mut self, log: &mut OpLog<char>, range: std::ops::Range<usize>, to: usize) {
        let sources: Vec<Lv> = range.clone().map(|p| self.item(p)).collect();
        log.begin_group(self.me);
        let v = log.add_delete(self.me, self.books.version(), range.clone());
        self.merge(log, &v);
        let at = if to > range.start { to - range.len() } else { to };
        let v = log.add_paste(self.me, self.books.version(), at, &sources);
        self.merge(log, &v);
        log.end_group(self.me);
    }

    fn undo(&mut self, log: &mut OpLog<char>) {
        let changes = self.books.undo_group(log, self.me).unwrap();
        self.apply(changes);
    }
}

/// "one two three", with a comment on "two" and a range on "three".
fn three() -> (OpLog<char>, Writer, Lv, Lv) {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    log.begin_group(w.me);
    let v = log.add_insert(w.me, &[], 0, "one two three".chars());
    w.merge(&log, &v);
    let v = log.add_comment(w.me, w.books.version(), w.item(4), w.item(6));
    let c = v[0];
    w.merge(&log, &v);
    let v = log.add_range(w.me, w.books.version(), w.item(8), w.item(12), "pov");
    let r = v[0];
    w.merge(&log, &v);
    log.end_group(w.me);
    (log, w, c, r)
}

#[test]
fn a_comment_on_moved_text_follows_it() {
    let (mut log, mut w, c, _) = three();
    w.move_items(&mut log, 4..8, 13);
    assert_eq!(w.text(), "one threetwo ");
    let span = w.books.comment(c).unwrap().span;
    assert_eq!(w.at(span), "two");
}

#[test]
fn a_range_on_moved_text_follows_it() {
    let (mut log, mut w, _, r) = three();
    w.move_items(&mut log, 8..13, 0);
    assert_eq!(w.text(), "threeone two ");
    let span = w.books.range(r).unwrap().span;
    assert_eq!(w.at(span), "three");
}

#[test]
fn undo_puts_them_back() {
    let (mut log, mut w, c, r) = three();
    w.move_items(&mut log, 4..13, 0);
    assert_eq!(w.text(), "two threeone ");
    assert_eq!(w.at(w.books.comment(c).unwrap().span), "two");
    assert_eq!(w.at(w.books.range(r).unwrap().span), "three");
    w.undo(&mut log);
    assert_eq!(w.text(), "one two three");
    assert_eq!(w.books.comment(c).unwrap().span, 4..7);
    assert_eq!(w.books.range(r).unwrap().span, 8..13);
}

/// A comment whose text only partly moved stays on what's left: the
/// editor is told and can re-pin it (#79), and until it does, nothing
/// guesses.
#[test]
fn one_partly_moved_stays_where_it_was() {
    let (mut log, mut w, c, _) = three();
    // "e t" moved to the end: the comment's "t" goes, its "wo" stays.
    w.move_items(&mut log, 2..5, 13);
    assert_eq!(w.text(), "onwo threee t");
    let span = w.books.comment(c).unwrap().span;
    assert_eq!(w.at(span), "wo", "what's left of it, where it was");
}

/// Another replica gets the move from the log alone, and agrees.
#[test]
fn they_follow_on_another_replica() {
    let (mut log, mut w, c, r) = three();
    let mut other = Writer::new(&mut log, "other");
    other.merge(&log, w.books.version());
    w.move_items(&mut log, 4..8, 13);
    other.merge(&log, w.books.version());
    assert_eq!(other.text(), w.text());
    assert_eq!(other.books.comment(c), w.books.comment(c));
    assert_eq!(other.books.range(r), w.books.range(r));
}

/// After a load, the books rebuild what's pinned from the log.
#[test]
fn they_follow_after_a_load() {
    let (mut log, mut w, c, r) = three();
    w.move_items(&mut log, 4..8, 13);
    let mut file = mamre_egg_walker::file::create();
    mamre_egg_walker::file::save(&mut file, &log, 0, w.doc.clone(), &w.books);
    let loaded = mamre_egg_walker::file::load::<char>(&file).unwrap();
    let snapshot = loaded.snapshot.expect("a snapshot");
    let mut again = Writer {
        me: w.me,
        books: snapshot.accountant,
        doc: snapshot.doc,
    };
    let version = loaded.log.version().to_vec();
    again.merge(&loaded.log, &version);
    assert_eq!(again.books.comment(c), w.books.comment(c));
    assert_eq!(again.books.range(r), w.books.range(r));
}

/// A comment added on moved text a moment after the move, by someone who
/// hadn't seen it, follows too: it names the originals.
#[test]
fn one_made_while_it_moved_follows() {
    let (mut log, mut w, _, _) = three();
    let mut other = Writer::new(&mut log, "other");
    other.merge(&log, w.books.version());
    let before = w.books.version().to_vec();
    w.move_items(&mut log, 4..8, 13);
    let v = log.add_comment(other.me, &before, other.item(4), other.item(6));
    let c = v[0];
    let all = log.union(w.books.version(), &v);
    w.merge(&log, &all);
    other.merge(&log, &all);
    assert_eq!(w.text(), "one threetwo ");
    assert_eq!(w.at(w.books.comment(c).unwrap().span), "two");
    assert_eq!(other.books.comment(c), w.books.comment(c));
}

// ---- What a move touched (#85) ----

use mamre_egg_walker::{Pin, Touch, Touched};

fn touched(pin: Pin, touch: Touch) -> Touched {
    Touched { pin, touch }
}

/// What moving positions `start..end` would touch.
fn touching(w: &Writer, start: usize, end: usize) -> Vec<Touched> {
    w.books.pins_touching(std::slice::from_ref(&(start..end)))
}

/// The mover's editor asks before it moves: "two " holds the comment, and
/// the range on "three" begins where it ends.
#[test]
fn the_mover_is_told_what_it_would_touch() {
    let (_, w, c, r) = three();
    assert_eq!(
        touching(&w, 4, 8),
        [touched(Pin::Comment(c), Touch::Inside), touched(Pin::Range(r), Touch::Edge)]
    );
    // "e t": part of the comment.
    assert_eq!(touching(&w, 2, 5), [touched(Pin::Comment(c), Touch::Partly)]);
    // "w": inside the comment, which holds more than what moves.
    assert_eq!(touching(&w, 5, 6), [touched(Pin::Comment(c), Touch::Partly)]);
    assert_eq!(touching(&w, 0, 2), []);
}

/// The author's editor hears when the move arrives, and again for its undo
/// and redo.
#[test]
fn a_merge_says_what_its_moves_touched() {
    let (mut log, mut w, c, r) = three();
    let mut other = Writer::new(&mut log, "other");
    other.merge(&log, w.books.version());
    w.move_items(&mut log, 4..8, 13);
    let (changes, got) = other.books.merge_noting_pins(&log, w.books.version());
    other.apply(changes);
    assert_eq!(other.text(), "one threetwo ");
    assert_eq!(
        got,
        [touched(Pin::Comment(c), Touch::Inside), touched(Pin::Range(r), Touch::Edge)]
    );
    // Undone here, it's a move back: "two " leaves the end, right after
    // "three".
    let back = [touched(Pin::Comment(c), Touch::Inside), touched(Pin::Range(r), Touch::Edge)];
    let v = w.books.make_undo(&mut log, w.me).unwrap();
    let (changes, got) = w.books.merge_noting_pins(&log, &v);
    w.apply(changes);
    assert_eq!(w.text(), "one two three");
    assert_eq!(got, back);
    let (changes, got) = other.books.merge_noting_pins(&log, w.books.version());
    other.apply(changes);
    assert_eq!(got, back, "and over there");
    // And redone.
    let v = w.books.make_redo(&mut log, w.me).unwrap();
    let (changes, got) = w.books.merge_noting_pins(&log, &v);
    w.apply(changes);
    assert_eq!(w.text(), "one threetwo ");
    assert_eq!(got, [touched(Pin::Comment(c), Touch::Inside), touched(Pin::Range(r), Touch::Edge)]);
}

/// Typing, deleting and pasting move nothing, so they touch nothing.
#[test]
fn edits_that_arent_moves_touch_nothing() {
    let (mut log, mut w, _, _) = three();
    for v in [
        log.add_insert(w.me, w.books.version(), 5, ['x']),
        log.add_delete(w.me, w.books.version(), 0..2),
        log.add_paste(w.me, w.books.version(), 0, &[4, 5, 6]),
    ] {
        let (changes, got) = w.books.merge_noting_pins(&log, &v);
        w.apply(changes);
        assert_eq!(got, []);
    }
}

// ---- Many writers ----

use mamre_egg_walker::{Aside, Follow, Op};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Act {
    Type(usize, prop::sample::Index, char),
    Delete(usize, prop::sample::Index),
    Move(usize, prop::sample::Index, prop::sample::Index, prop::sample::Index),
    Comment(usize, prop::sample::Index, prop::sample::Index),
    Range(usize, prop::sample::Index, prop::sample::Index),
    Undo(usize),
    Sync(usize, usize),
}

fn acts() -> impl Strategy<Value = Vec<Act>> {
    let i = || any::<prop::sample::Index>();
    let act = prop_oneof![
        4 => (0..3usize, i(), prop::char::range('a', 'e')).prop_map(|(w, p, c)| Act::Type(w, p, c)),
        2 => (0..3usize, i()).prop_map(|(w, p)| Act::Delete(w, p)),
        3 => (0..3usize, i(), i(), i()).prop_map(|(w, a, b, t)| Act::Move(w, a, b, t)),
        2 => (0..3usize, i(), i()).prop_map(|(w, a, b)| Act::Comment(w, a, b)),
        1 => (0..3usize, i(), i()).prop_map(|(w, a, b)| Act::Range(w, a, b)),
        1 => (0..3usize).prop_map(Act::Undo),
        2 => (0..3usize, 0..3usize).prop_map(|(a, b)| Act::Sync(a, b)),
    ];
    prop::collection::vec(act, 1..40)
}

/// Whether what `id` pins (its ends) is placed from the log alone: both
/// ends show, or both moved and still in order. Otherwise an end has fallen
/// back to where it was, which is each replica's own (#34 will make it
/// shared).
fn placed_by_the_log(w: &Writer, log: &OpLog<char>, id: Lv) -> bool {
    let (Op::Aside(Aside::CommentAdd { start, end }) | Op::Aside(Aside::RangeAdd { start, end, .. })) =
        log.event(id).op
    else {
        unreachable!("{id} pins nothing")
    };
    match (w.books.follow(start), w.books.follow(end)) {
        (Follow::Here(_), Follow::Here(_)) => true,
        (Follow::Moved(s), Follow::Moved(e)) => s <= e,
        _ => false,
    }
}

proptest! {
    // The default config (256 cases) so `PROPTEST_CASES` can raise it.
    #![proptest_config(ProptestConfig::default())]

    /// Three writers moving text (as a delete and a paste), commenting,
    /// tagging and undoing: once everyone has everything, they agree on
    /// every comment and range the log places.
    #[test]
    fn pinned_things_converge_through_moves(acts in acts()) {
        let mut log = OpLog::new();
        let mut ws: Vec<Writer> = ["a", "b", "c"].iter().map(|n| Writer::new(&mut log, n)).collect();
        for act in acts {
            let w = match act {
                Act::Sync(_, b) => b,
                Act::Type(w, ..) | Act::Delete(w, ..) | Act::Move(w, ..) | Act::Comment(w, ..)
                | Act::Range(w, ..) | Act::Undo(w) => w,
            };
            let me = ws[w].me;
            let len = ws[w].doc.len();
            match act {
                Act::Type(_, p, c) => {
                    log.begin_group(me);
                    let v = log.add_insert(me, ws[w].books.version(), p.index(len + 1), [c]);
                    ws[w].merge(&log, &v);
                    log.end_group(me);
                }
                Act::Delete(_, p) if len > 0 => {
                    log.begin_group(me);
                    let at = p.index(len);
                    let v = log.add_delete(me, ws[w].books.version(), at..at + 1);
                    ws[w].merge(&log, &v);
                    log.end_group(me);
                }
                Act::Move(_, a, b, t) if len > 0 => {
                    let (a, b) = (a.index(len), b.index(len));
                    let (a, b) = (a.min(b), a.max(b) + 1);
                    let to = t.index(len + 1);
                    if to <= a || to >= b {
                        ws[w].move_items(&mut log, a..b, to);
                    }
                }
                Act::Comment(_, a, b) | Act::Range(_, a, b) if len > 0 => {
                    let (a, b) = (a.index(len), b.index(len));
                    let (s, e) = (ws[w].item(a.min(b)), ws[w].item(a.max(b)));
                    log.begin_group(me);
                    let v = if matches!(act, Act::Comment(..)) {
                        log.add_comment(me, ws[w].books.version(), s, e)
                    } else {
                        log.add_range(me, ws[w].books.version(), s, e, "tag")
                    };
                    ws[w].merge(&log, &v);
                    log.end_group(me);
                }
                Act::Undo(_) => {
                    if let Some(changes) = ws[w].books.undo_group(&mut log, me) {
                        ws[w].apply(changes);
                    }
                }
                Act::Sync(a, b) => {
                    let to = ws[a].books.version().to_vec();
                    ws[b].merge(&log, &to);
                }
                _ => {}
            }
            for c in ws[w].books.comments() {
                prop_assert!(c.span.end <= ws[w].doc.len());
            }
            for r in ws[w].books.ranges() {
                prop_assert!(r.span.end <= ws[w].doc.len());
            }
        }
        let all = log.version().to_vec();
        for w in &mut ws {
            w.merge(&log, &all);
        }
        let comments = |w: &Writer| -> Vec<_> {
            w.books.comments().into_iter().filter(|c| placed_by_the_log(w, &log, c.id)).collect()
        };
        let ranges = |w: &Writer| -> Vec<_> {
            w.books.ranges().into_iter().filter(|r| placed_by_the_log(w, &log, r.id)).collect()
        };
        prop_assert_eq!(&ws[0].doc, &ws[1].doc);
        prop_assert_eq!(&ws[1].doc, &ws[2].doc);
        prop_assert_eq!(comments(&ws[0]), comments(&ws[1]));
        prop_assert_eq!(comments(&ws[1]), comments(&ws[2]));
        prop_assert_eq!(ranges(&ws[0]), ranges(&ws[1]));
        prop_assert_eq!(ranges(&ws[1]), ranges(&ws[2]));
    }
}
