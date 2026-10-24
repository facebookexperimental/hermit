//! Native input enrollment at the existing common-container launch boundary.
//! No target is inferred from argv, a manifest row name or the backend name.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use reverie::process::Command;
use reverie::process::Container;
use sha2::Digest;
use sha2::Sha256;

use super::container::declared_inputs::DeclaredInputRoot;
use super::container::declared_inputs::DeclaredRootAdditions;
use super::container::declared_inputs::PreparedDeclaredRoot;
use super::container::declared_inputs::RuntimeFile;
use super::native_exec_artifacts::QualifiedNativeArtifacts;
use super::native_exec_inputs::DynamicExecInputs;
use super::native_exec_inputs::GuestPath;
use super::native_exec_inputs::InputRole;
use super::native_exec_inputs::OpenedGuestRoot;
use super::native_exec_inputs::adopt_inputs;
use super::native_exec_inputs::capture_inputs;
use super::native_exec_inputs::retained_bytes;
use super::native_exec_inputs::seal_bytes;
use super::native_exec_run::NativeEndpoints;
use super::verify_inputs::SharedInputs;
use super::verify_inputs::SharedObjectMeta;

/// One capture retained by the caller through both real verify executions.
pub(super) struct Capture {
    declaration: File,
    artifacts: Arc<QualifiedNativeArtifacts>,
    additions: DeclaredRootAdditions,
    staging: tempfile::TempDir,
}
impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No descriptor paths, private role bytes or authority labels.
        f.write_str("Capture(<retained native inputs>)")
    }
}
impl Capture {
    /// Freeze the declared source graph and common data exactly once, before
    /// either physical root is prepared. Compilation expectations come from the
    /// real build-produced owner, never the declaration or a neighboring file.
    pub(super) fn prepare(declaration: &Path, command: &Command) -> Result<Arc<Self>> {
        let capture = Self::prepare_shared(declaration)?;
        capture.require_initial(command)?;
        Ok(Arc::new(capture))
    }
    /// Holder-side capture, independent of the pair program's command line.
    /// The bounded original byte-array declaration joins all adopted rows.
    pub(super) fn prepare_shared(declaration_path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(declaration_path)?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        let declaration_inputs = DynamicExecInputs::parse(&bytes)?;
        let opened = OpenedGuestRoot::open(Path::new("/"))?;
        let inputs = Arc::new(capture_inputs(&opened, declaration_inputs)?);
        let artifacts = Arc::new(QualifiedNativeArtifacts::capture(inputs)?);
        let declaration = seal_bytes(&bytes, InputRole::Data)?;
        let mut additions = DeclaredRootAdditions::default();
        artifacts.root_files(&mut additions)?;
        additions.capture_common_inputs()?;
        Self::from_retained(declaration, artifacts, additions)
    }
    pub(super) fn require_initial(&self, command: &Command) -> Result<()> {
        self.artifacts
            .inputs()
            .require_initial(&GuestPath::new(command.get_program().as_bytes().to_vec())?)
            .map(|_| ())
    }
    fn from_retained(
        declaration: File,
        artifacts: Arc<QualifiedNativeArtifacts>,
        additions: DeclaredRootAdditions,
    ) -> Result<Self> {
        // The common run mounts a private /tmp. Keep all staging names in the
        // actual caller's uncovered directory; the pin utility independently
        // refuses any later lexical replacement or covered mount source.
        let parent = std::env::current_dir()?;
        ensure!(
            !parent.starts_with("/tmp"),
            "native staging parent is covered by common /tmp"
        );
        let staging = tempfile::Builder::new()
            .prefix(".hermit-native-")
            .tempdir_in(&parent)
            .context("create caller-owned native staging directory")?;
        Ok(Self {
            declaration,
            artifacts,
            additions,
            staging,
        })
    }
    /// Plan exactly one common root and prebind endpoints, with no Tokio
    /// runtime or worker. All sealed backing remains shared across executions.
    pub(super) fn plan(&self, container: &mut Container) -> Result<Plan> {
        self.artifacts.inputs().recheck()?;
        let endpoints = NativeEndpoints::bind(self.staging.path())?;
        let mut additions = self.additions.try_clone()?;
        endpoints.add_to(&mut additions)?;
        let mut root = DeclaredInputRoot::prepare(
            self.artifacts.inputs().clone(),
            additions,
            self.staging.path(),
        )?;
        let runtime_files = root
            .runtime_files()
            .iter()
            .map(RuntimeFile::try_clone)
            .collect::<Result<Vec<_>>>()?;
        root.configure(container)?;
        Ok(Plan {
            root,
            runtime_files,
            artifacts: self.artifacts.clone(),
            endpoints,
        })
    }
}

fn require_role_path(actual: &Option<PathBuf>, expected: &Option<PathBuf>) -> Result<()> {
    ensure!(
        actual.as_ref().map(|path| path.as_os_str().as_bytes())
            == expected.as_ref().map(|path| path.as_os_str().as_bytes())
            && actual
                .as_ref()
                .is_none_or(|path| !path.as_os_str().is_empty()),
        "native shared role path differs from original placement"
    );
    Ok(())
}
fn producer() -> String {
    let source = detcore_model::liteinst_native_exec_sources::digest(
        reverie_inguest::native_exec_sources::digest(),
    );
    format!(
        "hermit-native-capture/v1:{}",
        source
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}
fn exported(
    path: Option<PathBuf>,
    role: &str,
    fd: BorrowedFd<'_>,
    native_role: InputRole,
) -> Result<(SharedObjectMeta, OwnedFd)> {
    let owned = fd.try_clone_to_owned()?;
    let file = File::from(owned);
    let bytes = retained_bytes(&file, native_role, 64 * 1024 * 1024)?;
    let metadata = file.metadata()?;
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    ensure!(seals >= 0, "shared native seals could not be read");
    Ok((
        SharedObjectMeta {
            path,
            role: role.into(),
            size: metadata.len(),
            sha256: Sha256::digest(bytes).into(),
            seals: seals as u32,
        },
        file.into(),
    ))
}
fn input_role(role: &str) -> Option<InputRole> {
    match role {
        "native-input-executable" => Some(InputRole::Executable),
        "native-input-loader" => Some(InputRole::Loader),
        "native-input-data" => Some(InputRole::Data),
        _ => None,
    }
}
fn primary_interpreter(inputs: &super::native_exec_inputs::HeldDynamicInputs) -> Result<PathBuf> {
    inputs
        .files()
        .iter()
        .filter(|file| file.role == InputRole::Executable)
        .filter_map(|file| file.interpreter.as_ref())
        .min()
        .map(|path| path.as_path().to_path_buf())
        .context("native cohort has no interpreter placement")
}
impl SharedInputs for Capture {
    fn export(&self) -> Result<(String, Vec<(SharedObjectMeta, OwnedFd)>)> {
        self.artifacts.inputs().recheck()?;
        let mut objects = Vec::new();
        for file in self.artifacts.inputs().files() {
            let role = match file.role {
                InputRole::Executable => "native-input-executable",
                InputRole::Loader => "native-input-loader",
                InputRole::Data => "native-input-data",
            };
            objects.push(exported(
                Some(file.path.as_path().to_path_buf()),
                role,
                file.descriptor(),
                file.role,
            )?);
        }
        for (path, role, fd, native_role) in [
            (
                None,
                "native-declaration/v1",
                self.declaration.as_fd(),
                InputRole::Data,
            ),
            (
                Some(primary_interpreter(self.artifacts.inputs())?),
                "native-bootstrap",
                self.artifacts.bootstrap(),
                InputRole::Executable,
            ),
            (
                Some(
                    Path::new(std::ffi::OsStr::from_bytes(
                        super::native_exec_artifacts::RUNNER_PATH,
                    ))
                    .into(),
                ),
                "native-runner",
                self.artifacts.runner(),
                InputRole::Executable,
            ),
            (
                Some(
                    Path::new(std::ffi::OsStr::from_bytes(
                        super::native_exec_artifacts::RUNTIME_PATH,
                    ))
                    .into(),
                ),
                "native-runtime",
                self.artifacts.runtime(),
                InputRole::Executable,
            ),
            (
                None,
                "native-config",
                self.artifacts.native_config()?,
                InputRole::Data,
            ),
            (
                Some(
                    Path::new(std::ffi::OsStr::from_bytes(
                        detcore_model::liteinst_native_exec_catalog::PATH,
                    ))
                    .into(),
                ),
                "native-driver-catalog",
                self.artifacts.driver_catalog(),
                InputRole::Data,
            ),
            (
                Some(PathBuf::from("/etc/group")),
                "native-common-group",
                self.additions.common_group()?.descriptor(),
                InputRole::Data,
            ),
        ] {
            objects.push(exported(path, role, fd, native_role)?);
        }
        Ok((producer(), objects))
    }
    fn adopt(producer_name: &str, objects: Vec<(SharedObjectMeta, OwnedFd)>) -> Result<Self> {
        ensure!(
            producer_name == producer(),
            "shared native producer/source differs"
        );
        ensure!(objects.len() <= 256 + 7, "excessive shared native objects");
        let mut source_objects = Vec::new();
        let mut roles = std::collections::BTreeMap::new();
        for (meta, fd) in objects {
            if let Some(role) = input_role(&meta.role) {
                let path = meta.path.context("native input has no original path")?;
                source_objects.push((
                    GuestPath::new(path.as_os_str().as_bytes().to_vec())?,
                    role,
                    File::from(fd),
                ));
            } else {
                ensure!(
                    [
                        "native-declaration/v1",
                        "native-bootstrap",
                        "native-runner",
                        "native-runtime",
                        "native-config",
                        "native-driver-catalog",
                        "native-common-group"
                    ]
                    .contains(&meta.role.as_str()),
                    "unknown native shared role"
                );
                ensure!(
                    roles
                        .insert(meta.role, (meta.path, File::from(fd)))
                        .is_none(),
                    "duplicate native shared role"
                );
            }
        }
        let (declaration_path, declaration) = roles
            .remove("native-declaration/v1")
            .context("missing native declaration")?;
        require_role_path(&declaration_path, &None)?;
        let declaration_bytes = retained_bytes(&declaration, InputRole::Data, 1024 * 1024)?;
        let inputs = Arc::new(adopt_inputs(
            DynamicExecInputs::parse(&declaration_bytes)?,
            source_objects,
        )?);
        let mut take = |role: &str, expected: Option<PathBuf>| -> Result<File> {
            let (path, file) = roles
                .remove(role)
                .context("missing native artifact/common role")?;
            require_role_path(&path, &expected)?;
            Ok(file)
        };
        let bootstrap = take("native-bootstrap", Some(primary_interpreter(&inputs)?))?;
        let runner = take(
            "native-runner",
            Some(
                Path::new(std::ffi::OsStr::from_bytes(
                    super::native_exec_artifacts::RUNNER_PATH,
                ))
                .into(),
            ),
        )?;
        let runtime = take(
            "native-runtime",
            Some(
                Path::new(std::ffi::OsStr::from_bytes(
                    super::native_exec_artifacts::RUNTIME_PATH,
                ))
                .into(),
            ),
        )?;
        let config = take("native-config", None)?;
        let catalog = take(
            "native-driver-catalog",
            Some(
                Path::new(std::ffi::OsStr::from_bytes(
                    detcore_model::liteinst_native_exec_catalog::PATH,
                ))
                .into(),
            ),
        )?;
        let group = take("native-common-group", Some(PathBuf::from("/etc/group")))?;
        ensure!(roles.is_empty(), "extra native shared role");
        let artifacts = Arc::new(QualifiedNativeArtifacts::adopt(
            inputs, bootstrap, runner, runtime, config, catalog,
        )?);
        let mut additions = DeclaredRootAdditions::default();
        artifacts.root_files(&mut additions)?;
        retained_bytes(&group, InputRole::Data, 64 * 1024 * 1024)?;
        additions.adopt_common_group(group.as_fd())?;
        Self::from_retained(declaration, artifacts, additions)
    }
}

/// Per-physical-run root recipe in the existing parent-held container guards.
pub(super) struct Plan {
    root: DeclaredInputRoot,
    runtime_files: Vec<RuntimeFile>,
    artifacts: Arc<QualifiedNativeArtifacts>,
    endpoints: NativeEndpoints,
}
impl Plan {
    /// # Safety
    /// Called once by the existing Container child after actual common setup,
    /// before any worker/runtime. The plan and common pin owners remain live.
    pub(super) unsafe fn materialize(mut self) -> Result<PhysicalContainer> {
        let root = unsafe { self.root.materialize()? };
        Ok(PhysicalContainer {
            root,
            runtime_files: self.runtime_files,
            artifacts: self.artifacts,
            endpoints: self.endpoints,
            _plan: self.root,
        })
    }
}
/// Actual root data retained until the same run's guest, helpers and drains end.
pub(super) struct PhysicalContainer {
    pub(super) root: PreparedDeclaredRoot,
    pub(super) runtime_files: Vec<RuntimeFile>,
    pub(super) artifacts: Arc<QualifiedNativeArtifacts>,
    pub(super) endpoints: NativeEndpoints,
    _plan: DeclaredInputRoot,
}

#[cfg(test)]
mod shared_tests {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[test]
    fn shared_native_role_paths_require_original_bytes_and_explicit_absence() {
        let original = Some(PathBuf::from(std::ffi::OsString::from_vec(
            b"/source-\xff".to_vec(),
        )));
        require_role_path(&original, &original).unwrap();
        require_role_path(&None, &None).unwrap();
        assert!(require_role_path(&None, &original).is_err());
        assert!(require_role_path(&Some(PathBuf::new()), &original).is_err());
        assert!(require_role_path(&Some(PathBuf::new()), &Some(PathBuf::new())).is_err());
        assert!(require_role_path(&Some(PathBuf::from("/source-�")), &original).is_err());
        assert!(require_role_path(&original, &None).is_err());
        let literal = Some(PathBuf::from("/.hermit-native/runtime"));
        for different_bytes in [
            "/.hermit-native//runtime",
            "/.hermit-native/./runtime",
            "/.hermit-native/runtime/",
            "/.hermit-native/runtime/.",
        ] {
            assert!(
                require_role_path(&Some(PathBuf::from(different_bytes)), &literal).is_err(),
                "path components must not replace exact original bytes: {different_bytes}"
            );
        }
    }
    #[test]
    fn shared_native_adoption_refuses_a_different_producer_without_capture() {
        assert!(Capture::adopt("hermit-sealed-files", Vec::new()).is_err());
    }

    fn actual_shared_capture() -> (tempfile::TempDir, Capture, PathBuf, PathBuf) {
        use std::os::unix::fs::symlink;
        let directory = tempfile::Builder::new()
            .prefix(".shared-native-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let source = directory.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let data = source.join("data");
        std::fs::write(&data, b"captured shared data").unwrap();
        let alias = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"alias-\xff".to_vec()));
        symlink("source", &alias).unwrap();
        let declared = alias.join("data");
        let mut document =
            super::super::native_exec_run::shared_input_declaration_fixture().unwrap();
        document["data_files"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(declared.as_os_str().as_bytes()));
        document["aliases"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
            "path":alias.as_os_str().as_bytes(),"target":b"source"}));
        let declaration = directory.path().join("inputs.json");
        std::fs::write(&declaration, serde_json::to_vec(&document).unwrap()).unwrap();
        let captured = Capture::prepare_shared(&declaration).unwrap();
        (directory, captured, data, alias)
    }
    fn clone_objects(objects: &[(SharedObjectMeta, OwnedFd)]) -> Vec<(SharedObjectMeta, OwnedFd)> {
        objects
            .iter()
            .map(|(meta, fd)| (meta.clone(), fd.try_clone().unwrap()))
            .collect()
    }
    fn actual_identities(
        objects: &[(SharedObjectMeta, OwnedFd)],
    ) -> Vec<(String, Option<PathBuf>, u64, u64)> {
        objects
            .iter()
            .map(|(meta, fd)| {
                let stat = File::from(fd.try_clone().unwrap()).metadata().unwrap();
                (meta.role.clone(), meta.path.clone(), stat.dev(), stat.ino())
            })
            .collect()
    }
    #[test]
    fn actual_capture_two_adoptions_keep_all_sealed_roles_and_original_graph() {
        use std::os::unix::fs::symlink;
        let (_directory, captured, data, alias) = actual_shared_capture();
        let (producer, objects) = captured.export().unwrap();
        let original_ids = actual_identities(&objects);
        assert!(
            objects
                .iter()
                .any(|(meta, _)| meta.role == "native-declaration/v1" && meta.path.is_none())
        );
        assert!(
            objects
                .iter()
                .any(|(meta, _)| meta.role == "native-config" && meta.path.is_none())
        );
        let catalog_id = captured.artifacts.driver_catalog_identity();
        std::fs::write(&data, b"mutated source").unwrap();
        std::fs::remove_file(&alias).unwrap();
        symlink("absent", &alias).unwrap();
        for _ in 0..2 {
            let adopted = Capture::adopt(&producer, clone_objects(&objects)).unwrap();
            assert_ne!(adopted.staging.path(), captured.staging.path());
            let (again_producer, again) = adopted.export().unwrap();
            assert_eq!(again_producer, producer);
            assert_eq!(actual_identities(&again), original_ids);
            assert_eq!(adopted.artifacts.driver_catalog_identity(), catalog_id);
            let held = adopted
                .artifacts
                .inputs()
                .files()
                .iter()
                .find(|file| file.role == InputRole::Data)
                .unwrap();
            assert_eq!(held.read_bytes().unwrap(), b"captured shared data");
            assert!(held.path.as_bytes().contains(&0xff));
            assert_eq!(held.resolved_path.as_path(), data);
            adopted
                .require_initial(&Command::new("/usr/bin/nice"))
                .unwrap();
            assert!(
                adopted
                    .require_initial(&Command::new("/undeclared"))
                    .is_err()
            );
            let role = &adopted.artifacts.catalog()[0];
            let first = File::from(role.stock_loader().try_clone_to_owned().unwrap())
                .metadata()
                .unwrap();
            let second = File::from(role.tool_loader().try_clone_to_owned().unwrap())
                .metadata()
                .unwrap();
            assert_eq!((first.dev(), first.ino()), (second.dev(), second.ino()));
        }
    }
    #[test]
    fn actual_capture_adoption_refuses_pathless_mislabels_missing_and_tampered_catalog() {
        let (_directory, captured, _data, _alias) = actual_shared_capture();
        let (producer, objects) = captured.export().unwrap();
        for role in [
            "native-declaration/v1",
            "native-config",
            "native-runtime",
            "native-input-executable",
        ] {
            let mut changed = clone_objects(&objects);
            let meta = &mut changed
                .iter_mut()
                .find(|(meta, _)| meta.role == role)
                .unwrap()
                .0;
            meta.path = if meta.path.is_none() {
                Some(PathBuf::from("/invented"))
            } else {
                None
            };
            assert!(Capture::adopt(&producer, changed).is_err());
        }
        for replacement in [Some(PathBuf::new()), Some(PathBuf::from("/different"))] {
            let mut changed = clone_objects(&objects);
            changed
                .iter_mut()
                .find(|(meta, _)| meta.role == "native-runtime")
                .unwrap()
                .0
                .path = replacement;
            assert!(Capture::adopt(&producer, changed).is_err());
        }
        let mut missing = clone_objects(&objects);
        missing.retain(|(meta, _)| meta.role != "native-driver-catalog");
        assert!(Capture::adopt(&producer, missing).is_err());
        let mut unknown = clone_objects(&objects);
        unknown[0].0.role = "unqualified-role".into();
        assert!(Capture::adopt(&producer, unknown).is_err());
        let mut tampered = clone_objects(&objects);
        let row = tampered
            .iter_mut()
            .find(|(meta, _)| meta.role == "native-driver-catalog")
            .unwrap();
        row.1 = seal_bytes(b"not the independently rebuilt catalog", InputRole::Data)
            .unwrap()
            .into();
        assert!(Capture::adopt(&producer, tampered).is_err());
    }
}
