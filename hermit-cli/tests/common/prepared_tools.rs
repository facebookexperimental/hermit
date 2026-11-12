/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The tool executables a harness declares for cases that otherwise search
//! fixed host paths, such as `/usr/bin/lua` or `/usr/bin/node`.
//!
//! `HERMIT_PREPARED_TOOLS` holds a JSON object from a tool's name to the
//! absolute path of its executable. A case that asks for a tool the object
//! names runs that executable. A tool the object does not name, or an unset
//! variable, keeps the case's own fixed search. A declaration that is present
//! but invalid fails, naming its first problem, instead of falling back.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

const PREPARED_TOOLS_ENV: &str = "HERMIT_PREPARED_TOOLS";

/// The executable declared for `name` in `HERMIT_PREPARED_TOOLS`, or `None`
/// when the variable is unset or does not name the tool. Panics, before the
/// tool runs, if the declaration is invalid.
pub fn prepared_tool(name: &str) -> Option<PathBuf> {
    parse_prepared_tools(std::env::var_os(PREPARED_TOOLS_ENV))
        .unwrap_or_else(|problem| panic!("{PREPARED_TOOLS_ENV} {problem}"))
        .remove(name)
}

/// Every declared tool, or an empty map for no declaration. Every entry is
/// checked before any is returned; the first problem found is the error.
pub fn parse_prepared_tools(raw: Option<OsString>) -> Result<BTreeMap<String, PathBuf>, String> {
    let Some(raw) = raw else {
        return Ok(BTreeMap::new());
    };
    let raw = raw.into_string().map_err(|_| "is not UTF-8".to_owned())?;
    let declared: BTreeMap<String, PathBuf> = serde_json::from_str(&raw)
        .map_err(|error| format!("is not a JSON object of paths: {error}"))?;
    if declared.is_empty() {
        return Err("names no tool".to_owned());
    }
    for (name, path) in &declared {
        check_tool(name, path)?;
    }
    Ok(declared)
}

fn check_tool(name: &str, path: &Path) -> Result<(), String> {
    if name.is_empty() || name.contains('/') {
        return Err(format!("names the tool {name:?}, which is not a file name"));
    }
    let shown = path.display();
    if !path.is_absolute() {
        return Err(format!("names {name} {shown}, which is not absolute"));
    }
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("names {name} {shown}: {error}"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "names {name} {shown}, which is not an executable file"
        ));
    }
    Ok(())
}
