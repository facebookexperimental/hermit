// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Known-gap ledgers. A ledger lists the tests of a suite that a validation node does not yet
//! require to pass on its backend, each with its tracking issue: the tests it runs and expects to
//! fail, and the tests it skips. So a node can run a suite on a backend that does not pass all of
//! it yet without counting a failure as a pass or hiding it.
//!
//! The format and its readers:
//! - `ci/kvm-gap-ledger.rs`. It is the node's runner, the static check, and the history gate that
//!   `make lint-checks` runs (`lint-check-known-gap-ledgers`).
//! - The validation DAG generator. It writes each [`Declaration`] into its node's environment as
//!   [`LEDGER_ENV`], so `ci/dag/validate.json` names the file the node judges.
//! - `ci/compat-envelope/scorecard.rs`. It lists every row in SCORECARD.md and in
//!   `ci/compat-envelope/known-gaps.json`, outside every E2E count.
//! - The dev-hermit receipt qualifier, owned outside this repository. It reads a node's
//!   [`LEDGER_ENV`] declaration from `validate.json` and the file as committed on main. A
//!   `diagnostic_fail` row qualifies only if its structured-result `id` equals a row's [`Row::id`].
//!   Do not change [`HEADER`] without telling that reader: the header is the schema's version.
//!   Its parser predates [`Outcome::Skipped`] and refuses that outcome. It reads a ledger only for
//!   a run that has diagnostic failures, which a ledger whose every row is skipped cannot produce;
//!   a ledger that mixes skipped rows with rows the node runs needs that parser to accept them
//!   first.
//!
//! # Format
//!
//! UTF-8 with LF line endings and a final newline. A line whose first character is `#` is a
//! comment, wherever it is; a blank line is an error. The first other line is [`HEADER`]. Every
//! later line is a row of six TAB-separated fields, none empty and none padded:
//!
//! | Column | Content |
//! | --- | --- |
//! | `id` | The test's structured-result id, `<cargo package>::<test binary>$<test function>`, exactly as the node's results name it. Rows are sorted by it, byte order, with no duplicate. |
//! | `gap` | The reason: a short gap code, no whitespace. The ledger's comments define each code. |
//! | `issue` | The tracking issue or pull request, as a full GitHub URL. |
//! | `outcome` | How the failing attempt must end: `failed`, or `cpu_timeout` (stopped by its CPU budget). Or `skipped`: the node does not run the test at all, it leaves it out of the run by name, so there is no attempt. |
//! | `signature` | For `failed`, text the test's output must show ([`signature_matches`]) when the node runs it with `RUST_BACKTRACE=0`, so a panic prints std's `note: run with` line rather than a backtrace; it may hold spaces, not TABs, and writes a newline as `\n`, a backslash as `\\` and any run of digits as `<N>`. For `cpu_timeout` and `skipped`, `-`. |
//! | `approval` | `baseline` for a row present, unchanged, since the ledger's introduction; otherwise the full URL of a specific approving comment, review or commit. |
//!
//! # What may change, checked against history
//!
//! A row excuses one test failing one way: its id, outcome and signature ([`Row::scope`]). The
//! history gate ([`judge_history`]) holds the ledger to the following rules along the first-parent
//! history that starts at its introduction:
//! - A row is a baseline row only if its scope has been in every committed version since that
//!   introduction. A row added later, or whose outcome or signature changed, needs an approval URL
//!   that a commit-message line binds to its id: `KVM-Gap-Approval: <URL> <id>`, in the commit
//!   that added it or a later one. So the baseline can only shrink, and a swap is an addition.
//!   Skipping a test is weaker than running it as a known gap (nothing then shows whether it still
//!   fails), so turning a row skipped is such a change, and so is turning it back. Deleting a
//!   skipped row needs nothing: its test becomes required, and the node runs it.
//! - A test that leaves the required set needs approval too. Moving into the ledger is an
//!   addition. Leaving the suite, or being `#[ignore]`d, needs `KVM-Required-Removal: <URL> <id>`.
//!   A rename leaves under the old id.
//!
//! The required set comes from the suite's source ([`suite_tests`]), whose parser refuses every
//! attribute form it does not understand on the lines it reads. The node also compares that set
//! with Nextest's list of the built suite, test by test. Together, the parser and the node see
//! every test added, removed or ignored. The history check alone, which is all the portable lanes
//! run, does not: its parser skips an attribute that follows a block comment on the same line,
//! takes a declaration faked inside a string or a block comment for a real one, and does not follow
//! `include!`, a `#[path]` or `#[macro_use]` module, or a macro. Only the node, which runs on KVM
//! hosts, catches a test added, removed or ignored through those.
//!
//! The walk follows first parents, so the verdict is the same on any linear (rebased) history: on
//! main, which lands by rebase, and on a rebased branch. On a merge commit, a row or removal enters
//! at the merge, and an approval line on the merged side branch is not read: such a change is
//! refused until a commit on the first-parent chain carries the line. GitHub's checkout of an
//! external pull request is such a merge (`refs/pull/N/merge`), so a pull request that adds an
//! approved row goes red there until it is rebased.
//!
//! The walk reads every committed version since the introduction. A version it cannot read (a
//! malformed ledger, a suite form the parser refuses, the suite file renamed) stops it at exit 2,
//! and only a change to the checker can repair that, so a change to a ledger or its suite runs
//! `make lint-check-known-gap-ledgers` before it lands. Validation runs that target in
//! `check.lint_checks` (`make lint-checks`, profiles full, portable and hosted-portable). Renaming the
//! suite file is moving the ledger: a new introduction, reviewed as one.
//!
//! What no offline check can see stays with review:
//! - whether an approval URL really points at the coordinator's approval of those rows;
//! - a test that returns early on one backend;
//! - a change to the checker itself;
//! - a new ledger, or a moved one, whose introduction starts a new baseline.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// The step environment variable through which a node declares its ledger: its value is the
/// ledger's path from the repository root.
pub const LEDGER_ENV: &str = "HERMIT_KNOWN_GAP_LEDGER";

/// The environment variable through which the node's runner asks its backend-proof test to record
/// the backend it ran on.
pub const BACKEND_PROOF_ENV: &str = "HERMIT_TEST_BACKEND_PROOF";

/// The first non-comment line of a ledger. It is the schema's version; see the module
/// documentation before changing it.
pub const HEADER: &str = "id\tgap\tissue\toutcome\tsignature\tapproval";

/// The approval field of a row present, unchanged, since the ledger's introduction.
pub const BASELINE_APPROVAL: &str = "baseline";

/// The signature field of a row whose outcome has no output to match.
pub const NO_SIGNATURE: &str = "-";

/// The commit-message key that binds an approval URL to the ledger rows it approves.
pub const GAP_APPROVAL_KEY: &str = "KVM-Gap-Approval:";

/// The commit-message key that approves a test leaving the required set other than into the
/// ledger: deleted from the suite, renamed, or `#[ignore]`d.
pub const REQUIRED_REMOVAL_KEY: &str = "KVM-Required-Removal:";

/// A validation node held to a known-gap ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Declaration {
    /// The node, `group.job`.
    pub node: &'static str,
    /// The ledger, from the repository root.
    pub ledger: &'static str,
    /// The suite's source, from the repository root.
    pub suite_source: &'static str,
    /// What every structured-result id of the suite starts with: `<package>::<binary>$`.
    pub id_prefix: &'static str,
    /// The backend the node runs the suite on, the value of the suite's `HERMIT_TEST_BACKEND`.
    pub backend: &'static str,
    /// The required test that proves the run was on [`Declaration::backend`]: given
    /// [`BACKEND_PROOF_ENV`], it appends the backend its verified run showed, one line per run.
    pub backend_proof_test: &'static str,
}

/// Every node held to a known-gap ledger. The DAG generator refuses a gap-ledger producer missing
/// here, and any other node that sets [`LEDGER_ENV`].
pub const DECLARATIONS: &[Declaration] = &[Declaration {
    node: "privileged-test.deadline_slice_cap_kvm",
    ledger: "hermit-cli/tests/deadline_slice_cap.kvm-gaps.tsv",
    suite_source: "hermit-cli/tests/deadline_slice_cap.rs",
    id_prefix: "hermit-run::deadline_slice_cap$",
    backend: "kvm",
    backend_proof_test: "capped_schedules_verify_bitwise",
}];

/// The declaration of `node`, if it has a ledger.
pub fn declaration_for_node(node: &str) -> Option<&'static Declaration> {
    DECLARATIONS
        .iter()
        .find(|declaration| declaration.node == node)
}

/// The declaration whose ledger is `ledger` (a path from the repository root).
pub fn declaration_for_ledger(ledger: &str) -> Option<&'static Declaration> {
    DECLARATIONS
        .iter()
        .find(|declaration| declaration.ledger == ledger)
}

/// The node's pinned number of required tests: its `NEXTEST_EXPECTED_EXECUTED`.
pub fn required_tests(declaration: &Declaration) -> Option<u64> {
    crate::validation_dag_static::expected_nextest_count(declaration.node)
}

/// How a gap row's failing attempt must end, or that the node does not run it.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Outcome {
    /// An ordinary failure, whose output must contain the row's signature.
    Failed,
    /// Stopped by its per-test CPU budget.
    CpuTimeout,
    /// Not run: the node leaves the test out of its run by name, so there is no attempt to judge.
    Skipped,
}

impl Outcome {
    /// The ledger's spelling. For an outcome the node runs, it is also dagrun's attempt-outcome
    /// spelling.
    pub fn value(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::CpuTimeout => "cpu_timeout",
            Self::Skipped => "skipped",
        }
    }

    /// Whether the node runs a row with this outcome.
    pub fn is_run(self) -> bool {
        self != Self::Skipped
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "failed" => Some(Self::Failed),
            "cpu_timeout" => Some(Self::CpuTimeout),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// A row's approval field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Approval {
    /// Present, unchanged, since the ledger's introduction.
    Baseline,
    /// The full URL of a specific approving comment, review or commit.
    Url(String),
}

/// One row of a ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub id: String,
    pub gap: String,
    pub issue: String,
    pub outcome: Outcome,
    /// `None` for an outcome with no output to match.
    pub signature: Option<String>,
    pub approval: Approval,
}

impl Row {
    /// What the row excuses: one test failing one way. Changing any part of it is adding a row.
    pub fn scope(&self) -> (&str, Outcome, Option<&str>) {
        (&self.id, self.outcome, self.signature.as_deref())
    }

    /// The test function the row names, if its id carries `prefix`.
    pub fn test<'a>(&'a self, prefix: &str) -> Option<&'a str> {
        self.id.strip_prefix(prefix)
    }
}

fn is_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn is_number(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('0') && value.chars().all(|c| c.is_ascii_digit())
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A structured-result test id: `<package>::<binary>$<test>`, whose test may be a `::` path.
pub fn is_test_id(value: &str) -> bool {
    let Some((binary, test)) = value.split_once('$') else {
        return false;
    };
    let Some((package, binary)) = binary.split_once("::") else {
        return false;
    };
    is_name(package)
        && !package.contains('.')
        && is_name(binary)
        && !binary.contains('.')
        && test.split("::").all(is_identifier)
}

/// A full GitHub issue or pull-request URL: `https://github.com/OWNER/REPO/issues/N` or
/// `.../pull/N`.
pub fn is_issue_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("https://github.com/") else {
        return false;
    };
    matches!(
        rest.split('/').collect::<Vec<_>>().as_slice(),
        [owner, repo, "issues" | "pull", number]
            if is_name(owner) && is_name(repo) && is_number(number)
    )
}

/// The full URL of one specific approving artifact: a comment on an issue or pull request
/// (`#issuecomment-N`), a review or review comment on a pull request (`#pullrequestreview-N`,
/// `#discussion_rN`), or a commit by its full 40-hex SHA. A bare issue or pull-request URL is
/// not specific: it names a thread, not the approval in it.
pub fn is_approval_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("https://github.com/") else {
        return false;
    };
    let (path, fragment) = match rest.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (rest, None),
    };
    let anchored = |prefixes: &[&str]| {
        fragment.is_some_and(|fragment| {
            prefixes
                .iter()
                .any(|prefix| fragment.strip_prefix(prefix).is_some_and(is_number))
        })
    };
    match path.split('/').collect::<Vec<_>>().as_slice() {
        [owner, repo, "issues", number] if is_name(owner) && is_name(repo) && is_number(number) => {
            anchored(&["issuecomment-"])
        }
        [owner, repo, "pull", number] if is_name(owner) && is_name(repo) && is_number(number) => {
            anchored(&["issuecomment-", "pullrequestreview-", "discussion_r"])
        }
        [owner, repo, "commit", sha] if is_name(owner) && is_name(repo) => {
            fragment.is_none()
                && sha.len() == 40
                && sha
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        }
        _ => false,
    }
}

/// Parse a ledger ([module documentation](self)). Returns the rows, or the first violation of the
/// format, with its line number.
pub fn parse(text: &str) -> Result<Vec<Row>, String> {
    if text.contains('\r') {
        return Err("the ledger has a CR; its lines end with LF alone".into());
    }
    if !text.ends_with('\n') {
        return Err("the ledger does not end with a newline".into());
    }
    let mut lines = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.starts_with('#'));
    match lines.next() {
        Some((_, line)) if line == HEADER => {}
        Some((index, line)) => {
            return Err(format!(
                "line {}: expected the header {HEADER:?}, found {line:?}",
                index + 1
            ));
        }
        None => return Err(format!("no header {HEADER:?}")),
    }
    let mut rows: Vec<Row> = Vec::new();
    for (index, line) in lines {
        let number = index + 1;
        let fields = line.split('\t').collect::<Vec<_>>();
        let [id, gap, issue, outcome, signature, approval] = fields.as_slice() else {
            return Err(format!(
                "line {number}: a row has six TAB-separated fields (id, gap, issue, outcome, signature, approval), found {}: {line:?}",
                fields.len()
            ));
        };
        for (name, value) in [
            ("id", id),
            ("gap", gap),
            ("issue", issue),
            ("outcome", outcome),
            ("approval", approval),
        ] {
            if value.is_empty() || value.chars().any(char::is_whitespace) {
                return Err(format!(
                    "line {number}: the {name} field must be one nonempty word, found {value:?}"
                ));
            }
        }
        if signature.is_empty()
            || signature.trim() != *signature
            || signature.chars().any(char::is_control)
        {
            return Err(format!(
                "line {number}: the signature field must be nonempty text without padding or control characters, found {signature:?}"
            ));
        }
        if *signature != NO_SIGNATURE {
            signature_pieces(signature)
                .map_err(|error| format!("line {number}: {id}: {error}, found {signature:?}"))?;
        }
        if !is_test_id(id) {
            return Err(format!(
                "line {number}: {id:?} is not a structured-result test id (<package>::<binary>$<test>)"
            ));
        }
        if !is_issue_url(issue) {
            return Err(format!(
                "line {number}: {id}: the issue field must be the tracking issue's full URL (https://github.com/OWNER/REPO/issues/N), found {issue:?}"
            ));
        }
        let outcome = Outcome::parse(outcome).ok_or_else(|| {
            format!("line {number}: {id}: the outcome field must be failed, cpu_timeout or skipped, found {outcome:?}")
        })?;
        let signature = match (outcome, *signature) {
            (Outcome::CpuTimeout | Outcome::Skipped, NO_SIGNATURE) => None,
            (Outcome::CpuTimeout | Outcome::Skipped, other) => {
                return Err(format!(
                    "line {number}: {id}: a {} row has no output to match, so its signature is {NO_SIGNATURE:?}, found {other:?}",
                    outcome.value()
                ));
            }
            (Outcome::Failed, NO_SIGNATURE) => {
                return Err(format!(
                    "line {number}: {id}: a failed row names the text its failure must show, so its signature cannot be {NO_SIGNATURE:?}"
                ));
            }
            (Outcome::Failed, text) => Some(text.to_string()),
        };
        let approval = if *approval == BASELINE_APPROVAL {
            Approval::Baseline
        } else if is_approval_url(approval) {
            Approval::Url(approval.to_string())
        } else {
            return Err(format!(
                "line {number}: {id}: the approval field must be {BASELINE_APPROVAL:?} or the full URL of a specific approving comment, review or commit (#issuecomment-N, #pullrequestreview-N, #discussion_rN, or commit/<40-hex SHA>), found {approval:?}"
            ));
        };
        if let Some(previous) = rows.last() {
            if previous.id == *id {
                return Err(format!("line {number}: {id} is listed twice"));
            }
            if previous.id.as_str() > *id {
                return Err(format!(
                    "line {number}: rows must stay sorted by id: {id} comes after {}",
                    previous.id
                ));
            }
        }
        rows.push(Row {
            id: id.to_string(),
            gap: gap.to_string(),
            issue: issue.to_string(),
            outcome,
            signature,
            approval,
        });
    }
    Ok(rows)
}

/// The placeholder in a signature that matches one or more ASCII digits.
pub const NUMBER_PLACEHOLDER: &str = "<N>";

/// A signature's literal pieces, split at each [`NUMBER_PLACEHOLDER`], with its escapes decoded:
/// `\n` is a newline and `\\` a backslash; any other backslash is refused. The first piece must
/// not be empty, and two placeholders cannot be adjacent.
pub fn signature_pieces(signature: &str) -> Result<Vec<String>, String> {
    let mut pieces = vec![String::new()];
    let mut chars = signature.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => pieces.last_mut().expect("one piece").push('\n'),
                Some('\\') => pieces.last_mut().expect("one piece").push('\\'),
                other => {
                    return Err(format!(
                        "a signature's backslash is followed by {other:?}; its only escapes are \\n (a newline) and \\\\ (a backslash)"
                    ));
                }
            },
            '<' if chars.clone().take(2).collect::<String>() == "N>" => {
                chars.next();
                chars.next();
                if pieces.last().is_some_and(String::is_empty) {
                    return Err(format!(
                        "a signature must start with literal text, and two {NUMBER_PLACEHOLDER} cannot be adjacent"
                    ));
                }
                pieces.push(String::new());
            }
            c => pieces.last_mut().expect("one piece").push(c),
        }
    }
    Ok(pieces)
}

/// Whether `output` shows `signature` ([`signature_pieces`]): its first piece somewhere, and each
/// later piece right after a run of ASCII digits that follows the piece before it.
pub fn signature_matches(signature: &str, output: &str) -> bool {
    fn follows(pieces: &[String], text: &str) -> bool {
        let Some((piece, rest)) = pieces.split_first() else {
            return true;
        };
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        (1..=digits).rev().any(|take| {
            text[take..].starts_with(piece.as_str()) && follows(rest, &text[take + piece.len()..])
        })
    }
    let Ok(pieces) = signature_pieces(signature) else {
        return false;
    };
    let (first, rest) = pieces.split_first().expect("one piece");
    output
        .match_indices(first.as_str())
        .any(|(at, _)| follows(rest, &output[at + first.len()..]))
}

/// `text` with the whitespace outside string literals removed: the form attributes are compared in.
fn compact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

/// The identifiers of an attribute, outside its string literals.
fn attribute_words(attribute: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in attribute.chars() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c);
            continue;
        }
        if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        in_string = c == '"';
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Whether an attribute could change which tests run or whether one is ignored: it names `test`
/// (in any identifier, as `tokio::test`, `test_case` or `rstest` do), `ignore`, `cfg` or `cfg_attr`.
fn names_test_machinery(attribute: &str) -> bool {
    attribute_words(attribute)
        .iter()
        .any(|word| word.contains("test") || matches!(word.as_str(), "ignore" | "cfg" | "cfg_attr"))
}

/// Whether `attribute` (compact) is a string-valued form `#[name = "..."]`: one string literal,
/// then the closing `]`.
fn is_string_valued(attribute: &str, name: &str) -> bool {
    let Some(rest) = attribute.strip_prefix(&format!("#[{name}=\"")) else {
        return false;
    };
    let mut escaped = false;
    for (offset, c) in rest.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return &rest[offset + 1..] == "]",
            _ => {}
        }
    }
    false
}

/// An attribute the parser accepts on an item, test functions included: lint and documentation
/// attributes, the common item attributes, and `#[path = "..."]` on a module.
fn is_neutral_attribute(attribute: &str) -> bool {
    [
        "#[allow(",
        "#[expect(",
        "#[warn(",
        "#[deny(",
        "#[forbid(",
        "#[doc(",
        "#[derive(",
        "#[repr(",
        "#[inline(",
    ]
    .iter()
    .any(|prefix| attribute.starts_with(prefix) && attribute.ends_with(")]"))
        || [
            "#[inline]",
            "#[must_use]",
            "#[track_caller]",
            "#[cold]",
            "#[non_exhaustive]",
        ]
        .contains(&attribute)
        || is_string_valued(attribute, "doc")
        || is_string_valued(attribute, "path")
}

/// One attribute, from the line that starts it to the `]` that closes it, and the index of the
/// line after it. Refuses an attribute that shares its last line with code.
fn attribute_at(lines: &[&str], start: usize) -> Result<(String, usize), String> {
    let mut text = String::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, line) in lines.iter().enumerate().skip(start) {
        for (offset, c) in line.char_indices() {
            if in_string {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '[' => depth += 1,
                ']' => {
                    depth = depth.checked_sub(1).ok_or_else(|| {
                        format!("line {}: an unbalanced ] in an attribute", index + 1)
                    })?;
                    if depth == 0 {
                        if !line[offset + 1..].trim().is_empty() {
                            return Err(format!(
                                "line {}: code shares a line with an attribute; the ledger's parser reads one attribute per line: {:?}",
                                index + 1,
                                line.trim()
                            ));
                        }
                        text.push_str(line);
                        return Ok((compact(&text), index + 1));
                    }
                }
                _ => {}
            }
        }
        text.push_str(line);
        text.push('\n');
    }
    Err(format!("line {}: an attribute is never closed", start + 1))
}

/// The name of the function a top-level line declares: `fn name(` or `fn name<`, optionally
/// `pub`, `pub(crate)` or `pub(super)`.
fn declared_fn(line: &str) -> Option<&str> {
    let rest = ["pub(crate) ", "pub(super) ", "pub "]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
        .unwrap_or(line);
    let rest = rest.strip_prefix("fn ")?;
    let end = rest.find(['(', '<'])?;
    let name = &rest[..end];
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then_some(name)
}

/// The test functions a suite's source declares, each with whether it is ignored.
///
/// A test is a top-level `fn` whose attributes include `#[test]`; it is ignored when they also
/// include `#[ignore]` or `#[ignore = "..."]`, before or after `#[test]`. Every other attribute on a
/// test must be neutral: `#[should_panic]` in any form, lint and documentation attributes. Among
/// the lines it reads, it REFUSES what it does not understand:
/// - any attribute that names `test`, `ignore`, `cfg` or `cfg_attr` other than the forms above,
///   anywhere in the file: `#[cfg_attr(all(), test)]`, `#[tokio::test]`, `#[cfg(test)]`, a
///   conditional `#[cfg_attr(.., ignore)]`;
/// - such an attribute on an indented line (a nested module's test, or a macro's);
/// - any attribute it does not know on a function, which could be a test-generating macro;
/// - `#[ignore]` without `#[test]`, `#[test]` on anything but a top-level `fn`, a test declared
///   twice, and an inner attribute other than a lint.
///
/// It is not a Rust parser: it skips an attribute that follows a block comment on the same line,
/// takes a declaration inside a string or a block comment for a real one, and does not follow
/// `include!`, `#[path]` and `#[macro_use]` modules, or macros. The node checks the result against
/// Nextest's own list of the built suite, which is authoritative, so the two together see every
/// test added, removed or ignored; this is the check that needs no build, and the one history can
/// make.
pub fn suite_tests(source: &str) -> Result<BTreeMap<String, bool>, String> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut tests = BTreeMap::new();
    let mut group: Vec<(usize, String)> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        let number = index + 1;
        if trimmed.starts_with("/*") {
            // A block comment, to the line that closes it.
            while !lines[index].contains("*/") {
                index += 1;
                if index == lines.len() {
                    return Err(format!("line {number}: a block comment is never closed"));
                }
            }
            index += 1;
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("//") {
            index += 1;
            continue;
        }
        if trimmed.starts_with('#') && trimmed[1..].trim_start().starts_with('!') {
            let (attribute, next) = attribute_at(&lines, index)?;
            let lint = [
                "#![allow(",
                "#![expect(",
                "#![warn(",
                "#![deny(",
                "#![forbid(",
            ]
            .iter()
            .any(|prefix| attribute.starts_with(prefix));
            if !lint || names_test_machinery(&attribute) {
                return Err(format!(
                    "line {number}: the inner attribute {attribute} could change which tests run; the ledger's parser accepts only lint attributes there"
                ));
            }
            index = next;
            continue;
        }
        if trimmed.starts_with('#') && trimmed[1..].trim_start().starts_with('[') {
            let (attribute, next) = attribute_at(&lines, index)?;
            let indented = line.starts_with(char::is_whitespace);
            if indented {
                if names_test_machinery(&attribute) {
                    return Err(format!(
                        "line {number}: the indented attribute {attribute} names test, ignore or cfg; the ledger's parser reads only top-level tests, so a nested module's or a macro's test is refused"
                    ));
                }
            } else {
                group.push((number, attribute));
            }
            index = next;
            continue;
        }
        // Code: the item the pending attributes belong to, if any.
        if !group.is_empty() {
            let item = if line.starts_with(char::is_whitespace) {
                None
            } else {
                declared_fn(line)
            };
            let mut is_test = false;
            let mut ignored = false;
            for (at, attribute) in &group {
                match attribute.as_str() {
                    "#[test]" if is_test => {
                        return Err(format!("line {at}: a second #[test] on one item"));
                    }
                    "#[test]" => is_test = true,
                    "#[ignore]" => ignored = true,
                    "#[should_panic]" => {}
                    _ if is_string_valued(attribute, "ignore") => ignored = true,
                    _ if is_string_valued(attribute, "should_panic")
                        || (attribute.starts_with("#[should_panic(")
                            && attribute.ends_with(")]")
                            && !names_test_machinery(&attribute["#[should_panic".len()..])) => {}
                    _ if names_test_machinery(attribute) => {
                        return Err(format!(
                            "line {at}: the attribute {attribute} could change which tests run or whether one is ignored; the ledger's parser accepts #[test], #[ignore], #[ignore = \"...\"], #[should_panic] in any form, and lint and documentation attributes"
                        ));
                    }
                    _ if is_neutral_attribute(attribute) => {}
                    _ if item.is_some() => {
                        return Err(format!(
                            "line {at}: the attribute {attribute} on fn {} is one the ledger's parser does not know; it could make the function a test",
                            item.unwrap_or_default()
                        ));
                    }
                    _ => {}
                }
            }
            match (is_test, item) {
                (true, Some(name)) => {
                    if tests.insert(name.to_string(), ignored).is_some() {
                        return Err(format!("line {number}: the suite declares {name} twice"));
                    }
                }
                (true, None) => {
                    return Err(format!(
                        "line {number}: #[test] is not on a top-level fn: {trimmed:?}"
                    ));
                }
                (false, _) if ignored => {
                    return Err(format!(
                        "line {number}: #[ignore] without #[test]: {trimmed:?}"
                    ));
                }
                (false, _) => {}
            }
            group.clear();
        }
        index += 1;
    }
    if let Some((at, attribute)) = group.first() {
        return Err(format!(
            "line {at}: the attribute {attribute} ends the file"
        ));
    }
    if tests.is_empty() {
        return Err("the suite declares no #[test] function".into());
    }
    Ok(tests)
}

/// The static rules of one version: every row names a test of the suite that is not ignored, and
/// the backend-proof test is a test of the suite, not ignored and not a row. Returns every
/// violation.
pub fn check(
    declaration: &Declaration,
    rows: &[Row],
    tests: &BTreeMap<String, bool>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for row in rows {
        let Some(test) = row.test(declaration.id_prefix) else {
            errors.push(format!(
                "{}: not a test of {}: every id of this ledger starts with {:?}",
                row.id, declaration.suite_source, declaration.id_prefix
            ));
            continue;
        };
        match tests.get(test) {
            None => errors.push(format!(
                "{}: {} declares no test {test}; delete the row, or correct its id",
                row.id, declaration.suite_source
            )),
            Some(true) => errors.push(format!(
                "{}: the test is #[ignore]d, so no run can show whether it still fails",
                row.id
            )),
            Some(false) => {}
        }
    }
    let proof = format!(
        "{}{}",
        declaration.id_prefix, declaration.backend_proof_test
    );
    match tests.get(declaration.backend_proof_test) {
        None => errors.push(format!(
            "{}: the backend-proof test is not in {}",
            declaration.backend_proof_test, declaration.suite_source
        )),
        Some(true) => errors.push(format!(
            "{}: the backend-proof test is #[ignore]d",
            declaration.backend_proof_test
        )),
        Some(false) => {}
    }
    if rows.iter().any(|row| row.id == proof) {
        errors.push(format!(
            "{proof}: the backend-proof test cannot be a gap row: it is what shows that the node ran on {}",
            declaration.backend
        ));
    }
    errors
}

/// The suite's required tests in one version: every test that is not ignored and not a row.
pub fn required(
    declaration: &Declaration,
    rows: &[Row],
    tests: &BTreeMap<String, bool>,
) -> BTreeSet<String> {
    let gaps = rows
        .iter()
        .map(|row| row.id.as_str())
        .collect::<BTreeSet<_>>();
    tests
        .iter()
        .filter(|(_, ignored)| !**ignored)
        .map(|(name, _)| format!("{}{name}", declaration.id_prefix))
        .filter(|id| !gaps.contains(id.as_str()))
        .collect()
}

/// Which approval a commit-message line records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrailerKind {
    /// [`GAP_APPROVAL_KEY`]: rows added to the ledger, or changed.
    GapApproval,
    /// [`REQUIRED_REMOVAL_KEY`]: tests that left the suite's required set but not into the ledger.
    RequiredRemoval,
}

/// One approval line of a commit message: its key, URL and test ids.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Trailer {
    pub kind: TrailerKind,
    pub url: String,
    pub ids: Vec<String>,
}

/// The approval lines of a commit message: `KEY URL ID...`, at the start of a line, separated by
/// single spaces, the URL a specific approval ([`is_approval_url`]) and each id a test id. A line
/// that only resembles one is not an approval and is ignored, which leaves its rows unapproved.
pub fn parse_trailers(message: &str) -> Vec<Trailer> {
    message
        .lines()
        .filter_map(|line| {
            let line = line.trim_end();
            let (kind, rest) = if let Some(rest) = line.strip_prefix(GAP_APPROVAL_KEY) {
                (TrailerKind::GapApproval, rest)
            } else {
                (
                    TrailerKind::RequiredRemoval,
                    line.strip_prefix(REQUIRED_REMOVAL_KEY)?,
                )
            };
            let mut words = rest.strip_prefix(' ')?.split(' ');
            let url = words.next()?;
            let ids = words.map(str::to_string).collect::<Vec<_>>();
            (is_approval_url(url) && !ids.is_empty() && ids.iter().all(|id| is_test_id(id))).then(
                || Trailer {
                    kind,
                    url: url.to_string(),
                    ids,
                },
            )
        })
        .collect()
}

/// One committed version of a ledger and its suite.
#[derive(Clone, Debug)]
pub struct Version {
    /// Its commit's position in the first-parent history that starts at the ledger's introduction
    /// (position 0). Versions are given in increasing position, the first at 0.
    pub position: usize,
    pub commit: String,
    /// The ledger's rows; empty if the commit has no ledger.
    pub rows: Vec<Row>,
    /// The suite's tests, each with whether it is ignored.
    pub tests: BTreeMap<String, bool>,
}

/// One commit message's approval lines, at its position in the same history.
#[derive(Clone, Debug)]
pub struct CommitTrailers {
    pub position: usize,
    pub commit: String,
    pub trailers: Vec<Trailer>,
}

/// The history gate's verdict for one ledger.
#[derive(Debug, Default)]
pub struct HistoryVerdict {
    pub violations: Vec<String>,
    /// Rows present, unchanged, since the introduction.
    pub baseline_rows: usize,
    /// Rows added or changed later, each with a bound approval.
    pub approved_rows: usize,
    /// Required tests at the last version.
    pub required: usize,
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// Hold a ledger to its history ([module documentation](self)). `versions` are the committed
/// versions at every commit that changed the ledger or the suite, from the introduction (position
/// 0) to the commit judged; `trailers` are the approval lines of every commit of that history.
pub fn judge_history(
    declaration: &Declaration,
    versions: &[Version],
    trailers: &[CommitTrailers],
) -> HistoryVerdict {
    let mut verdict = HistoryVerdict::default();
    let (Some(introduction), Some(head)) = (versions.first(), versions.last()) else {
        verdict.violations.push(format!(
            "{}: no committed version to judge",
            declaration.ledger
        ));
        return verdict;
    };
    if introduction.position != 0
        || versions
            .windows(2)
            .any(|pair| pair[0].position >= pair[1].position)
    {
        verdict.violations.push(format!(
            "{}: the versions do not start at the introduction and advance; the history walk is inconsistent",
            declaration.ledger
        ));
        return verdict;
    }
    let bound = |kind: TrailerKind, url: Option<&str>, id: &str, from: usize| {
        trailers
            .iter()
            .filter(|commit| commit.position >= from)
            .flat_map(|commit| &commit.trailers)
            .any(|trailer| {
                trailer.kind == kind
                    && url.is_none_or(|url| trailer.url == url)
                    && trailer.ids.iter().any(|named| named == id)
            })
    };
    for row in &head.rows {
        let scope = row.scope();
        let mut start = versions.len() - 1;
        while start > 0
            && versions[start - 1]
                .rows
                .iter()
                .any(|earlier| earlier.scope() == scope)
        {
            start -= 1;
        }
        let entered = &versions[start];
        match &row.approval {
            Approval::Baseline if start == 0 => verdict.baseline_rows += 1,
            Approval::Baseline => verdict.violations.push(format!(
                "{}: its approval field says {BASELINE_APPROVAL:?}, but this row (outcome {}, signature {:?}) entered the ledger at {}, after its introduction at {}. Only a row present, unchanged, since the introduction is a baseline row. A row added or changed later needs, in its approval field, the full URL of a specific approving comment, review or commit, and a commit-message line binding that URL to it: `{GAP_APPROVAL_KEY} <URL> {}`",
                row.id,
                row.outcome.value(),
                row.signature.as_deref().unwrap_or(NO_SIGNATURE),
                short(&entered.commit),
                short(&introduction.commit),
                row.id
            )),
            Approval::Url(url) => {
                if bound(TrailerKind::GapApproval, Some(url), &row.id, entered.position) {
                    verdict.approved_rows += 1;
                } else {
                    verdict.violations.push(format!(
                        "{}: its approval {url} is bound to it by no commit message from {}, where the row entered the ledger, onward. Add the line `{GAP_APPROVAL_KEY} {url} {}` to the commit message that adds the row (or to a later one)",
                        row.id,
                        short(&entered.commit),
                        row.id
                    ));
                }
            }
        }
    }
    // A test that left the required set, and is neither required again nor a row now (a row's own
    // approval covers a demotion above).
    let mut departed = BTreeMap::new();
    for index in 1..versions.len() {
        let before = required(
            declaration,
            &versions[index - 1].rows,
            &versions[index - 1].tests,
        );
        let after = required(declaration, &versions[index].rows, &versions[index].tests);
        for id in before.difference(&after) {
            departed.insert(id.clone(), index);
        }
        for id in after.difference(&before) {
            departed.remove(id);
        }
    }
    let rows_now = head
        .rows
        .iter()
        .map(|row| row.id.as_str())
        .collect::<BTreeSet<_>>();
    for (id, index) in &departed {
        if rows_now.contains(id.as_str()) {
            continue;
        }
        let left = &versions[*index];
        if !bound(TrailerKind::RequiredRemoval, None, id, left.position) {
            verdict.violations.push(format!(
                "{id}: a required test left the suite's required set at {} (deleted, renamed or #[ignore]d) with no approval. A required test leaves only with the full URL of a specific approval bound by a commit-message line `{REQUIRED_REMOVAL_KEY} <URL> {id}`",
                short(&left.commit)
            ));
        }
    }
    verdict.required = required(declaration, &head.rows, &head.tests).len();
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECLARATION: Declaration = Declaration {
        node: "privileged-test.example_kvm",
        ledger: "suite.kvm-gaps.tsv",
        suite_source: "suite.rs",
        id_prefix: "pkg::suite$",
        backend: "kvm",
        backend_proof_test: "proof",
    };
    const ISSUE: &str = "https://github.com/rrnewton/hermit/issues/4954";
    const APPROVAL: &str = "https://github.com/rrnewton/hermit/issues/4954#issuecomment-3456789012";

    fn ledger(rows: &[&str]) -> String {
        let mut text = format!("# a comment\n{HEADER}\n");
        for row in rows {
            text.push_str(row);
            text.push('\n');
        }
        text
    }

    fn failed(test: &str, approval: &str) -> String {
        format!("pkg::suite${test}\tT2\t{ISSUE}\tfailed\trefuses --max-timeslice\t{approval}")
    }

    fn tests(names: &[&str]) -> BTreeMap<String, bool> {
        names.iter().map(|name| (name.to_string(), false)).collect()
    }

    fn version(position: usize, rows: &[String], names: &[&str]) -> Version {
        let text = ledger(&rows.iter().map(String::as_str).collect::<Vec<_>>());
        Version {
            position,
            commit: format!("{position:040x}"),
            rows: parse(&text).unwrap(),
            tests: tests(names),
        }
    }

    fn trailers(position: usize, message: &str) -> CommitTrailers {
        CommitTrailers {
            position,
            commit: format!("{position:040x}"),
            trailers: parse_trailers(message),
        }
    }

    const SUITE: &[&str] = &["a", "b", "c", "g", "proof"];

    fn skipped(test: &str, approval: &str) -> String {
        format!("pkg::suite${test}\tT2\t{ISSUE}\tskipped\t-\t{approval}")
    }

    #[test]
    fn a_row_parses_with_every_field() {
        let rows = parse(&ledger(&[
            &failed("a", "baseline"),
            &format!("pkg::suite$b\tT2\t{ISSUE}\tcpu_timeout\t-\t{APPROVAL}"),
            &skipped("c", APPROVAL),
        ]))
        .unwrap();
        assert_eq!(rows[2].outcome, Outcome::Skipped);
        assert_eq!(rows[2].signature, None);
        assert!(!rows[2].outcome.is_run());
        assert!(rows[0].outcome.is_run() && rows[1].outcome.is_run());
        assert_eq!(
            rows[0],
            Row {
                id: "pkg::suite$a".into(),
                gap: "T2".into(),
                issue: ISSUE.into(),
                outcome: Outcome::Failed,
                signature: Some("refuses --max-timeslice".into()),
                approval: Approval::Baseline,
            }
        );
        assert_eq!(rows[1].outcome, Outcome::CpuTimeout);
        assert_eq!(rows[1].signature, None);
        assert_eq!(rows[1].approval, Approval::Url(APPROVAL.into()));
        assert_eq!(rows[1].test(DECLARATION.id_prefix), Some("b"));
    }

    #[test]
    fn malformed_ledgers_are_refused() {
        let cases: &[(String, &str)] = &[
            ("id\tgap\tissue\tapproval\n".into(), "expected the header"),
            (
                ledger(&["pkg::suite$a\tT2\tbaseline"]),
                "six TAB-separated fields",
            ),
            (
                ledger(&[&failed("a", "baseline"), ""]),
                "six TAB-separated fields",
            ),
            (
                ledger(&[&format!("a\tT2\t{ISSUE}\tfailed\tx\tbaseline")]),
                "not a structured-result test id",
            ),
            (
                ledger(&["pkg::suite$a\tT2\t-\tfailed\tx\tbaseline"]),
                "full URL",
            ),
            (
                ledger(&[&format!("pkg::suite$a\tT2\t{ISSUE}\thang\tx\tbaseline")]),
                "failed, cpu_timeout or skipped",
            ),
            (
                ledger(&[&format!(
                    "pkg::suite$a\tT2\t{ISSUE}\tcpu_timeout\tx\tbaseline"
                )]),
                "its signature is \"-\"",
            ),
            (
                ledger(&[&format!("pkg::suite$a\tT2\t{ISSUE}\tskipped\tx\tbaseline")]),
                "a skipped row has no output to match",
            ),
            (
                ledger(&[&format!("pkg::suite$a\tT2\t{ISSUE}\tfailed\t-\tbaseline")]),
                "cannot be \"-\"",
            ),
            (
                ledger(&[&format!("pkg::suite$a\tT2\t{ISSUE}\tfailed\t x\tbaseline")]),
                "without padding",
            ),
            (ledger(&[&failed("a", ISSUE)]), "specific approving"),
            (
                ledger(&[&failed("b", "baseline"), &failed("a", "baseline")]),
                "sorted",
            ),
            (
                ledger(&[&failed("a", "baseline"), &failed("a", "baseline")]),
                "listed twice",
            ),
            (
                ledger(&[&failed("a", "baseline")]).replace('\n', "\r\n"),
                "CR",
            ),
            (HEADER.to_string(), "newline"),
        ];
        for (text, expected) in cases {
            let error = parse(text).unwrap_err();
            assert!(
                error.contains(expected),
                "{expected:?} not in {error:?} for {text:?}"
            );
        }
    }

    #[test]
    fn only_a_specific_artifact_is_an_approval() {
        for url in [
            "https://github.com/rrnewton/hermit/issues/4954#issuecomment-3456789012",
            "https://github.com/rrnewton/hermit/pull/4990#issuecomment-3456789012",
            "https://github.com/rrnewton/hermit/pull/4990#pullrequestreview-3456789012",
            "https://github.com/rrnewton/hermit/pull/4990#discussion_r2456789012",
            "https://github.com/rrnewton/hermit/commit/a9ab15ccc8a7bcf5b03e212484cf81f466c2718d",
        ] {
            assert!(is_approval_url(url), "{url}");
        }
        for url in [
            ISSUE,
            "https://github.com/rrnewton/hermit/pull/4990",
            "https://github.com/rrnewton/hermit/issues/4954#pullrequestreview-1",
            "https://github.com/rrnewton/hermit/issues/4954#issuecomment-",
            "https://github.com/rrnewton/hermit/issues/4954#issuecomment-01",
            "https://github.com/rrnewton/hermit/commit/a9ab15cc",
            "https://github.com/rrnewton/hermit/commit/A9AB15CCC8A7BCF5B03E212484CF81F466C2718D",
            "http://github.com/rrnewton/hermit/issues/4954#issuecomment-3456789012",
            "baseline",
        ] {
            assert!(!is_approval_url(url), "{url}");
        }
    }

    #[test]
    fn the_static_rules_name_tests_of_the_suite() {
        let rows = parse(&ledger(&[
            "other::suite$b\tT2\thttps://github.com/rrnewton/hermit/issues/1\tfailed\tx\tbaseline",
            &failed("a", "baseline"),
            &failed("gone", "baseline"),
            &failed("proof", "baseline"),
        ]))
        .unwrap();
        let mut names = tests(&["a", "proof", "skipped"]);
        names.insert("skipped".into(), true);
        let errors = check(&DECLARATION, &rows, &names).join("\n");
        assert!(errors.contains("declares no test gone"), "{errors}");
        assert!(
            errors.contains("backend-proof test cannot be a gap row"),
            "{errors}"
        );
        assert!(
            errors.contains("every id of this ledger starts with"),
            "{errors}"
        );
        let ignored = parse(&ledger(&[&failed("skipped", "baseline")])).unwrap();
        assert!(check(&DECLARATION, &ignored, &names)[0].contains("#[ignore]d"));
        assert!(check(&DECLARATION, &[], &tests(&["a", "proof"])).is_empty());
    }

    #[test]
    fn trailers_are_read_only_from_exact_lines() {
        let message = format!(
            "Subject\n\n{GAP_APPROVAL_KEY} {APPROVAL} pkg::suite$a pkg::suite$b\n  {GAP_APPROVAL_KEY} {APPROVAL} pkg::suite$c\n{GAP_APPROVAL_KEY} {ISSUE} pkg::suite$d\n{GAP_APPROVAL_KEY} <URL> <id>\n{REQUIRED_REMOVAL_KEY} {APPROVAL} pkg::suite$e\n"
        );
        assert_eq!(
            parse_trailers(&message),
            vec![
                Trailer {
                    kind: TrailerKind::GapApproval,
                    url: APPROVAL.into(),
                    ids: vec!["pkg::suite$a".into(), "pkg::suite$b".into()],
                },
                Trailer {
                    kind: TrailerKind::RequiredRemoval,
                    url: APPROVAL.into(),
                    ids: vec!["pkg::suite$e".into()],
                },
            ]
        );
    }

    #[test]
    fn the_introduction_is_the_baseline() {
        let versions = [version(
            0,
            &[failed("a", "baseline"), failed("g", "baseline")],
            SUITE,
        )];
        let verdict = judge_history(&DECLARATION, &versions, &[]);
        assert!(verdict.violations.is_empty(), "{:?}", verdict.violations);
        assert_eq!(
            (
                verdict.baseline_rows,
                verdict.approved_rows,
                verdict.required
            ),
            (2, 0, 3)
        );
    }

    #[test]
    fn a_promotion_needs_nothing() {
        let versions = [
            version(
                0,
                &[failed("a", "baseline"), failed("g", "baseline")],
                SUITE,
            ),
            version(7, &[failed("a", "baseline")], SUITE),
        ];
        let verdict = judge_history(&DECLARATION, &versions, &[]);
        assert!(verdict.violations.is_empty(), "{:?}", verdict.violations);
        assert_eq!(verdict.required, 4);
    }

    /// The swap the review demonstrated: drop the G row, add a required test as "baseline", the
    /// count unchanged. Refused, whatever else changed with it.
    #[test]
    fn a_swap_is_an_addition_and_is_refused() {
        let versions = [
            version(
                0,
                &[failed("a", "baseline"), failed("g", "baseline")],
                SUITE,
            ),
            version(
                3,
                &[failed("a", "baseline"), failed("b", "baseline")],
                SUITE,
            ),
        ];
        let verdict = judge_history(&DECLARATION, &versions, &[]);
        assert_eq!(verdict.violations.len(), 1, "{:?}", verdict.violations);
        assert!(
            verdict.violations[0].starts_with("pkg::suite$b: its approval field says \"baseline\"")
        );
    }

    /// A promoted row cannot come back as "baseline".
    #[test]
    fn a_removed_baseline_row_cannot_return_as_baseline() {
        let versions = [
            version(0, &[failed("a", "baseline")], SUITE),
            version(2, &[], SUITE),
            version(5, &[failed("a", "baseline")], SUITE),
        ];
        let verdict = judge_history(&DECLARATION, &versions, &[]);
        assert_eq!(verdict.violations.len(), 1, "{:?}", verdict.violations);
        assert!(verdict.violations[0].contains("entered the ledger at"));
    }

    /// A baseline row whose signature is widened is a new row.
    #[test]
    fn a_changed_scope_is_an_addition() {
        let widened = format!("pkg::suite$a\tT2\t{ISSUE}\tfailed\tguest failed\tbaseline");
        let versions = [
            version(0, &[failed("a", "baseline")], SUITE),
            version(4, &[widened], SUITE),
        ];
        assert_eq!(
            judge_history(&DECLARATION, &versions, &[]).violations.len(),
            1
        );
        // A new issue URL or gap code changes nothing it excuses.
        let retracked = "pkg::suite$a\tT2b\thttps://github.com/rrnewton/hermit/issues/5000\tfailed\trefuses --max-timeslice\tbaseline".to_string();
        let versions = [
            version(0, &[failed("a", "baseline")], SUITE),
            version(4, &[retracked], SUITE),
        ];
        assert!(
            judge_history(&DECLARATION, &versions, &[])
                .violations
                .is_empty()
        );
    }

    /// Skipping a known gap changes what its row excuses (the test is no longer run at all), so
    /// it is an addition: refused as "baseline" or unbound, admitted with a bound approval. The
    /// test stays out of the required set throughout, so no removal approval is owed. Deleting
    /// the skipped row later needs nothing, and makes the test required again.
    #[test]
    fn a_skip_is_an_addition_and_its_deletion_a_promotion() {
        let initial = version(
            0,
            &[failed("a", "baseline"), failed("g", "baseline")],
            SUITE,
        );
        let as_baseline = [
            initial.clone(),
            version(
                4,
                &[failed("a", "baseline"), skipped("g", "baseline")],
                SUITE,
            ),
        ];
        let verdict = judge_history(&DECLARATION, &as_baseline, &[]);
        assert_eq!(verdict.violations.len(), 1, "{:?}", verdict.violations);
        assert!(
            verdict.violations[0].starts_with("pkg::suite$g: its approval field says \"baseline\""),
            "{:?}",
            verdict.violations
        );
        let approved = [
            initial.clone(),
            version(4, &[failed("a", "baseline"), skipped("g", APPROVAL)], SUITE),
        ];
        let unbound = judge_history(&DECLARATION, &approved, &[]);
        assert_eq!(unbound.violations.len(), 1, "{:?}", unbound.violations);
        assert!(unbound.violations[0].contains("is bound to it by no commit message"));
        let bound = [trailers(
            4,
            &format!("{GAP_APPROVAL_KEY} {APPROVAL} pkg::suite$g"),
        )];
        let verdict = judge_history(&DECLARATION, &approved, &bound);
        assert!(verdict.violations.is_empty(), "{:?}", verdict.violations);
        assert_eq!(
            (
                verdict.baseline_rows,
                verdict.approved_rows,
                verdict.required
            ),
            (1, 1, 3)
        );
        // Deleting the skipped row promotes its test, with no approval.
        let promoted = [
            initial,
            version(4, &[failed("a", "baseline"), skipped("g", APPROVAL)], SUITE),
            version(9, &[failed("a", "baseline")], SUITE),
        ];
        let verdict = judge_history(&DECLARATION, &promoted, &bound);
        assert!(verdict.violations.is_empty(), "{:?}", verdict.violations);
        assert_eq!(verdict.required, 4);
    }

    /// The review's made-up approval: a URL in the row and nothing binding it.
    #[test]
    fn an_unbound_approval_is_refused() {
        let fake = "https://github.com/rrnewton/hermit/issues/4954#issuecomment-1";
        let versions = [
            version(0, &[failed("a", "baseline")], SUITE),
            version(3, &[failed("a", "baseline"), failed("b", fake)], SUITE),
        ];
        let verdict = judge_history(&DECLARATION, &versions, &[]);
        assert_eq!(verdict.violations.len(), 1, "{:?}", verdict.violations);
        assert!(verdict.violations[0].contains("is bound to it by no commit message"));
        // Bound to another row, or by a commit before the row entered, it is still refused.
        let elsewhere = [
            trailers(3, &format!("{GAP_APPROVAL_KEY} {fake} pkg::suite$c")),
            trailers(2, &format!("{GAP_APPROVAL_KEY} {fake} pkg::suite$b")),
        ];
        assert_eq!(
            judge_history(&DECLARATION, &versions, &elsewhere)
                .violations
                .len(),
            1
        );
    }

    /// A bound approval is admitted, in the adding commit or a later one. Whether the URL is
    /// really the coordinator's approval is not visible offline: that part stays with review.
    #[test]
    fn a_bound_approval_is_admitted() {
        let versions = [
            version(0, &[failed("a", "baseline")], SUITE),
            version(3, &[failed("a", "baseline"), failed("b", APPROVAL)], SUITE),
        ];
        for position in [3, 9] {
            let bound = [trailers(
                position,
                &format!("{GAP_APPROVAL_KEY} {APPROVAL} pkg::suite$b"),
            )];
            let verdict = judge_history(&DECLARATION, &versions, &bound);
            assert!(verdict.violations.is_empty(), "{:?}", verdict.violations);
            assert_eq!((verdict.baseline_rows, verdict.approved_rows), (1, 1));
        }
    }

    #[test]
    fn a_required_test_leaves_only_with_approval() {
        let versions = [
            version(0, &[failed("a", "baseline")], SUITE),
            version(6, &[failed("a", "baseline")], &["a", "c", "g", "proof"]),
        ];
        let verdict = judge_history(&DECLARATION, &versions, &[]);
        assert_eq!(verdict.violations.len(), 1, "{:?}", verdict.violations);
        assert!(verdict.violations[0].starts_with("pkg::suite$b: a required test left"));
        let approved = [trailers(
            6,
            &format!("{REQUIRED_REMOVAL_KEY} {APPROVAL} pkg::suite$b"),
        )];
        assert!(
            judge_history(&DECLARATION, &versions, &approved)
                .violations
                .is_empty()
        );
        // #[ignore] is leaving too.
        let mut ignored = tests(SUITE);
        ignored.insert("b".into(), true);
        let versions = [
            version(0, &[failed("a", "baseline")], SUITE),
            Version {
                tests: ignored,
                ..version(6, &[failed("a", "baseline")], SUITE)
            },
        ];
        assert_eq!(
            judge_history(&DECLARATION, &versions, &[]).violations.len(),
            1
        );
        // A test that comes back owes nothing.
        let versions = [
            version(0, &[], SUITE),
            version(2, &[], &["a", "c", "g", "proof"]),
            version(4, &[], SUITE),
        ];
        assert!(
            judge_history(&DECLARATION, &versions, &[])
                .violations
                .is_empty()
        );
    }

    /// A demotion is refused once, as the row it adds, not twice.
    #[test]
    fn a_demotion_is_the_new_rows_violation() {
        let versions = [
            version(0, &[failed("a", "baseline")], SUITE),
            version(
                3,
                &[failed("a", "baseline"), failed("c", "baseline")],
                SUITE,
            ),
        ];
        let verdict = judge_history(&DECLARATION, &versions, &[]);
        assert_eq!(verdict.violations.len(), 1, "{:?}", verdict.violations);
        assert!(verdict.violations[0].starts_with("pkg::suite$c:"));
    }

    #[test]
    fn the_registered_ledger_matches_its_node() {
        let declaration = declaration_for_node("privileged-test.deadline_slice_cap_kvm").unwrap();
        assert_eq!(
            declaration_for_ledger(declaration.ledger),
            Some(declaration)
        );
        assert!(required_tests(declaration).is_some_and(|count| count > 0));
        assert!(DECLARATIONS.iter().all(|declaration| {
            declaration.id_prefix.ends_with('$')
                && is_test_id(&format!(
                    "{}{}",
                    declaration.id_prefix, declaration.backend_proof_test
                ))
        }));
        // Every declaration is a gap-ledger producer of the DAG and every producer has one; the
        // generator checks the second half for each node it materializes.
        assert_eq!(
            DECLARATIONS
                .iter()
                .map(|declaration| declaration.node)
                .collect::<BTreeSet<_>>(),
            crate::validation_dag_static::KVM_GAP_LEDGER_RESULT_PRODUCERS
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
        );
    }

    /// The suite source of a few tests, with `extra` appended.
    fn suite_source(extra: &str) -> String {
        format!(
            "/*\n * a licence\n */\n#[path = \"common/x.rs\"]\nmod common;\n\n/// A doc comment.\n#[test]\nfn a() {{}}\n\n#[test]\n// a comment\nfn b() {{\n    let _ = format!(\"#[test] {{}}\", 1);\n}}\n{extra}"
        )
    }

    #[test]
    fn the_parser_reads_every_supported_test_form() {
        let tests = suite_tests(&suite_source(concat!(
            "#[ignore]\n#[test]\nfn ignored_first() {}\n",
            "#[test]\n#[ignore = \"needs the timer\"]\nfn ignored_after() {}\n",
            "#[test]\n#[should_panic(expected = \"boom\")]\nfn panics() {}\n",
            "#[should_panic]\n#[allow(clippy::needless_return)]\n#[test]\npub fn public() {}\n",
            "#[test]\n#[should_panic(\n    expected = \"a long message\"\n)]\nfn wrapped() {}\n",
            "#[derive(Debug)]\nstruct NotATest;\n",
            "fn helper() {}\n",
        )))
        .unwrap();
        let expect = [
            ("a", false),
            ("b", false),
            ("ignored_after", true),
            ("ignored_first", true),
            ("panics", false),
            ("public", false),
            ("wrapped", false),
        ];
        assert_eq!(
            tests,
            expect
                .iter()
                .map(|(name, ignored)| (name.to_string(), *ignored))
                .collect::<BTreeMap<_, _>>()
        );
    }

    /// Every form that could add, remove or ignore a test unseen is refused.
    #[test]
    fn the_parser_refuses_what_it_does_not_understand() {
        let cases = [
            (
                "#[cfg_attr(all(), test)]\nfn replacement() {}\n",
                "could change which tests run",
            ),
            (
                "#[test]\n#[cfg_attr(target_os = \"linux\", ignore)]\nfn conditional() {}\n",
                "could change which tests run",
            ),
            (
                "#[cfg(target_arch = \"x86_64\")]\n#[test]\nfn arch_only() {}\n",
                "could change which tests run",
            ),
            (
                "#[tokio::test]\nasync fn asynchronous() {}\n",
                "could change which tests run",
            ),
            (
                "#[test_case(1)]\nfn case() {}\n",
                "could change which tests run",
            ),
            (
                "#[rstest]\nfn fixture() {}\n",
                "could change which tests run",
            ),
            ("#[serial]\n#[test]\nfn serialized() {}\n", "does not know"),
            ("#[my_harness]\nfn generated() {}\n", "does not know"),
            (
                "mod nested {\n    #[test]\n    fn inner() {}\n}\n",
                "indented attribute",
            ),
            (
                "macro_rules! make {\n    () => {\n        #[test]\n        fn made() {}\n    };\n}\n",
                "indented attribute",
            ),
            ("#![cfg(test)]\n", "inner attribute"),
            (
                "#[ignore]\nfn not_a_test() {}\n",
                "#[ignore] without #[test]",
            ),
            ("#[test]\nstruct NotAFunction;\n", "not on a top-level fn"),
            ("#[test]\n#[test]\nfn twice() {}\n", "a second #[test]"),
            ("#[test]\nfn a() {}\n", "declares a twice"),
            ("#[test] fn same_line() {}\n", "shares a line"),
            ("#[test]\n", "ends the file"),
            (
                "#[test]\n#[ignore = \"unterminated]\nfn broken() {}\n",
                "never closed",
            ),
        ];
        for (extra, expected) in cases {
            let error = suite_tests(&suite_source(extra)).unwrap_err();
            assert!(
                error.contains(expected),
                "{expected:?} not in {error:?} for {extra:?}"
            );
        }
        // A lint-only inner attribute is fine.
        assert!(suite_tests(&format!("#![allow(dead_code)]\n{}", suite_source(""))).is_ok());
    }

    /// The review's balanced substitution, as a suite edit: the parser refuses the cfg_attr
    /// replacement outright, and without it the ignored test is a departure from the required set.
    #[test]
    fn the_balanced_substitution_is_refused() {
        let substituted = suite_source(concat!(
            "#[ignore]\n#[test]\nfn proof() { /* existing assertions */ }\n\n",
            "#[cfg_attr(all(), test)]\nfn replacement() {}\n",
        ));
        let error = suite_tests(&substituted).unwrap_err();
        assert!(error.contains("#[cfg_attr(all(),test)]"), "{error}");
        // The same with a plain #[test] replacement: the count is kept, but the history refuses
        // the ignored test as leaving the required set, and the static rules the proof test.
        let plain = suite_source(concat!(
            "#[ignore]\n#[test]\nfn proof() {}\n\n",
            "#[test]\nfn replacement() {}\n",
        ));
        let tests = suite_tests(&plain).unwrap();
        assert_eq!(tests.get("proof"), Some(&true));
        assert_eq!(tests.get("replacement"), Some(&false));
        let before = suite_tests(&suite_source("#[test]\nfn proof() {}\n")).unwrap();
        let versions = [
            Version {
                position: 0,
                commit: format!("{:040x}", 0),
                rows: Vec::new(),
                tests: before.clone(),
            },
            Version {
                position: 4,
                commit: format!("{:040x}", 4),
                rows: Vec::new(),
                tests: tests.clone(),
            },
        ];
        let declaration = Declaration {
            id_prefix: "pkg::suite$",
            ..DECLARATION
        };
        let verdict = judge_history(&declaration, &versions, &[]);
        assert_eq!(verdict.required, before.len());
        assert_eq!(verdict.violations.len(), 1, "{:?}", verdict.violations);
        assert!(verdict.violations[0].starts_with("pkg::suite$proof: a required test left"));
        assert!(
            check(&declaration, &[], &tests)
                .iter()
                .any(|error| error.contains("backend-proof test is #[ignore]d"))
        );
    }

    #[test]
    fn signatures_decode_escapes_and_numbers() {
        assert_eq!(
            signature_pieces(r"a\nb\\c<N>d<N>").unwrap(),
            vec!["a\nb\\c".to_string(), "d".to_string(), String::new()]
        );
        for bad in [r"a\tb", r"a\", "<N>a", "a<N><N>b"] {
            assert!(signature_pieces(bad).is_err(), "{bad}");
        }
        assert!(signature_matches("x=<N> y", "the x=12 y"));
        assert!(!signature_matches("x=<N> y", "the x= y"));
        assert!(!signature_matches("x=<N> y", "the x=1z y"));
        assert!(signature_matches("x=<N>1 y", "x=21 y"));
        assert!(signature_matches(r"one\ntwo", "zero\none\ntwo\n"));
        assert!(!signature_matches(r"one\ntwo", "one two"));
        let rows = parse(&ledger(&[&format!(
            "pkg::suite$a\tD\t{ISSUE}\tfailed\tbad\\tescape\tbaseline"
        )]));
        assert!(rows.unwrap_err().contains("only escapes"));
    }

    /// A signature matches the real failure it was written for, and not nearby failures of the
    /// same test: the D and G signatures of the deadline_slice_cap ledger, as they stood until
    /// those rows were skipped on 2026-11-01, against those gaps' failures on KVM.
    #[test]
    fn the_d_and_g_signatures_match_only_their_gaps() {
        let d = r#""privfd" "5000"\nstatus: exit status: 122\nstdout:\n\nstderr:"#;
        let g = r"\npositive=<N> samples=20 progress=<N>\n\nnote: run with";
        for signature in [d, g] {
            let row = format!(
                "hermit-run::deadline_slice_cap$t\tD\t{ISSUE}\tfailed\t{signature}\tbaseline"
            );
            let parsed = parse(&ledger(&[&row])).unwrap();
            assert_eq!(parsed[0].signature.as_deref(), Some(signature));
        }
        // The failures recorded in the node's event stream: D at bc48233db5 in the pinned root (its failure became the clone refusal, exit 122), G at Hermit 7fc6645c6465.
        let d_failure = "\nthread 'itimer_a_private_descriptor_table_consumer_re_arms' (1270) panicked at hermit-cli/tests/deadline_slice_cap.rs:163:5:\nguest failed: HERMIT_EPOCH=\"2026-01-01T00:00:00Z\" \"timeout\" \"--kill-after=2s\" \"120s\" \"/src/target/validate/hermit\" \"--backend=kvm\" \"run\" \"--base-env=minimal\" \"--strict\" \"--epoch=2026-01-01T00:00:00Z\" \"--mount=type=tmpfs,target=/test\" \"--workdir=/test\" \"--\" \"/src/target/tmp/deadline-slice-cap-1269/itimer_coalesce\" \"privfd\" \"5000\"\nstatus: exit status: 122\nstdout:\n\nstderr:\n2026-11-01T00:59:59.862237Z ERROR detcore::syscalls::helpers: [dtid 3] clone refused by the backend: KVM cannot create a thread with a working directory or descriptor table of its own: CLONE_THREAD without CLONE_FS or CLONE_FILES (flags CloneFlags(CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM), exit signal 0). No child was created.\n";
        let g_failure = "\nthread 'itimer_mixed_masks_keep_the_timer_running' (2952) panicked at hermit-cli/tests/deadline_slice_cap.rs:878:5:\npositive=3 samples=20 progress=19882\n\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";
        assert!(signature_matches(d, d_failure), "{d}");
        assert!(signature_matches(g, g_failure), "{g}");
        // D: exit 122 with the refusal on stderr, another exit status, and output on stdout.
        for different in [
            d_failure.replace("\"privfd\" \"5000\"", "\"privfd2\" \"5000\""),
            d_failure.replace("exit status: 122", "exit status: 1"),
            d_failure.replace("stdout:\n\nstderr:", "stdout:\nhandled=1\n\nstderr:"),
            d_failure.replace("\"privfd\" \"5000\"", "\"private\" \"5000\""),
        ] {
            assert!(!signature_matches(d, &different), "{different}");
        }
        // G: the sample-count assertion, the native assertion, and a guest that failed and printed
        // the same line.
        for different in [
            g_failure.replace("\npositive=3 samples=20", "\nassertion `left == right` failed: positive=3 samples=19"),
            g_failure.replace("\npositive=3", "\nnative: positive=0"),
            g_failure.replace("878:5:\npositive=3 samples=20 progress=19882\n\nnote:", "163:5:\nguest failed: \"x\"\nstatus: exit status: 1\nstdout:\npositive=3 samples=20 progress=19882\n\nstderr:\n\nnote:"),
        ] {
            assert!(!signature_matches(g, &different), "{different}");
        }
    }
}
