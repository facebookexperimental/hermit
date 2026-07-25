/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use hermit::Context;
use hermit::Error;
use hermit::SerializableError;
use reverie::process::Container;
use reverie::process::Mount;
use reverie::process::Namespace;

const GROUP_FILE: &str = "/etc/group";
const NSCD_DIR: &str = "/var/run/nscd";
const OVERFLOW_GID: &str = "65534";

// Bind mount sources must outlive Reverie's pre-exec container setup, which
// applies the mounts in the forked child before exec. Hold this guard in the
// caller until after `Container::run` returns so the backing temp files still
// exist when the child binds them.
pub(super) struct IdentityGuard {
    _group_file: tempfile::NamedTempFile,
    _nscd_dir: Option<tempfile::TempDir>,
}

/// Snapshot the host group database into a private temp file, appending a
/// synthetic overflow group (`nobody:x:65534`) when the host lacks one. Binding
/// this frozen copy read-only over `/etc/group` keeps guest group-name
/// resolution stable across otherwise-identical runs.
fn frozen_group_file() -> Result<tempfile::NamedTempFile, Error> {
    let mut contents = fs::read_to_string(GROUP_FILE)
        .context("Failed to read the host group database for the guest")?;
    let has_overflow_group = contents.lines().any(|line| {
        line.split(':')
            .nth(2)
            .is_some_and(|gid| gid == OVERFLOW_GID)
    });
    if !has_overflow_group {
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str("nobody:x:");
        contents.push_str(OVERFLOW_GID);
        contents.push_str(":\n");
    }

    let mut group_file = tempfile::NamedTempFile::new()
        .context("Failed to create the frozen group database for the guest")?;
    group_file
        .write_all(contents.as_bytes())
        .context("Failed to populate the frozen group database for the guest")?;
    group_file
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))
        .context("Failed to set permissions on the frozen guest group database")?;
    Ok(group_file)
}

/// Deterministic identity-resolution mounts shared by `run`, `record`, and
/// `replay`: a frozen `/etc/group` and an empty directory over the host nscd
/// cache. These keep guest NSS lookups from reaching nondeterministic host
/// state (the nscd cache and the systemd-userdb socket), so record/replay
/// reproduce the same group/user resolution that `run` mode already enforces.
/// Returns the mounts plus a guard that must outlive container setup.
pub(super) fn identity_hardening_mounts() -> Result<(Vec<Mount>, IdentityGuard), Error> {
    let group_file = frozen_group_file()?;
    let mut mounts = vec![Mount::bind(group_file.path(), GROUP_FILE).readonly()];

    // Host nscd cache readiness is external state and can differ between runs.
    let nscd_dir = if Path::new(NSCD_DIR).is_dir() {
        let directory =
            tempfile::TempDir::new().context("Failed to create the empty guest nscd directory")?;
        mounts.push(Mount::bind(directory.path(), NSCD_DIR).readonly());
        Some(directory)
    } else {
        None
    };

    Ok((
        mounts,
        IdentityGuard {
            _group_file: group_file,
            _nscd_dir: nscd_dir,
        },
    ))
}

pub(super) fn apply_affinity(container: &mut Container, pin_threads: bool) {
    if pin_threads {
        let rand_core: usize = rand::random_range(0..num_cpus::get());
        tracing::info!("Pinning tracer and guest threads to core {}", rand_core);
        container.affinity(rand_core);
    }
}

pub fn default_container(pin_threads: bool) -> Container {
    let mut container = Container::new();
    container
        .unshare(Namespace::PID)
        .map_root()
        .hostname("hermetic-container.local")
        .domainname("local")
        .mount(Mount::proc());

    apply_affinity(&mut container, pin_threads);
    container
}

/// A [`default_container`] hardened with the deterministic identity mounts
/// (frozen `/etc/group`, hidden nscd cache) that `run` mode applies. Record and
/// replay use this so guest NSS resolution matches `run` and does not reach
/// nondeterministic host identity state. The returned [`IdentityGuard`] must be
/// held until after `Container::run` returns.
pub(super) fn deterministic_container() -> Result<(Container, IdentityGuard), Error> {
    let mut container = default_container(true);
    let (mounts, identity_guard) = identity_hardening_mounts()?;
    container.mounts(mounts);
    Ok((container, identity_guard))
}

/// Helper to run a function inside a container, taking care to display any
/// errors and propagate the exit status.
pub fn with_container<F, T>(container: &mut Container, mut f: F) -> Result<T, Error>
where
    F: FnMut() -> Result<T, Error>,
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    Ok(container
        .run(|| f().map_err(SerializableError::from))
        .context("Sandbox container exited unexpectedly")??)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The frozen group database must always resolve the overflow GID so that
    // guest group-name lookups (e.g. `groups`) do not depend on nondeterministic
    // host NSS. This is what keeps record/replay identity resolution matching
    // `run` mode regardless of whether the host `/etc/group` lists 65534.
    #[test]
    fn identity_hardening_freezes_group_with_overflow_entry() {
        let (mounts, guard) =
            identity_hardening_mounts().expect("identity hardening mounts should be constructible");
        assert!(
            !mounts.is_empty(),
            "expected at least the frozen /etc/group mount"
        );
        let contents = fs::read_to_string(guard._group_file.path())
            .expect("frozen group database should be readable");
        assert!(
            contents
                .lines()
                .any(|line| line.split(':').nth(2) == Some(OVERFLOW_GID)),
            "frozen group database must resolve overflow gid {OVERFLOW_GID}:\n{contents}"
        );
    }
}
