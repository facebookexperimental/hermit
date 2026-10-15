/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The native launcher's retained artifact catalog. A declaration, packet or
//! adjacent hash manifest cannot construct it. The host's own build embeds the
//! identities emitted by the actual qualified native producer. This module
//! captures those exact objects, checks the native ELF roles, and keeps their
//! sealed backing alive alongside the original declared input graph.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use goblin::elf::Elf;
use goblin::elf::dynamic;
use goblin::elf::header;
use goblin::elf::program_header as ph;
use goblin::elf::section_header;
use goblin::elf::sym;
use sha2::Digest;
use sha2::Sha256;

use super::container::declared_inputs::DeclaredRootAdditions;
use super::container::declared_inputs::FilePlacement;
use super::container::declared_inputs::RuntimeFile;
use super::container::declared_inputs::STOCK_LOADER_PATH;
use super::native_exec_inputs::CapturedIdentity;
use super::native_exec_inputs::GuestPath;
use super::native_exec_inputs::HeldDynamicInputs;
use super::native_exec_inputs::HeldInputFile;
use super::native_exec_inputs::InputRole;
use super::native_exec_inputs::retained_bytes;
use super::native_exec_inputs::seal_bytes;

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const PAGE: u64 = 4096;
const GATE: u64 = 0x5000_0000_0000;
pub(super) const RUNNER_PATH: &[u8] = b"/run/hermit/native/runner";
pub(super) const RUNTIME_PATH: &[u8] = b"/run/hermit/native/libdetcore_liteinst.so";
const RUNTIME_DIRECTORY: &[u8] = b"/run/hermit/native";
const INITIALIZER: &str = "detcore_liteinst_exec_initialize";

// The producer-generated Rust source is compiled into this host, not read from
// a runtime sidecar or supplied by an exec request. Generating it must follow
// successful real compilation and constructor/native-role qualification. The
// retained source digest is independently computed by the same compiled source
// helper on both sides; equality alone is not proof of a kernel exec.
struct CompiledFile {
    bytes: &'static [u8],
    digest: [u8; 32],
}
struct CompiledBundle {
    source_digest: [u8; 32],
    bootstrap: CompiledFile,
    runner: CompiledFile,
    runtime: CompiledFile,
    gate_digest: [u8; 32],
}
include!(concat!(env!("OUT_DIR"), "/native_exec_bundle.rs"));

struct HeldArtifact {
    file: File,
    identity: CapturedIdentity,
    bytes: Vec<u8>,
}
impl HeldArtifact {
    fn capture(compiled: &CompiledFile) -> Result<Self> {
        ensure!(
            compiled.digest != [0; 32],
            "missing compiled native identity"
        );
        ensure!(
            !compiled.bytes.is_empty() && compiled.bytes.len() as u64 <= MAX_BYTES,
            "compiled native artifact exceeds its content bound"
        );
        ensure!(
            <[u8; 32]>::from(Sha256::digest(compiled.bytes)) == compiled.digest,
            "native artifact differs from this host's compiled producer output"
        );
        // The actual producer's qualified output is embedded in this host.
        // Runtime use has no dependency on the producer's build directory or
        // an adjacent file that could select another private runtime.
        Self::seal(compiled.bytes.to_vec(), InputRole::Executable)
    }
    fn seal(bytes: Vec<u8>, role: InputRole) -> Result<Self> {
        let file = seal_bytes(&bytes, role)?;
        let metadata = file.metadata()?;
        let identity = CapturedIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            digest: Sha256::digest(&bytes).into(),
        };
        Ok(Self {
            file,
            identity,
            bytes,
        })
    }
    fn adopt(file: File, role: InputRole) -> Result<Self> {
        let bytes = retained_bytes(&file, role, MAX_BYTES)?;
        let metadata = file.metadata()?;
        Ok(Self {
            file,
            identity: CapturedIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                size: metadata.len(),
                digest: Sha256::digest(&bytes).into(),
            },
            bytes,
        })
    }
    fn descriptor(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

#[cfg(test)]
pub(super) fn bootstrap_mapping_fixture() -> Result<(File, CapturedIdentity)> {
    let bootstrap = HeldArtifact::capture(&NATIVE_BUNDLE.bootstrap)?;
    validate_bootstrap(&bootstrap.bytes, NATIVE_BUNDLE.gate_digest)?;
    Ok((bootstrap.file, bootstrap.identity))
}

/// Input discovery for the physical bootstrap RPC test. These bytes still
/// enter the ordinary Capture qualification; this accessor grants no readiness
/// or native admission and cannot substitute a test-built runtime.
#[cfg(test)]
pub(super) fn bootstrap_rpc_dependency_fixture() -> Result<(Vec<u8>, Vec<u8>)> {
    let runner = HeldArtifact::capture(&NATIVE_BUNDLE.runner)?;
    let runtime = HeldArtifact::capture(&NATIVE_BUNDLE.runtime)?;
    validate_runner(&runner.bytes)?;
    validate_runtime(&runtime.bytes)?;
    Ok((runner.bytes, runtime.bytes))
}

/// One independently built catalog row. The transaction's retained target
/// digest selects it; a request cannot select replacement role descriptors.
pub(super) struct NativeCatalogEntry {
    pub(super) target_digest: [u8; 32],
    pub(super) bootstrap_digest: [u8; 32],
    stock_loader: File,
    stock_loader_digest: [u8; 32],
    runner: Arc<HeldArtifact>,
    config: Arc<HeldArtifact>,
}
impl NativeCatalogEntry {
    pub(super) fn stock_loader(&self) -> BorrowedFd<'_> {
        self.stock_loader.as_fd()
    }
    pub(super) fn tool_loader(&self) -> BorrowedFd<'_> {
        self.stock_loader.as_fd()
    }
    pub(super) fn runner(&self) -> BorrowedFd<'_> {
        self.runner.descriptor()
    }
    pub(super) fn native_config(&self) -> BorrowedFd<'_> {
        self.config.descriptor()
    }
    pub(super) fn stock_loader_digest(&self) -> [u8; 32] {
        self.stock_loader_digest
    }
    pub(super) fn tool_loader_digest(&self) -> [u8; 32] {
        self.stock_loader_digest
    }
    pub(super) fn runner_digest(&self) -> [u8; 32] {
        self.runner.identity.digest
    }
    pub(super) fn native_config_digest(&self) -> [u8; 32] {
        self.config.identity.digest
    }
}

/// Private-constructor artifact ownership. It supplies only the artifact part
/// of launch authority: actual root setup, watched birth, filter, mappings,
/// native continuation and genuine lifecycle acknowledgements remain required.
pub(super) struct QualifiedNativeArtifacts {
    inputs: Arc<HeldDynamicInputs>,
    source_digest: [u8; 32],
    bootstrap: Arc<HeldArtifact>,
    runner: Arc<HeldArtifact>,
    runtime: Arc<HeldArtifact>,
    catalog: Vec<NativeCatalogEntry>,
    interpreters: BTreeSet<GuestPath>,
    driver_catalog: Arc<HeldArtifact>,
}
impl QualifiedNativeArtifacts {
    pub(super) fn capture(inputs: Arc<HeldDynamicInputs>) -> Result<Self> {
        inputs.recheck()?;
        Self::qualify_retained(
            inputs,
            Arc::new(HeldArtifact::capture(&NATIVE_BUNDLE.bootstrap)?),
            Arc::new(HeldArtifact::capture(&NATIVE_BUNDLE.runner)?),
            Arc::new(HeldArtifact::capture(&NATIVE_BUNDLE.runtime)?),
            Arc::new(HeldArtifact::seal(native_config()?, InputRole::Data)?),
            None,
        )
    }
    pub(super) fn adopt(
        inputs: Arc<HeldDynamicInputs>,
        bootstrap: File,
        runner: File,
        runtime: File,
        config: File,
        driver_catalog: File,
    ) -> Result<Self> {
        Self::qualify_retained(
            inputs,
            Arc::new(HeldArtifact::adopt(bootstrap, InputRole::Executable)?),
            Arc::new(HeldArtifact::adopt(runner, InputRole::Executable)?),
            Arc::new(HeldArtifact::adopt(runtime, InputRole::Executable)?),
            Arc::new(HeldArtifact::adopt(config, InputRole::Data)?),
            Some(Arc::new(HeldArtifact::adopt(
                driver_catalog,
                InputRole::Data,
            )?)),
        )
    }
    fn qualify_retained(
        inputs: Arc<HeldDynamicInputs>,
        bootstrap: Arc<HeldArtifact>,
        runner: Arc<HeldArtifact>,
        runtime: Arc<HeldArtifact>,
        config: Arc<HeldArtifact>,
        driver_catalog: Option<Arc<HeldArtifact>>,
    ) -> Result<Self> {
        let actual_source = detcore_model::liteinst_native_exec_sources::digest(
            reverie_inguest::native_exec_sources::digest(),
        );
        ensure!(
            actual_source != [0; 32] && actual_source == NATIVE_BUNDLE.source_digest,
            "native host/producer source identities differ"
        );
        inputs.recheck()?;
        for (held, compiled) in [
            (&bootstrap, &NATIVE_BUNDLE.bootstrap),
            (&runner, &NATIVE_BUNDLE.runner),
            (&runtime, &NATIVE_BUNDLE.runtime),
        ] {
            ensure!(
                compiled.digest != [0; 32]
                    && !compiled.bytes.is_empty()
                    && compiled.bytes.len() as u64 <= MAX_BYTES
                    && held.identity.digest == compiled.digest
                    && held.bytes == compiled.bytes,
                "native artifact differs from this host's compiled producer output"
            );
        }
        ensure!(
            config.bytes == native_config()?,
            "adopted native config differs"
        );
        validate_bootstrap(&bootstrap.bytes, NATIVE_BUNDLE.gate_digest)?;
        validate_runner(&runner.bytes)?;
        validate_runtime(&runtime.bytes)?;
        // These checks complement the producer's maintained constructor checks.
        // They never turn discovery of an export into a constructor receipt.
        check_library_inputs(&runner.bytes, &inputs, true)?;
        check_library_inputs(&runtime.bytes, &inputs, false)?;
        let mut interpreters = BTreeSet::new();
        let mut targets = BTreeMap::new();
        for target in inputs
            .files()
            .iter()
            .filter(|f| f.role == InputRole::Executable)
        {
            let interpreter = target
                .interpreter
                .as_ref()
                .context("dynamic target has no interpreter")?;
            let resolved = inputs.resolve_path(interpreter)?;
            let loader = inputs
                .files()
                .iter()
                .find(|file| file.role == InputRole::Loader && file.resolved_path == resolved)
                .context("target interpreter has no retained declared loader")?;
            validate_stock_loader(&loader.read_bytes()?)?;
            if let Some(old) = targets.insert(target.identity.digest, loader.identity.digest) {
                ensure!(
                    old == loader.identity.digest,
                    "equal target bytes select ambiguous loaders"
                );
                continue;
            }
            interpreters.insert(interpreter.clone());
        }
        ensure!(!targets.is_empty(), "native catalog has no dynamic targets");
        // Different loaders within one native run require separate private
        // library-closure qualification. Version one supports one exact stock
        // loader object, shared by its guest and Tool roles, for all targets.
        let loader_digests: BTreeSet<_> = targets.values().copied().collect();
        ensure!(
            loader_digests.len() == 1,
            "native catalog mixes stock loader objects"
        );
        let mut catalog = Vec::new();
        for (target_digest, loader_digest) in targets {
            let loader = inputs
                .files()
                .iter()
                .find(|file| {
                    file.role == InputRole::Loader && file.identity.digest == loader_digest
                })
                .context("retained stock loader disappeared")?;
            let fd = loader.descriptor().try_clone_to_owned()?;
            catalog.push(NativeCatalogEntry {
                target_digest,
                bootstrap_digest: bootstrap.identity.digest,
                stock_loader: File::from(fd),
                stock_loader_digest: loader_digest,
                runner: Arc::clone(&runner),
                config: Arc::clone(&config),
            });
        }
        let expected_catalog = driver_catalog_bytes(
            &inputs,
            &bootstrap,
            &runner,
            &runtime,
            &catalog,
            actual_source,
        )?;
        let driver_catalog = if let Some(held) = driver_catalog {
            ensure!(
                held.bytes == expected_catalog,
                "adopted catalog identity graph differs"
            );
            held
        } else {
            Arc::new(HeldArtifact::seal(expected_catalog, InputRole::Data)?)
        };
        Ok(Self {
            inputs,
            source_digest: actual_source,
            bootstrap,
            runner,
            runtime,
            catalog,
            interpreters,
            driver_catalog,
        })
    }
    pub(super) fn catalog(&self) -> &[NativeCatalogEntry] {
        &self.catalog
    }
    pub(super) fn native_config(&self) -> Result<BorrowedFd<'_>> {
        Ok(self
            .catalog
            .first()
            .context("empty native catalog")?
            .native_config())
    }
    pub(super) fn source_digest(&self) -> [u8; 32] {
        self.source_digest
    }
    pub(super) fn inputs(&self) -> &Arc<HeldDynamicInputs> {
        &self.inputs
    }
    pub(super) fn bootstrap(&self) -> BorrowedFd<'_> {
        self.bootstrap.descriptor()
    }
    pub(super) fn runtime(&self) -> BorrowedFd<'_> {
        self.runtime.descriptor()
    }
    pub(super) fn runner(&self) -> BorrowedFd<'_> {
        self.runner.descriptor()
    }
    pub(super) fn runner_identity(&self) -> CapturedIdentity {
        self.runner.identity
    }
    pub(super) fn bootstrap_identity(&self) -> CapturedIdentity {
        self.bootstrap.identity
    }
    pub(super) fn runtime_identity(&self) -> CapturedIdentity {
        self.runtime.identity
    }
    pub(super) fn driver_catalog(&self) -> BorrowedFd<'_> {
        self.driver_catalog.descriptor()
    }
    pub(super) fn driver_catalog_identity(&self) -> CapturedIdentity {
        self.driver_catalog.identity
    }
    pub(super) fn gate_bytes(&self) -> Result<&[u8]> {
        let elf = Elf::parse(&self.bootstrap.bytes)?;
        loaded_bytes(&self.bootstrap.bytes, &elf, GATE, PAGE)
    }
    pub(super) fn root_files(&self, additions: &mut DeclaredRootAdditions) -> Result<()> {
        for interpreter in &self.interpreters {
            additions.add_file(RuntimeFile::new(
                interpreter.clone(),
                self.bootstrap(),
                true,
                FilePlacement::InterpreterReplacement,
            )?)?;
        }
        let stock_loader = self.catalog.first().context("empty native catalog")?;
        additions.add_file(RuntimeFile::new(
            GuestPath::new(STOCK_LOADER_PATH.to_vec())?,
            stock_loader.stock_loader(),
            true,
            FilePlacement::Additional,
        )?)?;
        for (path, artifact) in [(RUNNER_PATH, &self.runner), (RUNTIME_PATH, &self.runtime)] {
            additions.add_file(RuntimeFile::new(
                GuestPath::new(path.to_vec())?,
                artifact.descriptor(),
                true,
                FilePlacement::Additional,
            )?)?;
        }
        additions.add_file(RuntimeFile::new(
            GuestPath::new(detcore_model::liteinst_native_exec_catalog::PATH.to_vec())?,
            self.driver_catalog(),
            false,
            FilePlacement::Additional,
        )?)?;
        Ok(())
    }
}

// Names come only from explicitly retained Executable rows. A resolved name
// identifies that same object; other descendants of a declared directory alias
// and Loader/Data rows do not acquire executable admission.
fn executable_target_names(
    inputs: &HeldDynamicInputs,
) -> Result<BTreeMap<GuestPath, &HeldInputFile>> {
    let mut names = BTreeMap::new();
    for file in inputs
        .files()
        .iter()
        .filter(|file| file.role == InputRole::Executable)
    {
        ensure!(
            inputs.resolve_path(&file.path)? == file.resolved_path,
            "native executable name differs from its retained resolution"
        );
        for path in [&file.path, &file.resolved_path] {
            if let Some(previous) = names.insert(path.clone(), file) {
                ensure!(
                    std::ptr::eq(previous, file),
                    "native executable name selects conflicting retained objects"
                );
            }
        }
    }
    Ok(names)
}

// Generated from the privately retained source/artifact owners once, before
// the physical roots diverge. Decoding it later is data transport, not proof.
// Config comes independently from the real InitialImage RPC admission.
fn driver_catalog_bytes(
    inputs: &HeldDynamicInputs,
    bootstrap: &HeldArtifact,
    runner: &HeldArtifact,
    runtime: &HeldArtifact,
    roles: &[NativeCatalogEntry],
    source: [u8; 32],
) -> Result<Vec<u8>> {
    use detcore_model::liteinst_native_exec_catalog as wire;
    let mut objects = Vec::<wire::Object>::new();
    let mut by_identity = BTreeMap::<(u64, u64), u16>::new();
    fn add(
        objects: &mut Vec<wire::Object>,
        index: &mut BTreeMap<(u64, u64), u16>,
        id: CapturedIdentity,
        bytes: &[u8],
        executable: bool,
        backing_path: Option<Vec<u8>>,
    ) -> Result<u16> {
        ensure!(
            bytes.len() as u64 == id.size && <[u8; 32]>::from(Sha256::digest(bytes)) == id.digest,
            "native catalog bytes differ from retained object"
        );
        let extents = if executable {
            let elf = Elf::parse(bytes)?;
            let mut extents = Vec::<wire::Extent>::new();
            for load in elf
                .program_headers
                .iter()
                .filter(|p| p.p_type == ph::PT_LOAD && p.p_flags & ph::PF_X != 0)
            {
                ensure!(
                    load.p_filesz > 0 && load.p_flags & ph::PF_W == 0,
                    "native catalog executable load is empty or writable"
                );
                let end = load
                    .p_offset
                    .checked_add(load.p_filesz)
                    .context("native executable extent overflow")?;
                ensure!(
                    end <= id.size,
                    "native executable extent escapes retained bytes"
                );
                extents.push(wire::Extent {
                    start: load.p_offset & !(PAGE - 1),
                    end: end
                        .checked_add(PAGE - 1)
                        .context("native executable page overflow")?
                        & !(PAGE - 1),
                });
            }
            extents.sort_by_key(|extent| extent.start);
            ensure!(
                !extents.is_empty(),
                "native executable role has no executable load"
            );
            extents
        } else {
            Vec::new()
        };
        let object = wire::Object {
            device: id.device,
            inode: id.inode,
            size: id.size,
            digest: id.digest,
            backing_path,
            executable: extents,
        };
        if let Some(existing) = index.get(&(id.device, id.inode)) {
            ensure!(
                objects[usize::from(*existing)] == object,
                "one native catalog object has conflicting roles"
            );
            return Ok(*existing);
        }
        ensure!(
            objects.len() < wire::MAX_OBJECTS,
            "native catalog object cap"
        );
        let next = u16::try_from(objects.len())?;
        objects.push(object);
        index.insert((id.device, id.inode), next);
        Ok(next)
    }
    let interpreter_paths = inputs.resolved_interpreters()?;
    for file in inputs.files() {
        let backing_path = if interpreter_paths.contains(&file.resolved_path) {
            STOCK_LOADER_PATH.to_vec()
        } else {
            file.resolved_path.as_bytes().to_vec()
        };
        add(
            &mut objects,
            &mut by_identity,
            file.identity,
            &file.read_bytes()?,
            file.role != InputRole::Data,
            Some(backing_path),
        )?;
    }
    let bootstrap_path = interpreter_paths
        .into_iter()
        .next()
        .context("native bootstrap lacks an actual interpreter placement")?;
    for (artifact, path) in [
        (bootstrap, bootstrap_path.as_bytes()),
        (runner, RUNNER_PATH),
        (runtime, RUNTIME_PATH),
    ] {
        add(
            &mut objects,
            &mut by_identity,
            artifact.identity,
            &artifact.bytes,
            true,
            Some(path.to_vec()),
        )?;
    }
    let first = roles.first().context("native catalog lacks role owner")?;
    let config_index = add(
        &mut objects,
        &mut by_identity,
        first.config.identity,
        &first.config.bytes,
        false,
        None,
    )?;
    let runner_index = *by_identity
        .get(&(runner.identity.device, runner.identity.inode))
        .context("native runner index absent")?;
    let mut targets = Vec::new();
    for (path, target) in executable_target_names(inputs)? {
        let role = roles
            .iter()
            .find(|role| role.target_digest == target.identity.digest)
            .context("native target lacks retained provider role")?;
        let loader = role.stock_loader.metadata()?;
        let stock_loader = *by_identity
            .get(&(loader.dev(), loader.ino()))
            .context("native loader index absent")?;
        targets.push(wire::Target {
            path: path.as_bytes().to_vec(),
            image: *by_identity
                .get(&(target.identity.device, target.identity.inode))
                .context("native target index absent")?,
            stock_loader,
            runner: runner_index,
            native_config: config_index,
        });
    }
    let schema = detcore::config_wire_fingerprint()
        .as_bytes()
        .try_into()
        .context("native catalog Config schema width")?;
    Ok(wire::encode(&wire::Catalog {
        bindings: wire::Bindings {
            source,
            schema,
            bootstrap: bootstrap.identity.digest,
        },
        objects,
        targets,
    })?)
}

fn native_config() -> Result<Vec<u8>> {
    let mut bytes = vec![0; 1552];
    bytes[..8].copy_from_slice(&1u64.to_le_bytes());
    bytes[8..16].copy_from_slice(&1552u64.to_le_bytes());
    {
        let (slot, path) = (&mut bytes[16..528], RUNNER_PATH);
        ensure!(
            path.len() < slot.len() && !path.contains(&0),
            "native runner path ABI"
        );
        slot[..path.len()].copy_from_slice(path);
    }
    ensure!(
        RUNTIME_DIRECTORY.len() < 1024 && !RUNTIME_DIRECTORY.contains(&0),
        "native runtime path ABI"
    );
    bytes[528..528 + RUNTIME_DIRECTORY.len()].copy_from_slice(RUNTIME_DIRECTORY);
    Ok(bytes)
}

fn image<'a>(bytes: &'a [u8], kind: u16) -> Result<Elf<'a>> {
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_BYTES,
        "native ELF size bound"
    );
    let elf = Elf::parse(bytes)?;
    ensure!(
        elf.is_64
            && elf.little_endian
            && elf.header.e_machine == header::EM_X86_64
            && elf.header.e_type == kind,
        "native ELF architecture/type mismatch"
    );
    let mut ends = Vec::new();
    for load in elf
        .program_headers
        .iter()
        .filter(|p| p.p_type == ph::PT_LOAD)
    {
        ensure!(
            load.p_memsz > 0
                && load.p_filesz <= load.p_memsz
                && load.p_flags & (ph::PF_W | ph::PF_X) != ph::PF_W | ph::PF_X
                && load.p_vaddr % PAGE == load.p_offset % PAGE,
            "native ELF invalid or writable-executable load"
        );
        let file_end = load
            .p_offset
            .checked_add(load.p_filesz)
            .context("native ELF file overflow")?;
        let end = load
            .p_vaddr
            .checked_add(load.p_memsz)
            .context("native ELF memory overflow")?;
        ensure!(
            file_end <= bytes.len() as u64 && end <= 0x0000_8000_0000_0000,
            "native ELF extent outside supported address/file domain"
        );
        let lo = load.p_vaddr & !(PAGE - 1);
        let hi = end
            .checked_add(PAGE - 1)
            .context("native ELF page overflow")?
            & !(PAGE - 1);
        ensure!(
            ends.iter().all(|(a, b)| hi <= *a || *b <= lo),
            "native ELF overlapping load pages"
        );
        ends.push((lo, hi));
    }
    ensure!(!ends.is_empty(), "native ELF lacks load mappings");
    let stacks: Vec<_> = elf
        .program_headers
        .iter()
        .filter(|p| p.p_type == ph::PT_GNU_STACK)
        .collect();
    ensure!(
        stacks.len() == 1 && stacks[0].p_flags & ph::PF_X == 0,
        "native ELF lacks unique non-executable stack policy"
    );
    if elf.entry != 0 {
        ensure!(
            elf.program_headers.iter().any(|p| p.p_type == ph::PT_LOAD
                && p.p_flags & (ph::PF_R | ph::PF_X) == ph::PF_R | ph::PF_X
                && p.p_flags & ph::PF_W == 0
                && elf.entry >= p.p_vaddr
                && elf.entry < p.p_vaddr + p.p_filesz),
            "native ELF entry not file-backed RX"
        );
    }
    Ok(elf)
}
fn loaded_bytes<'a>(bytes: &'a [u8], elf: &Elf<'_>, address: u64, size: u64) -> Result<&'a [u8]> {
    let end = address
        .checked_add(size)
        .context("native loaded range overflow")?;
    let mut found = None;
    for load in elf
        .program_headers
        .iter()
        .filter(|p| p.p_type == ph::PT_LOAD)
    {
        if address >= load.p_vaddr
            && end
                <= load
                    .p_vaddr
                    .checked_add(load.p_filesz)
                    .context("native load overflow")?
        {
            ensure!(found.is_none(), "ambiguous native file backing");
            let offset = load
                .p_offset
                .checked_add(address - load.p_vaddr)
                .context("native file offset overflow")?;
            let start = usize::try_from(offset)?;
            let finish = start
                .checked_add(usize::try_from(size)?)
                .context("native file slice overflow")?;
            found = Some(bytes.get(start..finish).context("native load truncated")?);
        }
    }
    found.context("native range has no complete file backing")
}
fn symbol(elf: &Elf<'_>, name: &str, dynamic: bool) -> Result<goblin::elf::sym::Sym> {
    let mut entries = if dynamic {
        elf.dynsyms.iter()
    } else {
        elf.syms.iter()
    };
    let strings = if dynamic { &elf.dynstrtab } else { &elf.strtab };
    let matching: Vec<_> = entries
        .by_ref()
        .filter(|s| strings.get_at(s.st_name) == Some(name))
        .collect();
    ensure!(
        matching.len() == 1,
        "native symbol {name} is missing or ambiguous"
    );
    Ok(matching[0])
}
fn validate_bootstrap(bytes: &[u8], gate_digest: [u8; 32]) -> Result<()> {
    let elf = image(bytes, header::ET_EXEC)?;
    ensure!(
        elf.interpreter.is_none()
            && elf.dynamic.is_none()
            && elf.dynrels.is_empty()
            && elf.dynrelas.is_empty()
            && elf.pltrelocs.is_empty(),
        "bootstrap requires relocation/another interpreter"
    );
    ensure!(
        elf.program_headers
            .iter()
            .filter(|p| p.p_type == ph::PT_LOAD)
            .count()
            == 4,
        "bootstrap load recipe differs"
    );
    for p in elf
        .program_headers
        .iter()
        .filter(|p| p.p_type == ph::PT_LOAD)
    {
        ensure!(
            p.p_vaddr >= GATE && p.p_vaddr + p.p_memsz <= GATE + 0x80000,
            "bootstrap load escapes its fixed region"
        );
    }
    let start = symbol(&elf, "_start", false)?;
    ensure!(
        start.st_type() == sym::STT_FUNC
            && start.st_value == elf.entry
            && start.st_size > 0
            && start.st_value >= GATE + 0x10000
            && start.st_value + start.st_size <= GATE + 0x20000,
        "bootstrap entry differs from genuine kernel entry"
    );
    ensure!(
        loaded_bytes(bytes, &elf, start.st_value, 4)? == [0xf3, 0x0f, 0x1e, 0xfa],
        "bootstrap entry is missing ENDBR64"
    );
    let gate = loaded_bytes(bytes, &elf, GATE, PAGE)?;
    ensure!(
        gate_digest != [0; 32] && <[u8; 32]>::from(Sha256::digest(gate)) == gate_digest,
        "bootstrap complete fixed gate differs from compiled recipe"
    );
    for (offset, expected) in [
        (0x7c, &[0xf3, 0x0f, 0x1e, 0xfa][..]),
        (0x80, &[0x0f, 0x05]),
        (0x200, &[0x0f, 0x05]),
        (0x300, &[0xf3, 0x0f, 0x1e, 0xfa]),
        (0x309, &[0x0f, 0x05]),
        (0x30b, &[0x0f, 0x0b]),
    ] {
        ensure!(
            &gate[offset..offset + expected.len()] == expected,
            "native gate entry ABI differs"
        );
    }
    Ok(())
}
fn validate_runner(bytes: &[u8]) -> Result<()> {
    let elf = image(bytes, header::ET_DYN)?;
    ensure!(
        elf.entry != 0 && elf.interpreter.is_some(),
        "native runner is not a real dynamic PIE"
    );
    ensure!(
        elf.libraries
            .iter()
            .filter(|name| **name == "libdetcore_liteinst.so")
            .count()
            == 1,
        "native runner lacks its genuine strong Detcore dependency"
    );
    let init = symbol(&elf, INITIALIZER, true)?;
    ensure!(
        init.st_bind() == sym::STB_GLOBAL
            && init.st_shndx == section_header::SHN_UNDEF as usize
            && init.st_value == 0,
        "native runner substitutes its own initializer"
    );
    let index = elf
        .dynsyms
        .iter()
        .position(|s| elf.dynstrtab.get_at(s.st_name) == Some(INITIALIZER))
        .context("native initializer relocation symbol missing")?;
    ensure!(
        elf.pltrelocs
            .iter()
            .chain(elf.dynrelas.iter())
            .any(|r| r.r_sym == index),
        "native runner does not bind the real initializer"
    );
    no_loader_overrides(&elf)
}
fn validate_runtime(bytes: &[u8]) -> Result<()> {
    let elf = image(bytes, header::ET_DYN)?;
    ensure!(elf.interpreter.is_none(), "native runtime must be a DSO");
    ensure!(
        elf.syms.iter().all(|s| elf
            .strtab
            .get_at(s.st_name)
            .is_none_or(|name| !name.contains("DETCORE_LITEINST_INIT"))),
        "native runtime retains the ordinary constructor registration"
    );
    let init = symbol(&elf, INITIALIZER, true)?;
    ensure!(
        init.st_bind() == sym::STB_GLOBAL
            && init.st_visibility() == sym::STV_PROTECTED
            && init.st_type() == sym::STT_FUNC
            && init.st_shndx != section_header::SHN_UNDEF as usize
            && init.st_size > 0,
        "native runtime initializer export is not strong PROTECTED code"
    );
    let body = loaded_bytes(bytes, &elf, init.st_value, init.st_size)?;
    ensure!(
        !body.is_empty()
            && elf.program_headers.iter().any(|p| p.p_type == ph::PT_LOAD
                && p.p_flags & (ph::PF_R | ph::PF_X) == ph::PF_R | ph::PF_X
                && p.p_flags & ph::PF_W == 0
                && init.st_value >= p.p_vaddr
                && init.st_value + init.st_size <= p.p_vaddr + p.p_filesz),
        "native initializer lacks complete immutable code backing"
    );
    no_loader_overrides(&elf)
}
fn validate_stock_loader(bytes: &[u8]) -> Result<()> {
    let elf = image(bytes, header::ET_DYN)?;
    ensure!(
        elf.entry != 0 && elf.interpreter.is_none(),
        "stock loader has no direct dynamic entry"
    );
    no_loader_overrides(&elf)
}
fn no_loader_overrides(elf: &Elf<'_>) -> Result<()> {
    if let Some(table) = &elf.dynamic {
        ensure!(
            table.dyns.iter().all(|d| !matches!(
                d.d_tag,
                dynamic::DT_RPATH
                    | dynamic::DT_RUNPATH
                    | dynamic::DT_TEXTREL
                    | dynamic::DT_AUDIT
                    | dynamic::DT_DEPAUDIT
            )),
            "native role requests unsupported loader search or text writes"
        );
    }
    Ok(())
}
fn check_library_inputs(bytes: &[u8], inputs: &HeldDynamicInputs, runner: bool) -> Result<()> {
    let elf = Elf::parse(bytes)?;
    for library in elf.libraries {
        if runner && library == "libdetcore_liteinst.so" {
            continue;
        }
        ensure!(
            !library.as_bytes().contains(&b'/'),
            "native library needs a path-based search"
        );
        let matching: Vec<_> = inputs
            .files()
            .iter()
            .filter(|f| {
                f.role == InputRole::Loader
                    && f.path.as_bytes().rsplit(|b| *b == b'/').next() == Some(library.as_bytes())
            })
            .collect();
        if matching.is_empty() {
            bail!("native role dependency is undeclared: {library}");
        }
        let digests: BTreeSet<_> = matching.iter().map(|f| f.identity.digest).collect();
        ensure!(
            digests.len() == 1,
            "native dependency name has ambiguous declared bytes: {library}"
        );
    }
    // This establishes closed declared dependency identities, not loader search
    // equivalence. Initial/Complete admission must additionally join every real
    // executable mapping to these retained objects and the actual stock loader.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_configuration_library_search_path_names_the_runtime_directory() {
        let bytes = native_config().unwrap();
        assert_eq!(bytes.len(), 1552);
        assert_eq!(u64::from_le_bytes(bytes[..8].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 1552);
        let field = |slot: &[u8]| {
            let end = slot.iter().position(|byte| *byte == 0).unwrap();
            assert!(end > 0, "native config field is empty");
            assert!(slot[end..].iter().all(|byte| *byte == 0));
            slot[..end].to_vec()
        };
        let runner_path = field(&bytes[16..528]);
        assert_eq!(runner_path, RUNNER_PATH);
        let library_directory = field(&bytes[528..1552]);
        let separator = RUNTIME_PATH.iter().rposition(|byte| *byte == b'/').unwrap();
        let expected_directory = &RUNTIME_PATH[..separator];
        let runtime_basename = &RUNTIME_PATH[separator + 1..];
        assert!(!runtime_basename.is_empty());
        let runner = HeldArtifact::capture(&NATIVE_BUNDLE.runner).unwrap();
        let runner_elf = Elf::parse(&runner.bytes).unwrap();
        let library = std::str::from_utf8(runtime_basename).unwrap();
        assert!(
            runner_elf.libraries.contains(&library),
            "compiled runner must need the installed runtime basename"
        );
        assert_eq!(
            library_directory, expected_directory,
            "native config tool_library_path is an LD_LIBRARY_PATH directory"
        );
        let mut joined = library_directory;
        joined.push(b'/');
        joined.extend_from_slice(runtime_basename);
        assert_eq!(joined, RUNTIME_PATH);
    }

    #[test]
    fn compiled_native_roles_refuse_swapped_and_changed_real_artifacts() {
        let bootstrap = HeldArtifact::capture(&NATIVE_BUNDLE.bootstrap).unwrap();
        let runner = HeldArtifact::capture(&NATIVE_BUNDLE.runner).unwrap();
        let runtime = HeldArtifact::capture(&NATIVE_BUNDLE.runtime).unwrap();
        validate_bootstrap(&bootstrap.bytes, NATIVE_BUNDLE.gate_digest).unwrap();
        validate_runner(&runner.bytes).unwrap();
        validate_runtime(&runtime.bytes).unwrap();
        assert!(validate_bootstrap(&runner.bytes, NATIVE_BUNDLE.gate_digest).is_err());
        assert!(validate_runner(&runtime.bytes).is_err());
        assert!(validate_runtime(&runner.bytes).is_err());
        let wrong_identity = CompiledFile {
            bytes: NATIVE_BUNDLE.bootstrap.bytes,
            digest: NATIVE_BUNDLE.runner.digest,
        };
        assert!(HeldArtifact::capture(&wrong_identity).is_err());
        let mut changed = bootstrap.bytes.clone();
        let elf = Elf::parse(&changed).unwrap();
        let gate = elf
            .program_headers
            .iter()
            .find(|p| p.p_type == ph::PT_LOAD && p.p_vaddr == GATE)
            .unwrap();
        let offset = gate.p_offset as usize + 0x80;
        changed[offset] ^= 1;
        assert!(validate_bootstrap(&changed, NATIVE_BUNDLE.gate_digest).is_err());
    }

    #[test]
    fn native_roles_refuse_writable_executable_and_overlapping_real_loads() {
        let original = HeldArtifact::capture(&NATIVE_BUNDLE.runner).unwrap();
        let elf = Elf::parse(&original.bytes).unwrap();
        let indices: Vec<_> = elf
            .program_headers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.p_type == ph::PT_LOAD)
            .map(|(i, _)| i)
            .collect();
        assert!(indices.len() >= 2);
        let phoff = elf.header.e_phoff as usize;
        let stride = elf.header.e_phentsize as usize;
        let first = phoff + indices[0] * stride;
        let second = phoff + indices[1] * stride;
        let mut wx = original.bytes.clone();
        wx[first + 4..first + 8].copy_from_slice(&(ph::PF_R | ph::PF_W | ph::PF_X).to_le_bytes());
        assert!(validate_runner(&wx).is_err());
        let mut overlap = original.bytes.clone();
        overlap[second + 8..second + 32].copy_from_slice(&original.bytes[first + 8..first + 32]);
        assert!(validate_runner(&overlap).is_err());
    }

    #[test]
    fn driver_catalog_preserves_declared_executable_names_and_refuses_other_roles() {
        use std::os::unix::fs::symlink;
        use std::path::Path;
        use std::path::PathBuf;

        use detcore_model::liteinst_native_exec_catalog as wire;

        use super::super::native_exec_inputs::DynamicExecInputs;
        use super::super::native_exec_inputs::OpenedGuestRoot;
        use super::super::native_exec_inputs::capture_inputs;

        // Capture genuine ELF and its complete real loader dependency graph.
        // This qualifies catalog-name mechanics, not a native launch.
        let program = std::fs::read("/bin/echo").unwrap();
        let elf = Elf::parse(&program).unwrap();
        let interpreter = elf.interpreter.unwrap().to_owned();
        let directories = [
            Path::new(&interpreter).parent().unwrap().to_path_buf(),
            PathBuf::from("/lib64"),
            PathBuf::from("/usr/lib64"),
            PathBuf::from("/lib/x86_64-linux-gnu"),
            PathBuf::from("/usr/lib/x86_64-linux-gnu"),
        ];
        let locate = |name: &str| {
            directories
                .iter()
                .map(|directory| directory.join(name))
                .find(|path| path.is_file())
                .expect("real ELF dependency is unavailable")
        };
        let mut pending = vec![PathBuf::from(&interpreter)];
        pending.extend(elf.libraries.iter().map(|name| locate(name)));
        let mut loaders = BTreeMap::new();
        while let Some(path) = pending.pop() {
            if loaders.contains_key(&path) {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let elf = Elf::parse(&bytes).unwrap();
            pending.extend(elf.libraries.iter().map(|name| locate(name)));
            loaders.insert(path, bytes);
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("usr/bin")).unwrap();
        std::fs::write(root.path().join("usr/bin/echo"), program).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.path().join("usr/bin/echo"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        std::fs::write(root.path().join("usr/bin/data"), b"non-executable data").unwrap();
        symlink("usr/bin", root.path().join("bin")).unwrap();
        symlink("usr/bin", root.path().join("compat")).unwrap();
        let mut loader_paths = Vec::new();
        for (path, bytes) in loaders {
            let destination = root.path().join(path.strip_prefix("/").unwrap());
            std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
            std::fs::write(destination, bytes).unwrap();
            loader_paths.push(path.as_os_str().as_encoded_bytes().to_vec());
        }
        let declaration = serde_json::json!({
            "version": 1,
            "executables": [b"/bin/echo".to_vec()],
            "loader_files": loader_paths,
            "data_files": [b"/bin/data".to_vec()],
            "aliases": [
                {"path": b"/bin".to_vec(), "target": b"usr/bin".to_vec()},
                {"path": b"/compat".to_vec(), "target": b"usr/bin".to_vec()}
            ]
        });
        let opened = OpenedGuestRoot::open(root.path()).unwrap();
        let inputs = capture_inputs(
            &opened,
            DynamicExecInputs::parse(&serde_json::to_vec(&declaration).unwrap()).unwrap(),
        )
        .unwrap();
        inputs.recheck().unwrap();
        let declared = GuestPath::new(b"/bin/echo".to_vec()).unwrap();
        let resolved = GuestPath::new(b"/usr/bin/echo".to_vec()).unwrap();
        let other_alias = GuestPath::new(b"/compat/echo".to_vec()).unwrap();
        let target = inputs.require_initial(&declared).unwrap();
        assert_eq!(target.resolved_path, resolved);
        assert_eq!(inputs.resolve_path(&other_alias).unwrap(), resolved);
        assert!(inputs.require_initial(&resolved).is_err());
        assert!(inputs.require_initial(&other_alias).is_err());
        let loader = inputs
            .files()
            .iter()
            .find(|file| file.path.as_bytes() == interpreter.as_bytes())
            .unwrap();
        assert_eq!(loader.role, InputRole::Loader);
        let bootstrap = HeldArtifact::capture(&NATIVE_BUNDLE.bootstrap).unwrap();
        let runner = Arc::new(HeldArtifact::capture(&NATIVE_BUNDLE.runner).unwrap());
        let runtime = HeldArtifact::capture(&NATIVE_BUNDLE.runtime).unwrap();
        let roles = [NativeCatalogEntry {
            target_digest: target.identity.digest,
            bootstrap_digest: bootstrap.identity.digest,
            stock_loader: File::from(loader.descriptor().try_clone_to_owned().unwrap()),
            stock_loader_digest: loader.identity.digest,
            runner: Arc::clone(&runner),
            config: Arc::new(
                HeldArtifact::seal(native_config().unwrap(), InputRole::Data).unwrap(),
            ),
        }];
        let bytes = driver_catalog_bytes(
            &inputs,
            &bootstrap,
            &runner,
            &runtime,
            &roles,
            NATIVE_BUNDLE.source_digest,
        )
        .expect("declared parent alias must retain both exact executable names");
        let catalog = wire::decode(&bytes).unwrap();
        assert_eq!(catalog.targets.len(), 2);
        assert_eq!(catalog.targets[0].path, declared.as_bytes());
        assert_eq!(catalog.targets[1].path, resolved.as_bytes());
        assert_eq!(catalog.targets[0].image, catalog.targets[1].image);
        assert_eq!(
            catalog.objects[usize::from(catalog.targets[0].image)].digest,
            target.identity.digest
        );
        let target_row = &catalog.objects[usize::from(catalog.targets[0].image)];
        assert_eq!(
            (target_row.device, target_row.inode),
            (target.identity.device, target.identity.inode)
        );
        assert_eq!(
            target_row.backing_path.as_deref(),
            Some(resolved.as_bytes())
        );
        let source_loader = &catalog.objects[usize::from(catalog.targets[0].stock_loader)];
        assert_eq!(
            (source_loader.device, source_loader.inode),
            (loader.identity.device, loader.identity.inode)
        );
        assert_eq!(
            source_loader.backing_path.as_deref(),
            Some(STOCK_LOADER_PATH)
        );
        let bootstrap_row = catalog
            .objects
            .iter()
            .find(|object| {
                object.device == bootstrap.identity.device
                    && object.inode == bootstrap.identity.inode
            })
            .unwrap();
        assert_eq!(
            bootstrap_row.backing_path.as_deref(),
            Some(
                inputs
                    .resolve_path(&GuestPath::new(interpreter.as_bytes().to_vec()).unwrap())
                    .unwrap()
                    .as_bytes()
            )
        );
        assert!(
            catalog.objects[usize::from(catalog.targets[0].native_config)]
                .backing_path
                .is_none()
        );
        for path in [
            b"/compat/echo".as_slice(),
            b"/bin/not-enrolled",
            b"/bin/data",
            b"/usr/bin/data",
        ] {
            assert!(catalog.targets.iter().all(|row| row.path != path));
        }
        for file in inputs
            .files()
            .iter()
            .filter(|file| file.role != InputRole::Executable)
        {
            assert!(
                catalog
                    .targets
                    .iter()
                    .all(|row| row.path != file.path.as_bytes()
                        && row.path != file.resolved_path.as_bytes())
            );
            assert!(inputs.require_initial(&file.path).is_err());
        }

        // Preserve the literal-name case above, then capture the same genuine
        // ELF graph with its loader declared under a resolved directory name.
        // PT_INTERP still names the original directory through an actual alias.
        let interpreter_directory = Path::new(&interpreter).parent().unwrap();
        let resolved_directory = Path::new("/resolved-interpreter");
        std::fs::rename(
            root.path()
                .join(interpreter_directory.strip_prefix("/").unwrap()),
            root.path().join("resolved-interpreter"),
        )
        .unwrap();
        symlink(
            resolved_directory,
            root.path()
                .join(interpreter_directory.strip_prefix("/").unwrap()),
        )
        .unwrap();
        let mut aliased_declaration = declaration.clone();
        for path in aliased_declaration["loader_files"].as_array_mut().unwrap() {
            let bytes: Vec<u8> = serde_json::from_value(path.clone()).unwrap();
            let original = GuestPath::new(bytes).unwrap();
            if let Ok(tail) = original.as_path().strip_prefix(interpreter_directory) {
                *path =
                    serde_json::json!(resolved_directory.join(tail).as_os_str().as_encoded_bytes());
            }
        }
        aliased_declaration["aliases"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "path": interpreter_directory.as_os_str().as_encoded_bytes(),
                "target": resolved_directory.as_os_str().as_encoded_bytes()
            }));
        let aliased_inputs = capture_inputs(
            &opened,
            DynamicExecInputs::parse(&serde_json::to_vec(&aliased_declaration).unwrap()).unwrap(),
        )
        .unwrap();
        aliased_inputs.recheck().unwrap();
        let interpreter_path = GuestPath::new(interpreter.as_bytes().to_vec()).unwrap();
        let resolved_interpreter = aliased_inputs.resolve_path(&interpreter_path).unwrap();
        let aliased_loader = aliased_inputs
            .files()
            .iter()
            .find(|file| file.resolved_path == resolved_interpreter)
            .unwrap();
        assert_eq!(aliased_loader.role, InputRole::Loader);
        assert_eq!(aliased_loader.path, resolved_interpreter);
        assert!(!aliased_inputs.is_interpreter(&aliased_loader.path));
        assert!(aliased_inputs.is_interpreter(&interpreter_path));
        let placements = aliased_inputs.resolved_interpreters().unwrap();
        assert_eq!(placements, [resolved_interpreter.clone()].into());
        assert!(placements.contains(&aliased_loader.resolved_path));
        assert!(!placements.contains(&declared));
        assert_eq!(aliased_loader.identity.digest, loader.identity.digest);
        let aliased_roles = [NativeCatalogEntry {
            target_digest: aliased_inputs
                .require_initial(&declared)
                .unwrap()
                .identity
                .digest,
            bootstrap_digest: bootstrap.identity.digest,
            stock_loader: File::from(aliased_loader.descriptor().try_clone_to_owned().unwrap()),
            stock_loader_digest: aliased_loader.identity.digest,
            runner: Arc::clone(&runner),
            config: Arc::clone(&roles[0].config),
        }];
        let aliased_catalog = wire::decode(
            &driver_catalog_bytes(
                &aliased_inputs,
                &bootstrap,
                &runner,
                &runtime,
                &aliased_roles,
                NATIVE_BUNDLE.source_digest,
            )
            .unwrap(),
        )
        .unwrap();
        let aliased_stock =
            &aliased_catalog.objects[usize::from(aliased_catalog.targets[0].stock_loader)];
        assert_eq!(aliased_stock.digest, loader.identity.digest);
        assert_eq!(
            aliased_stock.backing_path.as_deref(),
            Some(STOCK_LOADER_PATH)
        );
        let aliased_bootstrap = aliased_catalog
            .objects
            .iter()
            .find(|object| object.digest == bootstrap.identity.digest)
            .unwrap();
        assert_eq!(
            aliased_bootstrap.backing_path.as_deref(),
            Some(resolved_interpreter.as_bytes())
        );
    }

    #[test]
    fn actual_retained_artifact_adoption_keeps_compiled_bytes_and_inode() {
        for compiled in [
            &NATIVE_BUNDLE.bootstrap,
            &NATIVE_BUNDLE.runner,
            &NATIVE_BUNDLE.runtime,
        ] {
            let captured = HeldArtifact::capture(compiled).unwrap();
            for _ in 0..2 {
                let adopted =
                    HeldArtifact::adopt(captured.file.try_clone().unwrap(), InputRole::Executable)
                        .unwrap();
                assert_eq!(adopted.identity, captured.identity);
                assert_eq!(adopted.bytes, compiled.bytes);
            }
        }
    }
}
