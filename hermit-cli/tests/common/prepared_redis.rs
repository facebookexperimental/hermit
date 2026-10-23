/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The Redis executables a harness declares for the Redis workloads.
//!
//! `HERMIT_PREPARED_REDIS` holds a JSON object naming exactly `redis-cli` and
//! `redis-server`. When it is unset, callers keep their own fixed search; a
//! declaration that is present but invalid fails instead of falling back.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

const PREPARED_REDIS_ENV: &str = "HERMIT_PREPARED_REDIS";

/// A declared `redis-server` and `redis-cli`, both validated.
#[derive(Debug, PartialEq, Eq)]
pub struct PreparedRedis {
    pub server: PathBuf,
    pub cli: PathBuf,
}

/// The pair declared in `HERMIT_PREPARED_REDIS`, or `None` when it is unset.
/// Panics, before any Redis process starts, if the declaration is invalid.
pub fn prepared_redis() -> Option<PreparedRedis> {
    parse_prepared_redis(std::env::var_os(PREPARED_REDIS_ENV))
        .unwrap_or_else(|problem| panic!("{PREPARED_REDIS_ENV} {problem}"))
}

/// `Ok(None)` for no declaration; otherwise both executables, or the first
/// problem found. Both are checked before either is returned.
pub fn parse_prepared_redis(raw: Option<OsString>) -> Result<Option<PreparedRedis>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.into_string().map_err(|_| "is not UTF-8".to_owned())?;
    let mut declared: BTreeMap<String, PathBuf> = serde_json::from_str(&raw)
        .map_err(|error| format!("is not a JSON object of paths: {error}"))?;
    let names: Vec<&str> = declared.keys().map(String::as_str).collect();
    if names != ["redis-cli", "redis-server"] {
        return Err(format!(
            "must name exactly redis-cli and redis-server, not {names:?}"
        ));
    }
    let cli = executable(&mut declared, "redis-cli")?;
    let server = executable(&mut declared, "redis-server")?;
    Ok(Some(PreparedRedis { server, cli }))
}

fn executable(declared: &mut BTreeMap<String, PathBuf>, name: &str) -> Result<PathBuf, String> {
    let path = declared.remove(name).expect("the name was checked above");
    let shown = path.display();
    if !path.is_absolute() {
        return Err(format!("names {name} {shown}, which is not absolute"));
    }
    // Redis picks its sentinel and check-tool modes from argv[0]'s file name.
    if path.file_name().and_then(|file| file.to_str()) != Some(name) {
        return Err(format!(
            "names {name} {shown}, which does not keep the file name {name}"
        ));
    }
    let metadata =
        std::fs::metadata(&path).map_err(|error| format!("names {name} {shown}: {error}"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "names {name} {shown}, which is not an executable file"
        ));
    }
    Ok(path)
}
