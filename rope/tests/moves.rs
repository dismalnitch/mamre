//! Moving text and sections: the same as cutting and pasting a `String`,
//! chunks kept full, summaries right, and every position carried across.

use mamre_rope::{Address, Markdown, Moved, Newline, Rope, Sections};
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

    /// Moving a section is reordering whole lines, the last one ending in
    /// its implied newline (as in Vim) whether the text has one or not.
    /// Every char lands where `map` says, and the moved section's heading
    /// is at its new place.
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
        let want = reorder_lines(&text, range.clone(), to);
        let mut moved = rope.clone();
        let m = moved.move_section(address, to).unwrap();
        prop_assert_eq!(moved.chars().collect::<String>(), want.clone());
        moved.check().map_err(TestCaseError::fail)?;
        let fresh: Rope<Markdown> = Rope::from(want.as_str());
        prop_assert_eq!(moved.summary(), fresh.summary(), "summaries as if built afresh");
        for (i, c) in text.char_indices() {
            let j = m.map(i);
            prop_assert_eq!(want[j..].chars().next(), Some(c), "char at {} went to {}", i, j);
        }
        // It only turns when the text has no final newline to move with.
        if m.newline != Newline::Stays {
            prop_assert!(!text.ends_with('\n'));
        } else {
            let mut by_range = rope.clone();
            prop_assert_eq!(&m, &by_range.move_range(range.clone(), to));
        }
        // The heading is where the section now starts.
        let heading = m.map(range.start);
        prop_assert_eq!(moved.heading_at(moved.address_at(heading)), Some(heading));
    }
}

/// The model: the text as lines, each ending in a newline (the last one's
/// implied if the text has none), the section's lines moved to before the
/// line at `to` (or after them all), and joined back.
fn reorder_lines(text: &str, range: std::ops::Range<usize>, to: usize) -> String {
    let implied = !text.ends_with('\n');
    let mut whole = text.to_string();
    if implied {
        whole.push('\n');
    }
    // A line's index by its start; the end, past every line.
    let line = |at: usize| {
        if at == text.len() {
            whole.split_inclusive('\n').count()
        } else {
            text[..at].matches('\n').count()
        }
    };
    let mut lines: Vec<&str> = whole.split_inclusive('\n').collect();
    let (a, b, t) = (line(range.start), line(range.end), line(to));
    let section: Vec<&str> = lines.drain(a..b).collect();
    let t = if t > a { t - (b - a) } else { t };
    lines.splice(t..t, section);
    let mut out = lines.concat();
    if implied {
        out.pop();
    }
    out
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
            to: 13..36,
            newline: Newline::Stays,
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

/// The last section of a text with no final newline, moved earlier: it
/// takes the newline before it along, to end its own last line.
#[test]
fn the_last_section_moves_with_its_implied_newline() {
    let text = "# A\nSome text.\n# B\nThe last line";
    let mut rope: Rope<Markdown> = Rope::from(text);
    let b = Address::new(&[2]).unwrap();
    let moved = rope.move_section(b, 0).unwrap();
    assert_eq!(
        rope.chars().collect::<String>(),
        "# B\nThe last line\n# A\nSome text."
    );
    assert_eq!(
        moved,
        Moved {
            from: 14..32,
            to: 0..18,
            newline: Newline::ToEnd,
        }
    );
    // The newline that ended "Some text." now ends "The last line".
    assert_eq!(moved.map(14), 17);
    assert_eq!(moved.map(15), 0);
}

/// A section moved to the end of a text with no final newline goes after
/// the last line, its own newline now ending that line.
#[test]
fn a_section_moved_to_the_end_puts_its_newline_first() {
    let text = "# A\nSome text.\n# B\nThe last line";
    let mut rope: Rope<Markdown> = Rope::from(text);
    let a = Address::new(&[1]).unwrap();
    let moved = rope.move_section(a, text.len()).unwrap();
    assert_eq!(
        rope.chars().collect::<String>(),
        "# B\nThe last line\n# A\nSome text."
    );
    assert_eq!(moved.newline, Newline::ToStart);
    assert_eq!(moved.map(14), 17, "A's newline ends the line before it");
    assert_eq!(moved.map(0), 18, "A's heading");
}
