//! Fail-closed authority for strict determinism greens.
//!
//! This binary is the only writer of accepted strict-green records. It does
//! not infer a green from a test status: it dereferences and compares every
//! required artifact before emitting a record.

use std::collections::BTreeSet;
use std::fs::File;
use std::fs::{self};
use std::io::BufWriter;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

const AUTHORITY: &str = "hermit-strict-green-v1";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimsDocument {
    schema: u64,
    claims: Vec<Claim>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    run_id: String,
    hermit_sha: String,
    source_tree_dirty: bool,
    cell: Cell,
    tier: Tier,
    memory_cadence: u64,
    state: CellState,
    control_points: ControlPointPolicy,
    register_coverage: RegisterCoverage,
    runs: Vec<RunEvidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cell {
    test: String,
    mode: String,
    reference_backend: String,
    observed_backend: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Tier {
    Short,
    Large,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CellState {
    ci_enabled: bool,
    exercise: ExerciseState,
    execution_count: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ExerciseState {
    Exercised,
    NotExercised,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ControlPointPolicy {
    boundary: ControlBoundary,
    kind: ControlPointKind,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum ControlBoundary {
    #[serde(rename = "guest-logical-control-v1")]
    GuestLogicalControlV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum ControlPointKind {
    #[serde(rename = "syscall-exit")]
    SyscallExit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RegisterCoverage {
    kind: RegisterCoverageKind,
    cadence: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum RegisterCoverageKind {
    #[serde(rename = "not-included")]
    NotIncluded,
    #[serde(rename = "bounded-gpr-control-v1")]
    BoundedGprControlV1,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunEvidence {
    index: u64,
    control_point_count: u64,
    surfaces: SurfacePaths,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SurfacePaths {
    stdout: Option<ArtifactPair>,
    info_log: Option<ArtifactPair>,
    stack: Option<ArtifactPair>,
    heap: Option<ArtifactPair>,
    registers: Option<ArtifactPair>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactPair {
    reference_path: String,
    observed_path: String,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct AcceptedDocument {
    schema: u64,
    authority: &'static str,
    expected_sha: String,
    accepted_count: usize,
    records: Vec<AcceptedRecord>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct AcceptedRecord {
    run_id: String,
    hermit_sha: String,
    source_tree_dirty: bool,
    cell: Cell,
    tier: Tier,
    memory_cadence: u64,
    state: CellState,
    control_points: ControlPointPolicy,
    register_coverage: RegisterCoverage,
    runs: Vec<AcceptedRun>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct AcceptedRun {
    index: u64,
    control_point_count: u64,
    surfaces: AcceptedSurfaces,
}

#[derive(Debug, Default, Serialize)]
#[serde(deny_unknown_fields)]
struct AcceptedSurfaces {
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<AcceptedSurface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    info_log: Option<AcceptedSurface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stack: Option<AcceptedSurface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    heap: Option<AcceptedSurface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    registers: Option<AcceptedSurface>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct AcceptedSurface {
    reference_path: String,
    observed_path: String,
    bytes: usize,
    sha256: String,
}

struct Args {
    claims: PathBuf,
    evidence_root: PathBuf,
    expected_sha: String,
    accepted: PathBuf,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("strict-green-authority: REFUSED: {error}");
            ExitCode::from(1)
        }
    }
}

fn real_main() -> Result<(), String> {
    let args = parse_args(std::env::args().skip(1))?;
    if args.claims == args.accepted {
        return Err("--claims and --accepted must be different paths".into());
    }

    // Clear the authority output before looking at the claims. A failed run
    // must never leave a stale accepted green behind.
    let output = File::create(&args.accepted)
        .map_err(|error| format!("cannot clear {}: {error}", args.accepted.display()))?;
    let claims_bytes = fs::read(&args.claims)
        .map_err(|error| format!("cannot read {}: {error}", args.claims.display()))?;
    let claims: ClaimsDocument = serde_json::from_slice(&claims_bytes)
        .map_err(|error| format!("invalid claims document: {error}"))?;
    let accepted = verify_document(claims, &args.evidence_root, &args.expected_sha)?;

    let mut output = BufWriter::new(output);
    serde_json::to_writer_pretty(&mut output, &accepted)
        .map_err(|error| format!("cannot encode accepted records: {error}"))?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(|error| format!("cannot write {}: {error}", args.accepted.display()))?;
    println!(
        "ACCEPTED: {} strict green(s) at {}",
        accepted.accepted_count, accepted.expected_sha
    );
    Ok(())
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut claims = None;
    let mut evidence_root = None;
    let mut expected_sha = None;
    let mut accepted = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("{arg} requires a value"))?;
        match arg.as_str() {
            "--claims" => claims = Some(value.into()),
            "--evidence-root" => evidence_root = Some(value.into()),
            "--expected-sha" => expected_sha = Some(value),
            "--accepted" => accepted = Some(value.into()),
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    Ok(Args {
        claims: claims.ok_or("missing --claims")?,
        evidence_root: evidence_root.ok_or("missing --evidence-root")?,
        expected_sha: expected_sha.ok_or("missing --expected-sha")?,
        accepted: accepted.ok_or("missing --accepted")?,
    })
}

fn verify_document(
    document: ClaimsDocument,
    evidence_root: &Path,
    expected_sha: &str,
) -> Result<AcceptedDocument, String> {
    if document.schema != 1 {
        return Err(format!("claims schema must be 1, got {}", document.schema));
    }
    validate_sha(expected_sha, "--expected-sha")?;
    if document.claims.is_empty() {
        return Err("nonempty evidence required: claims contains zero cells".into());
    }
    let root = evidence_root.canonicalize().map_err(|error| {
        format!(
            "cannot resolve evidence root {}: {error}",
            evidence_root.display()
        )
    })?;
    if !root.is_dir() {
        return Err(format!(
            "evidence root is not a directory: {}",
            root.display()
        ));
    }

    let mut identities = BTreeSet::new();
    let mut used_artifacts = BTreeSet::new();
    let mut records = Vec::with_capacity(document.claims.len());
    for claim in document.claims {
        let identity = format!(
            "{}\0{}\0{}\0{}\0{}",
            claim.run_id,
            claim.cell.test,
            claim.cell.mode,
            claim.cell.reference_backend,
            claim.cell.observed_backend
        );
        if !identities.insert(identity) {
            return Err(format!("duplicate cell identity in run {}", claim.run_id));
        }
        records.push(verify_claim(
            claim,
            &root,
            expected_sha,
            &mut used_artifacts,
        )?);
    }
    Ok(AcceptedDocument {
        schema: 1,
        authority: AUTHORITY,
        expected_sha: expected_sha.to_string(),
        accepted_count: records.len(),
        records,
    })
}

fn verify_claim(
    claim: Claim,
    root: &Path,
    expected_sha: &str,
    used_artifacts: &mut BTreeSet<String>,
) -> Result<AcceptedRecord, String> {
    let context = || format!("{}/{}", claim.run_id, claim.cell.test);
    nonempty(&claim.run_id, "run_id")?;
    nonempty(&claim.cell.test, "cell.test")?;
    nonempty(&claim.cell.mode, "cell.mode")?;
    nonempty(&claim.cell.reference_backend, "cell.reference_backend")?;
    nonempty(&claim.cell.observed_backend, "cell.observed_backend")?;
    validate_sha(&claim.hermit_sha, "claim hermit_sha")?;
    if claim.hermit_sha != expected_sha {
        return Err(format!(
            "{}: exact SHA mismatch: claim={} expected={expected_sha}",
            context(),
            claim.hermit_sha
        ));
    }
    if claim.source_tree_dirty {
        return Err(format!(
            "{}: dirty source tree cannot earn a strict green",
            context()
        ));
    }
    if !claim.state.ci_enabled {
        return Err(format!(
            "{}: cell is disabled: ci_enabled must be true",
            context()
        ));
    }
    if matches!(claim.state.exercise, ExerciseState::NotExercised) {
        return Err(format!(
            "{}: cell is NOT-EXERCISED: exercise must be exercised",
            context()
        ));
    }
    if claim.state.execution_count == 0 || claim.runs.is_empty() {
        return Err(format!("{}: nonempty execution required", context()));
    }
    if claim.state.execution_count as usize != claim.runs.len() {
        return Err(format!(
            "{}: execution_count={} but {} run records were supplied",
            context(),
            claim.state.execution_count,
            claim.runs.len()
        ));
    }
    match claim.tier {
        Tier::Short if claim.memory_cadence != 1 => {
            return Err(format!(
                "{}: short tier requires memory_cadence=1",
                context()
            ));
        }
        Tier::Large if claim.memory_cadence < 2 => {
            return Err(format!(
                "{}: large tier requires memory_cadence>=2",
                context()
            ));
        }
        _ => {}
    }

    let register_cadence = match claim.register_coverage.kind {
        RegisterCoverageKind::NotIncluded => {
            if claim.register_coverage.cadence.is_some() {
                return Err(format!(
                    "{}: not-included register coverage cannot declare a cadence",
                    context()
                ));
            }
            None
        }
        RegisterCoverageKind::BoundedGprControlV1 => {
            let cadence = claim.register_coverage.cadence.ok_or_else(|| {
                format!("{}: bounded register coverage requires cadence", context())
            })?;
            if cadence == 0 {
                return Err(format!("{}: register cadence must be nonzero", context()));
            }
            Some(cadence)
        }
    };

    let mut runs = Vec::with_capacity(claim.runs.len());
    for (offset, run) in claim.runs.into_iter().enumerate() {
        let expected_index = offset as u64 + 1;
        if run.index != expected_index {
            return Err(format!(
                "{}: run indices must be contiguous from 1; expected {expected_index}, got {}",
                context(),
                run.index
            ));
        }
        if run.control_point_count == 0 {
            return Err(format!(
                "{} run {}: guest-logical-control evidence is empty",
                context(),
                run.index
            ));
        }
        let memory_sample = (run.index - 1).is_multiple_of(claim.memory_cadence);
        let require_memory = matches!(claim.tier, Tier::Short) || memory_sample;
        require_presence(&run.surfaces.stdout, true, "stdout", &context(), run.index)?;
        require_presence(
            &run.surfaces.info_log,
            true,
            "INFO log",
            &context(),
            run.index,
        )?;
        require_presence(
            &run.surfaces.stack,
            require_memory,
            "stack",
            &context(),
            run.index,
        )?;
        require_presence(
            &run.surfaces.heap,
            require_memory,
            "heap",
            &context(),
            run.index,
        )?;
        require_presence(
            &run.surfaces.registers,
            register_cadence.is_some(),
            "registers",
            &context(),
            run.index,
        )?;

        runs.push(AcceptedRun {
            index: run.index,
            control_point_count: run.control_point_count,
            surfaces: AcceptedSurfaces {
                stdout: verify_optional(
                    run.surfaces.stdout,
                    root,
                    used_artifacts,
                    "stdout",
                    None,
                    &context(),
                    run.index,
                )?,
                info_log: verify_optional(
                    run.surfaces.info_log,
                    root,
                    used_artifacts,
                    "INFO log",
                    None,
                    &context(),
                    run.index,
                )?,
                stack: verify_optional(
                    run.surfaces.stack,
                    root,
                    used_artifacts,
                    "stack",
                    None,
                    &context(),
                    run.index,
                )?,
                heap: verify_optional(
                    run.surfaces.heap,
                    root,
                    used_artifacts,
                    "heap",
                    None,
                    &context(),
                    run.index,
                )?,
                registers: verify_optional(
                    run.surfaces.registers,
                    root,
                    used_artifacts,
                    "registers",
                    register_cadence,
                    &context(),
                    run.index,
                )?,
            },
        });
    }

    Ok(AcceptedRecord {
        run_id: claim.run_id,
        hermit_sha: claim.hermit_sha,
        source_tree_dirty: claim.source_tree_dirty,
        cell: claim.cell,
        tier: claim.tier,
        memory_cadence: claim.memory_cadence,
        state: claim.state,
        control_points: claim.control_points,
        register_coverage: claim.register_coverage,
        runs,
    })
}

fn require_presence<T>(
    value: &Option<T>,
    required: bool,
    name: &str,
    context: &str,
    run: u64,
) -> Result<(), String> {
    if required && value.is_none() {
        return Err(format!(
            "{context} run {run}: required {name} evidence is missing"
        ));
    }
    if !required && value.is_some() && matches!(name, "stack" | "heap") {
        return Err(format!(
            "{context} run {run}: {name} evidence violates the declared memory cadence"
        ));
    }
    Ok(())
}

fn verify_optional(
    pair: Option<ArtifactPair>,
    root: &Path,
    used_artifacts: &mut BTreeSet<String>,
    surface: &str,
    register_cadence: Option<u64>,
    context: &str,
    run: u64,
) -> Result<Option<AcceptedSurface>, String> {
    pair.map(|pair| {
        verify_surface(
            pair,
            root,
            used_artifacts,
            surface,
            register_cadence,
            context,
            run,
        )
    })
    .transpose()
}

fn verify_surface(
    pair: ArtifactPair,
    root: &Path,
    used_artifacts: &mut BTreeSet<String>,
    surface: &str,
    register_cadence: Option<u64>,
    context: &str,
    run: u64,
) -> Result<AcceptedSurface, String> {
    if pair.reference_path == pair.observed_path {
        return Err(format!(
            "{context} run {run} {surface}: reference and observed paths must differ"
        ));
    }
    let reference = read_artifact(root, &pair.reference_path, surface)?;
    let observed = read_artifact(root, &pair.observed_path, surface)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if reference.metadata.dev() == observed.metadata.dev()
            && reference.metadata.ino() == observed.metadata.ino()
        {
            return Err(format!(
                "{context} run {run} {surface}: reference and observed are the same file"
            ));
        }
    }
    for (side, artifact, path) in [
        ("reference", &reference, &pair.reference_path),
        ("observed", &observed, &pair.observed_path),
    ] {
        if !used_artifacts.insert(artifact.identity.clone()) {
            return Err(format!(
                "{context} run {run} {surface}: {side} artifact is reused: {path}"
            ));
        }
    }
    if reference.bytes.is_empty() || observed.bytes.is_empty() {
        return Err(format!(
            "{context} run {run} {surface}: nonempty output required"
        ));
    }
    if reference.bytes != observed.bytes {
        return Err(format!(
            "{context} run {run} {surface}: known values differ bitwise"
        ));
    }
    validate_surface_shape(surface, &reference.bytes, register_cadence)
        .map_err(|error| format!("{context} run {run} {surface}: {error}"))?;
    let sha256 = format!("{:x}", Sha256::digest(&reference.bytes));
    Ok(AcceptedSurface {
        reference_path: pair.reference_path,
        observed_path: pair.observed_path,
        bytes: reference.bytes.len(),
        sha256,
    })
}

fn validate_surface_shape(
    surface: &str,
    bytes: &[u8],
    register_cadence: Option<u64>,
) -> Result<(), String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "evidence must be the textual observation emitted by Hermit")?;
    match surface {
        "stack" => {
            if !text.contains("[memory]") || !(text.contains("[stack]") || text.contains("Stack")) {
                return Err("evidence does not contain a Hermit stack-hash observation".into());
            }
        }
        "heap" => {
            if !text.contains("[memory]") || !(text.contains("[heap]") || text.contains("Heap")) {
                return Err("evidence does not contain a Hermit heap-hash observation".into());
            }
        }
        "registers" => {
            let cadence = register_cadence
                .ok_or("register evidence supplied without bounded register coverage")?;
            let tier = if cadence == 1 {
                "tier=full".to_string()
            } else {
                format!("tier=spot-1/{cadence}")
            };
            let lines: Vec<_> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            if lines.is_empty()
                || lines.iter().any(|line| {
                    !line.contains("[registers]")
                        || !line.contains("control_point=syscall-exit")
                        || !line.contains(&tier)
                })
            {
                return Err(format!(
                    "evidence must contain only syscall-exit register hashes with {tier}"
                ));
            }
        }
        "stdout" | "INFO log" => {}
        _ => return Err(format!("unknown surface: {surface}")),
    }
    Ok(())
}

struct Artifact {
    bytes: Vec<u8>,
    metadata: fs::Metadata,
    identity: String,
}

fn read_artifact(root: &Path, relative: &str, surface: &str) -> Result<Artifact, String> {
    let relative_path = Path::new(relative);
    if relative_path.as_os_str().is_empty()
        || !relative_path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "{surface}: artifact path must be a safe relative path: {relative}"
        ));
    }
    let path = root.join(relative_path);
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("{surface}: cannot resolve {relative}: {error}"))?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "{surface}: artifact escapes evidence root: {relative}"
        ));
    }
    let metadata = canonical
        .metadata()
        .map_err(|error| format!("{surface}: cannot stat {relative}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!(
            "{surface}: artifact is not a regular file: {relative}"
        ));
    }
    let bytes = fs::read(&canonical)
        .map_err(|error| format!("{surface}: cannot read {relative}: {error}"))?;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let identity = canonical.to_string_lossy().into_owned();
    Ok(Artifact {
        bytes,
        metadata,
        identity,
    })
}

fn nonempty(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} must be nonempty"))
    } else {
        Ok(())
    }
}

fn validate_sha(sha: &str, field: &str) -> Result<(), String> {
    if sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!("{field} must be an exact 40-hex commit SHA"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestEvidence {
        root: PathBuf,
    }

    impl TestEvidence {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hermit-strict-green-authority-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self { root }
        }

        fn pair(&self, name: &str, reference: &[u8], observed: &[u8]) -> ArtifactPair {
            let reference_path = format!("{name}.reference");
            let observed_path = format!("{name}.observed");
            fs::write(self.root.join(&reference_path), reference).unwrap();
            fs::write(self.root.join(&observed_path), observed).unwrap();
            ArtifactPair {
                reference_path,
                observed_path,
            }
        }
    }

    impl Drop for TestEvidence {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    fn clean_claim(evidence: &TestEvidence) -> Claim {
        Claim {
            run_id: "run-clean".into(),
            hermit_sha: SHA.into(),
            source_tree_dirty: false,
            cell: Cell {
                test: "backend-parity/example".into(),
                mode: "verify".into(),
                reference_backend: "ptrace".into(),
                observed_backend: "dbi".into(),
            },
            tier: Tier::Short,
            memory_cadence: 1,
            state: CellState {
                ci_enabled: true,
                exercise: ExerciseState::Exercised,
                execution_count: 1,
            },
            control_points: ControlPointPolicy {
                boundary: ControlBoundary::GuestLogicalControlV1,
                kind: ControlPointKind::SyscallExit,
            },
            register_coverage: RegisterCoverage {
                kind: RegisterCoverageKind::NotIncluded,
                cadence: None,
            },
            runs: vec![RunEvidence {
                index: 1,
                control_point_count: 3,
                surfaces: SurfacePaths {
                    stdout: Some(evidence.pair("stdout", b"observed=7\n", b"observed=7\n")),
                    info_log: Some(evidence.pair("info", b"INFO stable\n", b"INFO stable\n")),
                    stack: Some(evidence.pair(
                        "stack",
                        b"INFO detcore: [memory][dtid 3] [stack]->stack-hash\n",
                        b"INFO detcore: [memory][dtid 3] [stack]->stack-hash\n",
                    )),
                    heap: Some(evidence.pair(
                        "heap",
                        b"INFO detcore: [memory][dtid 3] [heap]->heap-hash\n",
                        b"INFO detcore: [memory][dtid 3] [heap]->heap-hash\n",
                    )),
                    registers: None,
                },
            }],
        }
    }

    fn document(claims: Vec<Claim>) -> ClaimsDocument {
        ClaimsDocument { schema: 1, claims }
    }

    #[test]
    fn clean_control_is_accepted() {
        let evidence = TestEvidence::new();
        let accepted = verify_document(document(vec![clean_claim(&evidence)]), &evidence.root, SHA)
            .expect("clean control must pass");
        assert_eq!(accepted.accepted_count, 1);
        assert_eq!(
            accepted.records[0].runs[0]
                .surfaces
                .stdout
                .as_ref()
                .unwrap()
                .bytes,
            11
        );
    }

    #[test]
    fn known_wrong_value_is_refused() {
        let evidence = TestEvidence::new();
        let mut claim = clean_claim(&evidence);
        claim.runs[0].surfaces.stdout =
            Some(evidence.pair("wrong", b"observed=7\n", b"observed=8\n"));
        let error = verify_document(document(vec![claim]), &evidence.root, SHA).unwrap_err();
        assert!(
            error.contains("stdout: known values differ bitwise"),
            "{error}"
        );
    }

    #[test]
    fn nonempty_evidence_is_required() {
        let evidence = TestEvidence::new();
        let empty_batch = verify_document(document(vec![]), &evidence.root, SHA).unwrap_err();
        assert!(empty_batch.contains("zero cells"), "{empty_batch}");

        let mut claim = clean_claim(&evidence);
        claim.runs[0].surfaces.stdout = Some(evidence.pair("empty", b"", b""));
        let empty_output = verify_document(document(vec![claim]), &evidence.root, SHA).unwrap_err();
        assert!(
            empty_output.contains("nonempty output required"),
            "{empty_output}"
        );
    }

    #[test]
    fn one_artifact_cannot_impersonate_multiple_surfaces() {
        let evidence = TestEvidence::new();
        let mut claim = clean_claim(&evidence);
        claim.runs[0].surfaces.info_log = claim.runs[0].surfaces.stdout.clone();
        let error = verify_document(document(vec![claim]), &evidence.root, SHA).unwrap_err();
        assert!(error.contains("artifact is reused"), "{error}");
    }

    #[test]
    fn disabled_and_not_exercised_are_refused_with_distinct_reasons() {
        let evidence = TestEvidence::new();
        let mut disabled = clean_claim(&evidence);
        disabled.state.ci_enabled = false;
        let error = verify_document(document(vec![disabled]), &evidence.root, SHA).unwrap_err();
        assert!(error.contains("cell is disabled"), "{error}");

        let mut not_exercised = clean_claim(&evidence);
        not_exercised.state.exercise = ExerciseState::NotExercised;
        let error =
            verify_document(document(vec![not_exercised]), &evidence.root, SHA).unwrap_err();
        assert!(error.contains("cell is NOT-EXERCISED"), "{error}");
    }

    #[test]
    fn large_tier_enforces_every_run_and_memory_cadence() {
        let evidence = TestEvidence::new();
        let mut claim = clean_claim(&evidence);
        claim.tier = Tier::Large;
        claim.memory_cadence = 2;
        claim.state.execution_count = 3;
        let base = claim.runs.pop().unwrap();
        claim.runs = (1..=3)
            .map(|index| RunEvidence {
                index,
                control_point_count: 2,
                surfaces: SurfacePaths {
                    stdout: Some(evidence.pair(
                        &format!("large-{index}-stdout"),
                        b"same\n",
                        b"same\n",
                    )),
                    info_log: Some(evidence.pair(
                        &format!("large-{index}-info"),
                        b"same-info\n",
                        b"same-info\n",
                    )),
                    stack: (index % 2 == 1).then(|| {
                        evidence.pair(
                            &format!("large-{index}-stack"),
                            b"INFO detcore: [memory][dtid 3] [stack]->stack-hash\n",
                            b"INFO detcore: [memory][dtid 3] [stack]->stack-hash\n",
                        )
                    }),
                    heap: (index % 2 == 1).then(|| {
                        evidence.pair(
                            &format!("large-{index}-heap"),
                            b"INFO detcore: [memory][dtid 3] [heap]->heap-hash\n",
                            b"INFO detcore: [memory][dtid 3] [heap]->heap-hash\n",
                        )
                    }),
                    registers: None,
                },
            })
            .collect();
        verify_document(document(vec![claim.clone()]), &evidence.root, SHA)
            .expect("clean large tier must pass");

        claim.runs[2].surfaces.heap = None;
        let error = verify_document(document(vec![claim]), &evidence.root, SHA).unwrap_err();
        assert!(
            error.contains("required heap evidence is missing"),
            "{error}"
        );
        drop(base);
    }

    #[test]
    fn bounded_register_claim_is_explicit_and_nonvacuous() {
        let evidence = TestEvidence::new();
        let mut claim = clean_claim(&evidence);
        claim.register_coverage = RegisterCoverage {
            kind: RegisterCoverageKind::BoundedGprControlV1,
            cadence: Some(4),
        };
        let missing =
            verify_document(document(vec![claim.clone()]), &evidence.root, SHA).unwrap_err();
        assert!(
            missing.contains("required registers evidence is missing"),
            "{missing}"
        );

        claim.runs[0].surfaces.registers = Some(evidence.pair(
            "registers",
            b"INFO detcore: [registers][dtid 3] control_point=syscall-exit tier=spot-1/4 register-hash\n",
            b"INFO detcore: [registers][dtid 3] control_point=syscall-exit tier=spot-1/4 register-hash\n",
        ));
        verify_document(document(vec![claim]), &evidence.root, SHA)
            .expect("bounded register evidence must pass when present");
    }
}
