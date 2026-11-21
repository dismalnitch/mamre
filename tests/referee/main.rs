//! The referee: checks mamre's tree against a model that does the same job
//! the slow, obvious way.
//!
//! - `model.rs`: the rules, as a `Vec`, a fold and a linear scan.
//! - `subject.rs`: what the referee needs from a tree.
//! - `ops.rs`: random operations, applied to both, checked after each one.
//! - `cheaters.rs`: trees that are wrong on purpose, to show the referee
//!   catches them.
//! - `mamre.rs`: where the real tree plugs in.
//!
//! Run all:        cargo test
//! Run the tree:   cargo test --test referee mamre
//!
//! When a test fails, proptest shrinks it to a small case and saves it under
//! `proptest-regressions/`, so it's tried first next time. Commit those.

mod cheaters;
mod mamre;
mod model;
mod ops;
mod subject;

use proptest::prelude::*;

proptest! {
    /// The harness checks itself: the model must agree with itself.
    #[test]
    fn the_model_passes(start in ops::lines(80), ops in ops::ops()) {
        ops::run::<subject::Model>(start, &ops).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn mamre_matches_the_model(start in ops::lines(80), ops in ops::ops()) {
        ops::run::<mamre::Tree>(start, &ops).map_err(TestCaseError::fail)?;
    }
}

proptest! {
    /// What runs before `append` and `Cursor` exist: build by pushing, then
    /// check the shape, items and summary. No seeks, so no `verify`.
    #[test]
    fn mamre_builds_like_the_model(start in ops::lines(5000)) {
        use subject::Subject;
        let tree = mamre::Tree::from_items(start.clone());
        tree.check().map_err(TestCaseError::fail)?;
        prop_assert_eq!(tree.items(), start.clone());
        prop_assert_eq!(tree.summary(), model::summarize(&start));
    }
}

proptest! {
    /// What runs before `slice` and `suffix` exist: seek to every boundary,
    /// the middle of every item and past the end, by every dimension, both
    /// ways, and check where it lands and the summary before it.
    #[test]
    fn mamre_seeks_like_the_model(start in ops::lines(200)) {
        use subject::Subject;
        let tree = mamre::Tree::from_items(start.clone());
        let mut before = model::Lines::default();
        for i in 0..=start.len() {
            let line = start.get(i);
            for (target, width) in [
                (model::Target::Count(before.count), 1),
                (model::Target::Len(before.len), line.map_or(3, |l| l.len as u64)),
                (model::Target::Height(before.height), line.map_or(3, |l| l.height as u64)),
            ] {
                let v = target.value();
                for value in [v, v + width / 2, v + width] {
                    let target = match target {
                        model::Target::Count(_) => model::Target::Count(value as usize),
                        model::Target::Len(_) => model::Target::Len(value),
                        model::Target::Height(_) => model::Target::Height(value),
                    };
                    for bias in [model::Bias::Left, model::Bias::Right] {
                        prop_assert_eq!(
                            tree.seek(target, bias),
                            model::seek(&start, target, bias),
                            "seek({:?}, {:?})", target, bias
                        );
                        prop_assert_eq!(
                            tree.prefix(target, bias),
                            model::prefix(&start, target, bias),
                            "prefix({:?}, {:?})", target, bias
                        );
                    }
                }
            }
            if let Some(line) = line {
                before = before.add(&model::Lines::of(line));
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// What runs before `Cursor` exists: build trees by pushing, then join
    /// them by appending on either side. Appending a small tree onto a big one
    /// and a big one onto a small one take different paths, so do both. After
    /// each join, check shape, items and summary, and that a copy made before
    /// it is untouched. No seeks, so no `verify`.
    #[test]
    fn mamre_appends_like_the_model(
        start in ops::lines(3000),
        joins in prop::collection::vec((ops::lines(3000), any::<bool>()), 1..6),
    ) {
        use subject::Subject;
        let mut tree = mamre::Tree::from_items(start.clone());
        let mut model = start;
        for (lines, after) in joins {
            let copy = tree.clone();
            let copy_model = model.clone();
            let other = mamre::Tree::from_items(lines.clone());
            if after {
                tree.append(other);
                model.extend(lines);
            } else {
                let mut front = other;
                front.append(tree);
                tree = front;
                model.splice(0..0, lines);
            }
            for (t, m) in [(&tree, &model), (&copy, &copy_model)] {
                t.check().map_err(TestCaseError::fail)?;
                prop_assert_eq!(&t.items(), m);
                prop_assert_eq!(t.summary(), model::summarize(m));
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Fewer cases, but trees deep enough for several levels of nodes.
    #[test]
    fn mamre_matches_the_model_when_deep(start in ops::lines(5000), ops in ops::ops()) {
        ops::run::<mamre::Tree>(start, &ops).map_err(TestCaseError::fail)?;
    }
}
