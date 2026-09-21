//! The private live-authority proof used by the validation driver. Wire evidence
//! is deliberately a different type: a caller's JSON cannot construct this one.

use std::path::Path;
use std::process::Command;

use dagrun::io::dag_to_json;
use dagrun::model::DagConfig;
use dagrun::model::dag_config_carry_diff;
use hermit_manifest_plan::ledger::ADMISSION_CONTEXT_V2_CONTRACT;
use hermit_manifest_plan::ledger::AdmissionAuthorityV1;
use hermit_manifest_plan::ledger::AdmissionContextArtifactV2;
use hermit_manifest_plan::ledger::AdmissionContextV2;
use hermit_manifest_plan::ledger::AdmissionEvidence;
use hermit_manifest_plan::ledger::AdmissionFloorEvidenceV2;
use hermit_manifest_plan::ledger::AdmissionFloorV1;
use hermit_manifest_plan::ledger::AdmissionPinBindingV1;
use hermit_manifest_plan::ledger::WorkspaceLocatorV2;
use hermit_manifest_plan::ledger::admission_context_v2_bytes;
use hermit_manifest_plan::ledger::admission_hex;
use hermit_manifest_plan::ledger::admission_sha256;
use serde_json::Value;

// A directory pathname is not this capability. Production constructs it only
// from the state descriptor in the already authenticated immutable tool proof.
pub(crate) struct VerifiedStateRoot {
    path: std::path::PathBuf,
    directory: std::fs::File,
    identity: (u64, u64),
}

pub(crate) struct RetainedFile {
    pub(crate) file: std::fs::File,
    pub(crate) locator: WorkspaceLocatorV2,
}

pub(crate) struct RetainedAdmission {
    pub(crate) evidence: AdmissionEvidence,
    artifact: RetainedFile,
    response: RetainedFile,
    response_bytes: Vec<u8>,
}

impl VerifiedStateRoot {
    pub(crate) fn from_tool_authority(
        authority: &super::ImmutableToolAuthority,
    ) -> Result<Self, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let held = format!("/proc/{}/fd/{}", authority.holder_pid, authority.state_fd);
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(held)
            .map_err(|e| format!("cannot retain authenticated state descriptor: {e}"))?;
        let result = Self {
            path: authority.state_root.clone(),
            directory,
            identity: (authority.state_dev, authority.state_ino),
        };
        result.verify()?;
        Ok(result)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn verify(&self) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let held = self.directory.metadata().map_err(|e| e.to_string())?;
        let live = std::fs::symlink_metadata(&self.path).map_err(|e| e.to_string())?;
        if !held.is_dir()
            || !live.is_dir()
            || (held.dev(), held.ino()) != self.identity
            || (live.dev(), live.ino()) != self.identity
            || self.path.canonicalize().map_err(|e| e.to_string())? != self.path
        {
            return Err("authenticated state root changed identity".into());
        }
        Ok(())
    }

    pub(crate) fn locator(&self, path: &Path) -> Result<WorkspaceLocatorV2, String> {
        self.verify()?;
        let relative = path
            .strip_prefix(&self.path)
            .map_err(|_| "log is outside authenticated state root")?;
        let locator = WorkspaceLocatorV2 {
            scope: "workspace".into(),
            path: relative
                .to_str()
                .ok_or("retained path is not UTF-8")?
                .into(),
        };
        locator.validate_retained_path()?;
        Ok(locator)
    }

    fn parent(
        &self,
        locator: &WorkspaceLocatorV2,
        create: bool,
    ) -> Result<(std::fs::File, std::ffi::CString), String> {
        use std::os::fd::AsRawFd;
        use std::os::fd::FromRawFd;
        self.verify()?;
        locator.validate_retained_path()?;
        let mut parts = locator.path.split('/').peekable();
        let mut directory = self.directory.try_clone().map_err(|e| e.to_string())?;
        while let Some(part) = parts.next() {
            let name = std::ffi::CString::new(part).map_err(|e| e.to_string())?;
            if parts.peek().is_none() {
                return Ok((directory, name));
            }
            if create {
                let rc = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
                if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                    return Err(format!(
                        "cannot create retained directory: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(format!(
                    "cannot open retained directory without following links: {}",
                    std::io::Error::last_os_error()
                ));
            }
            directory = unsafe { std::fs::File::from_raw_fd(fd) };
        }
        Err("retained locator has no filename".into())
    }

    fn open(&self, locator: &WorkspaceLocatorV2, create: bool) -> Result<RetainedFile, String> {
        use std::os::fd::AsRawFd;
        use std::os::fd::FromRawFd;
        use std::os::unix::fs::MetadataExt;
        let (parent, name) = self.parent(locator, create)?;
        let flags = libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if create {
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_APPEND
            } else {
                libc::O_RDONLY
            };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(format!(
                "cannot open retained regular file: {}",
                std::io::Error::last_os_error()
            ));
        }
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err("retained artifact is not one regular file".into());
        }
        self.verify()?;
        Ok(RetainedFile {
            file,
            locator: locator.clone(),
        })
    }

    pub(crate) fn create(&self, locator: &WorkspaceLocatorV2) -> Result<RetainedFile, String> {
        let file = self.open(locator, true)?;
        self.verify_file(&file)?;
        Ok(file)
    }

    pub(crate) fn verify_file(&self, retained: &RetainedFile) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let expected = retained.file.metadata().map_err(|e| e.to_string())?;
        let actual = self
            .open(&retained.locator, false)?
            .file
            .metadata()
            .map_err(|e| e.to_string())?;
        if !expected.is_file()
            || expected.nlink() != 1
            || (expected.dev(), expected.ino()) != (actual.dev(), actual.ino())
        {
            return Err("retained pathname no longer names the actual created/written file".into());
        }
        Ok(())
    }

    #[cfg(test)]
    fn fixture(path: &Path) -> Self {
        use std::os::unix::fs::MetadataExt;
        let directory = std::fs::File::open(path).unwrap();
        let metadata = directory.metadata().unwrap();
        Self {
            path: path.canonicalize().unwrap(),
            directory,
            identity: (metadata.dev(), metadata.ino()),
        }
    }
}

impl RetainedFile {
    fn write_exact(&self, bytes: &[u8]) -> Result<(), String> {
        use std::io::Write;
        (&self.file)
            .write_all(bytes)
            .and_then(|_| self.file.sync_all())
            .map_err(|e| e.to_string())?;
        self.check_bytes(bytes)
    }
    fn check_bytes(&self, expected: &[u8]) -> Result<(), String> {
        use std::os::unix::fs::FileExt;
        if self.file.metadata().map_err(|e| e.to_string())?.len() != expected.len() as u64 {
            return Err("retained context length changed".into());
        }
        let mut bytes = vec![0; expected.len()];
        self.file
            .read_exact_at(&mut bytes, 0)
            .map_err(|e| e.to_string())?;
        if bytes != expected {
            return Err("retained context bytes changed".into());
        }
        Ok(())
    }
}

impl RetainedAdmission {
    pub(crate) fn artifact_path(&self, root: &VerifiedStateRoot) -> std::path::PathBuf {
        root.path.join(&self.artifact.locator.path)
    }
    pub(crate) fn verify(
        &self,
        root: &VerifiedStateRoot,
        log: &RetainedFile,
    ) -> Result<(), String> {
        self.evidence.validate()?;
        root.verify_file(log)?;
        root.verify_file(&self.artifact)?;
        root.verify_file(&self.response)?;
        if self.evidence.log_identity() != Some(&log.locator)
            || self.evidence.artifact_locator() != Some(&self.artifact.locator)
        {
            return Err("retained evidence no longer binds original file identities".into());
        }
        self.response.check_bytes(&self.response_bytes)?;
        self.artifact.check_bytes(&self.evidence.context_bytes()?)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedValidationAdmission {
    authority: AdmissionAuthorityV1,
    floor: AdmissionFloorV1,
    target_tree: String,
    observed_at: String,
    response: Vec<u8>,
}

pub(crate) struct BoundExecutionPlan {
    pub(crate) cfg: DagConfig,
    pub(crate) second: Option<DagConfig>,
    pub(crate) canonical_sha256: String,
    pub(crate) execution_sha256: String,
    pub(crate) bindings: Vec<AdmissionPinBindingV1>,
}

fn graph_hash(cfg: &DagConfig, second: Option<&DagConfig>) -> String {
    fn identity(cfg: &DagConfig) -> Value {
        // The ordinary DAG serializer intentionally omits caller CPU policy
        // and some default bounds. Bind the complete in-memory configuration,
        // not just its persisted representation. Exhaustive destructuring
        // makes a future policy field an explicit review obligation here.
        let DagConfig {
            steps: _,
            description,
            resource_caps,
            mem_cap_factor,
            mem_cap_floor_bytes,
            outer_mem_safety_factor,
            default_step_timeout,
            default_jobs_flag,
            default_jobs_env,
            default_step_mem_cap_bytes,
            default_step_cpu_count,
            default_step_cpu_timeout,
            cpu_timeout_multiplier,
            cpu_timeout_platform,
            // dag_to_json below already includes both write-domain policy members.
            write_domain_policy: _,
        } = cfg;
        serde_json::json!({
            "dag": dag_to_json(cfg),
            "description": description, "resource_caps": resource_caps,
            "mem_cap_factor_bits": mem_cap_factor.to_bits(),
            "mem_cap_floor_bytes": mem_cap_floor_bytes,
            "outer_mem_safety_factor_bits": outer_mem_safety_factor.to_bits(),
            "default_step_timeout": default_step_timeout,
            "default_jobs_flag": default_jobs_flag, "default_jobs_env": default_jobs_env,
            "default_step_mem_cap_bytes": default_step_mem_cap_bytes,
            "default_step_cpu_count": default_step_cpu_count,
            "default_step_cpu_timeout": default_step_cpu_timeout,
            "cpu_timeout_multiplier_bits": cpu_timeout_multiplier.to_bits(),
            "cpu_timeout_platform": cpu_timeout_platform,
        })
    }
    admission_sha256(
        &serde_json::to_vec(&(identity(cfg), second.map(identity)))
            .expect("serializing graph identity cannot fail"),
    )
}

impl BoundExecutionPlan {
    pub(crate) fn bind(
        canonical: &DagConfig,
        second: Option<&DagConfig>,
        admission: Option<&AuthenticatedValidationAdmission>,
    ) -> Result<Self, String> {
        let mut result = Self {
            cfg: canonical.clone(),
            second: second.cloned(),
            canonical_sha256: graph_hash(canonical, second),
            execution_sha256: String::new(),
            bindings: Vec::new(),
        };
        if let Some(admission) = admission {
            for cfg in std::iter::once(&mut result.cfg).chain(result.second.iter_mut()) {
                for step in &mut cfg.steps {
                    let tag = step.tag();
                    let Some(expected) =
                        hermit_manifest_plan::validation_dag::admitted_pin_command(&tag, None)?
                    else {
                        continue;
                    };
                    if step.cmd != expected {
                        return Err(format!(
                            "refusing noncanonical admitted pin command for {tag}"
                        ));
                    }
                    let bound = hermit_manifest_plan::validation_dag::admitted_pin_command(
                        &tag,
                        Some(&admission.floor.sha),
                    )?
                    .ok_or("known pin command lost its renderer")?;
                    result.bindings.push(AdmissionPinBindingV1 {
                        node: tag,
                        base_sha: admission.floor.sha.clone(),
                        canonical_command_sha256: admission_sha256(step.cmd.as_bytes()),
                        execution_command_sha256: admission_sha256(bound.as_bytes()),
                    });
                    step.cmd = bound;
                }
            }
        }
        result.execution_sha256 = graph_hash(&result.cfg, result.second.as_ref());
        Ok(result)
    }

    pub(crate) fn verify(
        &self,
        canonical: &DagConfig,
        second: Option<&DagConfig>,
        admission: Option<&AuthenticatedValidationAdmission>,
    ) -> Result<(), String> {
        let expected = Self::bind(canonical, second, admission)?;
        let policy_matches = dag_config_carry_diff(&expected.cfg, &self.cfg).is_empty()
            && match (&expected.second, &self.second) {
                (Some(a), Some(b)) => dag_config_carry_diff(a, b).is_empty(),
                (None, None) => true,
                _ => false,
            };
        if self.canonical_sha256 != expected.canonical_sha256
            || self.execution_sha256 != expected.execution_sha256
            || graph_hash(&self.cfg, self.second.as_ref()) != expected.execution_sha256
            || self.bindings != expected.bindings
            || !policy_matches
        {
            return Err("admitted execution graph changed after its exact derivation".into());
        }
        Ok(())
    }

    pub(crate) fn cache_matches(
        &self,
        admission: &AuthenticatedValidationAdmission,
        evidence: &AdmissionEvidence,
    ) -> bool {
        let AdmissionEvidence::V2(evidence) = evidence else {
            return false;
        };
        admission.canonical()
            && evidence.validate().is_ok()
            && evidence.context.floor == admission.floor
            && evidence.context.authority.kind == "validate"
            && evidence.context.canonical_plan_sha256 == self.canonical_sha256
            && evidence.context.execution_plan_sha256 == self.execution_sha256
            && evidence.context.pin_bindings == self.bindings
    }

    pub(crate) fn retain(
        &self,
        admission: &AuthenticatedValidationAdmission,
        source: &Path,
        state: &VerifiedStateRoot,
        run_id: &str,
        started_at: &str,
        log: &RetainedFile,
    ) -> Result<RetainedAdmission, String> {
        state.verify_file(log)?;
        let name = log
            .locator
            .path
            .rsplit('/')
            .next()
            .ok_or("log has no filename")?;
        let locator = WorkspaceLocatorV2 {
            scope: "workspace".into(),
            path: format!("ignored/validate/admission/{name}/context.json"),
        };
        // The authenticated state tree may contain the source checkout. Prove
        // retention cannot add a nonignored input to source_identity.
        let artifact_path = state.path.join(&locator.path);
        let source = source.canonicalize().map_err(|e| e.to_string())?;
        if artifact_path.starts_with(&source) {
            let status = Command::new("git")
                .args(["check-ignore", "--quiet", "--no-index"])
                .arg(&artifact_path)
                .current_dir(&source)
                .status()
                .map_err(|e| e.to_string())?;
            if !status.success() {
                return Err("admission context would change nonignored source identity".into());
            }
        }
        let context = admission.context(
            run_id,
            started_at,
            log.locator.clone(),
            self.canonical_sha256.clone(),
            self.execution_sha256.clone(),
            self.bindings.clone(),
        );
        let bytes = admission_context_v2_bytes(&context)?;
        let artifact = state.create(&locator)?;
        artifact.write_exact(&bytes)?;
        let response_locator = WorkspaceLocatorV2 {
            scope: "workspace".into(),
            path: format!("ignored/validate/admission/{name}/authority-response.json"),
        };
        let response = state.create(&response_locator)?;
        response.write_exact(admission.response())?;
        let evidence = AdmissionEvidence::V2(AdmissionFloorEvidenceV2 {
            context,
            artifact: AdmissionContextArtifactV2 {
                locator,
                sha256: admission_sha256(&bytes),
                bytes: bytes.len() as u64,
            },
        });
        evidence.validate()?;
        let retained = RetainedAdmission {
            evidence,
            artifact,
            response,
            response_bytes: admission.response().to_vec(),
        };
        retained.verify(state, log)?;
        Ok(retained)
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_SHALLOW_FILE",
    ] {
        command.env_remove(key);
    }
    let out = command
        .arg("--no-replace-objects")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot query admitted Git objects: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "admitted Git query {args:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("admitted Git query returned invalid UTF-8: {e}"))
}

fn commit_tree(root: &Path, sha: &str) -> Result<String, String> {
    if !admission_hex(sha, 40) {
        return Err("admitted Git object is not a full lowercase SHA".into());
    }
    if git(
        root,
        &["rev-parse", "--verify", &format!("{sha}^{{commit}}")],
    )? != sha
    {
        return Err("admitted object is not the exact recorded commit".into());
    }
    let tree = git(root, &["rev-parse", "--verify", &format!("{sha}^{{tree}}")])?;
    if !admission_hex(&tree, 40) {
        return Err("admitted commit has an invalid tree identity".into());
    }
    Ok(tree)
}

pub(crate) fn post_run_main_observation(root: &Path) -> Result<Value, String> {
    let main = git(
        root,
        &["rev-parse", "--verify", "refs/remotes/origin/main^{commit}"],
    )?;
    commit_tree(root, &main)?;
    let target = git(root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    commit_tree(root, &target)?;
    let counts = git(
        root,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{main}...{target}"),
        ],
    )?;
    let counts = counts
        .split_whitespace()
        .map(str::parse::<i64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("invalid post-run main distance: {e}"))?;
    if counts.len() != 2 || counts.iter().any(|n| *n < 0) {
        return Err("invalid post-run main distance count".into());
    }
    Ok(
        serde_json::json!({"contract":"post-run-local-main-distance/v1", "observed_at":crate::utc_now(),
        "observed_main_sha":main, "target_sha":target, "behind":counts[0], "ahead":counts[1]}),
    )
}

pub(crate) fn pin_gate_passed(
    planned: &std::collections::BTreeSet<String>,
    outcomes: &[crate::StepOutcome],
) -> bool {
    let gates = ["pre.reverie_pin", "pre.reverie_pin_on_host"];
    gates.iter().any(|tag| planned.contains(*tag))
        && gates
            .iter()
            .filter(|tag| planned.contains(**tag))
            .all(|tag| {
                outcomes
                    .iter()
                    .any(|outcome| outcome.tag == *tag && outcome.ok)
            })
}

impl AuthenticatedValidationAdmission {
    pub(crate) fn from_status(
        root: &Path,
        status: &[u8],
        commit: &str,
        host: &str,
        boot_id: Option<&str>,
        identity_in_ancestry: &mut dyn FnMut(i32, u64) -> bool,
    ) -> Result<Self, String> {
        let value: Value = serde_json::from_slice(status)
            .map_err(|e| format!("the authority response is not valid JSON: {e}"))?;
        fn leaves<'a>(v: &'a Value, depth: usize, out: &mut Vec<&'a Value>) -> Result<(), String> {
            if depth > 16 {
                return Err("authority response nesting exceeds its structural bound".into());
            }
            if let Some(authorities) = v.get("authorities") {
                let authorities = authorities
                    .as_array()
                    .ok_or("authority slots must be an array")?;
                for leaf in authorities {
                    leaves(leaf, depth + 1, out)?;
                }
            } else {
                out.push(v);
            }
            Ok(())
        }
        let mut candidates = Vec::new();
        leaves(&value, 0, &mut candidates)?;
        let mut eligible = Vec::new();
        let mut reasons = Vec::new();
        for leaf in candidates {
            let bytes = serde_json::to_vec(leaf).map_err(|e| e.to_string())?;
            match crate::validate_lock_status_reason(
                &bytes,
                commit,
                host,
                boot_id,
                identity_in_ancestry,
            ) {
                Ok(()) => eligible.push(leaf),
                Err(e) => reasons.push(e),
            }
        }
        if eligible.len() != 1 {
            return Err(format!(
                "expected exactly one authenticated authority leaf, found {}: {}",
                eligible.len(),
                reasons.join("; ")
            ));
        }
        // Select first, parse its floor second. An eligible but malformed leaf
        // cannot borrow a different slot's convenient floor.
        let leaf = eligible[0];
        if leaf
            .get("admission_floor_reason")
            .is_some_and(|v| !v.is_null())
        {
            return Err(format!(
                "selected admission floor unavailable: {}",
                leaf["admission_floor_reason"]
            ));
        }
        let floor: AdmissionFloorV1 =
            serde_json::from_value(leaf.get("admission_floor").cloned().ok_or(
                "selected authority has no admission floor; parent capability unavailable",
            )?)
            .map_err(|e| format!("invalid selected admission floor: {e}"))?;
        let text = |object: &Value, key: &str| -> Result<String, String> {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("selected authority lacks {key}"))
        };
        let holder = &leaf["holder"];
        let owner = &leaf["owner"];
        let authority = AdmissionAuthorityV1 {
            slot: leaf
                .get("slot")
                .and_then(Value::as_u64)
                .ok_or("selected authority lacks integer slot")?,
            kind: text(holder, "kind")?,
            target: text(holder, "target")?,
            host: text(holder, "host")?,
            owner_host: text(owner, "host")?,
            owner_boot_id: text(owner, "boot_id")?,
            owner_pid: u32::try_from(owner["pid"].as_u64().ok_or("invalid owner PID")?)
                .map_err(|e| e.to_string())?,
            owner_start_ticks: owner["start_ticks"]
                .as_u64()
                .ok_or("invalid owner start ticks")?,
            response_sha256: admission_sha256(status),
        };
        let proof = Self {
            authority,
            floor,
            target_tree: commit_tree(root, commit)?,
            observed_at: crate::utc_now(),
            response: status.to_vec(),
        };
        proof.verify_source(root)?;
        Ok(proof)
    }

    pub(crate) fn target_tree(&self) -> &str {
        &self.target_tree
    }

    pub(crate) fn verify_source(&self, root: &Path) -> Result<(), String> {
        if git(root, &["rev-parse", "--verify", "HEAD"])? != self.authority.target
            || commit_tree(root, &self.authority.target)? != self.target_tree
            || commit_tree(root, &self.floor.sha)? != self.floor.tree
        {
            return Err("admitted target/floor commit or tree changed".into());
        }
        // It is a separately named observation, never a mutable reference.
        commit_tree(root, &self.floor.observed_main_sha)?;
        match (self.authority.kind.as_str(), self.floor.kind.as_str()) {
            ("validate", "current-main") if self.floor.sha == self.floor.observed_main_sha => {
                git(
                    root,
                    &[
                        "merge-base",
                        "--is-ancestor",
                        &self.floor.sha,
                        &self.authority.target,
                    ],
                )?;
            }
            ("frozen-validate", "frozen-target") if self.floor.sha == self.authority.target => {}
            _ => {
                return Err(
                    "selected floor does not match its authenticated authority kind".into(),
                );
            }
        }
        Ok(())
    }

    pub(crate) fn floor(&self) -> &AdmissionFloorV1 {
        &self.floor
    }

    pub(crate) fn canonical(&self) -> bool {
        self.authority.kind == "validate"
    }

    pub(crate) fn response(&self) -> &[u8] {
        &self.response
    }

    /// Mutable heartbeat/response times are intentionally absent. This witness
    /// can only reject a changed live authority; it never supplies authority.
    pub(crate) fn witness(&self) -> String {
        let a = &self.authority;
        admission_sha256(
            &serde_json::to_vec(&(
                a.slot,
                &a.kind,
                &a.target,
                &a.host,
                &a.owner_host,
                &a.owner_boot_id,
                a.owner_pid,
                a.owner_start_ticks,
                &self.floor,
                &self.target_tree,
            ))
            .expect("serializing fixed admission identity cannot fail"),
        )
    }

    pub(crate) fn check_witness(&self, root: &Path, run_state: &Path) -> Result<(), String> {
        if !run_state.starts_with(root.join("target/validation")) {
            return Err("admission witness is outside the verified run-state root".into());
        }
        let path = run_state.join("admission-floor-witness");
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            return Err(
                "verified replacement/nested run lacks its regular admission witness".into(),
            );
        }
        if std::fs::read(&path).map_err(|e| e.to_string())? != self.witness().as_bytes() {
            return Err(
                "live selected admission changed across replacement/nested execution".into(),
            );
        }
        Ok(())
    }

    pub(crate) fn write_witness(&self, root: &Path, run_state: &Path) -> Result<(), String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if !run_state.starts_with(root.join("target/validation")) {
            return Err(
                "admitted run-state witness must remain under ignored target/validation".into(),
            );
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(run_state.join("admission-floor-witness"))
            .map_err(|e| e.to_string())?;
        file.write_all(self.witness().as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        self.check_witness(root, run_state)
    }

    pub(crate) fn context(
        &self,
        run_id: &str,
        started_at: &str,
        log_identity: WorkspaceLocatorV2,
        canonical_plan_sha256: String,
        execution_plan_sha256: String,
        pin_bindings: Vec<AdmissionPinBindingV1>,
    ) -> AdmissionContextV2 {
        AdmissionContextV2 {
            contract: ADMISSION_CONTEXT_V2_CONTRACT.into(),
            observed_at: self.observed_at.clone(),
            target_sha: self.authority.target.clone(),
            target_tree: self.target_tree.clone(),
            host: self.authority.host.clone(),
            run_id: run_id.into(),
            started_at: started_at.into(),
            log_identity,
            authority: self.authority.clone(),
            floor: self.floor.clone(),
            canonical_plan_sha256,
            execution_plan_sha256,
            pin_bindings,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;
    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn source_root() -> std::path::PathBuf {
        let source = Path::new(file!());
        assert!(
            source.is_absolute(),
            "rust-script must retain the included source path: {}",
            source.display()
        );
        let root = source
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .unwrap();
        assert!(root.join("ci/dag/validate.json").is_file());
        root.to_path_buf()
    }

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "admission-consumer-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            let f = Self(path);
            f.command(&["init", "--quiet"]);
            f
        }
        fn command(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "core.hooksPath=/dev/null",
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
        fn commit(&self, name: &str) -> String {
            std::fs::write(self.0.join("input"), name).unwrap();
            self.command(&["add", "input"]);
            self.command(&["commit", "--quiet", "-m", name]);
            self.command(&["rev-parse", "HEAD"])
        }
        fn authority(&self, target: &str, floor: &str) -> Value {
            serde_json::json!({"schema_version":1,"slot":0,"state":"held","admissible":true,
                "lock_admissible":true,"reason_code":null,"cleanup_state":"active-bound","canonical_anchor_held":true,
                "holder":{"kind":"validate","target":target,"host":"fixture-host"},
                "owner":{"host":"fixture-host","boot_id":"fixture-boot","liveness":"alive","pid":4242,"start_ticks":987654},
                "admission_floor":{"kind":"current-main","sha":floor,"tree":self.command(&["rev-parse",&format!("{floor}^{{tree}}")]),"observed_main_sha":floor}})
        }
        fn admit(
            &self,
            value: &Value,
            target: &str,
        ) -> Result<AuthenticatedValidationAdmission, String> {
            AuthenticatedValidationAdmission::from_status(
                &self.0,
                &serde_json::to_vec(value).unwrap(),
                target,
                "fixture-host",
                Some("fixture-boot"),
                &mut |pid, ticks| pid == 4242 && ticks == 987654,
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn admission_selects_one_authenticated_leaf_and_never_borrows_a_floor() {
        let f = Fixture::new();
        let base = f.commit("base");
        let target = f.commit("target");
        let good = f.authority(&target, &base);
        assert!(f.admit(&good, &target).unwrap().canonical());
        let mut missing = good.clone();
        missing.as_object_mut().unwrap().remove("admission_floor");
        assert!(
            f.admit(&missing, &target)
                .unwrap_err()
                .contains("no admission floor")
        );
        let mut foreign = good.clone();
        foreign["slot"] = 1.into();
        foreign["owner"]["pid"] = 5000.into();
        assert!(
            f.admit(
                &serde_json::json!({"authorities":[missing,foreign]}),
                &target
            )
            .is_err()
        );
        assert!(
            f.admit(
                &serde_json::json!({"authorities":[good.clone(),good.clone()]}),
                &target
            )
            .is_err()
        );
        for (key, val) in [
            ("pid", serde_json::json!(4243)),
            ("start_ticks", serde_json::json!(987655)),
            ("host", serde_json::json!("other-host")),
            ("boot_id", serde_json::json!("other-boot")),
            ("liveness", serde_json::json!("dead")),
        ] {
            let mut bad = good.clone();
            bad["owner"][key] = val;
            assert!(f.admit(&bad, &target).is_err(), "{key}");
        }
        let mut bad = good.clone();
        bad["admission_floor"]["tree"] = "0".repeat(40).into();
        assert!(f.admit(&bad, &target).is_err());
        let mut frozen = f.authority(&target, &target);
        frozen["holder"]["kind"] = "frozen-validate".into();
        frozen["admissible"] = false.into();
        frozen["reason_code"] = "canonical-holder-kind-not-validate".into();
        frozen["admission_floor"]["kind"] = "frozen-target".into();
        assert!(!f.admit(&frozen, &target).unwrap().canonical());
    }

    #[test]
    fn admission_binds_the_actual_process_generation() {
        let f = Fixture::new();
        let target = f.commit("target");
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        let pid = std::process::id() as i32;
        let (_, ticks) = crate::validate_runtime::process_identity(pid).unwrap();
        let mut status = f.authority(&target, &target);
        status["owner"]["pid"] = pid.into();
        status["owner"]["start_ticks"] = ticks.into();
        status["owner"]["boot_id"] = boot.trim().into();
        let admit = |value: &Value| {
            AuthenticatedValidationAdmission::from_status(
                &f.0,
                &serde_json::to_vec(value).unwrap(),
                &target,
                "fixture-host",
                Some(boot.trim()),
                &mut crate::validate_runtime::identity_in_ancestry,
            )
        };
        assert!(admit(&status).is_ok());
        status["owner"]["start_ticks"] = (ticks + 1).into();
        assert!(admit(&status).is_err());
        status["owner"]["start_ticks"] = ticks.into();
        status["owner"]["pid"] = 1.into();
        assert!(admit(&status).is_err());
    }

    #[test]
    fn admission_immutable_git_view_survives_main_advance_but_refuses_grafts_and_replace_refs() {
        let f = Fixture::new();
        let base = f.commit("base");
        let target = f.commit("target");
        f.command(&["update-ref", "refs/remotes/origin/main", &base]);
        let proof = f.admit(&f.authority(&target, &base), &target).unwrap();
        let later = f.commit("later");
        f.command(&["checkout", "--quiet", "--detach", &target]);
        f.command(&["update-ref", "refs/remotes/origin/main", &later]);
        proof.verify_source(&f.0).unwrap();
        assert_eq!(post_run_main_observation(&f.0).unwrap()["behind"], 1);
        let falsely_admitted = f.authority(&target, &later);
        assert!(f.admit(&falsely_admitted, &target).is_err());
        // A literal commit can be made to claim false ancestry by .git/info/grafts.
        std::fs::write(
            f.0.join(".git/info/grafts"),
            format!("{target} {later}\n{later}\n"),
        )
        .unwrap();
        assert!(
            Command::new("git")
                .args(["merge-base", "--is-ancestor", &later, &target])
                .current_dir(&f.0)
                .status()
                .unwrap()
                .success()
        );
        assert!(f.admit(&falsely_admitted, &target).is_err());
        std::fs::remove_file(f.0.join(".git/info/grafts")).unwrap();
        f.command(&["replace", &target, &later]);
        assert!(f.admit(&falsely_admitted, &target).is_err());
        assert_eq!(commit_tree(&f.0, &target).unwrap(), proof.target_tree);
    }

    #[test]
    fn admission_execution_view_preserves_entire_plan_and_reexec_witness() {
        let f = Fixture::new();
        let base = f.commit("base");
        let target = f.commit("target");
        let proof = f.admit(&f.authority(&target, &base), &target).unwrap();
        let source = source_root();
        let canonical = crate::validate_plan::validation_config(&source).unwrap();
        let view = BoundExecutionPlan::bind(&canonical, None, Some(&proof)).unwrap();
        view.verify(&canonical, None, Some(&proof)).unwrap();
        assert!(view.bindings.iter().any(|b| b.node == "pre.reverie_pin"));
        assert!(view.bindings.iter().any(|b| b.node == "check.lint_checks"));
        for changed in &view.cfg.steps {
            let old = canonical
                .steps
                .iter()
                .find(|s| s.tag() == changed.tag())
                .unwrap();
            if view.bindings.iter().any(|b| b.node == changed.tag()) {
                assert!(changed.cmd.ends_with(&format!("'{base}'")));
            } else {
                assert_eq!(changed.cmd, old.cmd);
            }
        }
        let mut poisoned = canonical.clone();
        poisoned.steps[0].timeout += 1;
        assert!(view.verify(&poisoned, None, Some(&proof)).is_err());
        for mutate in [
            |cfg: &mut DagConfig| cfg.cpu_timeout_multiplier += 1.0,
            |cfg: &mut DagConfig| cfg.cpu_timeout_platform.push_str("changed"),
            |cfg: &mut DagConfig| cfg.default_step_cpu_timeout += 1,
            |cfg: &mut DagConfig| {
                cfg.default_step_cpu_count = Some(cfg.default_step_cpu_count.unwrap_or(0) + 1)
            },
            |cfg: &mut DagConfig| {
                cfg.default_step_mem_cap_bytes =
                    Some(cfg.default_step_mem_cap_bytes.unwrap_or(0) + 1)
            },
        ] {
            let mut poisoned = canonical.clone();
            mutate(&mut poisoned);
            assert_ne!(graph_hash(&canonical, None), graph_hash(&poisoned, None));
            assert!(view.verify(&poisoned, None, Some(&proof)).is_err());
            let mut changed = BoundExecutionPlan::bind(&canonical, None, Some(&proof)).unwrap();
            mutate(&mut changed.cfg);
            assert!(changed.verify(&canonical, None, Some(&proof)).is_err());
        }
        let mut poisoned = canonical.clone();
        poisoned
            .steps
            .iter_mut()
            .find(|s| s.tag() == "pre.reverie_pin")
            .unwrap()
            .cmd
            .push_str("; true");
        assert!(BoundExecutionPlan::bind(&poisoned, None, Some(&proof)).is_err());
        let state = f.0.join("target/validation/run");
        std::fs::create_dir_all(&state).unwrap();
        proof.write_witness(&f.0, &state).unwrap();
        proof.check_witness(&f.0, &state).unwrap();
        let changed = f.admit(&f.authority(&target, &target), &target).unwrap();
        assert!(changed.check_witness(&f.0, &state).is_err());
        assert!(proof.check_witness(&f.0, &f.0).is_err());
    }

    fn require_write_domain_hash_and_cache_binding(mutate: fn(&mut DagConfig)) {
        let f = Fixture::new();
        std::fs::write(f.0.join(".gitignore"), "ignored/\n").unwrap();
        f.command(&["add", ".gitignore"]);
        let base = f.commit("base");
        let target = f.commit("target");
        let proof = f.admit(&f.authority(&target, &base), &target).unwrap();
        let canonical = crate::validate_plan::validation_config(&source_root()).unwrap();
        let mut second = canonical.clone();
        second.steps.clear();
        let state = VerifiedStateRoot::fixture(&f.0);
        let mut missed = Vec::new();
        for (label, has_second, mutate_second) in [
            ("only-graph", false, false),
            ("first-of-two", true, false),
            ("second-graph", true, true),
        ] {
            let second_ref = has_second.then_some(&second);
            let original = BoundExecutionPlan::bind(&canonical, second_ref, Some(&proof)).unwrap();
            original
                .verify(&canonical, second_ref, Some(&proof))
                .unwrap();
            let log_path = f.0.join(format!("ignored/validate/{label}.log"));
            let log = state.create(&state.locator(&log_path).unwrap()).unwrap();
            log.write_exact(b"write-domain binding control\n").unwrap();
            let retained = original
                .retain(&proof, &f.0, &state, label, "2026-09-18T15:00:00Z", &log)
                .unwrap();
            assert!(original.cache_matches(&proof, &retained.evidence));
            let mut changed = canonical.clone();
            let mut changed_second = second.clone();
            mutate(if mutate_second {
                &mut changed_second
            } else {
                &mut changed
            });
            let current = BoundExecutionPlan::bind(
                &changed,
                has_second.then_some(&changed_second),
                Some(&proof),
            )
            .unwrap();
            let canonical_changed = original.canonical_sha256 != current.canonical_sha256;
            let execution_changed = original.execution_sha256 != current.execution_sha256;
            let cache_rejected = !current.cache_matches(&proof, &retained.evidence);
            // The existing in-process carry check must continue to refuse even
            // when the separate durable digest/cache assertions expose a gap.
            assert!(
                current
                    .verify(&canonical, second_ref, Some(&proof))
                    .is_err()
            );
            eprintln!(
                "{label}: canonical_changed={canonical_changed} execution_changed={execution_changed} cache_rejected={cache_rejected}"
            );
            if !canonical_changed || !execution_changed || !cache_rejected {
                missed.push((label, canonical_changed, execution_changed, cache_rejected));
            }
        }
        assert!(
            missed.is_empty(),
            "write-domain member was not bound: {missed:?}"
        );
    }

    #[test]
    fn admission_write_domain_require_explicit_changes_hash_and_cache() {
        require_write_domain_hash_and_cache_binding(|cfg| {
            cfg.write_domain_policy.require_explicit = !cfg.write_domain_policy.require_explicit;
        });
    }

    #[test]
    fn admission_write_domain_allowed_domains_changes_hash_and_cache() {
        require_write_domain_hash_and_cache_binding(|cfg| {
            assert!(
                cfg.write_domain_policy
                    .allowed_domains
                    .insert("opposing-domain".into())
            );
        });
    }

    #[test]
    fn admission_make_transport_is_literal_and_does_not_leak_to_recursive_fixtures() {
        let f = Fixture::new();
        let source = source_root();
        let makefile = source.join("Makefile");
        let probe = f.0.join("probe.mk");
        std::fs::write(&probe, "\n.PHONY: admission-probe admission-child\nadmission-probe:\n\t@printf 'PIN=%s\\n' \"$(_VALIDATE_PIN_ARG)\"\n\t@$(MAKE) --no-print-directory -s -f '$(lastword $(MAKEFILE_LIST))' admission-child\nadmission-child:\n\t@printf 'CHILD_PIN=%s\\nOTHER=%s\\n' \"$(origin VALIDATE_REVERIE_PIN_BASE_REF)\" \"$(OTHER)\"\n").unwrap();
        let run = |arg: Option<&str>, ambient: bool| {
            let mut command = Command::new("make");
            command
                .args(["--no-print-directory", "-s", "-f"])
                .arg(&makefile)
                .arg("-f")
                .arg(&probe)
                .arg("admission-probe")
                .arg("OTHER=two words")
                .current_dir(&f.0)
                .env_remove("MAKEFLAGS")
                .env_remove("MFLAGS")
                .env_remove("MAKEOVERRIDES")
                .env_remove("VALIDATE_REVERIE_PIN_BASE_REF");
            if let Some(value) = arg {
                if ambient {
                    command.env("VALIDATE_REVERIE_PIN_BASE_REF", value);
                } else {
                    command.arg(format!("VALIDATE_REVERIE_PIN_BASE_REF={value}"));
                }
            }
            command.output().unwrap()
        };
        let sha = "a".repeat(40);
        let ordinary = run(None, false);
        assert!(
            ordinary.status.success(),
            "{}",
            String::from_utf8_lossy(&ordinary.stderr)
        );
        assert!(String::from_utf8_lossy(&ordinary.stdout).contains("PIN=\n"));
        let good = run(Some(&sha), false);
        assert!(
            good.status.success(),
            "{}",
            String::from_utf8_lossy(&good.stderr)
        );
        let out = String::from_utf8(good.stdout).unwrap();
        assert!(out.contains(&format!("PIN=--base-ref '{sha}'\n")), "{out}");
        assert!(
            out.contains("CHILD_PIN=undefined\nOTHER=two words\n"),
            "{out}"
        );
        assert!(!run(Some(&sha), true).status.success());
        let marker = f.0.join("must-not-exist");
        for bad in [
            String::new(),
            "a".repeat(39),
            "a".repeat(41),
            "A".repeat(40),
            format!("$(shell touch {})", marker.display()),
            "';false;'".into(),
            "a a".into(),
        ] {
            assert!(!run(Some(&bad), false).status.success(), "accepted {bad:?}");
            assert!(!marker.exists(), "expanded untrusted Make value");
        }
    }

    #[test]
    fn admission_retention_cache_and_receipt_echo_keep_original_proof() {
        let f = Fixture::new();
        std::fs::write(f.0.join(".gitignore"), "ignored/\n").unwrap();
        f.command(&["add", ".gitignore"]);
        let base = f.commit("base");
        let target = f.commit("target");
        let proof = f.admit(&f.authority(&target, &base), &target).unwrap();
        let canonical = crate::validate_plan::validation_config(&source_root()).unwrap();
        let view = BoundExecutionPlan::bind(&canonical, None, Some(&proof)).unwrap();
        let state = VerifiedStateRoot::fixture(&f.0);
        let log_path = f.0.join("ignored/validate/driver.log");
        let log = state.create(&state.locator(&log_path).unwrap()).unwrap();
        log.write_exact(b"original driver log\n").unwrap();
        let before = f.command(&["status", "--porcelain=v1", "--untracked-files=all"]);
        let retained = view
            .retain(
                &proof,
                &f.0,
                &state,
                "fixture-actual-run",
                "2026-09-18T15:00:00Z",
                &log,
            )
            .unwrap();
        let evidence = &retained.evidence;
        assert_eq!(
            before,
            f.command(&["status", "--porcelain=v1", "--untracked-files=all"])
        );
        assert_eq!(
            std::fs::read(retained.artifact_path(&state)).unwrap(),
            evidence.context_bytes().unwrap()
        );
        assert!(view.cache_matches(&proof, evidence));
        let newer = f.admit(&f.authority(&target, &target), &target).unwrap();
        assert!(!view.cache_matches(&newer, evidence));
        let mut different_policy = canonical.clone();
        different_policy.cpu_timeout_multiplier += 1.0;
        assert!(
            !BoundExecutionPlan::bind(&different_policy, None, Some(&proof))
                .unwrap()
                .cache_matches(&proof, evidence)
        );
        let mut frozen = f.authority(&target, &target);
        frozen["holder"]["kind"] = "frozen-validate".into();
        frozen["admissible"] = false.into();
        frozen["reason_code"] = "canonical-holder-kind-not-validate".into();
        frozen["admission_floor"]["kind"] = "frozen-target".into();
        assert!(!view.cache_matches(&f.admit(&frozen, &target).unwrap(), evidence));
        let AdmissionEvidence::V2(v2) = evidence else {
            panic!("new emission must be v2")
        };
        let row = serde_json::json!({"result":"pass", "commit":target, "tree":v2.context.target_tree,
            "host":v2.context.host,"run_id":v2.context.run_id,"started_at":v2.context.started_at,
            "log_file":log_path,"log_identity":log.locator,"admission_floor_evidence":evidence,
            "cell_results":{"run_id":v2.context.run_id}});
        let raw = serde_json::to_string(&row).unwrap();
        assert!(crate::validate_history::admission_cache_row(&raw).is_some());
        let duplicate = raw.replacen(
            "\"cell_results\":{\"run_id\":",
            "\"cell_results\":{\"run_id\":\"wrong\",\"run_id\":",
            1,
        );
        assert_ne!(raw, duplicate);
        assert!(crate::validate_history::admission_cache_row(&duplicate).is_none());
        let repeated_claim = raw.replacen(
            "\"admission_floor_evidence\":",
            "\"admission_floor_evidence\":null,\"admission_floor_evidence\":",
            1,
        );
        assert!(crate::validate_history::admission_cache_row(&repeated_claim).is_none());
        let failure = duplicate.replacen("\"result\":\"pass\"", "\"result\":\"fail\"", 1);
        assert!(
            crate::validate_history::admission_cache_row(&failure).is_some(),
            "invalid claim must not erase the recorded failure"
        );
        assert_eq!(
            crate::validate_history::admission_cache_row(
                r#"{"result":"pass","value":1,"value":2}"#
            )
            .unwrap()["value"],
            2
        );
        // Exercise the production command builder across a real subprocess.
        // This fixture models the parent's two transport modes, not its event
        // authority. Without the preserving option it visibly loses duplicate
        // claims. The complete real parent adapter is qualified separately.
        let adapter = f.0.join("ignored/adapter.py");
        let adapter_input = f.0.join("ignored/rows.jsonl");
        std::fs::write(
            &adapter,
            concat!(
                "import json,sys\nfrom pathlib import Path\n",
                "raw=Path(__file__).with_name('rows.jsonl').read_text()\n",
                "if sys.argv[1:] == ['rows','--preserve-admission']:\n",
                "    sys.stdout.write(raw)\n",
                "elif sys.argv[1:] == ['rows']:\n",
                "    print('\\n'.join(json.dumps(json.loads(line)) for line in raw.splitlines()))\n",
                "else:\n",
                "    sys.exit(2)\n",
            ),
        )
        .unwrap();
        for (name, input, keep) in [
            ("valid", raw.as_str(), true),
            ("duplicate nested identity", duplicate.as_str(), false),
            ("duplicate claim", repeated_claim.as_str(), false),
            ("recorded failure", failure.as_str(), true),
            ("legacy", r#"{"result":"pass","value":1,"value":2}"#, true),
        ] {
            std::fs::write(&adapter_input, format!("{input}\n")).unwrap();
            let output = crate::validate_history::canonical_ledger_reader(&adapter)
                .output()
                .unwrap();
            assert!(output.status.success(), "{name}: {output:?}");
            assert_eq!(output.stdout, format!("{input}\n").as_bytes(), "{name}");
            assert_eq!(
                crate::validate_history::admission_cache_row(
                    std::str::from_utf8(&output.stdout).unwrap()
                )
                .is_some(),
                keep,
                "{name}"
            );
        }
        let unsafe_root = f.0.join("not-ignored");
        assert!(state.locator(&unsafe_root.join("driver.log")).is_err());
        assert!(!unsafe_root.exists());
        // Even an authenticated state-root path cannot introduce a nonignored
        // source input. This opposing repository has no ignore for retention.
        let unignored = Fixture::new();
        let unignored_state = VerifiedStateRoot::fixture(&unignored.0);
        let unignored_log = unignored_state
            .create(&WorkspaceLocatorV2 {
                scope: "workspace".into(),
                path: "ignored/validate/driver.log".into(),
            })
            .unwrap();
        assert!(
            view.retain(
                &proof,
                &unignored.0,
                &unignored_state,
                "fixture-actual-run",
                "2026-09-18T15:00:00Z",
                &unignored_log
            )
            .is_err()
        );
        assert!(!unignored.0.join("ignored/validate/admission").exists());

        // Exercise the production subprocess/echo boundary with a declared
        // protocol fixture. This is not an independently authenticated parent.
        let tool = f.0.join("ignored/tool");
        let helper = tool.join("ci-hub/validate/finalize_receipt.py");
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        let echo = serde_json::json!({"admission_floor_evidence":evidence,
            "base_observation":{"contract":"post-run-local-main-merge-base/v1"}});
        let install = |value: &Value| {
            let raw = serde_json::to_string(value).unwrap();
            std::fs::write(&helper, format!("import sys\nsys.stdout.write({raw:?})\n")).unwrap();
        };
        let call = || {
            crate::admitted_receipt_evidence(
                Some(&tool),
                &f.0,
                &log_path,
                &target,
                Some((&retained, &state, &log)),
            )
        };
        install(&echo);
        assert!(call().is_ok());
        let mut altered = evidence.clone();
        let AdmissionEvidence::V2(ref mut changed) = altered else {
            panic!("new emission must be v2")
        };
        changed.context.run_id = "different-run".into();
        let bytes = admission_context_v2_bytes(&changed.context).unwrap();
        changed.artifact.sha256 = admission_sha256(&bytes);
        changed.artifact.bytes = bytes.len() as u64;
        let mut wrong = echo.clone();
        wrong["admission_floor_evidence"] = serde_json::to_value(altered).unwrap();
        install(&wrong);
        assert!(call().is_err());
        install(&serde_json::json!({}));
        assert!(call().is_err());
        std::fs::write(&helper, "raise SystemExit(19)\n").unwrap();
        assert!(call().is_err());
        install(&echo);
        let original = std::fs::read(&helper).unwrap();
        let duplicate = serde_json::to_string(&echo).unwrap();
        let duplicate = duplicate.replacen(
            "\"contract\":\"ci-hub-admission-floor/v2\"",
            "\"contract\":\"ci-hub-admission-floor/v2\",\"contract\":\"ci-hub-admission-floor/v2\"",
            1,
        );
        std::fs::write(
            &helper,
            format!("import sys\nsys.stdout.write({duplicate:?})\n"),
        )
        .unwrap();
        assert!(call().is_err());
        std::fs::write(&helper, original).unwrap();
        let response_path = state.path().join(&retained.response.locator.path);
        let original_response = std::fs::read(&response_path).unwrap();
        let mut changed_response = original_response.clone();
        changed_response[0] ^= 1;
        std::fs::write(&response_path, &changed_response).unwrap();
        assert!(call().is_err());
        std::fs::write(&response_path, &original_response).unwrap();
        assert!(call().is_ok());
        // A source-drift failure must still identify the originally admitted
        // target in a no-cell row, while live source verification refuses.
        let mut failed = row.clone();
        failed.as_object_mut().unwrap().remove("cell_results");
        failed["result"] = "fail".into();
        let later = f.commit("later-source");
        assert!(proof.verify_source(&f.0).is_err());
        assert_ne!(commit_tree(&f.0, &later).unwrap(), proof.target_tree());
        failed["tree"] = proof.target_tree().into();
        evidence
            .validate_for_row(&serde_json::from_value(failed.clone()).unwrap())
            .unwrap();
        failed["tree"] = commit_tree(&f.0, &later).unwrap().into();
        assert!(
            evidence
                .validate_for_row(&serde_json::from_value(failed).unwrap())
                .is_err()
        );
        std::fs::write(retained.artifact_path(&state), b"tampered").unwrap();
        assert!(call().is_err());
        assert_eq!(crate::exit_code_with_evidence_refusal(0), 75);
        for failure in [1, 2, 75, 122] {
            assert_eq!(crate::exit_code_with_evidence_refusal(failure), failure);
        }
    }

    #[test]
    fn admission_retained_root_refuses_links_replacements_and_outside_paths() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let outside = Fixture::new();
        let state = VerifiedStateRoot::fixture(&f.0);
        assert!(
            state
                .locator(&outside.0.join("ignored/validate/log"))
                .is_err()
        );
        for path in [
            "ignored/validate/../log",
            "ignored/validate//log",
            "ignored/validation/log",
            "ignored/validate/\u{0085}",
        ] {
            assert!(
                state
                    .create(&WorkspaceLocatorV2 {
                        scope: "workspace".into(),
                        path: path.into()
                    })
                    .is_err()
            );
        }
        std::fs::create_dir(f.0.join("ignored")).unwrap();
        symlink(&outside.0, f.0.join("ignored/validate")).unwrap();
        let locator = WorkspaceLocatorV2 {
            scope: "workspace".into(),
            path: "ignored/validate/log".into(),
        };
        assert!(state.create(&locator).is_err());
        assert!(!outside.0.join("log").exists());
        std::fs::remove_file(f.0.join("ignored/validate")).unwrap();
        let log = state.create(&locator).unwrap();
        log.write_exact(b"original").unwrap();
        state.verify_file(&log).unwrap();
        std::fs::rename(
            f.0.join(&locator.path),
            f.0.join("ignored/validate/original"),
        )
        .unwrap();
        std::fs::write(f.0.join(&locator.path), b"replacement").unwrap();
        assert!(state.verify_file(&log).is_err());
        let moved = f.0.with_extension("moved");
        std::fs::rename(&f.0, &moved).unwrap();
        std::fs::create_dir(&f.0).unwrap();
        assert!(state.verify().is_err());
        std::fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn admission_tee_writes_created_descriptor_even_after_path_swap_and_refuses_receipt() {
        use std::io::Write;
        let f = Fixture::new();
        let state = VerifiedStateRoot::fixture(&f.0);
        let locator = WorkspaceLocatorV2 {
            scope: "workspace".into(),
            path: "ignored/validate/driver.log".into(),
        };
        let log = state.create(&locator).unwrap();
        let moved = f.0.join("ignored/validate/actual.log");
        std::fs::rename(f.0.join(&locator.path), &moved).unwrap();
        std::fs::write(f.0.join(&locator.path), b"foreign file\n").unwrap();
        let mut tee = crate::spawn_durable_tee(&log.file).unwrap();
        tee.stdin
            .take()
            .unwrap()
            .write_all(b"actual descriptor bytes\n")
            .unwrap();
        assert!(tee.wait().unwrap().success());
        assert_eq!(std::fs::read(moved).unwrap(), b"actual descriptor bytes\n");
        assert_eq!(
            std::fs::read(f.0.join(&locator.path)).unwrap(),
            b"foreign file\n"
        );
        assert!(state.verify_file(&log).is_err());
    }
}
