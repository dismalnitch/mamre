//! Moving text and sections: the same as cutting and pasting a `String`,
//! chunks kept full, summaries right, and every position carried across.

use mamre_rope::{Address, Markdown, MoveError, Moved, Rope, Sections};
use proptest::prelude::*;

/// Markdown-ish text: headings at a few levels, plain lines, some
/// multi-byte chars.
fn text() -> impl Strategy<Value = String> {
    let line = prop_oneof![
        3 => "[a-zé ]{0,12}",
        1 => Just("# Chapter".to_string()),
        1 => Just("## Section".to_string()),
        1 => Just("### Part".to_string()),
        1 => Just("".to_string()),
    ];
    prop::collection::vec(line, 0..30).prop_map(|lines| lines.join("\n"))
}

/// A char boundary of `s`, about this far in.
fn boundary(s: &str, at: prop::sample::Index) -> usize {
    let bounds: Vec<usize> = s.char_indices().map(|(i, _)| i).chain([s.len()]).collect();
    bounds[at.index(bounds.len())]
}

fn check_move<const N: usize>(
    text: &str,
    a: usize,
    b: usize,
    to: usize,
) -> Result<(), TestCaseError> {
    let (a, b) = (a.min(b), a.max(b));
    if to > a && to < b {
        return Ok(());
    }
    let mut rope: Rope<Markdown, N> = Rope::from(text);
    let moved = rope.move_range(a..b, to);
    // The model: cut and paste.
    let mut want = String::new();
    if to <= a {
        want.push_str(&text[..to]);
        want.push_str(&text[a..b]);
        want.push_str(&text[to..a]);
        want.push_str(&text[b..]);
    } else {
        want.push_str(&text[..a]);
        want.push_str(&text[b..to]);
        want.push_str(&text[a..b]);
        want.push_str(&text[to..]);
    }
    prop_assert_eq!(rope.chars().collect::<String>(), want.clone());
    rope.check().map_err(TestCaseError::fail)?;
    let fresh: Rope<Markdown, N> = Rope::from(want.as_str());
    prop_assert_eq!(
        rope.summary(),
        fresh.summary(),
        "summaries as if built afresh"
    );
    prop_assert_eq!(&want[moved.to.clone()], &text[a..b]);
    // Every char lands where map says.
    for (i, c) in text.char_indices() {
        let j = moved.map(i);
        prop_assert_eq!(
            want[j..].chars().next(),
            Some(c),
            "char at {} went to {}",
            i,
            j
        );
    }
    Ok(())
}

/// The section at each heading, from the lines' own addresses: the
/// heading's line through to the next heading not under it.
fn sections(rope: &Rope<Markdown>, text: &str) -> Vec<(Address, std::ops::Range<usize>)> {
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let mut headings = Vec::new();
    let mut prev = Address::default();
    for &s in &starts {
        let here = rope.address_at(s);
        if here != prev {
            headings.push((here, s));
        }
        prev = here;
    }
    headings
        .iter()
        .map(|&(address, start)| {
            let end = headings
                .iter()
                .find(|&&(other, s)| s > start && !address.contains(&other))
                .map_or(text.len(), |&(_, s)| s);
            (address, start..end)
        })
        .collect()
}

proptest! {
    #[test]
    fn moves_like_cut_and_paste(
        text in text(),
        a in any::<prop::sample::Index>(),
        b in any::<prop::sample::Index>(),
        to in any::<prop::sample::Index>(),
    ) {
        let (a, b, to) = (boundary(&text, a), boundary(&text, b), boundary(&text, to));
        check_move::<4>(&text, a, b, to)?;
        check_move::<512>(&text, a, b, to)?;
    }

    #[test]
    fn section_ranges(text in text()) {
        let rope: Rope<Markdown> = Rope::from(text.as_str());
        for (address, range) in sections(&rope, &text) {
            prop_assert_eq!(rope.section_range(address), Some(range.clone()), "section {}", address);
        }
    }

    /// Moving a section before another, or to the end, is the same string
    /// surgery, and the moved section's heading is at its new place.
    #[test]
    fn moves_sections(text in text(), pick in any::<prop::sample::Index>(), dest in any::<prop::sample::Index>()) {
        let rope: Rope<Markdown> = Rope::from(text.as_str());
        let all = sections(&rope, &text);
        if all.is_empty() {
            return Ok(());
        }
        let (address, range) = all[pick.index(all.len())].clone();
        // Before another section's heading outside this one, or the end.
        let targets: Vec<usize> = all
            .iter()
            .map(|(_, r)| r.start)
            .chain([text.len()])
            .filter(|&t| t <= range.start || t >= range.end)
            .collect();
        let to = targets[dest.index(targets.len())];
        let mut moved = rope.clone();
        // A move that would run lines together is refused, and nothing
        // changes.
        let in_place = to == range.start || to == range.end;
        let joins = !in_place
            && (!text[..range.end].ends_with('\n') || to == text.len() && !text.ends_with('\n'));
        let m = match moved.move_section(address, to) {
            Err(MoveError::WouldJoinLines) => {
                prop_assert!(joins, "refused a move that joins nothing");
                prop_assert_eq!(moved.chars().collect::<String>(), text);
                return Ok(());
            }
            other => {
                prop_assert!(!joins, "moved, joining lines");
                other.unwrap()
            }
        };
        let mut by_range = rope.clone();
        let want = by_range.move_range(range.clone(), to);
        prop_assert_eq!(&m, &want);
        prop_assert_eq!(moved.chars().collect::<String>(), by_range.chars().collect::<String>());
        prop_assert_eq!(moved.heading_at(moved.address_at(m.to.start)), Some(m.to.start));
    }
}

#[test]
fn moving_a_chapter_carries_its_sections() {
    let text = "# One\nfirst\n## One.A\na\n# Two\nsecond\n# Three\nthird\n";
    let mut rope: Rope<Markdown> = Rope::from(text);
    let one = Address::new(&[1]).unwrap();
    let three = rope.heading_at(Address::new(&[3]).unwrap()).unwrap();
    let moved = rope.move_section(one, three).unwrap();
    assert_eq!(
        rope.chars().collect::<String>(),
        "# Two\nsecond\n# One\nfirst\n## One.A\na\n# Three\nthird\n"
    );
    assert_eq!(
        moved,
        Moved {
            from: 0..23,
            to: 13..36
        }
    );
    // Its subsection came with it, now 2.1.
    let sub = rope.heading_at(Address::new(&[2, 1]).unwrap()).unwrap();
    assert_eq!(
        &rope.chars().collect::<String>()[sub..sub + 10],
        "## One.A\na"
    );
    let _: Option<Sections> = None;
}

#[test]
#[should_panic(expected = "into itself")]
fn not_into_itself() {
    let mut rope: Rope = Rope::from("hello world");
    rope.move_range(0..5, 3);
}
