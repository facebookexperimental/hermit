/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! The parity population is the verify matrix: every manifest test whose
//! ptrace verify cell full validation selects, crossed with every candidate
//! backend column. These tests rebuild that set from the manifests through
//! the runner's own selection, without the parity module's population code,
//! and require the two to agree, so neither a selection file nor a backend
//! column can drift out of the population unnoticed.

use std::collections::BTreeSet;
use std::path::PathBuf;

use hermit_manifest_plan::parity;
use hermit_manifest_plan::parity::ParityBackend;
use hermit_manifest_plan::parity::ParityCellId;
use hermit_manifest_plan::runner::ManifestSet;
use hermit_manifest_plan::runner::Population;
use hermit_manifest_plan::runner::Selection;

fn manifests() -> ManifestSet {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    ManifestSet::load(&root).expect("the shipped manifests load")
}

/// {test ids whose ptrace verify cell full validation selects} x
/// ParityBackend::ALL, built from `ManifestSet::select` alone, equals the
/// parity population.
#[test]
fn the_parity_population_is_every_selected_ptrace_verify_test_on_every_candidate() {
    let manifests = manifests();
    let reference_tests = manifests
        .select(&Selection {
            population: Some(Population::Required),
            mode: Some("verify".to_string()),
            backend: Some("ptrace".to_string()),
            ..Selection::default()
        })
        .expect("full validation's selection")
        .into_iter()
        .map(|cell| {
            assert_eq!(cell.id.mode, "verify");
            assert_eq!(cell.id.backend.as_deref(), Some("ptrace"));
            cell.id.test
        })
        .collect::<BTreeSet<_>>();
    assert!(!reference_tests.is_empty());
    let expected = reference_tests
        .iter()
        .flat_map(|test| {
            ParityBackend::ALL.into_iter().map(|backend| ParityCellId {
                test_id: test.clone(),
                backend,
            })
        })
        .collect::<BTreeSet<_>>();
    let population = parity::population(&manifests).expect("the parity population");
    let missing = expected.difference(&population).collect::<Vec<_>>();
    let extra = population.difference(&expected).collect::<Vec<_>>();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "missing from the population: {missing:?}; not in the verify matrix: {extra:?}"
    );
    assert_eq!(
        population.len(),
        reference_tests.len() * ParityBackend::ALL.len()
    );

    // The committed snapshot's population is the same set.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let snapshot: parity::ParityCells = serde_json::from_str(
        &std::fs::read_to_string(root.join(parity::PARITY_CELLS_PATH)).unwrap(),
    )
    .unwrap();
    let committed = snapshot
        .cells
        .iter()
        .filter(|cell| cell.selected)
        .map(|cell| ParityCellId {
            test_id: cell.test_id.clone(),
            backend: cell.backend,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(committed, expected);
}

/// ParityBackend::ALL is exactly the set of non-ptrace backends that appear
/// as verify columns in the manifests (enabled or disabled), so a new backend
/// column cannot be left out of parity.
#[test]
fn every_candidate_verify_column_is_a_parity_backend() {
    let manifests = manifests();
    let columns = manifests
        .all_tests()
        .filter_map(|(_, _, _, test)| test.modes.get("verify"))
        .flat_map(|recipe| {
            recipe
                .backends_enabled
                .iter()
                .cloned()
                .chain(recipe.backends_disabled.keys().cloned())
                .collect::<Vec<_>>()
        })
        .filter(|backend| backend != "ptrace")
        .collect::<BTreeSet<_>>();
    let parity_backends = ParityBackend::ALL
        .into_iter()
        .map(|backend| backend.as_str().to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(columns, parity_backends);
}
