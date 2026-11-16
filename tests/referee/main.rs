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
//! Run the tree:   cargo test --test referee mamre -- --include-ignored
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
    #[ignore = "wire mamre's tree up in tests/referee/mamre.rs"]
    fn mamre_matches_the_model(start in ops::lines(80), ops in ops::ops()) {
        ops::run::<mamre::Tree>(start, &ops).map_err(TestCaseError::fail)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Fewer cases, but trees deep enough for several levels of nodes.
    #[test]
    #[ignore = "wire mamre's tree up in tests/referee/mamre.rs"]
    fn mamre_matches_the_model_when_deep(start in ops::lines(5000), ops in ops::ops()) {
        ops::run::<mamre::Tree>(start, &ops).map_err(TestCaseError::fail)?;
    }
}
