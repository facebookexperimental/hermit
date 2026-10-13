//! Compatibility-corpus sections of E2E manifest documents.
//!
//! A corpus is one lane's settings written once and a list of `(label, argv)`
//! rows written once. A manifest document may carry it as a `corpus:` section;
//! [`expand_corpus`] turns every row into an ordinary test recipe before either
//! validator or the runner sees the document, so a corpus row is validated,
//! selected, executed and reported exactly like a hand-written test. The
//! expansion adds no semantics of its own: everything it writes is an existing
//! manifest key.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde_yaml::Mapping;
use serde_yaml::Value;

/// The backends a manifest mode partitions into enabled and disabled.
const BACKENDS: [&str; 6] = ["ptrace", "dbt", "kvm", "sabre", "liteinst", "in-guest-trap"];

/// The modes every test recipe declares, in the order an expanded test lists
/// them.
const MODES: [&str; 5] = ["verify", "naked", "replay", "chaos", "custom"];
/// Every mode but `verify`.
#[cfg(test)]
const NON_VERIFY_MODES: [&str; 4] = ["naked", "replay", "chaos", "custom"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    /// Prefix of every expanded test's description.
    description: String,
    lane: String,
    requires: Vec<String>,
    /// The one backend every row's verify cell runs on.
    backend: String,
    verify: CorpusVerify,
    /// Rows whose measured failure is a non-blocking diagnostic.
    #[serde(default)]
    diagnostic: Option<CorpusRowPolicy>,
    /// Rows whose corpus-backend cell gets a longer budget than `verify`'s,
    /// each with the reason.
    #[serde(default)]
    heavy: Option<CorpusRowPolicy>,
    /// Verify cells on further backends that a focused run type adds to the
    /// rows; the default (full) validation does not select them.
    #[serde(default)]
    focused: Vec<CorpusFocused>,
    /// Verify cells on further backends that the default (full) validation
    /// runs, each on the rows it names, with that backend's own Hermit flags.
    #[serde(default)]
    additional: Vec<CorpusAdditional>,
    /// Enabled cells measured red: each stays enabled with `ci: false` and a
    /// structured `ci_disabled_reason`, so its run type does not require it.
    #[serde(default)]
    unselected: Vec<CorpusUnselected>,
    /// A replay cell on the corpus backend that a focused run type adds to
    /// each row's own test; the default (full) validation does not select
    /// it.
    #[serde(default)]
    replay: Option<CorpusReplay>,
    /// Second tests on the corpus backend that a focused run type adds to the
    /// rows under its own Hermit flags and budget; the default (full)
    /// validation does not select them.
    #[serde(default)]
    variants: Vec<CorpusVariant>,
    rows: Vec<CorpusRow>,
}

/// One focused run type's replay cell (`hermit record start --verify`) on the
/// corpus backend, in the own test of every row except the named ones, so a
/// program's verify and replay cells are two cells of one test. The cell
/// records and replays the row's argv with none of the corpus's verify
/// settings: no Hermit flags or comparator, which the harness accepts only
/// on verify, no environment (it declares an empty one rather than
/// inheriting verify's), and one attempt, as the harness gives every replay
/// cell. It is never a diagnostic, and its budget is its own.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusReplay {
    /// The run type, written as the replay cell's label (runner::cell_labels).
    label: String,
    timeout_seconds: u64,
    cpu_timeout_seconds: u64,
    slow_reason: String,
    /// Rows without a replay cell, grouped by the reason.
    #[serde(default)]
    except: Vec<CorpusRowGroup>,
    /// Replay cells measured red, as in the corpus's own `unselected`.
    #[serde(default)]
    unselected: Vec<CorpusVariantUnselected>,
}

/// One focused run type's own test of each row except the named ones:
/// `<bucket>/<id_prefix><id>`, whose one verify cell runs on the corpus
/// backend. It shares the corpus's environment and comparator; its Hermit
/// flags, budget and single-attempt reason are its own, and it is never a
/// diagnostic.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusVariant {
    /// The run type, written as each variant test's label.
    label: String,
    /// Prepended to the row's test id suffix.
    id_prefix: String,
    /// Prefix of every variant test's description.
    description: String,
    #[serde(default)]
    hermit_args: Vec<String>,
    hermit_args_reason: Option<String>,
    timeout_seconds: u64,
    cpu_timeout_seconds: u64,
    slow_reason: String,
    no_retry_reason: String,
    /// Rows without a variant test, grouped by the reason.
    #[serde(default)]
    except: Vec<CorpusRowGroup>,
    /// Variant cells measured red, as in the corpus's own `unselected`.
    #[serde(default)]
    unselected: Vec<CorpusVariantUnselected>,
}

/// Rows sharing one reason.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusRowGroup {
    reason: String,
    rows: Vec<String>,
}

/// One failure class of a variant's or the replay run type's cells, which
/// all run on the corpus backend.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusVariantUnselected {
    result: crate::ci_selection::CiDisabledResult,
    evidence: String,
    reason: String,
    rows: Vec<String>,
}

/// One failure class: the cells on `backend` of the named rows, with the
/// result class, the evidence (an issue) and the reason every one carries.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusUnselected {
    backend: String,
    result: crate::ci_selection::CiDisabledResult,
    evidence: String,
    reason: String,
    rows: Vec<String>,
}

/// One focused run type's verify cell on one more backend, on every row
/// except the named ones. The cell shares the corpus's verify settings
/// (environment, comparator, single attempt, budget), except `hermit_args`,
/// which belong to the corpus backend alone, and diagnostic status, which no
/// focused cell has.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusFocused {
    /// The run type, written as the cell's label (runner::cell_labels).
    label: String,
    backend: String,
    /// Rows without this cell, each with the reason that becomes the
    /// backend's `backends_disabled` entry on that row.
    #[serde(default)]
    except: BTreeMap<String, String>,
}

/// A verify cell on one more backend, on the named rows only, that the default
/// (full) validation runs: it carries none of the row's run-type labels. The
/// cell shares the corpus's environment, comparator and single-attempt reason,
/// with optional environment additions for individual rows on this backend;
/// its Hermit flags and their reason are its own, it has the global default
/// budget, and it is never a diagnostic. In the expanded test the flags join
/// the verify mode's per-backend `hermit_args`, so the manifest validator's
/// flag allowlist applies to them as to every other cell's, and a row whose
/// flagged cells have different reasons gets a per-backend
/// `hermit_args_reason`, so each cell's relaxation records only its own.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusAdditional {
    backend: String,
    #[serde(default)]
    hermit_args: Vec<String>,
    hermit_args_reason: Option<String>,
    /// The rows with this cell.
    rows: Vec<String>,
    /// Guest variables added only to the named row's cell on this backend.
    /// Keys are row labels in `rows`; common environment names cannot recur.
    #[serde(default)]
    row_env: BTreeMap<String, BTreeMap<String, String>>,
    /// The backend's `backends_disabled` reason on every other row.
    disabled_reason: String,
    /// Rows without this cell whose reason is their own rather than
    /// `disabled_reason`.
    #[serde(default)]
    disabled: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusVerify {
    #[serde(default)]
    hermit_args: Vec<String>,
    hermit_args_reason: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    comparator: Option<String>,
    comparator_reason: Option<String>,
    no_retry_reason: Option<String>,
    timeout_seconds: u64,
    cpu_timeout_seconds: u64,
    slow_reason: String,
}

/// A budget and a reason per named row.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusRowPolicy {
    timeout_seconds: u64,
    cpu_timeout_seconds: u64,
    slow_reason: String,
    rows: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusRow {
    /// The program's name in the corpus; diagnostic policy names rows by it.
    label: String,
    /// The test id's suffix after `<bucket>/`, when the label is not a valid
    /// one (test ids are lowercase letters, digits and `-`, so `g++` needs
    /// one). Defaults to the label.
    #[serde(default)]
    id: Option<String>,
    argv: Vec<String>,
    /// Run types of the row's verify cells (on the corpus backend and on each
    /// focused backend); empty means the default (full) validation. A replay
    /// cell carries only its own run type, and a cell on an additional backend
    /// none: it is always the default (full) validation's.
    #[serde(default)]
    labels: Vec<String>,
}

fn string(value: &str) -> Value {
    Value::String(value.to_string())
}

fn strings(values: &[String]) -> Value {
    Value::Sequence(values.iter().map(|value| string(value)).collect())
}

fn mapping<'a>(entries: impl IntoIterator<Item = (&'a str, Value)>) -> Value {
    let mut out = Mapping::new();
    for (key, value) in entries {
        out.insert(string(key), value);
    }
    Value::Mapping(out)
}

/// Expand a document's `corpus:` section, if it has one, into ordinary `test:`
/// recipes appended after any hand-written ones. A document without the
/// section is returned unchanged.
pub fn expand_corpus(mut document: Value) -> Result<Value, String> {
    let Some(table) = document.as_mapping_mut() else {
        return Ok(document);
    };
    let Some(section) = table.remove(string("corpus")) else {
        return Ok(document);
    };
    let bucket = table
        .get(string("bucket"))
        .and_then(Value::as_str)
        .ok_or("a manifest with a corpus section needs a string bucket")?
        .to_string();
    let corpus: Corpus = serde_yaml::from_value(section)
        .map_err(|error| format!("{bucket}: invalid corpus section: {error}"))?;
    let tests = expand(&bucket, &corpus)?;
    let entry = table
        .entry(string("test"))
        .or_insert_with(|| Value::Sequence(Vec::new()));
    let Value::Sequence(list) = entry else {
        return Err(format!("{bucket}: test must be a list"));
    };
    list.extend(tests);
    Ok(document)
}

fn nonempty(bucket: &str, what: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.trim() != value {
        return Err(format!(
            "{bucket}: corpus {what} must be nonempty and trimmed"
        ));
    }
    Ok(())
}

/// Check one run type's `except` groups and `unselected` classes against the
/// corpus rows, and return the rows it excepts. `what` names a field of the
/// run type in an error, and `missing` is what an excepted row lacks.
fn check_row_groups<'a>(
    bucket: &str,
    what: &dyn Fn(&str) -> String,
    missing: &str,
    labels: &BTreeSet<&str>,
    except: &'a [CorpusRowGroup],
    unselected: &[CorpusVariantUnselected],
) -> Result<BTreeSet<&'a str>, String> {
    let mut excepted = BTreeSet::new();
    for group in except {
        nonempty(bucket, &what("except reason"), &group.reason)?;
        if group.rows.is_empty() {
            return Err(format!(
                "{bucket}: corpus {} names no rows",
                what("except group")
            ));
        }
        for label in &group.rows {
            if !labels.contains(label.as_str()) {
                return Err(format!(
                    "{bucket}: corpus {} names `{label}`, which is no row",
                    what("except")
                ));
            }
            if !excepted.insert(label.as_str()) {
                return Err(format!(
                    "{bucket}: corpus {} names `{label}` twice",
                    what("except")
                ));
            }
        }
    }
    let mut red = BTreeSet::new();
    for class in unselected {
        nonempty(bucket, &what("unselected evidence"), &class.evidence)?;
        nonempty(bucket, &what("unselected reason"), &class.reason)?;
        if class.rows.is_empty() {
            return Err(format!(
                "{bucket}: corpus {} names no rows",
                what("unselected class")
            ));
        }
        for label in &class.rows {
            if !labels.contains(label.as_str()) || excepted.contains(label.as_str()) {
                return Err(format!(
                    "{bucket}: corpus {} names `{label}`, which has no {missing}",
                    what("unselected")
                ));
            }
            if !red.insert(label.as_str()) {
                return Err(format!(
                    "{bucket}: corpus {} names `{label}` twice",
                    what("unselected")
                ));
            }
        }
    }
    Ok(excepted)
}

fn expand(bucket: &str, corpus: &Corpus) -> Result<Vec<Value>, String> {
    if !BACKENDS.contains(&corpus.backend.as_str()) {
        return Err(format!(
            "{bucket}: corpus backend `{}` is not one of {BACKENDS:?}",
            corpus.backend
        ));
    }
    nonempty(bucket, "description", &corpus.description)?;
    let mut labels = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for row in &corpus.rows {
        if !labels.insert(row.label.as_str()) {
            return Err(format!("{bucket}: corpus row `{}` is repeated", row.label));
        }
        if row.id.as_deref() == Some(row.label.as_str()) {
            return Err(format!(
                "{bucket}: corpus row `{}` repeats its label as its id",
                row.label
            ));
        }
        if !ids.insert(row.id.as_deref().unwrap_or(&row.label)) {
            return Err(format!(
                "{bucket}: corpus row `{}` repeats another row's test id",
                row.label
            ));
        }
        if row.argv.is_empty() {
            return Err(format!(
                "{bucket}: corpus row `{}` has an empty argv",
                row.label
            ));
        }
    }
    if labels.is_empty() {
        return Err(format!("{bucket}: corpus has no rows"));
    }
    let policy_rows = |what: &str, rows: &BTreeMap<String, String>| -> Result<(), String> {
        for (label, reason) in rows {
            if !labels.contains(label.as_str()) {
                return Err(format!(
                    "{bucket}: corpus {what} names `{label}`, which is no row"
                ));
            }
            nonempty(bucket, &format!("{what} reason for `{label}`"), reason)?;
        }
        Ok(())
    };
    if let Some(diagnostic) = &corpus.diagnostic {
        policy_rows("diagnostic", &diagnostic.rows)?;
    }
    if let Some(heavy) = &corpus.heavy {
        policy_rows("heavy", &heavy.rows)?;
        if let Some(both) = heavy.rows.keys().find(|label| {
            corpus
                .diagnostic
                .as_ref()
                .is_some_and(|diagnostic| diagnostic.rows.contains_key(*label))
        }) {
            return Err(format!(
                "{bucket}: corpus row `{both}` is both diagnostic and heavy"
            ));
        }
    }
    let mut focused_backends = BTreeSet::new();
    for focused in &corpus.focused {
        if focused.backend == corpus.backend || !BACKENDS.contains(&focused.backend.as_str()) {
            return Err(format!(
                "{bucket}: corpus focused backend `{}` must be one of {BACKENDS:?} other than `{}`",
                focused.backend, corpus.backend
            ));
        }
        if !focused_backends.insert(focused.backend.as_str()) {
            return Err(format!(
                "{bucket}: corpus focused backend `{}` is repeated",
                focused.backend
            ));
        }
        nonempty(bucket, "focused label", &focused.label)?;
        policy_rows(
            &format!("focused {} except", focused.backend),
            &focused.except,
        )?;
    }
    // The manifest validator refuses a mode whose flags and reason disagree;
    // with additional cells the lane's half of the mode could hide behind
    // theirs, so the lane's pair is checked on its own first.
    if !corpus.additional.is_empty()
        && corpus.verify.hermit_args.is_empty() != corpus.verify.hermit_args_reason.is_none()
    {
        return Err(format!(
            "{bucket}: corpus verify hermit_args and hermit_args_reason must be given together"
        ));
    }
    let mut additional_backends = BTreeSet::new();
    for additional in &corpus.additional {
        let backend = additional.backend.as_str();
        if backend == corpus.backend
            || !BACKENDS.contains(&backend)
            || focused_backends.contains(backend)
        {
            return Err(format!(
                "{bucket}: corpus additional backend `{backend}` must be one of {BACKENDS:?} other than `{}` and every focused backend",
                corpus.backend
            ));
        }
        if !additional_backends.insert(backend) {
            return Err(format!(
                "{bucket}: corpus additional backend `{backend}` is repeated"
            ));
        }
        nonempty(
            bucket,
            &format!("additional {backend} disabled_reason"),
            &additional.disabled_reason,
        )?;
        match (
            additional.hermit_args.is_empty(),
            additional.hermit_args_reason.as_deref(),
        ) {
            (true, None) => {}
            (false, Some(reason)) => nonempty(
                bucket,
                &format!("additional {backend} hermit_args_reason"),
                reason,
            )?,
            (false, None) => {
                return Err(format!(
                    "{bucket}: corpus additional {backend} hermit_args relax determinism and require a hermit_args_reason"
                ));
            }
            (true, Some(_)) => {
                return Err(format!(
                    "{bucket}: corpus additional {backend} hermit_args_reason without hermit_args"
                ));
            }
        }
        if additional.rows.is_empty() {
            return Err(format!(
                "{bucket}: corpus additional {backend} names no rows"
            ));
        }
        let mut with_cell = BTreeSet::new();
        for label in &additional.rows {
            if !labels.contains(label.as_str()) {
                return Err(format!(
                    "{bucket}: corpus additional {backend} names `{label}`, which is no row"
                ));
            }
            if !with_cell.insert(label.as_str()) {
                return Err(format!(
                    "{bucket}: corpus additional {backend} names `{label}` twice"
                ));
            }
        }
        for (label, env) in &additional.row_env {
            if !with_cell.contains(label.as_str()) {
                return Err(format!(
                    "{bucket}: corpus additional {backend} row_env names `{label}`, which has no enabled cell there"
                ));
            }
            if env.is_empty() {
                return Err(format!(
                    "{bucket}: corpus additional {backend} row_env for `{label}` is empty"
                ));
            }
        }
        policy_rows(
            &format!("additional {backend} disabled"),
            &additional.disabled,
        )?;
        if let Some(label) = additional
            .disabled
            .keys()
            .find(|label| with_cell.contains(label.as_str()))
        {
            return Err(format!(
                "{bucket}: corpus additional {backend} names `{label}` both with and without the cell"
            ));
        }
    }
    let mut unselected_cells = BTreeMap::<(&str, &str), &CorpusUnselected>::new();
    for class in &corpus.unselected {
        nonempty(bucket, "unselected evidence", &class.evidence)?;
        nonempty(bucket, "unselected reason", &class.reason)?;
        if class.rows.is_empty() {
            return Err(format!("{bucket}: corpus unselected class names no rows"));
        }
        for label in &class.rows {
            if !labels.contains(label.as_str()) {
                return Err(format!(
                    "{bucket}: corpus unselected names `{label}`, which is no row"
                ));
            }
            let on_backend = class.backend == corpus.backend
                || corpus.focused.iter().any(|focused| {
                    focused.backend == class.backend && !focused.except.contains_key(label)
                })
                || corpus.additional.iter().any(|additional| {
                    additional.backend == class.backend && additional.rows.contains(label)
                });
            if !on_backend {
                return Err(format!(
                    "{bucket}: corpus unselected names `{label}` on `{}`, which has no cell there",
                    class.backend
                ));
            }
            if unselected_cells
                .insert((class.backend.as_str(), label.as_str()), class)
                .is_some()
            {
                return Err(format!(
                    "{bucket}: corpus unselected names `{label}` on `{}` twice",
                    class.backend
                ));
            }
        }
    }
    let replay_excepted = match &corpus.replay {
        Some(replay) => {
            let what = |field: &str| format!("replay {field}");
            nonempty(bucket, &what("label"), &replay.label)?;
            nonempty(bucket, &what("slow_reason"), &replay.slow_reason)?;
            check_row_groups(
                bucket,
                &what,
                "replay cell",
                &labels,
                &replay.except,
                &replay.unselected,
            )?
        }
        None => BTreeSet::new(),
    };
    let mut variant_labels = BTreeSet::new();
    let mut variant_ids = BTreeSet::<String>::new();
    for variant in &corpus.variants {
        nonempty(bucket, "variant label", &variant.label)?;
        if !variant_labels.insert(variant.label.as_str()) {
            return Err(format!(
                "{bucket}: corpus variant `{}` is repeated",
                variant.label
            ));
        }
        let what = |field: &str| format!("variant {} {field}", variant.label);
        nonempty(bucket, &what("id_prefix"), &variant.id_prefix)?;
        nonempty(bucket, &what("description"), &variant.description)?;
        nonempty(bucket, &what("slow_reason"), &variant.slow_reason)?;
        nonempty(bucket, &what("no_retry_reason"), &variant.no_retry_reason)?;
        let excepted = check_row_groups(
            bucket,
            &what,
            "variant test",
            &labels,
            &variant.except,
            &variant.unselected,
        )?;
        for row in corpus
            .rows
            .iter()
            .filter(|row| !excepted.contains(row.label.as_str()))
        {
            let id = format!("{}{}", variant.id_prefix, row_id(row));
            if ids.contains(id.as_str()) || variant_ids.contains(&id) {
                return Err(format!(
                    "{bucket}: corpus {} gives row `{}` the test id `{id}`, which another test has",
                    what("id_prefix"),
                    row.label
                ));
            }
            variant_ids.insert(id);
        }
    }
    let off_reason = format!(
        "A {bucket} corpus row runs only its lane's verify cell on {}",
        corpus.backend
    );
    let replay_unselected = corpus
        .replay
        .iter()
        .flat_map(|replay| &replay.unselected)
        .flat_map(|class| class.rows.iter().map(move |label| (label.as_str(), class)))
        .collect::<BTreeMap<_, _>>();
    let mut tests = Vec::with_capacity(corpus.rows.len());
    for row in &corpus.rows {
        let diagnostic = corpus
            .diagnostic
            .as_ref()
            .and_then(|policy| policy.rows.get(&row.label).map(|reason| (policy, reason)));
        let mut heavy_reason = None;
        let (timeout, cpu_timeout, slow_reason) = match diagnostic {
            Some((policy, _)) => (
                policy.timeout_seconds,
                policy.cpu_timeout_seconds,
                policy.slow_reason.as_str(),
            ),
            None => match corpus
                .heavy
                .as_ref()
                .filter(|heavy| heavy.rows.contains_key(&row.label))
            {
                Some(heavy) => (
                    heavy.timeout_seconds,
                    heavy.cpu_timeout_seconds,
                    heavy_reason
                        .insert(format!("{}: {}", heavy.slow_reason, heavy.rows[&row.label]))
                        .as_str(),
                ),
                None => (
                    corpus.verify.timeout_seconds,
                    corpus.verify.cpu_timeout_seconds,
                    corpus.verify.slow_reason.as_str(),
                ),
            },
        };
        // The corpus backend first, then every additional cell this row has,
        // then every focused cell it keeps.
        let additional = corpus
            .additional
            .iter()
            .filter(|additional| additional.rows.contains(&row.label))
            .collect::<Vec<_>>();
        let focused = corpus
            .focused
            .iter()
            .filter(|focused| !focused.except.contains_key(&row.label))
            .collect::<Vec<_>>();
        let cells = std::iter::once(CorpusCell {
            backend: corpus.backend.as_str(),
            kind: CellKind::Lane,
            budget: Some((timeout, cpu_timeout, slow_reason)),
        })
        .chain(additional.iter().map(|additional| CorpusCell {
            backend: additional.backend.as_str(),
            kind: CellKind::Additional,
            budget: None,
        }))
        .chain(focused.iter().map(|focused| CorpusCell {
            backend: focused.backend.as_str(),
            kind: CellKind::Focused(focused.label.as_str()),
            budget: Some((
                corpus.verify.timeout_seconds,
                corpus.verify.cpu_timeout_seconds,
                corpus.verify.slow_reason.as_str(),
            )),
        }))
        .collect::<Vec<_>>();
        let enabled = cells.iter().map(|cell| cell.backend).collect::<Vec<_>>();
        let replay = corpus
            .replay
            .as_ref()
            .filter(|_| !replay_excepted.contains(row.label.as_str()));
        let names =
            |backends: &mut dyn Iterator<Item = &str>| backends.collect::<Vec<_>>().join(" and ");
        let mut also = Vec::new();
        if !additional.is_empty() {
            let backends = names(&mut additional.iter().map(|cell| cell.backend.as_str()));
            also.push(format!("a verify cell on {backends}"));
        }
        if !focused.is_empty() {
            let backends = names(&mut focused.iter().map(|cell| cell.backend.as_str()));
            also.push(format!("a focused run type's verify cell on {backends}"));
        }
        if replay.is_some() {
            also.push(format!(
                "a focused run type's replay cell on {}",
                corpus.backend
            ));
        }
        let off_reason = match also.as_slice() {
            [] => off_reason.clone(),
            [only] => format!("{off_reason}, and {only}"),
            [init @ .., last] => format!("{off_reason}, {}, and {last}", init.join(", ")),
        };
        let disabled = BACKENDS
            .iter()
            .filter(|backend| !enabled.contains(backend))
            .map(|backend| {
                let focused_reason = corpus
                    .focused
                    .iter()
                    .find(|focused| focused.backend == *backend)
                    .and_then(|focused| focused.except.get(&row.label));
                let additional_reason = corpus
                    .additional
                    .iter()
                    .find(|additional| additional.backend == *backend)
                    .map(|additional| {
                        additional
                            .disabled
                            .get(&row.label)
                            .unwrap_or(&additional.disabled_reason)
                    });
                let reason = focused_reason
                    .or(additional_reason)
                    .map_or(off_reason.as_str(), String::as_str);
                (*backend, string(reason))
            });
        // An additional cell has the global default budget, so it names none.
        let per_cell = |value: &dyn Fn(u64, u64, &str) -> Value| {
            mapping(cells.iter().filter_map(|cell| {
                cell.budget
                    .map(|(timeout, cpu, slow)| (cell.backend, value(timeout, cpu, slow)))
            }))
        };
        let unselected = enabled
            .iter()
            .filter_map(|backend| {
                unselected_cells
                    .get(&(*backend, row.label.as_str()))
                    .map(|class| (*backend, *class))
            })
            .collect::<Vec<_>>();
        let ci = if unselected.is_empty() {
            Value::Bool(true)
        } else {
            mapping(enabled.iter().map(|backend| {
                (
                    *backend,
                    Value::Bool(!unselected.iter().any(|(off, _)| off == backend)),
                )
            }))
        };
        let mut verify = vec![
            ("ci", ci),
            (
                "backends_enabled",
                Value::Sequence(enabled.iter().map(|backend| string(backend)).collect()),
            ),
            ("backends_disabled", mapping(disabled)),
            (
                "timeout_seconds",
                per_cell(&|timeout, _, _| Value::from(timeout)),
            ),
            (
                "cpu_timeout_seconds",
                per_cell(&|_, cpu, _| Value::from(cpu)),
            ),
            ("slow_reason", per_cell(&|_, _, slow| string(slow))),
        ];
        if !unselected.is_empty() {
            verify.push((
                "ci_disabled_reason",
                mapping(unselected.iter().map(|(backend, class)| {
                    (
                        *backend,
                        ci_disabled_reason(class.result, &class.evidence, &class.reason),
                    )
                })),
            ));
        }
        // The row's own run types label each of its verify cells but the
        // additional ones, which are the default (full) validation's, and a
        // focused cell carries its focused run type too. They are cell labels,
        // not test labels, so the row's replay cell carries only its own.
        let verify_labels = cells
            .iter()
            .filter_map(|cell| {
                let mut labels = match cell.kind {
                    CellKind::Additional => return None,
                    CellKind::Lane | CellKind::Focused(_) => row.labels.clone(),
                };
                if let CellKind::Focused(focused) = cell.kind {
                    if !labels.iter().any(|label| label == focused) {
                        labels.push(focused.to_string());
                    }
                }
                (!labels.is_empty()).then(|| (cell.backend, strings(&labels)))
            })
            .collect::<Vec<_>>();
        if !verify_labels.is_empty() {
            verify.push(("labels", mapping(verify_labels)));
        }
        // The lane's flags on the corpus backend, and each additional cell's
        // own on its backend; focused cells take none.
        let flagged = std::iter::once((
            corpus.backend.as_str(),
            &corpus.verify.hermit_args,
            corpus.verify.hermit_args_reason.as_deref(),
        ))
        .chain(additional.iter().map(|additional| {
            (
                additional.backend.as_str(),
                &additional.hermit_args,
                additional.hermit_args_reason.as_deref(),
            )
        }))
        .filter(|(_, args, _)| !args.is_empty())
        .collect::<Vec<_>>();
        if !flagged.is_empty() {
            verify.push((
                "hermit_args",
                mapping(
                    flagged
                        .iter()
                        .map(|(backend, args, _)| (*backend, strings(args))),
                ),
            ));
        }
        if let Some(reason) = hermit_args_reason(
            &flagged,
            corpus.backend.as_str(),
            corpus.verify.hermit_args_reason.as_deref(),
        ) {
            verify.push(("hermit_args_reason", reason));
        }
        push_shared_verify_settings(&mut verify, &corpus.verify);
        let row_env = additional
            .iter()
            .filter_map(|additional| {
                additional.row_env.get(&row.label).map(|env| {
                    (
                        additional.backend.as_str(),
                        mapping(
                            env.iter()
                                .map(|(name, value)| (name.as_str(), string(value))),
                        ),
                    )
                })
            })
            .collect::<Vec<_>>();
        if !row_env.is_empty() {
            verify.push(("env_by_backend", mapping(row_env)));
        }
        if let Some(reason) = &corpus.verify.no_retry_reason {
            verify.push(("no_retry_reason", string(reason)));
        }
        if let Some((_, reason)) = diagnostic {
            verify.push((
                "diagnostic",
                mapping([(corpus.backend.as_str(), string(reason))]),
            ));
        }
        let mut modes = vec![("verify", verify)];
        if let Some(replay) = replay {
            let backend = corpus.backend.as_str();
            let per_cell = |value: Value| mapping([(backend, value)]);
            let class = replay_unselected.get(row.label.as_str());
            let mut recipe = vec![
                (
                    "ci",
                    class.map_or(Value::Bool(true), |_| per_cell(Value::Bool(false))),
                ),
                ("backends_enabled", strings(&[backend.to_string()])),
                (
                    "backends_disabled",
                    mapping(
                        BACKENDS
                            .iter()
                            .filter(|other| **other != backend)
                            .map(|other| (*other, string(&off_reason))),
                    ),
                ),
                (
                    "timeout_seconds",
                    per_cell(Value::from(replay.timeout_seconds)),
                ),
                (
                    "cpu_timeout_seconds",
                    per_cell(Value::from(replay.cpu_timeout_seconds)),
                ),
                ("slow_reason", per_cell(string(&replay.slow_reason))),
            ];
            if let Some(class) = class {
                recipe.push((
                    "ci_disabled_reason",
                    per_cell(ci_disabled_reason(
                        class.result,
                        &class.evidence,
                        &class.reason,
                    )),
                ));
            }
            recipe.push((
                "labels",
                per_cell(strings(std::slice::from_ref(&replay.label))),
            ));
            // Declared empty, so the cell does not inherit the verify cell's
            // environment (runner::cell_mode_env).
            recipe.push(("env", Value::Mapping(Mapping::new())));
            modes.push(("replay", recipe));
        }
        tests.push(test_recipe(
            corpus,
            &format!("{bucket}/{}", row_id(row)),
            &format!("{} `{}`", corpus.description, row.label),
            row,
            modes,
            &off_reason,
            &[],
        ));
    }
    for variant in &corpus.variants {
        let backend = corpus.backend.as_str();
        let excepted = variant
            .except
            .iter()
            .flat_map(|group| &group.rows)
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let unselected = variant
            .unselected
            .iter()
            .flat_map(|class| class.rows.iter().map(move |label| (label.as_str(), class)))
            .collect::<BTreeMap<_, _>>();
        let off_reason = format!(
            "A {bucket} corpus row's {} test runs only its verify cell on {backend}",
            variant.label
        );
        let per_cell = |value: Value| mapping([(backend, value)]);
        for row in corpus
            .rows
            .iter()
            .filter(|row| !excepted.contains(row.label.as_str()))
        {
            let class = unselected.get(row.label.as_str());
            let mut recipe = vec![
                (
                    "ci",
                    class.map_or(Value::Bool(true), |_| per_cell(Value::Bool(false))),
                ),
                ("backends_enabled", strings(&[backend.to_string()])),
                (
                    "backends_disabled",
                    mapping(
                        BACKENDS
                            .iter()
                            .filter(|other| **other != backend)
                            .map(|other| (*other, string(&off_reason))),
                    ),
                ),
                (
                    "timeout_seconds",
                    per_cell(Value::from(variant.timeout_seconds)),
                ),
                (
                    "cpu_timeout_seconds",
                    per_cell(Value::from(variant.cpu_timeout_seconds)),
                ),
                ("slow_reason", per_cell(string(&variant.slow_reason))),
            ];
            if let Some(class) = class {
                recipe.push((
                    "ci_disabled_reason",
                    per_cell(ci_disabled_reason(
                        class.result,
                        &class.evidence,
                        &class.reason,
                    )),
                ));
            }
            if !variant.hermit_args.is_empty() {
                recipe.push(("hermit_args", per_cell(strings(&variant.hermit_args))));
            }
            if let Some(reason) = &variant.hermit_args_reason {
                recipe.push(("hermit_args_reason", string(reason)));
            }
            push_shared_verify_settings(&mut recipe, &corpus.verify);
            recipe.push(("no_retry_reason", string(&variant.no_retry_reason)));
            tests.push(test_recipe(
                corpus,
                &format!("{bucket}/{}{}", variant.id_prefix, row_id(row)),
                &format!("{} `{}`", variant.description, row.label),
                row,
                vec![("verify", recipe)],
                &off_reason,
                std::slice::from_ref(&variant.label),
            ));
        }
    }
    Ok(tests)
}

/// What a row's verify cell is to the corpus.
#[derive(Clone, Copy)]
enum CellKind<'a> {
    /// The lane's cell on the corpus backend.
    Lane,
    /// A cell on an additional backend, the default (full) validation's.
    Additional,
    /// A focused run type's cell, labelled with that run type.
    Focused(&'a str),
}

/// One verify cell of a row: its backend, its kind, and its budget (wall
/// seconds, CPU seconds, reason), or none for the global default.
struct CorpusCell<'a> {
    backend: &'a str,
    kind: CellKind<'a>,
    budget: Option<(u64, u64, &'a str)>,
}

/// The verify mode's `hermit_args_reason` for a row whose cells carry the
/// `flagged` flags (backend, flags, reason). Without an additional cell's flags
/// it is the lane's reason exactly as written; when every flagged cell has the
/// same reason it is that one string; otherwise it maps each flagged backend to
/// its own reason, so every cell's relaxation records only its own.
fn hermit_args_reason(
    flagged: &[(&str, &Vec<String>, Option<&str>)],
    lane_backend: &str,
    lane_reason: Option<&str>,
) -> Option<Value> {
    if flagged.iter().all(|(backend, ..)| *backend == lane_backend) {
        return lane_reason.map(string);
    }
    let reasons = flagged
        .iter()
        .map(|(backend, _, reason)| {
            (
                *backend,
                reason.expect("expand checked that every flag set has its reason"),
            )
        })
        .collect::<Vec<_>>();
    let first = reasons[0].1;
    if reasons.iter().all(|(_, reason)| *reason == first) {
        return Some(string(first));
    }
    Some(mapping(
        reasons
            .into_iter()
            .map(|(backend, reason)| (backend, string(reason))),
    ))
}

/// The test id's suffix after `<bucket>/`.
fn row_id(row: &CorpusRow) -> &str {
    row.id.as_deref().unwrap_or(&row.label)
}

/// One structured `ci_disabled_reason` entry.
fn ci_disabled_reason(
    result: crate::ci_selection::CiDisabledResult,
    evidence: &str,
    reason: &str,
) -> Value {
    mapping([
        (
            "result",
            serde_yaml::to_value(result).expect("a result class serializes"),
        ),
        ("evidence", string(evidence)),
        ("reason", string(reason)),
    ])
}

/// The verify settings every cell of the corpus shares: environment and
/// comparator.
fn push_shared_verify_settings(verify: &mut Vec<(&str, Value)>, settings: &CorpusVerify) {
    if !settings.env.is_empty() {
        verify.push((
            "env",
            mapping(settings.env.iter().map(|(k, v)| (k.as_str(), string(v)))),
        ));
    }
    if let Some(comparator) = &settings.comparator {
        verify.push(("comparator", string(comparator)));
    }
    if let Some(reason) = &settings.comparator_reason {
        verify.push(("comparator_reason", string(reason)));
    }
}

/// One expanded test: the row's argv under its enabled modes, with every other
/// mode off for `off_reason`, all in [`MODES`] order.
fn test_recipe(
    corpus: &Corpus,
    id: &str,
    description: &str,
    row: &CorpusRow,
    mut enabled: Vec<(&str, Vec<(&str, Value)>)>,
    off_reason: &str,
    labels: &[String],
) -> Value {
    let mut modes = Vec::with_capacity(MODES.len());
    for mode in MODES {
        if let Some(index) = enabled.iter().position(|(name, _)| *name == mode) {
            let (_, recipe) = enabled.remove(index);
            modes.push((mode, mapping(recipe)));
            continue;
        }
        let backends_disabled = if mode == "naked" {
            mapping([("native", string(off_reason))])
        } else {
            mapping(
                BACKENDS
                    .iter()
                    .map(|backend| (*backend, string(off_reason))),
            )
        };
        modes.push((
            mode,
            mapping([
                ("ci", Value::Bool(false)),
                ("ci_disabled_reason", string(off_reason)),
                ("backends_enabled", Value::Sequence(Vec::new())),
                ("backends_disabled", backends_disabled),
            ]),
        ));
    }
    assert!(
        enabled.is_empty(),
        "{id}: an expanded test enables a mode outside {MODES:?}"
    );
    let mut test = vec![
        ("id", string(id)),
        ("description", string(description)),
        ("lane", string(&corpus.lane)),
        ("requires", strings(&corpus.requires)),
        ("occasional", Value::Bool(false)),
        ("direct", strings(&row.argv)),
        (
            "observation",
            mapping([
                ("status", Value::Bool(true)),
                ("stdout", Value::Bool(true)),
                ("stderr", Value::Bool(true)),
                ("artifacts", Value::Sequence(Vec::new())),
            ]),
        ),
        ("modes", mapping(modes)),
    ];
    if !labels.is_empty() {
        test.push(("labels", strings(labels)));
    }
    mapping(test)
}

/// The repository root, in a `direct` argv element.
pub const ROOT_DIR_PLACEHOLDER: &str = "{{ROOT_DIR}}";
/// The validation's per-run state directory, in a `direct` argv element: the
/// value of the harness's `VALIDATE_RUN_STATE`. A cell that names it in a run
/// without one is refused rather than run with the literal text.
pub const VALIDATE_RUN_STATE_PLACEHOLDER: &str = "{{VALIDATE_RUN_STATE}}";
const VALIDATE_RUN_STATE_ENV: &str = "VALIDATE_RUN_STATE";
/// The cell's own copy of tests/e2e/xdg-config, in a `direct` argv element:
/// the directory the guest's `XDG_CONFIG_HOME` names. The harness prepares it
/// in the cell directory before the run, and a verify guest on every backend
/// sees it at /tmp/e2e/xdg-config, bound from there (the runner's
/// equalized inputs), so every directory on a path below it is one the cell's
/// own run owns: the guest's private /tmp, the /tmp/e2e directory Hermit
/// creates in it for the binds, and the bound copy. A program that stats every
/// ancestor of its argument (`readlink -f`, `realpath`) therefore reads no
/// directory another cell can change, which a path below
/// {{VALIDATE_RUN_STATE}} cannot promise
/// (<https://github.com/rrnewton/hermit/issues/3975>). Any other mode names the
/// cell directory's copy on the host.
pub const XDG_CONFIG_HOME_PLACEHOLDER: &str = "{{XDG_CONFIG_HOME}}";
const XDG_CONFIG_HOME_ENV: &str = "XDG_CONFIG_HOME";

/// Every placeholder a `direct` argv element may spell.
const DIRECT_PLACEHOLDERS: [&str; 3] = [
    ROOT_DIR_PLACEHOLDER,
    VALIDATE_RUN_STATE_PLACEHOLDER,
    XDG_CONFIG_HOME_PLACEHOLDER,
];

/// Refuse a `direct` argv that spells any `{{...}}` token other than the
/// known placeholders, so a typo cannot reach a guest as literal text.
pub fn check_direct_placeholders(id: &str, argv: &[String]) -> Result<(), String> {
    for arg in argv {
        let mut rest = arg.as_str();
        while let Some(start) = rest.find("{{") {
            let tail = &rest[start..];
            let known = DIRECT_PLACEHOLDERS
                .into_iter()
                .find(|token| tail.starts_with(token));
            let Some(token) = known else {
                return Err(format!(
                    "{id}: direct argv element `{arg}` names an unknown placeholder; only {ROOT_DIR_PLACEHOLDER}, {VALIDATE_RUN_STATE_PLACEHOLDER} and {XDG_CONFIG_HOME_PLACEHOLDER} exist"
                ));
            };
            rest = &tail[token.len()..];
        }
    }
    Ok(())
}

/// Substitute the placeholders in a `direct` argv. `run_state` is the
/// harness's `VALIDATE_RUN_STATE`, when it has one, and `xdg_config_home` the
/// cell's XDG configuration directory, when the caller has a cell.
pub fn resolve_direct_placeholders(
    id: &str,
    argv: &[String],
    root: &std::path::Path,
    run_state: Option<&std::ffi::OsStr>,
    xdg_config_home: Option<&std::path::Path>,
) -> Result<Vec<String>, String> {
    check_direct_placeholders(id, argv)?;
    argv.iter()
        .map(|arg| {
            let mut out = arg.replace(ROOT_DIR_PLACEHOLDER, &root.to_string_lossy());
            if out.contains(VALIDATE_RUN_STATE_PLACEHOLDER) {
                let state = run_state.filter(|state| !state.is_empty()).ok_or_else(|| {
                    format!(
                        "{id}: its argv names {VALIDATE_RUN_STATE_PLACEHOLDER} but {VALIDATE_RUN_STATE_ENV} is not set"
                    )
                })?;
                out = out.replace(VALIDATE_RUN_STATE_PLACEHOLDER, &state.to_string_lossy());
            }
            if out.contains(XDG_CONFIG_HOME_PLACEHOLDER) {
                let xdg = xdg_config_home.ok_or_else(|| {
                    format!(
                        "{id}: its argv names {XDG_CONFIG_HOME_PLACEHOLDER} but this caller has no cell XDG configuration directory"
                    )
                })?;
                out = out.replace(XDG_CONFIG_HOME_PLACEHOLDER, &xdg.to_string_lossy());
            }
            Ok(out)
        })
        .collect()
}

/// The harness's `VALIDATE_RUN_STATE`, for [`resolve_direct_placeholders`].
pub fn validate_run_state() -> Option<std::ffi::OsString> {
    std::env::var_os(VALIDATE_RUN_STATE_ENV)
}

/// Render one `direct` argv element as a shell word for a command line run
/// from the repository root, as the rerunnable command files do: literal text
/// through `quote`, `{{ROOT_DIR}}` as `"$PWD"`, and `{{VALIDATE_RUN_STATE}}`
/// and `{{XDG_CONFIG_HOME}}` as expansions of their variables that refuse to
/// run when the variable is unset, as the harness does.
pub fn direct_shell_word(
    id: &str,
    arg: &str,
    quote: impl Fn(&str) -> String,
) -> Result<String, String> {
    check_direct_placeholders(id, &[arg.to_string()])?;
    let mut out = String::new();
    let mut rest = arg;
    while let Some((index, token)) = DIRECT_PLACEHOLDERS
        .into_iter()
        .filter_map(|token| rest.find(token).map(|index| (index, token)))
        .min()
    {
        if index > 0 {
            out.push_str(&quote(&rest[..index]));
        }
        out.push_str(
            match token {
                ROOT_DIR_PLACEHOLDER => "\"$PWD\"".to_string(),
                VALIDATE_RUN_STATE_PLACEHOLDER => format!(
                    "\"${{{VALIDATE_RUN_STATE_ENV}:?{VALIDATE_RUN_STATE_ENV} is not set}}\""
                ),
                _ => format!("\"${{{XDG_CONFIG_HOME_ENV}:?{XDG_CONFIG_HOME_ENV} is not set}}\""),
            }
            .as_str(),
        );
        rest = &rest[index + token.len()..];
    }
    if !rest.is_empty() || out.is_empty() {
        out.push_str(&quote(rest));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(corpus: &str) -> Value {
        serde_yaml::from_str(&format!("schema: 3\nbucket: fixture\ncorpus:\n{corpus}")).unwrap()
    }

    const CORPUS: &str = r#"
  description: Fixture program
  lane: portable
  requires: [linux]
  backend: ptrace
  verify:
    hermit_args: [--no-virtualize-cpuid]
    hermit_args_reason: fixture configuration
    env: {TMPDIR: /tmp}
    comparator: stripped
    comparator_reason: fixture policy
    no_retry_reason: fixture single run
    timeout_seconds: 60
    cpu_timeout_seconds: 59
    slow_reason: fixture budget
  diagnostic:
    timeout_seconds: 20
    cpu_timeout_seconds: 19
    slow_reason: shortened fixture budget
    rows: {slow: a bounded fixture probe}
  rows:
    - {label: "echo", argv: ["/bin/echo", "x"]}
    - {label: "g++", id: gxx, argv: ["/usr/bin/g++", "--version"]}
    - {label: "slow", argv: ["/bin/true"]}
"#;

    fn tests_of(document: &Value) -> &Vec<Value> {
        document["test"].as_sequence().unwrap()
    }

    /// The shipped compat manifest, read at compile time relative to this
    /// file, because scripts/manifest-to-commands.rs compiles this module
    /// under rust-script, where CARGO_MANIFEST_DIR is not ci/manifest-plan.
    const COMPAT_YAML: &str = include_str!("../../../tests/e2e/manifests/compat.yaml");

    /// The 26 corpus rows the strict variant took on 2026-10-10, when the
    /// compat corpus was given uniform strict treatment: the 23 the strict
    /// corpus never had, and the lua, perl and df `-direct` twins. They are labelled
    /// sabre-compat-only, so their corpus cells are not the full run's and
    /// ci/compat/corpus-strict.json, which validate.rs binds to the full run's
    /// portable rows, does not hold them.
    const STRICT_VARIANT_ROWS_BEYOND_THE_STRICT_CORPUS: [&str; 26] = [
        "basenc",
        "col",
        "colrm",
        "crc32",
        "cscope",
        "df-direct",
        "diff3",
        "dos2unix",
        "envsubst",
        "fallocate",
        "flex",
        "getconf",
        "lua-direct",
        "mountpoint",
        "msgfmt",
        "msgunfmt",
        "namei",
        "pathchk",
        "perl-direct",
        "setfacl",
        "setfattr",
        "shred",
        "sync",
        "timeout",
        "truncate",
        "uuidgen",
    ];

    /// compat.yaml's strict variant runs exactly the programs of
    /// ci/compat/corpus-strict.json, the corpus STRICT_COMPAT_TOTAL counts and
    /// the super suite still reads, plus the named rows beyond it: a row added
    /// to either file, or to the strict variant, without the others fails here
    /// instead of silently changing the strict run type.
    #[test]
    fn the_strict_variant_runs_exactly_the_strict_corpus_programs() {
        let document: Value = serde_yaml::from_str(COMPAT_YAML).unwrap();
        let label_of = document["corpus"]["rows"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|row| {
                let label = row["label"].as_str().unwrap();
                (
                    row["id"].as_str().unwrap_or(label).to_owned(),
                    label.to_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let expanded = expand_corpus(document).unwrap();
        let strict_label = Value::from("strict-compat-only");
        let variant = tests_of(&expanded)
            .iter()
            .filter(|test| {
                test["labels"]
                    .as_sequence()
                    .is_some_and(|labels| labels.contains(&strict_label))
            })
            .map(|test| {
                let id = test["id"].as_str().unwrap();
                let row = id.strip_prefix("compat/strict-").unwrap_or_else(|| {
                    panic!("{id}: a strict-compat-only test outside the strict variant")
                });
                label_of[row].clone()
            })
            .collect::<BTreeSet<_>>();
        let corpus: serde_json::Value =
            serde_json::from_str(include_str!("../../compat/corpus-strict.json")).unwrap();
        let strict = corpus["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["label"].as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(strict.len(), 193);
        let mut expected = strict.clone();
        for row in STRICT_VARIANT_ROWS_BEYOND_THE_STRICT_CORPUS {
            assert!(
                expected.insert(row.to_owned()),
                "{row} is already in the strict corpus"
            );
        }
        assert_eq!(expected.len(), 219);
        assert_eq!(
            variant.symmetric_difference(&expected).collect::<Vec<_>>(),
            Vec::<&String>::new()
        );
    }

    /// compat.yaml's replay cells cover every row but the lua, perl and df
    /// `-direct` twins and netlink-sock-diag, whose own check asserts run-mode
    /// metadata virtualization (https://github.com/rrnewton/hermit/issues/3965):
    /// the retired rr lane's 139 programs (the count its RR_COMPAT_EXPECTED
    /// guard held before ci/compat/corpus-rr.json was retired) and, since a
    /// three-run survey on 2026-10-09, 76 of the 77 rows the rr lane never
    /// listed as passing. Each is the replay cell of its row's own test,
    /// carrying the rr run type alone and none of the verify cell's settings.
    /// 214 are selected, and the 1 the survey saw fail stays enabled with
    /// `ci: false` and the issue of their failure (`UNSELECTED_REPLAY`).
    /// A row moved into or out of the run type, a second test for a program,
    /// the rr run type reaching a verify cell, or a cell quietly unselected or
    /// reselected, fails here.
    #[test]
    fn the_replay_cells_keep_the_rr_lane_programs_and_gate_only_those_that_replay() {
        let expanded = expand_corpus(serde_yaml::from_str(COMPAT_YAML).unwrap()).unwrap();
        let rr_label = Value::from("rr-compat-only");
        let seq = |text: &str| serde_yaml::from_str::<Value>(text).unwrap();
        let carries_rr = |labels: &Value| {
            labels
                .as_sequence()
                .is_some_and(|labels| labels.contains(&rr_label))
        };
        let (mut gated, mut refused) = (0, 0);
        for test in tests_of(&expanded) {
            let id = test["id"].as_str().unwrap();
            assert!(!id.starts_with("compat/rr-"), "{id}");
            assert!(!carries_rr(&test["labels"]), "{id}");
            if let Some(labels) = test["modes"]["verify"]["labels"].as_mapping() {
                assert!(!labels.values().any(carries_rr), "{id}");
            }
            let replay = &test["modes"]["replay"];
            if replay["backends_enabled"] == seq("[]") {
                assert!(replay.get("labels").is_none(), "{id}");
                continue;
            }
            assert_eq!(replay["backends_enabled"], seq("[ptrace]"), "{id}");
            assert_eq!(replay["labels"], seq("{ptrace: [rr-compat-only]}"), "{id}");
            assert_eq!(replay["env"], seq("{}"), "{id}");
            assert_eq!(replay["timeout_seconds"], seq("{ptrace: 60}"), "{id}");
            for absent in ["hermit_args", "comparator", "no_retry_reason", "diagnostic"] {
                assert!(replay.get(absent).is_none(), "{id}: {absent}");
            }
            match &replay["ci"] {
                Value::Bool(true) => gated += 1,
                ci => {
                    assert_eq!(ci["ptrace"], Value::Bool(false), "{id}");
                    let evidence = replay["ci_disabled_reason"]["ptrace"]["evidence"].as_str();
                    assert!(
                        UNSELECTED_REPLAY.contains(&(id, evidence.unwrap_or_default())),
                        "{id}: {evidence:?}"
                    );
                    refused += 1;
                }
            }
        }
        assert_eq!((gated, refused), (214, UNSELECTED_REPLAY.len()));
    }

    /// The replay cells the 2026-10-09 survey saw fail, each with its issue.
    /// compat/flex and compat/timeout, two more, are selected since their
    /// replay stall (https://github.com/rrnewton/hermit/issues/3964) and false
    /// deadlock (https://github.com/rrnewton/hermit/issues/3963) were fixed.
    const UNSELECTED_REPLAY: [(&str, &str); 1] = [(
        "compat/lsof",
        "https://github.com/rrnewton/hermit/issues/3966",
    )];

    #[test]
    fn a_corpus_row_expands_into_one_verify_cell_with_the_lane_settings() {
        let expanded = expand_corpus(document(CORPUS)).unwrap();
        assert!(expanded.get("corpus").is_none());
        let tests = tests_of(&expanded);
        let ids = tests
            .iter()
            .map(|test| test["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["fixture/echo", "fixture/gxx", "fixture/slow"]);
        let echo = &tests[0];
        assert_eq!(echo["description"], "Fixture program `echo`");
        assert_eq!(
            echo["direct"],
            serde_yaml::from_str::<Value>("[/bin/echo, x]").unwrap()
        );
        let verify = &echo["modes"]["verify"];
        assert_eq!(verify["ci"], true);
        assert_eq!(
            verify["backends_enabled"],
            serde_yaml::from_str::<Value>("[ptrace]").unwrap()
        );
        for backend in ["dbt", "kvm", "sabre", "liteinst"] {
            assert!(
                verify["backends_disabled"][backend].as_str().is_some(),
                "{backend}"
            );
        }
        assert_eq!(verify["hermit_args"]["ptrace"][0], "--no-virtualize-cpuid");
        assert_eq!(verify["env"]["TMPDIR"], "/tmp");
        assert_eq!(verify["comparator"], "stripped");
        assert_eq!(verify["no_retry_reason"], "fixture single run");
        assert_eq!(verify["timeout_seconds"]["ptrace"], 60);
        assert_eq!(verify["cpu_timeout_seconds"]["ptrace"], 59);
        assert!(verify.get("diagnostic").is_none());
        for mode in NON_VERIFY_MODES {
            let table = &echo["modes"][mode];
            assert_eq!(table["ci"], false, "{mode}");
            assert!(table["ci_disabled_reason"].as_str().is_some(), "{mode}");
            assert_eq!(
                table["backends_enabled"],
                Value::Sequence(Vec::new()),
                "{mode}"
            );
        }
        // A diagnostic row carries its reason and its own shortened budget.
        let slow = &tests[2]["modes"]["verify"];
        assert_eq!(slow["diagnostic"]["ptrace"], "a bounded fixture probe");
        assert_eq!(slow["timeout_seconds"]["ptrace"], 20);
        assert_eq!(slow["cpu_timeout_seconds"]["ptrace"], 19);
        // A row without focused cells carries no labels.
        assert!(echo.get("labels").is_none());
        assert!(verify.get("labels").is_none());
        // A document without the section is unchanged.
        let plain: Value = serde_yaml::from_str("schema: 3\nbucket: plain\ntest: []").unwrap();
        assert_eq!(expand_corpus(plain.clone()).unwrap(), plain);
    }

    /// The corpus with a focused SaBRe run type, a heavy row, a row only that
    /// run type has, and two red cells.
    fn focused_corpus() -> String {
        CORPUS
            .replace(
                "  rows:\n    - {label: \"echo\"",
                r#"  heavy:
    timeout_seconds: 600
    cpu_timeout_seconds: 599
    slow_reason: heavy fixture budget
    rows: {big: a compile workload}
  focused:
    - label: sabre-compat-only
      backend: sabre
      except: {slow: not in the fixture SaBRe corpus}
  unselected:
    - backend: sabre
      result: crash-error
      evidence: https://github.com/rrnewton/hermit/issues/1
      reason: fixture execution-path failure
      rows: [g++]
    - backend: ptrace
      result: determinism-failure
      evidence: https://github.com/rrnewton/hermit/issues/2
      reason: fixture divergence
      rows: [big]
  rows:
    - {label: "echo""#,
            )
            .replace(
                "    - {label: \"slow\", argv: [\"/bin/true\"]}\n",
                "    - {label: \"slow\", argv: [\"/bin/true\"]}\n    - {label: \"big\", argv: [\"/bin/true\"], labels: [sabre-compat-only]}\n",
            )
    }

    #[test]
    fn a_focused_run_type_adds_labelled_cells_and_records_red_ones() {
        let expanded = expand_corpus(document(&focused_corpus())).unwrap();
        let tests = tests_of(&expanded);
        let by_id = |id: &str| {
            tests
                .iter()
                .find(|test| test["id"] == id)
                .unwrap_or_else(|| panic!("{id}"))
        };
        let seq = |text: &str| serde_yaml::from_str::<Value>(text).unwrap();
        // echo: ptrace for the default run type plus a SaBRe cell labelled
        // with the focused run type, which inherits no ptrace hermit_args.
        let echo = &by_id("fixture/echo")["modes"]["verify"];
        assert_eq!(echo["backends_enabled"], seq("[ptrace, sabre]"));
        assert!(echo["backends_disabled"].get("sabre").is_none());
        assert!(
            echo["backends_disabled"]["dbt"]
                .as_str()
                .unwrap()
                .ends_with("and a focused run type's verify cell on sabre")
        );
        assert_eq!(echo["labels"], seq("{sabre: [sabre-compat-only]}"));
        assert_eq!(
            echo["hermit_args"],
            seq("{ptrace: [--no-virtualize-cpuid]}")
        );
        assert_eq!(echo["timeout_seconds"], seq("{ptrace: 60, sabre: 60}"));
        assert_eq!(echo["ci"], true);
        // slow: excepted, so SaBRe is not applicable, with the except reason;
        // its diagnostic budget stays on ptrace only.
        let slow = &by_id("fixture/slow")["modes"]["verify"];
        assert_eq!(slow["backends_enabled"], seq("[ptrace]"));
        assert_eq!(
            slow["backends_disabled"]["sabre"],
            "not in the fixture SaBRe corpus"
        );
        assert!(slow.get("labels").is_none());
        // g++: its SaBRe cell is enabled but red.
        let gxx = &by_id("fixture/gxx")["modes"]["verify"];
        assert_eq!(gxx["ci"], seq("{ptrace: true, sabre: false}"));
        assert_eq!(
            gxx["ci_disabled_reason"],
            seq(
                "{sabre: {result: crash-error, evidence: 'https://github.com/rrnewton/hermit/issues/1', reason: fixture execution-path failure}}"
            )
        );
        // big: both verify cells belong to the focused run type, by cell
        // labels rather than test labels, and its ptrace cell has the heavy
        // budget with its reason and is red.
        let big = by_id("fixture/big");
        assert!(big.get("labels").is_none());
        let verify = &big["modes"]["verify"];
        assert_eq!(
            verify["labels"],
            seq("{ptrace: [sabre-compat-only], sabre: [sabre-compat-only]}")
        );
        assert_eq!(verify["timeout_seconds"], seq("{ptrace: 600, sabre: 60}"));
        assert_eq!(
            verify["cpu_timeout_seconds"],
            seq("{ptrace: 599, sabre: 59}")
        );
        assert_eq!(
            verify["slow_reason"]["ptrace"],
            "heavy fixture budget: a compile workload"
        );
        assert_eq!(verify["ci"], seq("{ptrace: false, sabre: true}"));
    }

    /// The focused corpus with cells on two additional backends: liteinst on
    /// echo and on big (a row of the focused run type, heavy and red on
    /// ptrace), with its own reason for slow; in-guest-trap on echo alone.
    fn additional_corpus() -> String {
        focused_corpus().replace(
            "  unselected:\n",
            r#"  additional:
    - backend: liteinst
      hermit_args: [--max-timeslice=disabled]
      hermit_args_reason: fixture in-guest reason
      rows: [echo, big]
      disabled_reason: not measured on liteinst
      disabled: {slow: diverged on liteinst}
    - backend: in-guest-trap
      hermit_args: [--max-timeslice=disabled]
      hermit_args_reason: fixture in-guest reason
      rows: [echo]
      disabled_reason: not measured on in-guest-trap
  unselected:
"#,
        )
    }

    #[test]
    fn an_additional_backend_adds_full_validation_cells_with_their_own_flags() {
        let corpus = additional_corpus();
        assert!(corpus.contains("  additional:\n"));
        let expanded = expand_corpus(document(&corpus)).unwrap();
        let tests = tests_of(&expanded);
        let focused = expand_corpus(document(&focused_corpus())).unwrap();
        let seq = |text: &str| serde_yaml::from_str::<Value>(text).unwrap();
        let by_id = |tests: &Vec<Value>, id: &str| {
            tests
                .iter()
                .find(|test| test["id"] == id)
                .unwrap_or_else(|| panic!("{id}"))
                .clone()
        };
        // No test is added: additional cells are cells of the rows' own tests.
        assert_eq!(
            tests
                .iter()
                .map(|test| test["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["fixture/echo", "fixture/gxx", "fixture/slow", "fixture/big"]
        );
        // echo: the lane's cell, both additional cells, then the focused one.
        let echo = &by_id(tests, "fixture/echo")["modes"]["verify"];
        assert_eq!(
            echo["backends_enabled"],
            seq("[ptrace, liteinst, in-guest-trap, sabre]")
        );
        assert_eq!(
            echo["hermit_args"],
            seq(
                "{ptrace: [--no-virtualize-cpuid], liteinst: [--max-timeslice=disabled], in-guest-trap: [--max-timeslice=disabled]}"
            )
        );
        // Each flagged backend's own reason, so each cell's relaxation names
        // only its own.
        assert_eq!(
            echo["hermit_args_reason"],
            seq(
                "{ptrace: fixture configuration, liteinst: fixture in-guest reason, in-guest-trap: fixture in-guest reason}"
            )
        );
        // The global default budget, so no budget entry; no run-type label, so
        // the default (full) validation runs them; the corpus's shared settings.
        assert_eq!(echo["timeout_seconds"], seq("{ptrace: 60, sabre: 60}"));
        assert_eq!(echo["cpu_timeout_seconds"], seq("{ptrace: 59, sabre: 59}"));
        assert_eq!(echo["slow_reason"].as_mapping().unwrap().len(), 2);
        assert_eq!(echo["labels"], seq("{sabre: [sabre-compat-only]}"));
        assert_eq!(echo["ci"], true);
        assert_eq!(echo["env"]["TMPDIR"], "/tmp");
        assert_eq!(echo["comparator"], "stripped");
        assert_eq!(echo["no_retry_reason"], "fixture single run");
        assert!(echo.get("diagnostic").is_none());
        assert_eq!(
            echo["backends_disabled"],
            seq(
                "{dbt: 'A fixture corpus row runs only its lane''s verify cell on ptrace, a verify cell on liteinst and in-guest-trap, and a focused run type''s verify cell on sabre', kvm: 'A fixture corpus row runs only its lane''s verify cell on ptrace, a verify cell on liteinst and in-guest-trap, and a focused run type''s verify cell on sabre'}"
            )
        );
        // big: its liteinst cell carries none of the row's run-type labels, is
        // selected though the ptrace cell is red, and has the default budget;
        // in-guest-trap is off with that backend's reason.
        let big = &by_id(tests, "fixture/big")["modes"]["verify"];
        assert_eq!(big["backends_enabled"], seq("[ptrace, liteinst, sabre]"));
        assert_eq!(
            big["labels"],
            seq("{ptrace: [sabre-compat-only], sabre: [sabre-compat-only]}")
        );
        assert_eq!(
            big["ci"],
            seq("{ptrace: false, liteinst: true, sabre: true}")
        );
        assert_eq!(big["timeout_seconds"], seq("{ptrace: 600, sabre: 60}"));
        assert_eq!(
            big["backends_disabled"]["in-guest-trap"],
            "not measured on in-guest-trap"
        );
        assert_eq!(
            big["hermit_args"],
            seq("{ptrace: [--no-virtualize-cpuid], liteinst: [--max-timeslice=disabled]}")
        );
        assert_eq!(
            big["hermit_args_reason"],
            seq("{ptrace: fixture configuration, liteinst: fixture in-guest reason}")
        );
        // A row without any additional cell is exactly its focused-corpus test
        // but for the two backends' disabled reasons.
        for (id, liteinst, in_guest_trap) in [
            (
                "fixture/slow",
                "diverged on liteinst",
                "not measured on in-guest-trap",
            ),
            (
                "fixture/gxx",
                "not measured on liteinst",
                "not measured on in-guest-trap",
            ),
        ] {
            let mut test = by_id(tests, id);
            let disabled = &mut test["modes"]["verify"]["backends_disabled"];
            assert_eq!(disabled["liteinst"], liteinst, "{id}");
            assert_eq!(disabled["in-guest-trap"], in_guest_trap, "{id}");
            let unchanged = by_id(tests_of(&focused), id);
            disabled["liteinst"] =
                unchanged["modes"]["verify"]["backends_disabled"]["liteinst"].clone();
            disabled["in-guest-trap"] =
                unchanged["modes"]["verify"]["backends_disabled"]["in-guest-trap"].clone();
            assert_eq!(test, unchanged, "{id}");
        }
        // One reason shared by every flagged cell is written once.
        let shared = expand_corpus(document(
            &corpus.replace("fixture in-guest reason", "fixture configuration"),
        ))
        .unwrap();
        assert_eq!(
            by_id(tests_of(&shared), "fixture/echo")["modes"]["verify"]["hermit_args_reason"],
            "fixture configuration"
        );
        // An additional cell may be measured red like any other cell.
        let red = expand_corpus(document(&corpus.replace(
            "  unselected:\n",
            "  unselected:\n    - backend: liteinst\n      result: determinism-failure\n      evidence: https://github.com/rrnewton/hermit/issues/5\n      reason: fixture in-guest divergence\n      rows: [echo]\n",
        )))
        .unwrap();
        let echo = &by_id(tests_of(&red), "fixture/echo")["modes"]["verify"];
        assert_eq!(
            echo["ci"],
            seq("{ptrace: true, liteinst: false, in-guest-trap: true, sabre: true}")
        );
        assert_eq!(
            echo["ci_disabled_reason"]["liteinst"]["evidence"],
            "https://github.com/rrnewton/hermit/issues/5"
        );
        // An additional cell without flags carries none, and the lane's reason
        // stays exactly as written.
        let bare = expand_corpus(document(&corpus.replace(
            "      hermit_args: [--max-timeslice=disabled]\n      hermit_args_reason: fixture in-guest reason\n      rows: [echo]\n",
            "      rows: [echo]\n",
        )))
        .unwrap();
        let echo = &by_id(tests_of(&bare), "fixture/echo")["modes"]["verify"];
        assert_eq!(
            echo["hermit_args"],
            seq("{ptrace: [--no-virtualize-cpuid], liteinst: [--max-timeslice=disabled]}")
        );
        assert_eq!(
            echo["hermit_args_reason"],
            seq("{ptrace: fixture configuration, liteinst: fixture in-guest reason}")
        );
    }

    #[test]
    fn a_malformed_additional_backend_is_refused() {
        let corpus = additional_corpus();
        let refused = |from: &str, to: &str| {
            assert!(corpus.contains(from), "{from}");
            expand_corpus(document(&corpus.replace(from, to))).unwrap_err()
        };
        let liteinst = "    - backend: liteinst\n";
        for backend in ["ptrace", "sabre", "e9patch"] {
            assert!(
                refused(liteinst, &format!("    - backend: {backend}\n"))
                    .contains("must be one of"),
                "{backend}"
            );
        }
        assert!(
            refused(
                "    - backend: in-guest-trap\n",
                "    - backend: liteinst\n"
            )
            .contains("additional backend `liteinst` is repeated")
        );
        assert!(
            refused(
                "      hermit_args_reason: fixture in-guest reason\n      rows: [echo, big]\n",
                "      rows: [echo, big]\n"
            )
            .contains("require a hermit_args_reason")
        );
        assert!(
            refused("      hermit_args: [--max-timeslice=disabled]\n      hermit_args_reason: fixture in-guest reason\n      rows: [echo, big]\n", "      hermit_args_reason: fixture in-guest reason\n      rows: [echo, big]\n")
                .contains("hermit_args_reason without hermit_args")
        );
        assert!(
            refused(
                "hermit_args_reason: fixture in-guest reason\n      rows: [echo, big]",
                "hermit_args_reason: \"\"\n      rows: [echo, big]"
            )
            .contains("additional liteinst hermit_args_reason must be nonempty")
        );
        assert!(refused("rows: [echo, big]", "rows: []").contains("names no rows"));
        assert!(refused("rows: [echo, big]", "rows: [echo, absent]").contains("which is no row"));
        assert!(refused("rows: [echo, big]", "rows: [echo, echo]").contains("twice"));
        assert!(
            refused("{slow: diverged", "{echo: diverged")
                .contains("both with and without the cell")
        );
        assert!(refused("{slow: diverged", "{absent: diverged").contains("which is no row"));
        assert!(refused("{slow: diverged on liteinst}", "{slow: \"\"}").contains("nonempty"));
        assert!(
            refused("      disabled_reason: not measured on liteinst\n", "")
                .contains("invalid corpus section")
        );
        assert!(
            refused(
                "disabled_reason: not measured on liteinst",
                "disabled_reason: \" \""
            )
            .contains("additional liteinst disabled_reason must be nonempty")
        );
        assert!(
            refused(
                "      disabled_reason: not measured on liteinst\n",
                "      disabled_reason: not measured on liteinst\n      timeout_seconds: 60\n"
            )
            .contains("invalid corpus section")
        );
        // A red cell must be one the row has.
        assert!(
            refused(
                "  unselected:\n",
                "  unselected:\n    - backend: in-guest-trap\n      result: crash-error\n      evidence: https://github.com/rrnewton/hermit/issues/5\n      reason: fixture\n      rows: [big]\n",
            )
            .contains("which has no cell there")
        );
        // The lane's flags and reason are checked together, so neither can
        // hide behind an additional cell's half of the mode.
        assert!(
            refused("    hermit_args_reason: fixture configuration\n", "")
                .contains("must be given together")
        );
    }

    /// The corpus with a strict variant: one row excepted, one red.
    fn variant_corpus() -> String {
        CORPUS.replace(
            "  rows:\n    - {label: \"echo\"",
            r#"  variants:
    - label: strict-compat-only
      id_prefix: strict-
      description: Fixture program without relaxations
      timeout_seconds: 30
      cpu_timeout_seconds: 29
      slow_reason: strict fixture budget
      no_retry_reason: strict fixture single run
      except:
        - reason: not in the fixture strict corpus
          rows: [echo]
      unselected:
        - result: determinism-failure
          evidence: https://github.com/rrnewton/hermit/issues/3
          reason: fixture strict divergence
          rows: [slow]
  rows:
    - {label: "echo""#,
        )
    }

    #[test]
    fn additional_row_env_keeps_sibling_rows_backends_and_strict_variants_unchanged() {
        let corpus = variant_corpus().replace(
            "  variants:\n",
            "  additional:\n    - backend: liteinst\n      rows: ['g++']\n      disabled_reason: fixture not enrolled\n  variants:\n",
        );
        let with_env = |settings: &str| {
            corpus.replace(
                "      rows: ['g++']\n",
                &format!("      rows: ['g++']\n      row_env: {settings}\n"),
            )
        };
        let original = expand_corpus(document(&corpus)).unwrap();
        let mut expanded =
            expand_corpus(document(&with_env("{'g++': {FIXTURE_CELL: 'on'}}"))).unwrap();
        let verify = &mut expanded["test"][1]["modes"]["verify"];
        assert_eq!(
            verify["env_by_backend"],
            serde_yaml::from_str::<Value>("{liteinst: {FIXTURE_CELL: 'on'}}").unwrap()
        );
        verify
            .as_mapping_mut()
            .unwrap()
            .remove(string("env_by_backend"));
        // Removing exactly the addition restores the entire expansion, including
        // the strict variant of this same row and every sibling's full recipe.
        assert_eq!(expanded, original);
        for (settings, needle) in [
            ("{absent: {FIXTURE_CELL: 'on'}}", "no enabled cell there"),
            ("{slow: {FIXTURE_CELL: 'on'}}", "no enabled cell there"),
            ("{'g++': {}}", "is empty"),
        ] {
            let error = expand_corpus(document(&with_env(settings))).unwrap_err();
            assert!(error.contains(needle), "{error}");
        }
    }

    #[test]
    fn a_variant_adds_one_labelled_test_per_row_on_the_corpus_backend() {
        let expanded = expand_corpus(document(&variant_corpus())).unwrap();
        let tests = tests_of(&expanded);
        let ids = tests
            .iter()
            .map(|test| test["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        // The rows' own tests are unchanged and come first; echo is excepted.
        assert_eq!(
            ids,
            [
                "fixture/echo",
                "fixture/gxx",
                "fixture/slow",
                "fixture/strict-gxx",
                "fixture/strict-slow"
            ]
        );
        assert_eq!(
            tests[..3],
            tests_of(&expand_corpus(document(CORPUS)).unwrap())[..]
        );
        let seq = |text: &str| serde_yaml::from_str::<Value>(text).unwrap();
        let gxx = &tests[3];
        assert_eq!(
            gxx["description"],
            "Fixture program without relaxations `g++`"
        );
        assert_eq!(gxx["labels"], seq("[strict-compat-only]"));
        assert_eq!(gxx["direct"], seq("[/usr/bin/g++, --version]"));
        let verify = &gxx["modes"]["verify"];
        assert_eq!(verify["ci"], true);
        assert_eq!(verify["backends_enabled"], seq("[ptrace]"));
        assert!(verify["backends_disabled"].get("ptrace").is_none());
        assert!(verify["backends_disabled"]["sabre"].as_str().is_some());
        // No corpus hermit_args, its own budget and single-attempt reason, the
        // corpus's environment and comparator, and no cell labels.
        assert!(verify.get("hermit_args").is_none());
        assert!(verify.get("hermit_args_reason").is_none());
        assert_eq!(verify["timeout_seconds"], seq("{ptrace: 30}"));
        assert_eq!(verify["cpu_timeout_seconds"], seq("{ptrace: 29}"));
        assert_eq!(
            verify["slow_reason"],
            seq("{ptrace: strict fixture budget}")
        );
        assert_eq!(verify["no_retry_reason"], "strict fixture single run");
        assert_eq!(verify["env"]["TMPDIR"], "/tmp");
        assert_eq!(verify["comparator"], "stripped");
        assert!(verify.get("labels").is_none());
        // slow: a diagnostic row in the corpus, but no variant cell is one;
        // its variant cell is red.
        let slow = &tests[4]["modes"]["verify"];
        assert!(slow.get("diagnostic").is_none());
        assert_eq!(slow["timeout_seconds"], seq("{ptrace: 30}"));
        assert_eq!(slow["ci"], seq("{ptrace: false}"));
        assert_eq!(
            slow["ci_disabled_reason"],
            seq(
                "{ptrace: {result: determinism-failure, evidence: 'https://github.com/rrnewton/hermit/issues/3', reason: fixture strict divergence}}"
            )
        );
        // Variant Hermit flags replace the corpus's.
        let flagged = expand_corpus(document(&variant_corpus().replace(
            "      timeout_seconds: 30",
            "      hermit_args: [--fixture-flag]\n      hermit_args_reason: fixture reason\n      timeout_seconds: 30",
        )))
        .unwrap();
        let verify = &tests_of(&flagged)[3]["modes"]["verify"];
        assert_eq!(verify["hermit_args"], seq("{ptrace: [--fixture-flag]}"));
        assert_eq!(verify["hermit_args_reason"], "fixture reason");
    }

    #[test]
    fn a_malformed_variant_is_refused() {
        let variant = variant_corpus();
        let refused = |from: &str, to: &str| {
            assert!(variant.contains(from), "{from}");
            expand_corpus(document(&variant.replace(from, to))).unwrap_err()
        };
        assert!(refused("rows: [echo]", "rows: [absent]").contains("which is no row"));
        assert!(refused("rows: [echo]", "rows: [echo, echo]").contains("twice"));
        assert!(refused("rows: [echo]", "rows: []").contains("names no rows"));
        assert!(refused("rows: [slow]", "rows: [echo]").contains("which has no variant test"));
        assert!(refused("rows: [slow]", "rows: [slow, slow]").contains("twice"));
        assert!(refused("rows: [slow]", "rows: []").contains("names no rows"));
        assert!(refused("id_prefix: strict-", "id_prefix: \"\"").contains("nonempty"));
        // strict-gxx would collide with a row's own test id.
        assert!(
            refused(
                "{label: \"slow\", argv",
                "{label: \"slow\", id: strict-gxx, argv"
            )
            .contains("which another test has")
        );
        assert!(
            refused(
                "  rows:\n    - {label: \"echo\"",
                &format!(
                    "{}  rows:\n    - {{label: \"echo\"",
                    &variant[variant.find("    - label: strict").unwrap()
                        ..variant.find("  rows:\n").unwrap()]
                )
            )
            .contains("is repeated")
        );
        assert!(
            refused("      id_prefix", "      surprise: 1\n      id_prefix")
                .contains("invalid corpus section")
        );
    }

    /// The focused corpus with a replay run type: one row excepted, one red.
    fn replay_corpus() -> String {
        focused_corpus().replace(
            "  rows:\n    - {label: \"echo\"",
            r#"  replay:
    label: rr-compat-only
    timeout_seconds: 30
    cpu_timeout_seconds: 29
    slow_reason: replay fixture budget
    except:
      - reason: not in the fixture rr corpus
        rows: [echo]
    unselected:
      - result: replay-failure
        evidence: https://github.com/rrnewton/hermit/issues/4
        reason: fixture recording refusal
        rows: [slow]
  rows:
    - {label: "echo""#,
        )
    }

    #[test]
    fn a_replay_run_type_adds_a_labelled_replay_cell_to_each_rows_own_test() {
        let expanded = expand_corpus(document(&replay_corpus())).unwrap();
        let tests = tests_of(&expanded);
        let focused = expand_corpus(document(&focused_corpus())).unwrap();
        let seq = |text: &str| serde_yaml::from_str::<Value>(text).unwrap();
        // No test is added: the replay cell is a cell of the row's own test.
        assert_eq!(
            tests
                .iter()
                .map(|test| test["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["fixture/echo", "fixture/gxx", "fixture/slow", "fixture/big"]
        );
        // echo is excepted: exactly its test without the run type.
        assert_eq!(tests[0], tests_of(&focused)[0]);
        let by_id = |id: &str| {
            tests
                .iter()
                .find(|test| test["id"] == id)
                .unwrap_or_else(|| panic!("{id}"))
        };
        let modes_of = |id: &str| by_id(id)["modes"].as_mapping().unwrap().clone();
        let gxx = modes_of("fixture/gxx");
        assert_eq!(
            gxx.keys()
                .map(|mode| mode.as_str().unwrap())
                .collect::<Vec<_>>(),
            MODES
        );
        let both = "A fixture corpus row runs only its lane's verify cell on ptrace, a focused run type's verify cell on sabre, and a focused run type's replay cell on ptrace";
        let replay = &gxx["replay"];
        assert_eq!(replay["ci"], true);
        assert_eq!(replay["backends_enabled"], seq("[ptrace]"));
        for backend in ["dbt", "kvm", "sabre", "liteinst"] {
            assert_eq!(replay["backends_disabled"][backend], both, "{backend}");
        }
        assert_eq!(replay["labels"], seq("{ptrace: [rr-compat-only]}"));
        assert_eq!(replay["timeout_seconds"], seq("{ptrace: 30}"));
        assert_eq!(replay["cpu_timeout_seconds"], seq("{ptrace: 29}"));
        assert_eq!(
            replay["slow_reason"],
            seq("{ptrace: replay fixture budget}")
        );
        // An empty environment rather than none, so it does not inherit the
        // verify cell's TMPDIR, and none of the verify-only settings.
        assert_eq!(replay["env"], seq("{}"));
        for absent in [
            "hermit_args",
            "hermit_args_reason",
            "comparator",
            "comparator_reason",
            "no_retry_reason",
            "diagnostic",
            "ci_disabled_reason",
        ] {
            assert!(replay.get(absent).is_none(), "{absent}");
        }
        // Its verify cells keep their settings and run types; only the reason
        // the other backends are off names the replay cell too.
        let verify = &gxx["verify"];
        assert_eq!(verify["labels"], seq("{sabre: [sabre-compat-only]}"));
        assert_eq!(verify["env"]["TMPDIR"], "/tmp");
        assert_eq!(verify["backends_disabled"]["dbt"], both);
        let mut unchanged = verify.clone();
        unchanged["backends_disabled"] =
            tests_of(&focused)[1]["modes"]["verify"]["backends_disabled"].clone();
        assert_eq!(unchanged, tests_of(&focused)[1]["modes"]["verify"]);
        for mode in ["naked", "chaos", "custom"] {
            assert_eq!(gxx[mode]["ci_disabled_reason"], both, "{mode}");
        }
        // slow: no SaBRe cell, a diagnostic verify cell, and a red replay
        // cell that is not a diagnostic.
        let slow = modes_of("fixture/slow");
        assert_eq!(
            slow["replay"]["backends_disabled"]["dbt"],
            "A fixture corpus row runs only its lane's verify cell on ptrace, and a focused run type's replay cell on ptrace"
        );
        assert_eq!(slow["replay"]["ci"], seq("{ptrace: false}"));
        assert_eq!(
            slow["replay"]["ci_disabled_reason"],
            seq(
                "{ptrace: {result: replay-failure, evidence: 'https://github.com/rrnewton/hermit/issues/4', reason: fixture recording refusal}}"
            )
        );
        assert!(slow["replay"].get("diagnostic").is_none());
        assert_eq!(
            slow["verify"]["diagnostic"]["ptrace"],
            "a bounded fixture probe"
        );
        // big: the row's own run type labels its verify cells and not its
        // replay cell, which is selected though its ptrace verify cell is red.
        let big = by_id("fixture/big");
        assert!(big.get("labels").is_none());
        let big = modes_of("fixture/big");
        assert_eq!(
            big["verify"]["labels"],
            seq("{ptrace: [sabre-compat-only], sabre: [sabre-compat-only]}")
        );
        assert_eq!(big["replay"]["labels"], seq("{ptrace: [rr-compat-only]}"));
        assert_eq!(big["replay"]["ci"], true);
        assert_eq!(big["verify"]["ci"], seq("{ptrace: false, sabre: true}"));
    }

    #[test]
    fn a_malformed_replay_run_type_is_refused() {
        let replay = replay_corpus();
        let refused = |from: &str, to: &str| {
            assert!(replay.contains(from), "{from}");
            expand_corpus(document(&replay.replace(from, to))).unwrap_err()
        };
        assert!(refused("rows: [echo]", "rows: [absent]").contains("which is no row"));
        assert!(refused("rows: [echo]", "rows: [echo, echo]").contains("twice"));
        assert!(refused("rows: [echo]", "rows: []").contains("names no rows"));
        assert!(refused("rows: [slow]", "rows: [echo]").contains("which has no replay cell"));
        assert!(refused("rows: [slow]", "rows: [absent]").contains("which has no replay cell"));
        assert!(refused("rows: [slow]", "rows: [slow, slow]").contains("twice"));
        assert!(refused("rows: [slow]", "rows: []").contains("names no rows"));
        assert!(
            refused("slow_reason: replay fixture budget", "slow_reason: \"\"")
                .contains("replay slow_reason must be nonempty")
        );
        assert!(
            refused("label: rr-compat-only", "label: \"\"")
                .contains("replay label must be nonempty")
        );
        // None of the verify-only settings is accepted on the replay run type.
        for setting in [
            "no_retry_reason: single run",
            "hermit_args: [--fixture-flag]",
            "comparator: stripped",
            "env: {TMPDIR: /tmp}",
        ] {
            assert!(
                refused(
                    "    timeout_seconds: 30",
                    &format!("    {setting}\n    timeout_seconds: 30")
                )
                .contains("invalid corpus section"),
                "{setting}"
            );
        }
        // A variant is a verify test with its single-attempt reason; the
        // replay variant it once could be is gone.
        let variant = variant_corpus();
        assert!(
            expand_corpus(document(
                &variant.replace("      no_retry_reason: strict fixture single run\n", "")
            ))
            .unwrap_err()
            .contains("no_retry_reason")
        );
        assert!(
            expand_corpus(document(&variant.replace(
                "id_prefix: strict-",
                "id_prefix: strict-\n      mode: replay"
            )))
            .unwrap_err()
            .contains("invalid corpus section")
        );
    }

    #[test]
    fn a_malformed_corpus_is_refused() {
        let refused = |from: &str, to: &str| {
            assert!(CORPUS.contains(from), "{from}");
            expand_corpus(document(&CORPUS.replace(from, to))).unwrap_err()
        };
        assert!(refused("label: \"slow\"", "label: \"echo\"").contains("is repeated"));
        assert!(refused("id: gxx", "id: echo").contains("repeats another row's test id"));
        assert!(refused("id: gxx", "id: g++").contains("repeats its label as its id"));
        assert!(refused("[\"/bin/true\"]", "[]").contains("has an empty argv"));
        assert!(refused("{slow: a bounded", "{absent: a bounded").contains("which is no row"));
        assert!(refused("backend: ptrace", "backend: e9patch").contains("is not one of"));
        let focused = focused_corpus();
        let refused_focused = |from: &str, to: &str| {
            assert!(focused.contains(from), "{from}");
            expand_corpus(document(&focused.replace(from, to))).unwrap_err()
        };
        assert!(
            refused_focused(
                "      backend: sabre\n      except",
                "      backend: ptrace\n      except"
            )
            .contains("other than `ptrace`")
        );
        assert!(refused_focused("{slow: not in", "{absent: not in").contains("which is no row"));
        assert!(refused_focused("rows: [g++]", "rows: [slow]").contains("which has no cell there"));
        assert!(refused_focused("rows: [g++]", "rows: [absent]").contains("which is no row"));
        assert!(refused_focused("rows: [big]", "rows: [big, big]").contains("twice"));
        assert!(refused_focused("rows: [g++]", "rows: []").contains("names no rows"));
        assert!(
            refused_focused("rows: {big: a compile", "rows: {slow: a compile")
                .contains("both diagnostic and heavy")
        );
        assert!(
            refused("  lane: portable", "  lane: portable\n  surprise: 1")
                .contains("invalid corpus section")
        );
    }

    #[test]
    fn placeholders_resolve_or_refuse() {
        let root = std::path::Path::new("/repo");
        let xdg = std::path::Path::new("/cell/xdg-config");
        let argv = [
            "{{ROOT_DIR}}/tool".to_string(),
            "{{VALIDATE_RUN_STATE}}/in".to_string(),
            "{{XDG_CONFIG_HOME}}/git/config".to_string(),
        ];
        assert_eq!(
            resolve_direct_placeholders(
                "t",
                &argv,
                root,
                Some(std::ffi::OsStr::new("/state")),
                Some(xdg)
            )
            .unwrap(),
            ["/repo/tool", "/state/in", "/cell/xdg-config/git/config"]
        );
        for missing in [None, Some(std::ffi::OsStr::new(""))] {
            assert!(
                resolve_direct_placeholders("t", &argv, root, missing, Some(xdg))
                    .unwrap_err()
                    .contains("VALIDATE_RUN_STATE is not set")
            );
        }
        // A caller without a cell refuses the cell placeholder rather than
        // passing its literal text to a guest.
        assert!(
            resolve_direct_placeholders(
                "t",
                &argv,
                root,
                Some(std::ffi::OsStr::new("/state")),
                None
            )
            .unwrap_err()
            .contains("no cell XDG configuration directory")
        );
        assert_eq!(
            resolve_direct_placeholders(
                "t",
                &argv[..2],
                root,
                Some(std::ffi::OsStr::new("/s")),
                None
            )
            .unwrap(),
            ["/repo/tool", "/s/in"]
        );
        assert!(
            check_direct_placeholders("t", &["{{ROOT}}".to_string()])
                .unwrap_err()
                .contains("unknown placeholder")
        );
        assert!(
            check_direct_placeholders("t", &["{{XDG_CONFIG}}".to_string()])
                .unwrap_err()
                .contains("unknown placeholder")
        );
    }

    #[test]
    fn a_direct_shell_word_expands_its_placeholders_in_the_shell() {
        let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
        let words = [
            "plain",
            "{{ROOT_DIR}}/a b",
            "x={{VALIDATE_RUN_STATE}}/{{ROOT_DIR}}",
            "",
            "{{XDG_CONFIG_HOME}}/git/config",
        ]
        .map(|arg| direct_shell_word("t", arg, quote).unwrap());
        let script = format!("cd /tmp && printf '%s\\n' {}", words.join(" "));
        let run = |state: Option<&str>| {
            let mut command = std::process::Command::new("bash");
            command
                .args(["-c", &script])
                .env_remove("VALIDATE_RUN_STATE")
                .env("XDG_CONFIG_HOME", "/xdg");
            if let Some(state) = state {
                command.env("VALIDATE_RUN_STATE", state);
            }
            command.output().unwrap()
        };
        let output = run(Some("/state"));
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "plain\n/tmp/a b\nx=/state//tmp\n\n/xdg/git/config\n"
        );
        let output = run(None);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("VALIDATE_RUN_STATE is not set"));
        let output = std::process::Command::new("bash")
            .args(["-c", &format!("printf '%s\\n' {}", words[4])])
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("XDG_CONFIG_HOME is not set"));
        assert!(direct_shell_word("t", "{{TYPO}}", quote).is_err());
    }
}
