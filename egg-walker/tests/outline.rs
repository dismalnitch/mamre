//! A deleted heading (#19): the section stays, headless, at the heading's
//! place by default; policies can carry it on or fold it; every deletion
//! that leaves a section headless is reported once, and so is its end.

use mamre_egg_walker::copies;
use mamre_egg_walker::{
    Accountant, Ambiguity, Lv, Next, Op, OpLog, Policy, Resolution, Section, Start,
};
use proptest::prelude::*;

/// A replica: its log is shared, its books and document its own.
struct Replica {
    books: Accountant,
    doc: Vec<char>,
}

impl Replica {
    fn new() -> Self {
        Replica {
            books: Accountant::new(),
            doc: Vec::new(),
        }
    }

    fn merge(&mut self, log: &OpLog<char>, to: &[Lv]) -> Vec<Ambiguity> {
        for c in self.books.merge(log, to) {
            c.apply(&mut self.doc);
        }
        self.books.take_ambiguities()
    }

    fn sections(&self) -> Vec<(usize, Lv, u8, Start)> {
        self.books
            .sections()
            .iter()
            .map(
                |&Section {
                     item,
                     pos,
                     level,
                     start,
                 }| (pos, item, level, start),
            )
            .collect()
    }
}

/// "Hello" as section 1 at 0 and "world" as section 1 at 5, then the
/// second heading deleted: the log, the version, and the books' replica.
fn two_sections() -> (OpLog<char>, Vec<Lv>, Replica) {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, "Helloworld".chars());
    let v = log.add_mark(a, &v, 0, 1);
    let v = log.add_mark(a, &v, 5, 1);
    let mut r = Replica::new();
    r.merge(&log, &v);
    assert!(r.books.take_ambiguities().is_empty());
    (log, v, r)
}

#[test]
fn a_deleted_heading_leaves_its_section_headless() {
    let (mut log, v, mut r) = two_sections();
    let a = log.agent("a");
    let v = log.add_delete(a, &v, 5..6);
    let reports = r.merge(&log, &v);
    assert_eq!(
        reports,
        vec![Ambiguity::HeadingDeleted {
            section: 5,
            level: 1,
            next: Next::Text,
            chose: Policy::Headless
        }]
    );
    assert_eq!(
        r.sections(),
        vec![(0, 0, 1, Start::Heading), (5, 5, 1, Start::Headless)]
    );
    assert_eq!(r.books.section_range(0), Some(0..5));
    assert_eq!(r.books.section_range(5), Some(5..9));
    assert_eq!(r.books.headless(), vec![5]);
    // Reported once: nothing more until something changes.
    let v = log.add_insert(a, &v, 0, "x".chars());
    assert!(r.merge(&log, &v).is_empty());
    assert_eq!(r.books.section_range(5), Some(6..10));
}

#[test]
fn policies_carry_on_or_fold() {
    let (mut log, v, mut r) = two_sections();
    r.books.set_policy(Next::Text, Policy::CarryOn);
    let a = log.agent("a");
    let v = log.add_delete(a, &v, 5..6);
    let reports = r.merge(&log, &v);
    assert!(matches!(
        reports[..],
        [Ambiguity::HeadingDeleted {
            chose: Policy::CarryOn,
            ..
        }]
    ));
    assert_eq!(r.sections()[1], (5, 5, 1, Start::CarriedOn));
    // Folded: it leaves the outline, and the section above runs on.
    r.books.set_policy(Next::Text, Policy::Fold);
    assert_eq!(r.sections(), vec![(0, 0, 1, Start::Heading)]);
    assert_eq!(r.books.section_range(0), Some(0..9));
    assert_eq!(r.books.section_range(5), None);
    // Still an open ambiguity, whatever shows.
    assert_eq!(r.books.headless(), vec![5]);
}

#[test]
fn what_follows_a_deleted_heading() {
    // "AbCDe": A at 1, C at 1, D at 2; the cases by deleting each.
    let build = || {
        let mut log = OpLog::new();
        let a = log.agent("a");
        let v = log.add_insert(a, &[], 0, "AbCDe".chars());
        let v = log.add_mark(a, &v, 0, 1);
        let v = log.add_mark(a, &v, 2, 1);
        let v = log.add_mark(a, &v, 3, 2);
        let mut r = Replica::new();
        r.merge(&log, &v);
        (log, v, r)
    };
    let next_after = |range: std::ops::Range<usize>| {
        let (mut log, v, mut r) = build();
        let a = log.agent("a");
        let v = log.add_delete(a, &v, range);
        match r.merge(&log, &v)[..] {
            [Ambiguity::HeadingDeleted { next, .. }] => next,
            ref other => panic!("{other:?}"),
        }
    };
    assert_eq!(next_after(0..1), Next::Text, "b follows A");
    assert_eq!(next_after(0..2), Next::Heading, "C follows A, at its level");
    assert_eq!(next_after(2..3), Next::Subsection, "D follows C, below it");
    assert_eq!(next_after(3..5), Next::End, "nothing follows D");
}

#[test]
fn an_undo_restores_the_heading() {
    let (mut log, v, mut r) = two_sections();
    let a = log.agent("a");
    let v = log.add_delete(a, &v, 5..6);
    r.merge(&log, &v);
    let d = v[0];
    let v = log.add_undelete(a, &v, [d]);
    assert_eq!(
        r.merge(&log, &v),
        vec![Ambiguity::Resolved {
            section: 5,
            by: Resolution::Restored
        }]
    );
    assert_eq!(r.sections()[1], (5, 5, 1, Start::Heading));
    assert!(r.books.headless().is_empty());
}

#[test]
fn a_concurrent_unmark_resolves_it() {
    let (mut log, base, mut r) = two_sections();
    let (a, b) = (log.agent("a"), log.agent("b"));
    let deleted = log.add_delete(a, &base, 5..6);
    let unmarked = log.add_mark(b, &base, 5, 0);
    r.merge(&log, &deleted);
    assert_eq!(
        r.merge(&log, &unmarked),
        vec![Ambiguity::Resolved {
            section: 5,
            by: Resolution::Unmarked
        }]
    );
    assert_eq!(r.sections(), vec![(0, 0, 1, Start::Heading)]);
}

#[test]
fn typing_at_the_place_goes_in_the_section_and_a_new_heading_replaces_it() {
    let (mut log, v, mut r) = two_sections();
    let a = log.agent("a");
    let v = log.add_delete(a, &v, 5..6);
    r.merge(&log, &v);
    // Typed where the heading was: inside the headless section.
    let v = log.add_insert(a, &v, 5, "W".chars());
    assert!(r.merge(&log, &v).is_empty());
    assert_eq!(r.books.section_range(5), Some(5..10));
    assert_eq!(r.doc.iter().collect::<String>(), "HelloWorld");
    // Marked as the new heading: the ambiguity is over.
    let w = v[0];
    let v = log.add_mark(a, &v, 5, 1);
    assert_eq!(
        r.merge(&log, &v),
        vec![Ambiguity::Resolved {
            section: 5,
            by: Resolution::Replaced { by: w }
        }]
    );
    assert_eq!(
        r.sections(),
        vec![(0, 0, 1, Start::Heading), (5, w, 1, Start::Heading)]
    );
    assert!(r.books.headless().is_empty());
}

#[test]
fn a_headless_section_moves_with_its_text() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, "AaaBbbCcc".chars());
    let v = log.add_mark(a, &v, 0, 1);
    let v = log.add_mark(a, &v, 3, 1);
    let v = log.add_mark(a, &v, 6, 1);
    let mut r = Replica::new();
    r.merge(&log, &v);
    let v = log.add_delete(a, &v, 3..4);
    r.merge(&log, &v);
    // "bb" is the headless section: moved to the end, it goes along.
    let range = r.books.section_range(3).unwrap();
    assert_eq!(range, 3..5);
    let v = log.add_move(a, &v, range, 8);
    r.merge(&log, &v);
    assert_eq!(r.doc.iter().collect::<String>(), "AaaCccbb");
    assert_eq!(
        r.sections(),
        vec![
            (0, 0, 1, Start::Heading),
            (3, 6, 1, Start::Heading),
            (6, 3, 1, Start::Headless)
        ]
    );
}

#[test]
fn headless_sections_are_saved_with_the_books() {
    let (mut log, v, mut r) = two_sections();
    let a = log.agent("a");
    // The heading, and then the end: one ghost mid-text, one at the end.
    let v = log.add_mark(a, &v, 9, 2);
    let v = log.add_delete(a, &v, 9..10);
    let v = log.add_delete(a, &v, 5..6);
    r.merge(&log, &v);
    let mut file = mamre_egg_walker::file::create();
    mamre_egg_walker::file::save(&mut file, &log, 0, r.doc.iter().cloned(), &r.books);
    let loaded = mamre_egg_walker::file::load::<char>(&file).unwrap();
    let back = loaded.snapshot.unwrap().accountant;
    assert_eq!(back.sections(), r.books.sections());
    assert_eq!(back.headless(), vec![5, 9]);
}

// ---- Many writers, any policies ----

#[derive(Clone, Debug)]
enum Edit {
    Insert(prop::sample::Index, char),
    Delete(prop::sample::Index, usize),
    Move(
        prop::sample::Index,
        prop::sample::Index,
        prop::sample::Index,
    ),
    Undo(prop::sample::Index),
    Mark(prop::sample::Index, u8),
}

#[derive(Clone, Debug)]
enum Action {
    Edit(usize, Edit),
    Merge(usize, usize),
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        4 => (any::<prop::sample::Index>(), prop::char::range('a', 'z')).prop_map(|(p, c)| Edit::Insert(p, c)),
        2 => (any::<prop::sample::Index>(), 1usize..4).prop_map(|(p, n)| Edit::Delete(p, n)),
        1 => (any::<prop::sample::Index>(), any::<prop::sample::Index>(), any::<prop::sample::Index>())
            .prop_map(|(a, b, t)| Edit::Move(a, b, t)),
        2 => any::<prop::sample::Index>().prop_map(Edit::Undo),
        3 => (any::<prop::sample::Index>(), 0u8..4).prop_map(|(p, l)| Edit::Mark(p, l)),
    ]
}

fn actions() -> impl Strategy<Value = Vec<Action>> {
    let action = prop_oneof![
        4 => (0..3usize, edit()).prop_map(|(w, e)| Action::Edit(w, e)),
        1 => (0..3usize, 0..3usize).prop_map(|(a, b)| Action::Merge(a, b)),
    ];
    prop::collection::vec(action, 1..50)
}

/// The history the actions make, and every version a writer reached.
fn history(actions: &[Action]) -> (OpLog<char>, Vec<Vec<Lv>>) {
    let mut log: OpLog<char> = OpLog::new();
    let agents: Vec<_> = ["a", "b", "c"].iter().map(|n| log.agent(n)).collect();
    let mut versions: Vec<Vec<Lv>> = vec![Vec::new(); 3];
    let mut seen = Vec::new();
    for action in actions.iter().cloned() {
        match action {
            Action::Merge(a, b) => versions[a] = log.union(&versions[a], &versions[b]),
            Action::Edit(w, edit) => {
                let v = versions[w].clone();
                let len = copies::checkout(&log, &v).len();
                let agent = agents[w];
                versions[w] = match edit {
                    Edit::Insert(p, c) => log.add_insert(agent, &v, p.index(len + 1), [c]),
                    Edit::Delete(p, n) if len > 0 => {
                        let pos = p.index(len);
                        log.add_delete(agent, &v, pos..(pos + n).min(len))
                    }
                    Edit::Move(a, b, t) if len > 0 => {
                        let (a, b) = (a.index(len), b.index(len));
                        let (a, b) = (a.min(b), a.max(b) + 1);
                        let to = t.index(len + 1);
                        if to > a && to < b {
                            continue;
                        }
                        log.add_move(agent, &v, a..b, to)
                    }
                    Edit::Undo(k) => {
                        let deletes: Vec<Lv> = (0..log.len())
                            .filter(|&lv| {
                                log.contains(&v, lv)
                                    && matches!(log.event(lv).op, Op::Delete { .. })
                            })
                            .collect();
                        if deletes.is_empty() {
                            continue;
                        }
                        log.add_undelete(agent, &v, [deletes[k.index(deletes.len())]])
                    }
                    Edit::Mark(p, level) if len > 0 => log.add_mark(agent, &v, p.index(len), level),
                    _ => continue,
                };
            }
        }
        seen.push(
            versions
                .iter()
                .fold(Vec::new(), |acc, v| log.union(&acc, v)),
        );
        seen.extend(versions.iter().cloned());
    }
    (log, seen)
}

fn describe(log: &OpLog<char>) -> String {
    (0..log.len())
        .map(|lv| {
            let e = log.event(lv);
            format!("{lv}: {:?} after {:?}\n", e.op, e.parents)
        })
        .collect()
}

fn policy() -> impl Strategy<Value = Policy> {
    prop_oneof![
        Just(Policy::Headless),
        Just(Policy::CarryOn),
        Just(Policy::Fold)
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Replicas with any policies, each merging the versions in its own
    /// order: text and marks converge, and every deleted heading is
    /// reported once while it's open, and its end once.
    #[test]
    fn policies_never_break_convergence(
        actions in actions(),
        policies in prop::collection::vec(prop::collection::vec(policy(), 4), 3),
        orders in prop::collection::vec(any::<prop::sample::Index>(), 3),
    ) {
        let (log, versions) = history(&actions);
        let nexts = [Next::Text, Next::Heading, Next::Subsection, Next::End];
        let mut replicas = Vec::new();
        for (k, ps) in policies.iter().enumerate() {
            let mut r = Replica::new();
            for (n, &p) in nexts.iter().zip(ps) {
                r.books.set_policy(*n, p);
            }
            // Its own order: the versions, rotated.
            let mut order = versions.clone();
            if !order.is_empty() {
                let at = orders[k].index(order.len());
                order.rotate_left(at);
            }
            let mut open: Vec<Lv> = Vec::new();
            for v in &order {
                for report in r.merge(&log, v) {
                    match report {
                        Ambiguity::HeadingDeleted { section, .. } => {
                            prop_assert!(!open.contains(&section), "{} reported twice\n{}", section, describe(&log));
                            open.push(section);
                        }
                        Ambiguity::Resolved { section, .. } => {
                            prop_assert!(open.contains(&section), "{} resolved but not open\n{}", section, describe(&log));
                            open.retain(|&s| s != section);
                        }
                    }
                }
                open.sort_unstable();
                prop_assert_eq!(&open, &r.books.headless(), "open ambiguities at {:?}\n{}", r.books.version(), describe(&log));
                let at = r.books.version().to_vec();
                prop_assert_eq!(&r.doc, &copies::checkout(&log, &at), "text at {:?}\n{}", at, describe(&log));
                let levels = copies::levels(&log, &at);
                for item in 0..log.len() {
                    prop_assert_eq!(r.books.level(item), levels.get(&item).copied().unwrap_or(0), "level of {} at {:?}\n{}", item, at, describe(&log));
                }
                // The outline is in order, and in the document.
                let sections = r.books.sections();
                prop_assert!(sections.windows(2).all(|w| w[0].pos <= w[1].pos));
                prop_assert!(sections.iter().all(|s| s.pos <= r.doc.len()));
                let ids = copies::identities(&log, &at);
                for s in &sections {
                    if s.start == Start::Heading {
                        prop_assert_eq!(ids[s.pos], s.item);
                    } else {
                        prop_assert!(!ids.contains(&s.item), "a headless section's heading shows\n{}", describe(&log));
                    }
                }
            }
            replicas.push(r);
        }
        // Everything merged: the same text and marks everywhere.
        let all = versions.iter().fold(Vec::new(), |acc, v| log.union(&acc, v));
        for r in &mut replicas {
            r.merge(&log, &all);
        }
        for r in &replicas[1..] {
            prop_assert_eq!(&r.doc, &replicas[0].doc);
            for item in 0..log.len() {
                prop_assert_eq!(r.books.level(item), replicas[0].books.level(item));
            }
        }
    }
}
