//! Counts and seeks by unit through mamre-text (#31): through random
//! edits, moves, section marks and merges, every count and seek matches a
//! naive recount of the text.

use mamre_text::{Text, Unit, Units};
use proptest::prelude::*;

const UNITS: [Unit; 5] = [
    Unit::Word,
    Unit::VimWord,
    Unit::Sentence,
    Unit::Paragraph,
    Unit::VimParagraph,
];

/// The char boundaries of `s`: every byte offset an edit can name.
fn boundaries(s: &str) -> Vec<usize> {
    s.char_indices().map(|(i, _)| i).chain([s.len()]).collect()
}

/// Checks every count and seek `text` answers against `unit.ranges`.
fn check(text: &Text<Units>) -> Result<(), TestCaseError> {
    let s = text.to_string();
    let bs = boundaries(&s);
    for unit in UNITS {
        let ranges = unit.ranges(&s);
        let starts: Vec<usize> = ranges.iter().map(|r| r.start).collect();
        prop_assert_eq!(text.count(unit), ranges.len(), "{:?} in {:?}", unit, s);
        for (n, r) in ranges.iter().enumerate() {
            prop_assert_eq!(
                text.unit_range(n, unit),
                Some(r.clone()),
                "{:?} {} in {:?}",
                unit,
                n,
                s
            );
        }
        prop_assert_eq!(text.unit_start(ranges.len(), unit), None);
        for &at in &bs {
            let before = starts.iter().filter(|&&p| p < at).count();
            prop_assert_eq!(
                text.units_before(at, unit).unwrap(),
                before,
                "{:?} before {} in {:?}",
                unit,
                at,
                s
            );
            let holding = ranges.iter().position(|r| r.contains(&at));
            prop_assert_eq!(
                text.unit_at(at, unit).unwrap(),
                holding,
                "{:?} at {} in {:?}",
                unit,
                at,
                s
            );
            prop_assert_eq!(
                text.start_of(at, unit).unwrap(),
                holding.map(|n| ranges[n].start)
            );
            for n in 1..=3 {
                let next = starts.iter().filter(|&&p| p > at).nth(n - 1).copied();
                prop_assert_eq!(
                    text.next_start(at, unit, n).unwrap(),
                    next,
                    "{:?} {}th after {} in {:?}",
                    unit,
                    n,
                    at,
                    s
                );
                let prev = starts.iter().rev().filter(|&&p| p < at).nth(n - 1).copied();
                prop_assert_eq!(
                    text.prev_start(at, unit, n).unwrap(),
                    prev,
                    "{:?} {}th before {} in {:?}",
                    unit,
                    n,
                    at,
                    s
                );
            }
        }
        // Ranges, and each section of the outline.
        for (i, &a) in bs.iter().enumerate() {
            for &b in &bs[i..] {
                let want = starts.iter().filter(|&&p| a <= p && p < b).count();
                prop_assert_eq!(text.count_in(a..b, unit).unwrap(), want);
            }
        }
        for (id, got) in text.section_counts(unit) {
            let range = text.section_range(id).unwrap();
            let want = starts.iter().filter(|&&p| range.contains(&p)).count();
            prop_assert_eq!(got, want, "{:?} in section {:?} of {:?}", unit, range, s);
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
enum Act {
    Insert(usize, prop::sample::Index, String),
    Delete(usize, prop::sample::Index, prop::sample::Index),
    Move(
        usize,
        prop::sample::Index,
        prop::sample::Index,
        prop::sample::Index,
    ),
    Mark(usize, prop::sample::Index, u8),
    Sync(usize, usize),
}

fn acts() -> impl Strategy<Value = Vec<Act>> {
    let text = prop::collection::vec(
        prop::sample::select(vec![
            "a", "b", "_", " ", "\n", "\n\n", ".", "! ", " \n", "é", "\"",
        ]),
        1..5,
    )
    .prop_map(|v| v.concat());
    let i = || any::<prop::sample::Index>();
    let act = prop_oneof![
        5 => (0..3usize, i(), text).prop_map(|(r, p, t)| Act::Insert(r, p, t)),
        2 => (0..3usize, i(), i()).prop_map(|(r, a, b)| Act::Delete(r, a, b)),
        1 => (0..3usize, i(), i(), i()).prop_map(|(r, a, b, t)| Act::Move(r, a, b, t)),
        1 => (0..3usize, i(), 0u8..3).prop_map(|(r, p, l)| Act::Mark(r, p, l)),
        2 => (0..3usize, 0..3usize).prop_map(|(a, b)| Act::Sync(a, b)),
    ];
    prop::collection::vec(act, 1..30)
}

/// Brings `b` up to everything `a` has.
fn sync(rs: &mut [Text<Units>], a: usize, b: usize) {
    if a == b {
        return;
    }
    let mut to_b = vec![rs[a].hello()];
    let mut to_a = vec![rs[b].hello()];
    for _ in 0..20 {
        if to_a.is_empty() && to_b.is_empty() {
            return;
        }
        for m in std::mem::take(&mut to_b) {
            to_a.extend(rs[b].receive(&m).unwrap().reply);
        }
        for m in std::mem::take(&mut to_a) {
            to_b.extend(rs[a].receive(&m).unwrap().reply);
        }
    }
    panic!("they kept talking");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn counts_and_seeks_match_a_recount(acts in acts()) {
        let mut rs: Vec<Text<Units>> = ["a", "b", "c"].iter().map(|n| Text::with_measure(n)).collect();
        for act in acts {
            let touched = match act {
                Act::Insert(r, p, t) => {
                    let at = *p.get(&boundaries(&rs[r].to_string()));
                    rs[r].insert(at, &t).unwrap();
                    r
                }
                Act::Delete(r, a, b) => {
                    let bs = boundaries(&rs[r].to_string());
                    let (a, b) = (*a.get(&bs), *b.get(&bs));
                    rs[r].delete(a.min(b)..a.max(b)).unwrap();
                    r
                }
                Act::Move(r, a, b, t) => {
                    let bs = boundaries(&rs[r].to_string());
                    let (a, b, t) = (*a.get(&bs), *b.get(&bs), *t.get(&bs));
                    rs[r].move_text(a.min(b)..a.max(b), t).unwrap();
                    r
                }
                Act::Mark(r, p, level) => {
                    let bs = boundaries(&rs[r].to_string());
                    if bs.len() > 1 {
                        let at = *p.get(&bs[..bs.len() - 1]);
                        rs[r].mark_section(at, level).unwrap();
                    }
                    r
                }
                Act::Sync(a, b) => {
                    sync(&mut rs, a, b);
                    b
                }
            };
            check(&rs[touched])?;
        }
    }
}

#[test]
fn a_chapter_word_count() {
    let mut t: Text<Units> = Text::with_measure("me");
    t.insert(
        0,
        "One\nthe first words. More here.\nTwo\nsecond chapter, four words\n",
    )
    .unwrap();
    let (one, _) = t.mark_section(0, 1).unwrap();
    let two_at = t.to_string().find("Two").unwrap();
    let (two, _) = t.mark_section(two_at, 1).unwrap();
    assert_eq!(t.section_count(one, Unit::Word), Some(6));
    assert_eq!(t.section_count(two, Unit::Word), Some(5));
    // A sentence runs across a single newline: "One / the first words."
    assert_eq!(t.section_count(one, Unit::Sentence), Some(2));
    assert_eq!(t.section_counts(Unit::Word), vec![(one, 6), (two, 5)]);
    // "The 2nd sentence after here", and the start of this paragraph.
    let here = t.to_string().find("first").unwrap();
    let more = t.to_string().find("More").unwrap();
    assert_eq!(t.next_start(here, Unit::Sentence, 1).unwrap(), Some(more));
    assert_eq!(t.start_of(more + 2, Unit::Word).unwrap(), Some(more));
}
