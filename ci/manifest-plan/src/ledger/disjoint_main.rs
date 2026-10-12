//! The `disjoint-main` admission proof, shared by every reader that admits or
//! re-checks one: the validation driver (`scripts/lib/validate_admission.rs`)
//! and the parent's admission and receipt finalization, which link this crate.
//! One implementation, so the readers cannot drift.
//!
//! A target that contains the main E it entered the queue with is admitted
//! after main moved to M when the move is one a straightforward rebase absorbs
//! and that leaves every floor-based check's verdict unchanged. The floor and
//! every pin base stay E. The parent's 2-hour age bound on E is not part of the
//! floor record, so no reader of this proof can re-check it.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Output;

use super::DISJOINT_MAIN_MAX_DELTA_COMMITS;
use super::admission_hex;

/// Files that carry a pin a floor-based check compares against the floor. A
/// target that changes one is refused this kind: its pin checks would compare
/// a new pin against E while bindings elsewhere in the tree (for example the
/// LiteInst cache keys that check-reverie-pin.rs greps the whole tree for)
/// could have been added by E..M. Such a target is validated against the real
/// current main instead. Same pattern forms as the floor-sensitive list.
///
/// The floor-sensitive list itself is one of these. The parent reads the list
/// from the target's tree, the one input the target controls, so a target that
/// rewrote it (to `README`, say) could have the proof accept an E..M that edits
/// the validation DAG. A target that touches the list is validated against the
/// real current main instead.
pub const TARGET_PIN_FILE_PATTERNS: &str = "**/Cargo.toml\n**/Cargo.lock\nhermit-cli/src/metadata.rs\nhermit-cli/BUCK\nci/admission/floor-sensitive-paths.txt\n";

#[derive(Clone, Debug, PartialEq, Eq)]
enum FloorSensitivePattern {
    /// `dir/`: every path under the directory.
    Directory(String),
    /// `prefix*`: every path starting with the prefix.
    Prefix(String),
    /// `**/NAME`: every path whose final component is NAME.
    FileName(String),
    /// An exact path.
    Exact(String),
}

impl FloorSensitivePattern {
    fn parse(line: &str) -> Result<Self, &'static str> {
        if line.starts_with('/') || line.contains('\\') {
            return Err("paths are repository-relative with no leading `/` or `\\`");
        }
        if let Some(name) = line.strip_prefix("**/") {
            if name.is_empty() || name.contains('/') || name.contains('*') {
                return Err("`**/` must be followed by one file name");
            }
            Ok(Self::FileName(name.into()))
        } else if let Some(prefix) = line.strip_suffix('*') {
            if prefix.is_empty() || prefix.contains('*') {
                return Err("a prefix pattern has one trailing `*` after a nonempty prefix");
            }
            Ok(Self::Prefix(prefix.into()))
        } else if line.contains('*') {
            Err("`*` is allowed only as `**/NAME` or one trailing `*`")
        } else if line.ends_with('/') {
            Ok(Self::Directory(line.into()))
        } else {
            Ok(Self::Exact(line.into()))
        }
    }

    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Directory(prefix) | Self::Prefix(prefix) => path.starts_with(prefix.as_str()),
            Self::FileName(name) => path.rsplit('/').next() == Some(name.as_str()),
            Self::Exact(exact) => path == exact,
        }
    }
}

/// A list in the format of `ci/admission/floor-sensitive-paths.txt`: one
/// pattern per line, `#` comment lines, and exactly four forms (`dir/`,
/// `prefix*`, `**/NAME`, an exact path).
#[derive(Clone, Debug)]
pub struct FloorSensitivePaths(Vec<FloorSensitivePattern>);

impl FloorSensitivePaths {
    /// Parse the list, refusing anything outside the four forms so a pattern
    /// two readers would interpret differently can never be accepted.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut patterns = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            patterns
                .push(FloorSensitivePattern::parse(line).map_err(|why| {
                    format!("floor-sensitive line {}: {why}: {raw:?}", index + 1)
                })?);
        }
        if patterns.is_empty() {
            return Err("the floor-sensitive path list is empty".into());
        }
        Ok(Self(patterns))
    }

    /// Whether a repository path matches any pattern.
    pub fn matches(&self, path: &str) -> bool {
        self.0.iter().any(|pattern| pattern.matches(path))
    }

    /// Whether every path `pattern` (one line in the same four forms) can
    /// match is matched by this list.
    pub fn covers(&self, pattern: &str) -> Result<bool, String> {
        let pattern = FloorSensitivePattern::parse(pattern.trim())
            .map_err(|why| format!("{why}: {pattern:?}"))?;
        Ok(match &pattern {
            FloorSensitivePattern::Exact(path) => self.matches(path),
            FloorSensitivePattern::Directory(prefix) | FloorSensitivePattern::Prefix(prefix) => {
                self.0.iter().any(|own| match own {
                    FloorSensitivePattern::Directory(p) | FloorSensitivePattern::Prefix(p) => {
                        prefix.starts_with(p.as_str())
                    }
                    _ => false,
                })
            }
            FloorSensitivePattern::FileName(_) => self.0.contains(&pattern),
        })
    }
}

/// What [`disjoint_main_proof`] established from Git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisjointMainProof {
    /// `git rev-list --first-parent --count E..M`, within
    /// 1..=[`DISJOINT_MAIN_MAX_DELTA_COMMITS`].
    pub main_delta_commits: u64,
    /// Every path any commit in E..M touched (merges against each parent).
    pub main_touched_paths: BTreeSet<String>,
    /// Every path whose content differs between E and the target.
    pub target_changed_paths: BTreeSet<String>,
}

/// Prove from Git, in `repo`, that `target` may be admitted against the queued
/// main `queued` (E) although main is now `observed` (M), failing closed on
/// any error. `list_text` is the floor-sensitive list as read from the
/// target's tree (`ci/admission/floor-sensitive-paths.txt`). In order:
///
/// 1. E and M are distinct full SHAs, and so is the target, which is not E
///    (a target equal to E is a `main-ancestor` run);
/// 2. E is an ancestor of the target and of M;
/// 3. M is not an ancestor of the target (that is a current-main run);
/// 4. the first-parent count n of E..M is 1..=12, and E is on M's
///    first-parent line (`M~n` is E), so a side-branch commit merged into
///    main is never a floor;
/// 5. the paths changed between E and M and between E and the target (end
///    trees, renames split) are disjoint;
/// 6. E..target changes no pin file ([`TARGET_PIN_FILE_PATTERNS`]);
/// 7. no commit in E..M touched a floor-sensitive path. This is the per-commit
///    set, not the E-to-M tree diff: check-record-version-floor.rs reads the
///    floor's history (`git log {floor} -- hermit-cli/src/metadata.rs`), so a
///    change made and reverted inside E..M still changes its verdict.
pub fn disjoint_main_proof(
    repo: &Path,
    queued: &str,
    observed: &str,
    target: &str,
    list_text: &str,
) -> Result<DisjointMainProof, String> {
    if ![queued, observed, target]
        .iter()
        .all(|s| admission_hex(s, 40))
    {
        return Err("disjoint-main proof needs full lowercase commit SHAs".into());
    }
    if queued == observed {
        return Err("disjoint-main needs an observed main that differs from the floor".into());
    }
    if queued == target {
        return Err(
            "the target is the floor itself, which main already contains: a main-ancestor run"
                .into(),
        );
    }
    let sensitive = FloorSensitivePaths::parse(list_text)?;
    let pin_files = FloorSensitivePaths::parse(TARGET_PIN_FILE_PATTERNS)?;
    if !is_ancestor(repo, queued, target)? {
        return Err(format!(
            "disjoint-main floor {queued} is not an ancestor of the target"
        ));
    }
    if !is_ancestor(repo, queued, observed)? {
        return Err(format!(
            "disjoint-main floor {queued} is not an ancestor of the observed main {observed}"
        ));
    }
    if is_ancestor(repo, observed, target)? {
        return Err(format!(
            "the target contains the observed main {observed}, so its floor is current-main"
        ));
    }
    let counted = git_text(
        repo,
        &[
            "rev-list",
            "--first-parent",
            "--count",
            &format!("{queued}..{observed}"),
        ],
    )?;
    let main_delta_commits: u64 = counted
        .trim()
        .parse()
        .map_err(|e| format!("invalid first-parent count {counted:?}: {e}"))?;
    if !(1..=DISJOINT_MAIN_MAX_DELTA_COMMITS).contains(&main_delta_commits) {
        return Err(format!(
            "main moved {main_delta_commits} first-parent commits since the floor, outside 1..={DISJOINT_MAIN_MAX_DELTA_COMMITS}"
        ));
    }
    // E is a first-parent ancestor of M exactly when the n-th first parent of
    // M is E: walking first parents from M, the walk leaves E..M at E.
    let on_line = git_text(
        repo,
        &[
            "rev-parse",
            "--verify",
            &format!("{observed}~{main_delta_commits}^{{commit}}"),
        ],
    )?;
    if on_line.trim() != queued {
        return Err(format!(
            "the floor {queued} is not on the observed main's first-parent line (main~{main_delta_commits} is {})",
            on_line.trim()
        ));
    }
    let main_changed = end_tree_changes(repo, queued, observed)?;
    let target_changed_paths = end_tree_changes(repo, queued, target)?;
    let shared: Vec<&String> = main_changed.intersection(&target_changed_paths).collect();
    if !shared.is_empty() {
        return Err(format!(
            "main and the target both changed {} path(s) since the floor, e.g. {:?}",
            shared.len(),
            &shared[..shared.len().min(5)]
        ));
    }
    let pins: Vec<&String> = target_changed_paths
        .iter()
        .filter(|p| pin_files.matches(p))
        .collect();
    if !pins.is_empty() {
        return Err(format!(
            "the target changes {} pin file(s) since the floor, e.g. {:?}; validate it against the current main",
            pins.len(),
            &pins[..pins.len().min(5)]
        ));
    }
    let main_touched_paths = touched_by_commits(repo, queued, observed)?;
    let touched: Vec<&String> = main_touched_paths
        .iter()
        .filter(|p| sensitive.matches(p))
        .collect();
    if !touched.is_empty() {
        return Err(format!(
            "commits in main since the floor touched {} floor-sensitive path(s), e.g. {:?}",
            touched.len(),
            &touched[..touched.len().min(5)]
        ));
    }
    Ok(DisjointMainProof {
        main_delta_commits,
        main_touched_paths,
        target_changed_paths,
    })
}

/// One Git query over the repository's own objects: no inherited repository
/// redirection, replace refs, grafts or shallow file.
fn git_output(repo: &Path, args: &[&str]) -> Result<Output, String> {
    let mut command = crate::git_environment::git_command();
    command
        .env_remove("GIT_SHALLOW_FILE")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .arg("--no-replace-objects")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("cannot run git {args:?}: {e}"))
}

fn git_ok(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = git_output(repo, args)?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

fn git_text(repo: &Path, args: &[&str]) -> Result<String, String> {
    String::from_utf8(git_ok(repo, args)?)
        .map_err(|e| format!("git {args:?} returned invalid UTF-8: {e}"))
}

/// Whether `ancestor` is an ancestor of (or equal to) `descendant`. Git's
/// "no" (exit 1) is the only failure read as false; anything else is an error.
fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let args = ["merge-base", "--is-ancestor", ancestor, descendant];
    let out = git_output(repo, &args)?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git {args:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

fn nul_paths(bytes: Vec<u8>) -> Result<BTreeSet<String>, String> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| {
            String::from_utf8(p.to_vec()).map_err(|e| format!("git named a non-UTF-8 path: {e}"))
        })
        .collect()
}

/// Every path whose content differs between two commits' trees, renames split
/// into their deletion and addition so neither name is hidden. Plumbing, so no
/// porcelain diff configuration applies.
fn end_tree_changes(repo: &Path, from: &str, to: &str) -> Result<BTreeSet<String>, String> {
    nul_paths(git_ok(
        repo,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--name-only",
            from,
            to,
        ],
    )?)
}

/// Every path any commit in `from..to` touched. All commits, not only the
/// first-parent chain, because the floor checks walk the floor's whole history
/// (`git log {floor} -- path` follows every parent). Merges are diffed
/// against each parent (`--diff-merges=separate`, the explicit spelling of
/// `-m`, so `log.diffMerges` cannot change it); against a side parent that can
/// list paths changed before the floor, which only refuses more. Renames are
/// split, and signatures are not printed into the output.
fn touched_by_commits(repo: &Path, from: &str, to: &str) -> Result<BTreeSet<String>, String> {
    nul_paths(git_ok(
        repo,
        &[
            "log",
            "-z",
            "--no-show-signature",
            "--no-renames",
            "--diff-merges=separate",
            "--name-only",
            "--format=",
            &format!("{from}..{to}"),
        ],
    )?)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Repo(PathBuf);
    impl Repo {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "disjoint-main-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            let repo = Self(path);
            repo.git(&["init", "--quiet"]);
            repo
        }
        fn git(&self, args: &[&str]) -> String {
            let out = crate::git_environment::git_command()
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&self.0)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().into()
        }
        /// Write `content` to each path, commit, and return the SHA.
        fn commit(&self, content: &str, paths: &[&str]) -> String {
            for path in paths {
                let full = self.0.join(path);
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(&full, content).unwrap();
                self.git(&["add", "--", path]);
            }
            self.git(&["commit", "--quiet", "--allow-empty", "-m", content]);
            self.git(&["rev-parse", "HEAD"])
        }
        /// Commit on top of `base`.
        fn commit_on(&self, base: &str, content: &str, paths: &[&str]) -> String {
            self.git(&["checkout", "--quiet", "--detach", base]);
            self.commit(content, paths)
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    // A subset of the real ci/admission/floor-sensitive-paths.txt, which lists
    // ci/admission/ (the list itself) as well.
    const LIST: &str = "ci/dag/\nci/admission/\n**/Cargo.toml\n**/Cargo.lock\nhermit-cli/src/metadata.rs\nhermit-cli/BUCK\n";

    /// E with the files the cases below edit, M one docs-only commit later,
    /// and a target that changes one source file on E.
    fn fixture() -> (Repo, String, String, String) {
        let repo = Repo::new();
        repo.commit(
            "before",
            &[
                "src/lib.rs",
                "docs/a.md",
                "ci/dag/x",
                "Cargo.toml",
                "Cargo.lock",
                "hermit-cli/src/metadata.rs",
            ],
        );
        let queued = repo.commit("queued main E", &["README"]);
        let observed = repo.commit_on(&queued, "main moves on docs", &["docs/a.md"]);
        let target = repo.commit_on(&queued, "target change", &["src/lib.rs"]);
        (repo, queued, observed, target)
    }

    #[test]
    fn floor_sensitive_matcher_accepts_exactly_four_pattern_forms() {
        use FloorSensitivePattern::*;
        let list = FloorSensitivePaths::parse(
            "# comment\n\n  ci/\nscripts/lib/validate*\n  # indented comment\n**/Cargo.lock\nhermit-cli/BUCK\n",
        )
        .unwrap();
        assert_eq!(
            list.0,
            [
                Directory("ci/".into()),
                Prefix("scripts/lib/validate".into()),
                FileName("Cargo.lock".into()),
                Exact("hermit-cli/BUCK".into()),
            ]
        );
        for path in [
            "ci/x",
            "ci/dag/validate.json",
            "scripts/lib/validate",
            "scripts/lib/validate_admission.rs",
            "Cargo.lock",
            "a/b/Cargo.lock",
            "hermit-cli/BUCK",
        ] {
            assert!(list.matches(path), "{path}");
        }
        for path in [
            "ci",
            "src/ci/x",
            "scripts/lib/valid.rs",
            "scripts/validate.rs",
            "Cargo.lock.orig",
            "xCargo.lock",
            "a/fooCargo.lock",
            "a/Cargo.lock/b",
            "hermit-cli/BUCK.v2",
            "hermit-cli/BUC",
            "docs/x",
            "",
        ] {
            assert!(!list.matches(path), "{path}");
        }
        for bad in [
            "/ci/", "ci/**/x", "*.rs", "a*b", "**/", "**/a/b", "**/a*", "*", "a**", "a\\b",
            "# only\n",
        ] {
            assert!(FloorSensitivePaths::parse(bad).is_err(), "{bad:?}");
        }
        // Coverage of a pattern, not just a path.
        for (pattern, covered) in [
            ("**/Cargo.lock", true),
            ("**/Cargo.toml", false),
            ("ci/dag/", true),
            ("ci/dag*", true),
            ("ci", false),
            ("scripts/lib/validate_x.rs", true),
            ("scripts/lib/", false),
            ("hermit-cli/BUCK", true),
        ] {
            assert_eq!(list.covers(pattern).unwrap(), covered, "{pattern}");
        }
        assert!(list.covers("a*b").is_err());
        // Every pin file a target may not change is itself floor-sensitive.
        let real = FloorSensitivePaths::parse(LIST).unwrap();
        for pattern in TARGET_PIN_FILE_PATTERNS.lines() {
            assert!(real.covers(pattern).unwrap(), "{pattern}");
        }
    }

    /// The proof admits a bounded, path-disjoint delta and refuses each
    /// condition in turn, naming it.
    #[test]
    fn disjoint_main_proof_admits_only_a_disjoint_bounded_main_delta() {
        let (repo, queued, observed, target) = fixture();
        let proof = |e: &str, m: &str, t: &str| disjoint_main_proof(&repo.0, e, m, t, LIST);
        let admitted = proof(&queued, &observed, &target).unwrap();
        assert_eq!(admitted.main_delta_commits, 1);
        assert_eq!(
            admitted.main_touched_paths,
            ["docs/a.md".to_string()].into()
        );
        assert_eq!(
            admitted.target_changed_paths,
            ["src/lib.rs".to_string()].into()
        );

        let refused = |e: &str, m: &str, t: &str| proof(e, m, t).unwrap_err();
        assert!(refused(&queued, &queued, &target).contains("differs from the floor"));
        assert!(refused(&queued, &observed, &queued).contains("main-ancestor"));
        assert!(refused(&queued, &observed, "HEAD").contains("full lowercase"));
        let overlap = repo.commit_on(&queued, "main edits src", &["src/lib.rs"]);
        assert!(refused(&queued, &overlap, &target).contains("both changed 1 path"));
        let dag = repo.commit_on(&queued, "main edits the dag", &["ci/dag/x"]);
        let e = refused(&queued, &dag, &target);
        assert!(
            e.contains("floor-sensitive") && e.contains("ci/dag/x"),
            "{e}"
        );
        let on_main = repo.commit_on(&observed, "target on M", &["src/lib.rs"]);
        assert!(refused(&queued, &observed, &on_main).contains("current-main"));
        let before = repo.git(&["rev-parse", &format!("{queued}~1")]);
        let stale = repo.commit_on(&before, "target before E", &["src/lib.rs"]);
        assert!(refused(&queued, &observed, &stale).contains("not an ancestor of the target"));
        let rewritten = repo.commit_on(&before, "rewritten main", &["docs/c.md"]);
        assert!(refused(&queued, &rewritten, &target).contains("observed main"));
        let mut far = observed.clone();
        for n in 0..DISJOINT_MAIN_MAX_DELTA_COMMITS {
            far = repo.commit_on(&far, &format!("main {n}"), &[&format!("docs/{n}.md")]);
        }
        let e = refused(&queued, &far, &target);
        assert!(e.contains("moved 13 first-parent commits"), "{e}");
        let bad = disjoint_main_proof(&repo.0, &queued, &observed, &target, "a*b").unwrap_err();
        assert!(bad.contains("floor-sensitive line 1"), "{bad}");
    }

    /// A floor-sensitive file changed and then restored inside E..M leaves the
    /// E-to-M trees identical there, but check-record-version-floor.rs reads
    /// the floor's history, so the proof refuses it.
    #[test]
    fn disjoint_main_proof_reads_every_commit_of_the_main_delta() {
        let (repo, queued, observed, target) = fixture();
        let original = std::fs::read_to_string(repo.0.join("hermit-cli/src/metadata.rs")).unwrap();
        let bumped = repo.commit_on(
            &observed,
            "bump RECORD_VERSION",
            &["hermit-cli/src/metadata.rs"],
        );
        let reverted = repo.commit_on(&bumped, &original, &["hermit-cli/src/metadata.rs"]);
        // The end-tree diff does not see it: what an end-tree-only proof read.
        let ends = end_tree_changes(&repo.0, &queued, &reverted).unwrap();
        assert_eq!(ends, ["docs/a.md".to_string()].into());
        let touched = touched_by_commits(&repo.0, &queued, &reverted).unwrap();
        assert!(
            touched.contains("hermit-cli/src/metadata.rs"),
            "{touched:?}"
        );
        let e = disjoint_main_proof(&repo.0, &queued, &reverted, &target, LIST).unwrap_err();
        assert!(
            e.contains("floor-sensitive") && e.contains("hermit-cli/src/metadata.rs"),
            "{e}"
        );
        // Merges are read against each parent: a side branch's change counts.
        let side = repo.commit_on(&queued, "side edits a lockfile", &["sub/Cargo.lock"]);
        repo.git(&["checkout", "--quiet", "--detach", &observed]);
        repo.git(&["merge", "--quiet", "--no-ff", "--no-edit", &side]);
        let merged = repo.git(&["rev-parse", "HEAD"]);
        let e = disjoint_main_proof(&repo.0, &queued, &merged, &target, LIST).unwrap_err();
        assert!(e.contains("sub/Cargo.lock"), "{e}");
    }

    /// A floor that main contains only through a merged side branch is not on
    /// main's first-parent line and is refused, even when every other
    /// condition holds.
    #[test]
    fn disjoint_main_proof_refuses_a_floor_off_mains_first_parent_line() {
        let (repo, queued, observed, _) = fixture();
        // A side branch from E carries the floor S; main merges it, so S is an
        // ancestor of the merge without being one of its first parents.
        let side = repo.commit_on(&queued, "side floor S", &["docs/side.md"]);
        repo.git(&["checkout", "--quiet", "--detach", &observed]);
        repo.git(&["merge", "--quiet", "--no-ff", "--no-edit", &side]);
        let merged = repo.git(&["rev-parse", "HEAD"]);
        let target = repo.commit_on(&side, "target on S", &["src/lib.rs"]);
        let e = disjoint_main_proof(&repo.0, &side, &merged, &target, LIST).unwrap_err();
        assert!(
            e.contains("not on the observed main's first-parent line"),
            "{e}"
        );
        // The same merge is admitted against the first-parent floor E.
        let on_e = repo.commit_on(&queued, "target on E", &["src/lib.rs"]);
        let proof = disjoint_main_proof(&repo.0, &queued, &merged, &on_e, LIST).unwrap();
        assert_eq!(proof.main_delta_commits, 2);
    }

    /// A target that changes a pin file is never admitted against an older
    /// floor, even when main's move is disjoint from it.
    #[test]
    fn disjoint_main_proof_refuses_a_target_that_changes_a_pin_file() {
        let (repo, queued, observed, _) = fixture();
        for pin in [
            "Cargo.toml",
            "nested/Cargo.toml",
            "Cargo.lock",
            "hermit-cli/src/metadata.rs",
            "hermit-cli/BUCK",
            "ci/admission/floor-sensitive-paths.txt",
        ] {
            let target = repo.commit_on(&queued, &format!("target bumps {pin}"), &[pin]);
            let e = disjoint_main_proof(&repo.0, &queued, &observed, &target, LIST).unwrap_err();
            assert!(e.contains("pin file") && e.contains(pin), "{pin}: {e}");
        }
        let fine = repo.commit_on(&queued, "target edits a non-pin manifest", &["xCargo.toml"]);
        disjoint_main_proof(&repo.0, &queued, &observed, &fine, LIST).unwrap();

        // The review's attack: the target rewrites the list to name only
        // README, the parent reads that list from the target's tree, and E..M
        // edits the validation DAG. Refused on the target side, whatever list
        // text the caller passes.
        let dag_main = repo.commit_on(&queued, "main edits the DAG", &["ci/dag/x"]);
        let rewrites = repo.commit_on(
            &queued,
            "README",
            &["ci/admission/floor-sensitive-paths.txt"],
        );
        let e =
            disjoint_main_proof(&repo.0, &queued, &dag_main, &rewrites, "README\n").unwrap_err();
        assert!(
            e.contains("pin file") && e.contains("ci/admission/floor-sensitive-paths.txt"),
            "{e}"
        );
    }
}
