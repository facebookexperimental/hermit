//! The archived execution paths: the backend and the preprocessing step the
//! owner stopped developing, whose tests left the per-commit validation for
//! the nightly pressure run.
//!
//! Owner direction, 2026-10-25: "mark our sabre and e9patch backends as
//! archived for now and move their tests to nightly only and not actively
//! develop them." Archived code still builds in every validation (the
//! third-party-backends workspace build, Clippy and the SaBRe smoke test
//! `test.sabre_examples`), so it cannot rot silently, but no feature work
//! lands on it.
//!
//! A manifest cell on an archived backend that would belong to the full run
//! type belongs to [`ARCHIVED_RUN_TYPE`] instead ([`crate::runner::cell_labels`]).
//! Full validation, its portable and hosted subsets, and the Buck route (whose
//! cells are `ci/expected-e2e-plan.json`'s) therefore do not select it, and the
//! scorecard reports it as not selected by full: a red cell, which the nightly
//! pressure sweep's red part runs three times a night, and a verify cell, which
//! its parity-all part measures beside ptrace. A cell that already names a
//! focused run type, such as `sabre-compat-only`, keeps it.

/// What an archived entry is. Hermit AGENTS.md, "Backend Definition": only a
/// path that loads Detcore as a Reverie tool is a backend; e9patch rewrites a
/// binary that then runs on the ptrace backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchivedKind {
    /// A Reverie backend running `Detcore<XxxGuest>`, named by manifest cells.
    Backend,
    /// Preprocessing whose output runs on the ptrace backend. It has no
    /// manifest cells; its corpus is `ci/compat/corpus-e9patch.json`, run by
    /// `scripts/validate.rs --e9patch-compat-only`.
    PreprocessingWithPtrace,
}

/// One archived execution path.
#[derive(Clone, Copy, Debug)]
pub struct Archived {
    /// The `--backend` spelling.
    pub name: &'static str,
    pub kind: ArchivedKind,
    /// The day of the owner's direction (US Eastern).
    pub since: &'static str,
    /// Where its tests run now that per-commit validation runs only its smoke
    /// checks: "nightly only" when a scheduled run exercises them, "on demand
    /// only" when nothing scheduled does.
    pub schedule: &'static str,
    /// What still runs per commit, and what runs only nightly.
    pub coverage: &'static str,
}

/// Every archived execution path. Restoring one is removing its entry and
/// running `ci/sync-cell-config.sh`, which returns its cells to the full run
/// type and every derived file with them.
pub const ARCHIVED: &[Archived] = &[
    Archived {
        name: "sabre",
        kind: ArchivedKind::Backend,
        since: "2026-10-25",
        schedule: "nightly only",
        coverage: "Per commit: the third-party-backends build and Clippy, and the \
                   test.sabre_examples smoke test (ptrace and SaBRe parity and a \
                   canonical --verify-strict run of one shell guest, and DETLOG \
                   agreement through post-exec). Nightly: every \
                   manifest cell, in the pressure sweep, and the SaBRe run type \
                   (scripts/validate.rs --sabre-compat-only: its compat cells and \
                   sabrecompat.sabre_examples, the other 34 tests of \
                   hermit-cli/tests/sabre_examples.rs), in the sweep's \
                   sabre-examples part.",
    },
    Archived {
        name: "e9patch",
        kind: ArchivedKind::PreprocessingWithPtrace,
        since: "2026-10-25",
        schedule: "nightly only",
        coverage: "Per commit: the e9patch feature build, the e9patch corpus driver \
                   contract (check.e9patch_corpus) and the e9patch legs of the CLI \
                   tests. Nightly: the corpus itself (scripts/validate.rs \
                   --e9patch-compat-only, which builds e9patch and e9tool through \
                   hermit-install), in the sweep's e9patch-corpus part; it has no \
                   manifest cells.",
    },
];

/// The run type of an archived backend's cells that would otherwise be full's.
/// No validation profile selects it; the pressure test runs these cells under
/// it, as it runs every focused run type's.
pub const ARCHIVED_RUN_TYPE: &str = "archived-nightly";

/// Whether manifest cells on `backend` are archived.
pub fn is_archived_backend(backend: &str) -> bool {
    ARCHIVED
        .iter()
        .any(|entry| entry.kind == ArchivedKind::Backend && entry.name == backend)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sabre_is_an_archived_backend_and_e9patch_is_archived_preprocessing() {
        assert!(is_archived_backend("sabre"));
        // e9patch is not a backend, so no manifest cell is archived by its name.
        assert!(!is_archived_backend("e9patch"));
        assert!(
            ARCHIVED.iter().any(|entry| entry.name == "e9patch"
                && entry.kind == ArchivedKind::PreprocessingWithPtrace)
        );
        for backend in [
            "ptrace",
            "dbt",
            "kvm",
            "liteinst",
            "in-guest-trap",
            "native",
        ] {
            assert!(!is_archived_backend(backend), "{backend}");
        }
    }
}
