//! Isolates: markers in the history, so they're where every replica and
//! every save has them, and text goes in or out of one by which side of
//! a marker it's put.

use mamre_text::{Direction, Edit, Isolate, IsolateId, Text, TextError};
use proptest::prelude::*;
use unicode_bidi::{BidiInfo, Level};

use Direction::{LeftToRight as Ltr, RightToLeft as Rtl};

fn text(s: &str) -> Text {
    let mut t = Text::new("me");
    t.insert(0, s).unwrap();
    t
}

/// Each isolate as its text, its direction, and its parent's text.
fn shown(t: &Text) -> Vec<(String, Direction, Option<String>)> {
    let all = t.isolates();
    let of = |i: &Isolate| t.slice(i.range.clone()).unwrap().to_string();
    all.iter()
        .map(|i| {
            let parent = i
                .parent
                .map(|p| of(all.iter().find(|o| o.id == p).unwrap()));
            (of(i), i.direction, parent)
        })
        .collect()
}

fn one(s: &str, direction: Direction, parent: Option<&str>) -> (String, Direction, Option<String>) {
    (s.to_string(), direction, parent.map(str::to_string))
}

/// Marks the bytes of `what` in `t`.
fn mark(t: &mut Text, what: &str, direction: Direction) -> IsolateId {
    let at = t.to_string().find(what).unwrap();
    t.mark_isolate(at..at + what.len(), direction).unwrap().0
}

/// Sends each what the other lacks, until neither has anything to say.
fn sync(a: &mut Text, b: &mut Text) {
    let mut to_b = vec![a.hello()];
    let mut to_a = vec![b.hello()];
    while !to_a.is_empty() || !to_b.is_empty() {
        let mut next_a = Vec::new();
        for m in to_b.drain(..) {
            next_a.extend(b.receive(&m).unwrap().reply);
        }
        for m in to_a.drain(..) {
            to_b.extend(a.receive(&m).unwrap().reply);
        }
        to_a = next_a;
    }
}

#[test]
fn an_isolate_is_not_text() {
    let mut t = text("abc אבג def");
    let (id, edits) = t.mark_isolate(4..10, Rtl).unwrap();
    assert_eq!(edits, [Edit::Isolates]);
    // The text is what it was, to the byte.
    assert_eq!((t.to_string().as_str(), t.len()), ("abc אבג def", 14));
    assert_eq!(
        t.isolates(),
        [Isolate {
            id,
            range: 4..10,
            direction: Rtl,
            parent: None
        }]
    );
    // Removing it leaves the text too.
    assert_eq!(t.remove_isolate(id), Ok(vec![Edit::Isolates]));
    assert_eq!(
        (t.to_string().as_str(), t.isolates()),
        ("abc אבג def", vec![])
    );
    assert_eq!(t.remove_isolate(id), Err(TextError::NoSuchIsolate(id)));
}

#[test]
fn text_goes_in_or_out_by_which_side_its_put() {
    let mut t = text("abc אבג def");
    let id = mark(&mut t, "אבג", Rtl);
    // Inside is inside, whatever the script: nothing looks at it again.
    t.insert(6, "q").unwrap();
    assert_eq!(shown(&t), [one("אqבג", Rtl, None)]);
    // At its end: outside, unless it's said to be in.
    t.insert(11, "X").unwrap();
    assert_eq!(shown(&t), [one("אqבג", Rtl, None)]);
    t.insert_in(11, "ד", id).unwrap();
    assert_eq!(shown(&t), [one("אqבגד", Rtl, None)]);
    assert_eq!(t.to_string(), "abc אqבגדX def");
    // And at its start.
    t.insert(4, "Y").unwrap();
    t.insert_in(5, "ה", id).unwrap();
    assert_eq!(shown(&t), [one("האqבגד", Rtl, None)]);
    assert_eq!(t.to_string(), "abc YהאqבגדX def");
    // Somewhere it doesn't reach, it can't be in.
    assert_eq!(t.insert_in(0, "z", id), Err(TextError::NoSuchIsolate(id)));
    assert_eq!(t.isolates()[0].id, id);
}

#[test]
fn isolates_nest() {
    // An English word in a Hebrew sentence in English: the case the
    // characters alone can't say.
    let mut t = text("say אבג abc דהו end");
    let sentence = mark(&mut t, "אבג abc דהו", Rtl);
    mark(&mut t, "abc", Ltr);
    assert_eq!(
        shown(&t),
        [
            one("אבג abc דהו", Rtl, None),
            one("abc", Ltr, Some("אבג abc דהו"))
        ]
    );
    assert_eq!(t.isolates()[1].parent, Some(sentence));
    // Where two end together, text goes in whichever it's told, or
    // neither.
    let mut t = text("אבג");
    let outer = mark(&mut t, "אבג", Rtl);
    let inner = mark(&mut t, "בג", Ltr);
    t.insert_in(6, "1", inner).unwrap();
    t.insert_in(7, "2", outer).unwrap();
    t.insert(8, "3").unwrap();
    assert_eq!(
        shown(&t),
        [one("אבג12", Rtl, None), one("בג1", Ltr, Some("אבג12"))]
    );
    assert_eq!(t.to_string(), "אבג123");
}

#[test]
fn marking_an_isolates_own_range_sets_its_direction() {
    let mut t = text("abc אבג def");
    let id = mark(&mut t, "אבג", Rtl);
    // The same direction again: nothing to do.
    assert_eq!(t.mark_isolate(4..10, Rtl), Ok((id, vec![])));
    // The other: the same isolate, turned round. Not a second one in it.
    assert_eq!(t.mark_isolate(4..10, Ltr), Ok((id, vec![Edit::Isolates])));
    assert_eq!(
        t.isolates(),
        [Isolate {
            id,
            range: 4..10,
            direction: Ltr,
            parent: None
        }]
    );
    // And everyone else's turns with it.
    let mut other = Text::new("you");
    sync(&mut t, &mut other);
    assert_eq!(other.isolates(), t.isolates());
    let mut file = Vec::new();
    t.save(&mut file);
    assert_eq!(Text::open("again", &file).unwrap().isolates(), t.isolates());
}

#[test]
fn an_isolate_cant_take_part_of_another() {
    let mut t = text("abc אבג def");
    mark(&mut t, "אבג", Rtl);
    assert_eq!(t.mark_isolate(2..6, Ltr), Err(TextError::CrossesIsolate));
    assert_eq!(t.mark_isolate(8..12, Ltr), Err(TextError::CrossesIsolate));
    assert!(t.mark_isolate(4..4, Ltr).is_err());
    // Round it, or within it, is fine.
    assert!(t.mark_isolate(2..12, Ltr).is_ok());
    assert!(t.mark_isolate(6..8, Ltr).is_ok());
    assert_eq!(t.isolates().len(), 3);
}

#[test]
fn deleting_text_takes_an_isolate_only_whole() {
    let mut t = text("abc אבג def");
    let id = mark(&mut t, "אבג", Rtl);
    // Part of it, and what's left is still it.
    assert_eq!(t.delete(2..6), Ok(vec![Edit::Delete { range: 2..6 }]));
    assert_eq!(
        (t.to_string().as_str(), shown(&t)),
        ("abבג def", vec![one("בג", Rtl, None)])
    );
    assert_eq!(t.isolates()[0].id, id);
    t.delete(4..7).unwrap();
    assert_eq!(
        (t.to_string().as_str(), shown(&t)),
        ("abבdef", vec![one("ב", Rtl, None)])
    );
    // All of it, and it's gone, markers and all: text there now is in
    // nothing.
    assert_eq!(
        t.delete(2..4),
        Ok(vec![Edit::Delete { range: 2..4 }, Edit::Isolates])
    );
    assert_eq!(t.isolates(), []);
    t.insert(2, "ג").unwrap();
    assert_eq!(t.isolates(), []);
    assert_eq!(t.insert_in(2, "x", id), Err(TextError::NoSuchIsolate(id)));
    // A range over a whole isolate and more takes it too.
    let mut t = text("abc אבג def");
    mark(&mut t, "אבג", Rtl);
    assert_eq!(
        t.delete(1..12),
        Ok(vec![Edit::Delete { range: 1..12 }, Edit::Isolates])
    );
    assert_eq!((t.to_string().as_str(), t.isolates()), ("aef", vec![]));
}

#[test]
fn moved_text_keeps_its_direction() {
    let mut t = text("abc אבג def");
    let id = mark(&mut t, "אבג", Rtl);
    // All of an isolate, moved: the isolate goes with it.
    t.move_text(4..10, 0).unwrap();
    assert_eq!(t.to_string(), "אבגabc  def");
    assert_eq!(
        t.isolates(),
        [Isolate {
            id,
            range: 0..6,
            direction: Rtl,
            parent: None
        }]
    );
    // Part of it, moved out: the rest is still it, and the part is an
    // isolate of its own, set as it was.
    let edits = t.move_text(0..2, 11).unwrap();
    assert_eq!(t.to_string(), "בגabc  אdef");
    assert_eq!(shown(&t), [one("בג", Rtl, None), one("א", Rtl, None)]);
    assert_eq!(t.isolates()[0].id, id);
    assert_eq!(edits.last(), Some(&Edit::Isolates));
    // Moved about inside its isolate, it needs none.
    let mut t = text("abc אבג def");
    mark(&mut t, "אבג", Rtl);
    t.move_text(4..6, 10).unwrap();
    assert_eq!(shown(&t), [one("בגא", Rtl, None)]);
    // Nor moved into one that runs the same way.
    let mut t = text("אבג abc דהו");
    mark(&mut t, "אבג", Rtl);
    mark(&mut t, "דהו", Rtl);
    t.move_text(0..2, 13).unwrap();
    assert_eq!(t.to_string(), "בג abc דאהו");
    assert_eq!(shown(&t), [one("בג", Rtl, None), one("דאהו", Rtl, None)]);
}

#[test]
fn a_move_over_an_isolates_edge_leaves_the_edge() {
    // "c " from outside and "א" from inside: the marker between them
    // stays, so the isolate neither stretches nor breaks.
    let mut t = text("abc אבג def");
    let id = mark(&mut t, "אבג", Rtl);
    t.move_text(2..6, 14).unwrap();
    assert_eq!(t.to_string(), "abבג defc א");
    assert_eq!(shown(&t), [one("בג", Rtl, None), one("א", Rtl, None)]);
    assert_eq!(t.isolates()[0].id, id);
    // The same, moved earlier.
    let mut t = text("abc אבג def");
    let id = mark(&mut t, "אבג", Rtl);
    t.move_text(8..12, 0).unwrap();
    assert_eq!(t.to_string(), "ג dabc אבef");
    assert_eq!(shown(&t), [one("ג", Rtl, None), one("אב", Rtl, None)]);
    assert_eq!(t.isolates()[1].id, id);
    // A whole isolate and text either side: it goes as it is.
    let mut t = text("abc אבג def");
    let id = mark(&mut t, "אבג", Rtl);
    t.move_text(2..12, 0).unwrap();
    assert_eq!(t.to_string(), "c אבג dabef");
    assert_eq!(
        t.isolates(),
        [Isolate {
            id,
            range: 2..8,
            direction: Rtl,
            parent: None
        }]
    );
}

#[test]
fn two_isolates_made_at_once_over_the_same_text_cross() {
    let mut a = text("abc אבג דהו def");
    let mut b = Text::new("you");
    sync(&mut a, &mut b);
    // One takes the first word and the space; the other the space and
    // the second word.
    let first = a.mark_isolate(4..11, Rtl).unwrap().0;
    let second = b.mark_isolate(10..17, Rtl).unwrap().0;
    let mut edits = Vec::new();
    for m in b.receive(&a.hello()).unwrap().reply {
        for back in a.receive(&m).unwrap().reply {
            edits.extend(b.receive(&back).unwrap().edits);
        }
    }
    sync(&mut a, &mut b);
    assert!(edits.contains(&Edit::Isolates));
    // The one that opens first is shown; the other is lost, and says
    // what it lost to. Both replicas say the same.
    assert_eq!(
        a.isolates(),
        [Isolate {
            id: first,
            range: 4..11,
            direction: Rtl,
            parent: None
        }]
    );
    assert_eq!(
        a.lost_isolates(),
        [Isolate {
            id: second,
            range: 10..17,
            direction: Rtl,
            parent: Some(first)
        }]
    );
    assert_eq!(
        (b.isolates(), b.lost_isolates()),
        (a.isolates(), a.lost_isolates())
    );
    // Its writer clears it away and marks what it meant again.
    assert_eq!(b.remove_isolate(second), Ok(vec![Edit::Isolates]));
    assert_eq!(b.lost_isolates(), []);
    b.mark_isolate(11..17, Rtl).unwrap();
    sync(&mut a, &mut b);
    assert_eq!(shown(&a), [one("אבג ", Rtl, None), one("דהו", Rtl, None)]);
    assert_eq!((b.isolates(), a.lost_isolates()), (a.isolates(), vec![]));
}

#[test]
fn every_replica_has_them() {
    let mut a = text("abc אבג def");
    let id = mark(&mut a, "אבג", Rtl);
    let mut b = Text::new("you");
    sync(&mut a, &mut b);
    assert_eq!(b.to_string(), "abc אבג def");
    assert_eq!(b.isolates(), a.isolates());
    // One types at its end inside, the other at the same place outside,
    // at the same time: both keep what they meant.
    a.insert_in(10, "ד", id).unwrap();
    b.insert(10, "X").unwrap();
    sync(&mut a, &mut b);
    assert_eq!(a.to_string(), "abc אבגדX def");
    assert_eq!(b.to_string(), a.to_string());
    assert_eq!(shown(&a), [one("אבגד", Rtl, None)]);
    assert_eq!(b.isolates(), a.isolates());
    // One removes it while the other types in it: the text stays.
    a.remove_isolate(id).unwrap();
    b.insert(6, "q").unwrap();
    sync(&mut a, &mut b);
    assert_eq!(
        (a.to_string().as_str(), a.isolates()),
        ("abc אqבגדX def", vec![])
    );
    assert_eq!((b.to_string(), b.isolates()), (a.to_string(), vec![]));
}

#[test]
fn what_another_writer_does_to_them_is_an_edit() {
    let mut a = text("abc אבג def");
    let mut b = Text::new("you");
    sync(&mut a, &mut b);
    mark(&mut a, "אבג", Rtl);
    let mut edits = Vec::new();
    for m in b.receive(&a.hello()).unwrap().reply {
        for back in a.receive(&m).unwrap().reply {
            edits.extend(b.receive(&back).unwrap().edits);
        }
    }
    assert!(edits.contains(&Edit::Isolates), "{edits:?}");
    assert!(
        edits.iter().all(|e| *e == Edit::Isolates),
        "the text didn't change: {edits:?}"
    );
    assert_eq!(b.isolates(), a.isolates());
}

#[test]
fn a_file_keeps_them() {
    let mut t = text("say אבג abc דהו end");
    mark(&mut t, "אבג abc דהו", Rtl);
    mark(&mut t, "abc", Ltr);
    let mut file = Vec::new();
    t.save(&mut file);
    let mut back = Text::open("me again", &file).unwrap();
    assert_eq!(back.to_string(), t.to_string());
    assert_eq!(back.isolates(), t.isolates());
    // And it's still an isolate to type in.
    let word = back.isolates()[1].clone();
    back.insert_in(word.range.end, "d", word.id).unwrap();
    assert_eq!(shown(&back)[1], one("abcd", Ltr, Some("אבג abcd דהו")));
    // A text with none is written as it always was.
    let mut plain = Vec::new();
    text("abc אבג def").save(&mut plain);
    let mut marked = Vec::new();
    let mut m = text("abc אבג def");
    let id = mark(&mut m, "אבג", Rtl);
    m.remove_isolate(id).unwrap();
    m.save(&mut marked);
    assert!(
        marked.len() > plain.len(),
        "the history says an isolate came and went"
    );
    assert_eq!(Text::open("x", &plain).unwrap().isolates(), []);
}

#[test]
fn anchors_and_sections_count_text_not_markers() {
    let mut t = text("abc אבג def\nnext");
    let before = t.anchor(11).unwrap();
    mark(&mut t, "אבג", Rtl);
    let after = t.anchor(11).unwrap();
    let (section, _) = t.mark_section(15, 1).unwrap();
    assert_eq!(
        (t.position(&before), t.position(&after)),
        (Some(11), Some(11))
    );
    assert_eq!(t.sections()[0].at, 15);
    assert_eq!(t.section_range(section), Some(15..19));
    t.insert(0, "xx").unwrap();
    assert_eq!(
        (t.position(&before), t.position(&after)),
        (Some(13), Some(13))
    );
    assert_eq!(
        (t.sections()[0].at, t.isolates()[0].range.clone()),
        (17, 6..12)
    );
}

// What an editor would do: find the stretches Unicode's algorithm sets
// against what they're in, and mark them. mamre doesn't; this is here to
// check that marked isolates can say what the algorithm says.

fn nest(
    levels: &[Level],
    range: std::ops::Range<usize>,
    base: u8,
    out: &mut Vec<(std::ops::Range<usize>, u8)>,
) {
    let mut i = range.start;
    while i < range.end {
        if levels[i].number() <= base {
            i += 1;
            continue;
        }
        let start = i;
        let mut lowest = u8::MAX;
        while i < range.end && levels[i].number() > base {
            lowest = lowest.min(levels[i].number());
            i += 1;
        }
        out.push((start..i, lowest));
        nest(levels, start..i, lowest, out);
    }
}

fn find_isolates(t: &mut Text) {
    let s = t.to_string();
    let info = BidiInfo::new(&s, None);
    let mut found = Vec::new();
    for p in &info.paragraphs {
        nest(&info.levels, p.range.clone(), p.level.number(), &mut found);
    }
    for (range, level) in found {
        let direction = if level % 2 == 1 { Rtl } else { Ltr };
        t.mark_isolate(range, direction).unwrap();
    }
}

fn pieces() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("a"),
        Just("bc"),
        Just("א"),
        Just("בג"),
        Just("ש\u{5c1}"),
        Just("1"),
        Just("23"),
        Just(" "),
        Just(" "),
        Just(", "),
        Just("."),
        Just("-"),
        Just("("),
        Just(")"),
        Just("\n"),
        Just("\u{2067}"),
        Just("\u{2066}"),
        Just("\u{2069}"),
        Just("ع"),
        Just("é"),
        Just("\u{fdd0}"),
    ];
    prop::collection::vec(piece, 0..40).prop_map(|v| v.concat())
}

/// Something done to one of two replicas, or a sync. Numbers are taken
/// modulo what there is.
///
/// Moves aren't among what's tried at random: a move and an insert made
/// at the same time can panic the merge with no isolate in sight (see
/// `a_move_and_an_insert_at_once` in `text.rs`). `Do::Move` is here for
/// when that's mended.
#[derive(Clone, Debug)]
#[allow(dead_code)]
enum Do {
    Insert(bool, usize, String),
    InsertIn(bool, usize, bool, String),
    Delete(bool, usize, usize),
    Move(bool, usize, usize, usize),
    Mark(bool, usize, usize, bool),
    Remove(bool, usize),
    Sync,
}

fn doing() -> impl Strategy<Value = Do> {
    let n = || 0usize..64;
    let s = || {
        prop_oneof![Just("x"), Just("א"), Just("12"), Just(" "), Just("\n")]
            .prop_map(str::to_string)
    };
    prop_oneof![
        4 => (any::<bool>(), n(), s()).prop_map(|(w, a, t)| Do::Insert(w, a, t)),
        3 => (any::<bool>(), n(), any::<bool>(), s()).prop_map(|(w, i, e, t)| Do::InsertIn(w, i, e, t)),
        2 => (any::<bool>(), n(), n()).prop_map(|(w, a, b)| Do::Delete(w, a, b)),
        3 => (any::<bool>(), n(), n(), any::<bool>()).prop_map(|(w, a, b, d)| Do::Mark(w, a, b, d)),
        1 => (any::<bool>(), n()).prop_map(|(w, i)| Do::Remove(w, i)),
        2 => Just(Do::Sync),
    ]
}

/// The `n`th char boundary of `s`, modulo how many it has.
fn boundary(s: &str, n: usize) -> usize {
    let all: Vec<usize> = s.char_indices().map(|(i, _)| i).chain([s.len()]).collect();
    all[n % all.len()]
}

fn apply(editor: &mut String, edits: &[Edit]) {
    for edit in edits {
        match edit {
            Edit::Insert { at, text } => editor.insert_str(*at, text),
            Edit::Delete { range } => {
                editor.drain(range.clone());
            }
            Edit::Moved { from, to } => {
                assert!(*from <= editor.len() && to.end <= editor.len());
            }
            Edit::Section { .. } | Edit::Isolates | Edit::Lost { .. } => {}
        }
    }
}

/// The isolates must nest, lie on char boundaries, and none be empty.
fn check(t: &Text) -> Result<(), TestCaseError> {
    let s = t.to_string();
    let all = t.isolates();
    for pair in all.windows(2) {
        prop_assert!(pair[0].range.start <= pair[1].range.start);
    }
    for i in &all {
        prop_assert!(i.range.start < i.range.end && i.range.end <= s.len());
        prop_assert!(s.is_char_boundary(i.range.start) && s.is_char_boundary(i.range.end));
        if let Some(p) = i.parent {
            let parent = all.iter().find(|o| o.id == p);
            // A parent with nothing in it but markers isn't shown; its
            // child can't be then either, having nothing in it too.
            let parent = parent.expect("a parent that's shown");
            prop_assert!(parent.range.start <= i.range.start && i.range.end <= parent.range.end);
        }
        for o in &all {
            let apart = o.range.end <= i.range.start || i.range.end <= o.range.start;
            let within = |a: &Isolate, b: &Isolate| {
                b.range.start <= a.range.start && a.range.end <= b.range.end
            };
            prop_assert!(apart || within(i, o) || within(o, i), "{:?} and {:?}", i, o);
        }
    }
    Ok(())
}

proptest! {
    /// Marked as the algorithm finds them, isolates set every character
    /// the way the algorithm does; and a file and another replica have
    /// the same ones.
    #[test]
    fn isolates_can_say_what_the_algorithm_says(s in pieces()) {
        let mut t = text(&s);
        find_isolates(&mut t);
        prop_assert_eq!(t.to_string(), s.clone());
        check(&t)?;
        let info = BidiInfo::new(&s, None);
        let all = t.isolates();
        for (at, _) in s.char_indices() {
            let paragraph = info.paragraphs.iter().find(|p| p.range.contains(&at)).unwrap();
            let innermost = all.iter().filter(|i| i.range.contains(&at)).min_by_key(|i| i.range.len());
            let got = innermost.map_or(if paragraph.level.is_rtl() { Rtl } else { Ltr }, |i| i.direction);
            let want = if info.levels[at].is_rtl() { Rtl } else { Ltr };
            prop_assert_eq!(got, want, "byte {} of {:?}", at, s);
        }
        let mut file = Vec::new();
        t.save(&mut file);
        let back = Text::open("again", &file).unwrap();
        prop_assert_eq!(back.to_string(), s.clone());
        prop_assert_eq!(back.isolates(), t.isolates());
        let mut other = Text::new("you");
        sync(&mut t, &mut other);
        prop_assert_eq!(other.to_string(), s);
        prop_assert_eq!(other.isolates(), t.isolates());
    }

    /// Two writers typing, deleting, moving and marking at once: each
    /// replica's edits are its text's, and once they've talked they have
    /// the same text and the same isolates, as does a file.
    #[test]
    fn replicas_agree(start in pieces(), ops in prop::collection::vec(doing(), 0..40)) {
        let mut a = text(&start);
        let mut b = Text::new("you");
        sync(&mut a, &mut b);
        let mut editors = [a.to_string(), b.to_string()];
        for op in ops {
            if let Do::Sync = op {
                // Edits from the other side come out of `receive`: sync
                // by hand, so the editors hear them.
                let mut to_b = vec![a.hello()];
                let mut to_a = vec![b.hello()];
                while !to_a.is_empty() || !to_b.is_empty() {
                    let mut next_a = Vec::new();
                    for m in to_b.drain(..) {
                        let got = b.receive(&m).unwrap();
                        apply(&mut editors[1], &got.edits);
                        next_a.extend(got.reply);
                    }
                    for m in to_a.drain(..) {
                        let got = a.receive(&m).unwrap();
                        apply(&mut editors[0], &got.edits);
                        to_b.extend(got.reply);
                    }
                    to_a = next_a;
                }
                prop_assert_eq!(a.to_string(), b.to_string());
                prop_assert_eq!(a.isolates(), b.isolates());
            } else {
                let (who, t) = match op {
                    Do::Insert(w, ..) | Do::InsertIn(w, ..) | Do::Delete(w, ..) | Do::Move(w, ..) | Do::Mark(w, ..) | Do::Remove(w, ..) => {
                        if w { (0, &mut a) } else { (1, &mut b) }
                    }
                    Do::Sync => unreachable!(),
                };
                let s = t.to_string();
                let edits = match op {
                    Do::Insert(_, at, text) => t.insert(boundary(&s, at), &text).unwrap(),
                    Do::InsertIn(_, i, end, text) => {
                        let all = t.isolates();
                        if all.is_empty() {
                            Vec::new()
                        } else {
                            let iso = &all[i % all.len()];
                            let at = if end { iso.range.end } else { iso.range.start };
                            let edits = t.insert_in(at, &text, iso.id).unwrap();
                            let now = t.isolates();
                            let grown = now.iter().find(|n| n.id == iso.id).expect("still there");
                            prop_assert_eq!(grown.range.len(), iso.range.len() + text.len());
                            edits
                        }
                    }
                    Do::Delete(_, x, y) => {
                        let (x, y) = (boundary(&s, x), boundary(&s, y));
                        t.delete(x.min(y)..x.max(y)).unwrap()
                    }
                    Do::Move(_, x, y, to) => {
                        let (x, y, to) = (boundary(&s, x), boundary(&s, y), boundary(&s, to));
                        t.move_text(x.min(y)..x.max(y), to).unwrap()
                    }
                    Do::Mark(_, x, y, rtl) => {
                        let (x, y) = (boundary(&s, x), boundary(&s, y));
                        match t.mark_isolate(x.min(y)..x.max(y), if rtl { Rtl } else { Ltr }) {
                            Ok((_, edits)) => edits,
                            Err(_) => Vec::new(),
                        }
                    }
                    Do::Remove(_, i) => {
                        let all = t.isolates();
                        if all.is_empty() { Vec::new() } else { t.remove_isolate(all[i % all.len()].id).unwrap() }
                    }
                    Do::Sync => unreachable!(),
                };
                apply(&mut editors[who], &edits);
                prop_assert_eq!(&editors[who], &t.to_string(), "the edits are the text's");
                check(t)?;
            }
        }
        sync(&mut a, &mut b);
        prop_assert_eq!(a.to_string(), b.to_string());
        prop_assert_eq!(a.isolates(), b.isolates());
        check(&a)?;
        let mut file = Vec::new();
        a.save(&mut file);
        let back = Text::open("again", &file).unwrap();
        prop_assert_eq!(back.to_string(), a.to_string());
        prop_assert_eq!(back.isolates(), a.isolates());
    }
}
