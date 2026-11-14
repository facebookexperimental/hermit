/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The branch settings of `ci/landing-branches.env`.
//!
//! The landing branch is the branch changes land on: the merge queue's base,
//! the branch validation measures a checkout against, and (for Reverie) the
//! history a Hermit Reverie pin must be on. The upstream branch is the
//! fast-forward-only inbox that mirrors facebookexperimental. They are
//! separate settings, both `main` today.
//!
//! A setting is the environment variable named after its key when that is
//! set, and otherwise the key's `KEY=value` line in the checkout's
//! [`SETTINGS_FILE`]. There is no built-in default: a setting that is missing,
//! empty, repeated or not a plain branch name is an error, so no reader can
//! quietly fall back to `main`. `scripts/lib/landing_branch.sh` and
//! `scripts/landing_branch.py` implement the same rule.
//!
//! Standard library only: `scripts/check-reverie-pin.rs` and
//! `scripts/check-record-version-floor.rs` include this file with `#[path]`
//! and are compiled by plain `rustc --edition=2021`, while Buck compiles this
//! library as edition 2024.

use std::ffi::OsString;
use std::path::Path;

/// The settings file, relative to a Hermit checkout's root.
pub const SETTINGS_FILE: &str = "ci/landing-branches.env";

/// The key of Hermit's landing branch.
pub const HERMIT_LANDING_BRANCH: &str = "HERMIT_LANDING_BRANCH";

/// The key of Reverie's landing branch, the history a Hermit Reverie pin must
/// be on.
pub const REVERIE_LANDING_BRANCH: &str = "REVERIE_LANDING_BRANCH";

/// Hermit's landing branch for the checkout at `root`.
pub fn hermit(root: &Path) -> Result<String, String> {
    setting(root, HERMIT_LANDING_BRANCH)
}

/// Reverie's landing branch, as the Hermit checkout at `root` records it.
pub fn reverie(root: &Path) -> Result<String, String> {
    setting(root, REVERIE_LANDING_BRANCH)
}

/// The branch `key` names for the checkout at `root`: the environment
/// variable `key` when it is set, otherwise `key`'s line in `root`'s
/// [`SETTINGS_FILE`].
pub fn setting(root: &Path, key: &str) -> Result<String, String> {
    let file = root.join(SETTINGS_FILE);
    resolve(key, std::env::var_os(key), || {
        std::fs::read_to_string(&file).map_err(|error| {
            format!(
                "cannot read {}, and {key} is not set: {error}",
                file.display()
            )
        })
    })
}

/// [`setting`] with its sources passed in, so the rule is testable without
/// the process environment or a checkout: `env` is the variable's value
/// (`None` when it is unset), and `file` reads the settings file, which only
/// happens when `env` is `None`.
pub fn resolve(
    key: &str,
    env: Option<OsString>,
    file: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    let (source, value) = match env {
        Some(value) => (
            format!("environment variable {key}"),
            value
                .into_string()
                .map_err(|_| format!("environment variable {key} is not UTF-8"))?,
        ),
        None => (
            SETTINGS_FILE.to_string(),
            value_in(&file()?, key)?
                .ok_or_else(|| format!("{SETTINGS_FILE} does not set {key}"))?,
        ),
    };
    if !is_branch_name(&value) {
        return Err(format!(
            "{source} gives {key} the value {value:?}, which is not a branch name"
        ));
    }
    Ok(value)
}

/// The value `text`, the settings file, gives `key`, or `None` when no line
/// sets it. Every line is blank, a `#` comment, or exactly `KEY=value` with
/// no surrounding space; anything else, and a key set twice, is an error.
pub fn value_in(text: &str, key: &str) -> Result<Option<String>, String> {
    let mut found = None;
    for (index, raw) in text.split('\n').enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=').filter(|(name, _)| is_key(name)) else {
            return Err(format!(
                "{SETTINGS_FILE}:{} is not a KEY=value line",
                index + 1
            ));
        };
        if name != key {
            continue;
        }
        if found.replace(value.to_string()).is_some() {
            return Err(format!("{SETTINGS_FILE} sets {key} more than once"));
        }
    }
    Ok(found)
}

/// Whether `name` is a setting key: an uppercase letter, then uppercase
/// letters, digits and `_`.
fn is_key(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Whether `name` is a plain branch name: `/`-separated components, each
/// starting with an ASCII letter or digit, made of letters, digits, `.`, `_`
/// and `-`, and not ending in `.` or `.lock`; no `..` anywhere.
pub fn is_branch_name(name: &str) -> bool {
    !name.contains("..")
        && name.split('/').all(|component| {
            component.starts_with(|c: char| c.is_ascii_alphanumeric())
                && !component.ends_with('.')
                && !component.ends_with(".lock")
                && component
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str =
        "# comment\n\nHERMIT_LANDING_BRANCH=main\nREVERIE_LANDING_BRANCH=integration\n";

    fn from_file(text: &'static str) -> impl FnOnce() -> Result<String, String> {
        move || Ok(text.to_string())
    }

    fn unread() -> Result<String, String> {
        panic!("the settings file must not be read when the variable is set")
    }

    #[test]
    fn the_variable_wins_and_the_file_is_then_not_read() {
        assert_eq!(
            resolve(HERMIT_LANDING_BRANCH, Some("integration".into()), unread),
            Ok("integration".to_string())
        );
    }

    #[test]
    fn the_file_answers_when_the_variable_is_unset() {
        assert_eq!(
            resolve(HERMIT_LANDING_BRANCH, None, from_file(FILE)),
            Ok("main".to_string())
        );
        assert_eq!(
            resolve(REVERIE_LANDING_BRANCH, None, from_file(FILE)),
            Ok("integration".to_string())
        );
        assert_eq!(
            resolve(
                HERMIT_LANDING_BRANCH,
                None,
                from_file("HERMIT_LANDING_BRANCH=main\r\n")
            ),
            Ok("main".to_string())
        );
    }

    #[test]
    fn there_is_no_default_branch() {
        for (env, text) in [
            (None, "REVERIE_LANDING_BRANCH=main\n"),
            (None, ""),
            (None, "HERMIT_LANDING_BRANCH=\n"),
            (Some(""), FILE),
        ] {
            let outcome = resolve(
                HERMIT_LANDING_BRANCH,
                env.map(OsString::from),
                from_file(text),
            );
            assert!(outcome.is_err(), "{env:?} {text:?}: {outcome:?}");
        }
        let unreadable = resolve(HERMIT_LANDING_BRANCH, None, || Err("no such file".into()));
        assert_eq!(unreadable, Err("no such file".to_string()));
    }

    #[test]
    fn a_malformed_or_repeated_setting_is_refused() {
        for text in [
            "HERMIT_LANDING_BRANCH=main\nHERMIT_LANDING_BRANCH=main\n",
            "HERMIT_LANDING_BRANCH = main\n",
            "hermit_landing_branch=main\n",
            "just words\n",
            "HERMIT_LANDING_BRANCH=main \n",
        ] {
            let outcome = resolve(HERMIT_LANDING_BRANCH, None, from_file(text));
            assert!(outcome.is_err(), "{text:?}: {outcome:?}");
        }
    }

    #[test]
    fn branch_names_are_plain() {
        for name in ["main", "integration", "release/0.5.6", "a", "a-b_c.d"] {
            assert!(is_branch_name(name), "{name}");
        }
        for name in [
            "",
            "-x",
            ".x",
            "x.",
            "a..b",
            "a/",
            "/a",
            "a//b",
            "x.lock",
            "a.lock/b",
            "a b",
            "a~1",
            "a^",
            "a:b",
            "refs/heads/../x",
            "\u{e9}",
        ] {
            assert!(!is_branch_name(name), "{name}");
        }
    }
}
