#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! ```cargo
//! [dependencies]
//! serde_json = "1"
//! ```
//!
//! Refuse a bare URL in a doc comment that `cargo doc` lints, in seconds and
//! without building anything.
//!
//! Validation node `doc.rustdoc` runs `RUSTDOCFLAGS=-D warnings cargo doc
//! --workspace --no-deps --all-features`, and rustdoc's `bare_urls` lint
//! rejects a URL written as plain text, for example the issue link in
//! `/// diff (https://github.com/rrnewton/hermit/issues/3606).` That node needs
//! the pinned root and a full documentation build, and soft-green landing runs
//! validation only after the push, so such lines reached `main` repeatedly
//! (two of them: detcore-sabre/src/glibc_compat.rs, fixed by 54c4f7c69d;
//! ci/manifest-plan/src/bin/test-harness.rs, added by 6166181d8f and fixed by
//! 1b1783e604). This checker runs from `make lint-checks`, which soft green
//! also runs after the push, and from the tracked pre-push hook. The hook runs
//! before the push, but only in a checkout that ran scripts/setup-hooks.sh, and
//! never for a merge made on GitHub (`gh pr merge`, which land-pr.sh uses) or a
//! push with `--no-verify`. Those still reach `doc.rustdoc` unchecked.
//!
//! WHICH DOC COMMENTS COUNT. rustdoc lints only what it documents: crate and
//! module `//!` docs; the outer `///` of every module, private and
//! `#[doc(hidden)]` ones included; `///` on a `pub` item; the fields, variants
//! and trait items of a `pub` struct, enum or trait, private fields included;
//! `pub` methods in an inherent impl; items of a trait impl; a
//! `#[macro_export]` macro; `///` inside the body of a local `macro_rules!`
//! the crate invokes; and every `///` in a binary target outside function
//! bodies. It does not lint any other private or `pub(crate)` item in a
//! library, an item marked `#[doc(hidden)]` other than a module (though it
//! does lint a hidden variant or field of a documented type, and the items
//! inside a hidden inline module), the doc on a `use` line, or anything
//! declared inside a function body. Cargo also skips a binary whose crate name
//! equals its package's library (the `hermit` binary in hermit-cli is one).
//!
//! This checker applies those rules to the targets `cargo metadata` lists,
//! following `mod` declarations from each target root and tracking the brace
//! blocks rustfmt lays out. It ignores module privacy: a `pub` item counts even
//! inside a private module, because deciding whether rustdoc reaches it
//! (through a `pub use`, for example) needs name resolution. That makes it
//! stricter than rustdoc, and the fix for a position rustdoc would not flag is
//! the same one-line `<...>` wrap. It is also stricter on a module rustdoc
//! never compiles when what excludes it is a `#[cfg(test)]` above the doc
//! comment, a `cfg` that is false when rustdoc builds, or an enclosing const
//! block: it checks that module's outer doc. It skips a module with
//! `#[cfg(test)]` between its doc and the `mod` line, and the bodies of items
//! other than modules marked `#[doc(hidden)]`, as rustdoc does. It also skips
//! every `macro_rules!` body by its braces, which rustdoc does not: a `///`
//! inside a local macro the crate invokes is linted after expansion, and this
//! checker misses it. It does not read `#[doc = ...]`.
//!
//! Measured 2026-10-04 on this repository by injecting a distinct bare URL
//! into each of 6251 doc blocks and running rustdoc nightly under
//! `-W rustdoc::bare_urls`: rustdoc flagged 2141 positions and this checker
//! reported 2128 of them (99.4%), plus 576 that rustdoc did not flag. All 13
//! misses are items of a trait impl or the doc of an impl block, where the
//! deciding fact (is the type public?) needs name resolution; `doc.rustdoc`
//! still catches those after landing.
//!
//! What it cannot parse is an error, not a pass: a `mod` declaration it cannot
//! read, a `mod name;` or `mod $name;` inside a `macro_rules!` body (its file
//! depends on where the macro is invoked), an attribute whose brackets never balance
//! (this also refuses a multi-line string line that starts with an unbalanced
//! `#[`), an inline module or `#[cfg(test)]` module that never closes, a macro
//! body whose braces never balance, and a file whose blocks do not match.
//!
//! A URL is not bare inside `<...>`, inside a Markdown link's text or target,
//! inside a code span, or in a fenced or indented code block.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Library,
    Binary,
}

#[derive(Debug, PartialEq, Eq)]
struct Finding {
    line: usize,
    url: String,
}

const LIBRARY_KINDS: &[&str] = &["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

/// The documented target roots in `cargo metadata --no-deps` output.
fn documented_roots(metadata: &serde_json::Value) -> Result<Vec<(PathBuf, Kind)>, String> {
    let packages = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata has no packages list")?;
    let mut roots = Vec::new();
    for package in packages {
        let targets = package["targets"]
            .as_array()
            .ok_or("cargo metadata package has no targets list")?;
        let mut library_crate = None;
        let mut entries = Vec::new();
        for target in targets {
            let name = target["name"].as_str().ok_or("target has no name")?;
            let path = target["src_path"].as_str().ok_or("target has no src_path")?;
            let documented = target["doc"].as_bool().unwrap_or(true);
            let kinds: Vec<&str> = target["kind"]
                .as_array()
                .ok_or("target has no kind list")?
                .iter()
                .filter_map(|kind| kind.as_str())
                .collect();
            let kind = if kinds.iter().any(|kind| LIBRARY_KINDS.contains(kind)) {
                library_crate = Some(name.replace('-', "_"));
                Kind::Library
            } else if kinds.contains(&"bin") {
                Kind::Binary
            } else {
                continue;
            };
            if documented {
                entries.push((name.replace('-', "_"), PathBuf::from(path), kind));
            }
        }
        for (crate_name, path, kind) in entries {
            // Cargo documents only the library when a binary's crate name
            // collides with it.
            if kind == Kind::Binary && library_crate.as_deref() == Some(crate_name.as_str()) {
                continue;
            }
            roots.push((path, kind));
        }
    }
    Ok(roots)
}

/// The first line of a `macro_rules!` body that contains a `mod NAME;`
/// declaration, as its offset in `body` and its text: a file module, whose
/// path depends on where the macro is invoked. NAME may be a metavariable
/// (`mod $name;`) and the declaration may sit inside a repetition
/// (`$(mod $name;)*`) or follow a `{` on the same line. `mod NAME {` declares
/// no file, and neither does text in a string literal or a comment that closes
/// on the same line.
fn macro_body_module_declaration<'a>(body: &[&'a str]) -> Option<(usize, &'a str)> {
    // A string or comment that spans lines usually opens on a line whose
    // literals cannot be delimited. (A nested `/* /* */` comment, or a
    // non-ASCII char literal next to a quote, can defeat that.) Every later
    // line of the body is then matched as written, `//` included: a quote on
    // it may close the earlier literal, pairing quotes within one line would
    // blank a real declaration between two of them, and a `//` may sit inside
    // the literal rather than start a comment.
    let mut undelimited = false;
    for (offset, line) in body.iter().enumerate() {
        let line = if undelimited {
            line.trim()
        } else {
            without_line_comment(line.trim())
        };
        let code = match without_literals(line) {
            Some(code) if !undelimited => code,
            code => {
                undelimited |= code.is_none();
                line.to_string()
            }
        };
        if declares_a_module_file_as_written(&code) {
            return Some((offset, line));
        }
    }
    None
}

/// Whether `code` contains a `mod NAME;` declaration, without blanking
/// literals first: a declaration inside a string or comment counts too.
fn declares_a_module_file_as_written(code: &str) -> bool {
    let mut rest = code;
    while let Some(at) = rest.find("mod") {
        let before = rest[..at].chars().next_back();
        let after = &rest[at + 3..];
        rest = after;
        // Only a character that can continue an identifier (`amod`, `$mod`,
        // `r#mod`) makes this something other than the keyword.
        if before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '#'))
            || !after.starts_with(char::is_whitespace)
        {
            continue;
        }
        let name = after.trim_start();
        let name = name.strip_prefix('$').unwrap_or(name);
        let name = name.strip_prefix("r#").unwrap_or(name);
        let tail = name.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_');
        if tail.len() < name.len() && tail.trim_start().starts_with(';') {
            return true;
        }
    }
    false
}

/// `line` with the contents of its string literals, character literals and
/// `/* */` comments replaced by spaces, or `None` when a string or comment
/// does not close on the line or a string is raw (`r"..."`, `r#"..."#`), whose
/// end this does not find. The line may then be the end of a string opened
/// earlier, or a raw string may hold a `"` or end in `\`.
fn without_literals(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut code = String::with_capacity(line.len());
    let mut index = 0;
    while index < bytes.len() {
        let skip_to = match bytes[index] {
            b'"' => {
                if bytes[..index].iter().rev().find(|b| **b != b'#') == Some(&b'r') {
                    return None;
                }
                let mut end = index + 1;
                while end < bytes.len() && bytes[end] != b'"' {
                    end += if bytes[end] == b'\\' { 2 } else { 1 };
                }
                if end >= bytes.len() {
                    return None;
                }
                end + 1
            }
            b'\'' if bytes.get(index + 2) == Some(&b'\'') => index + 3,
            b'\'' if bytes.get(index + 1) == Some(&b'\\') => bytes[index + 2..]
                .iter()
                .position(|b| *b == b'\'')
                .map_or(index + 1, |offset| index + offset + 3),
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index + 4 + line[index + 2..].find("*/")?
            }
            _ => {
                let c = line[index..].chars().next().unwrap_or(' ');
                code.push(c);
                index += c.len_utf8();
                continue;
            }
        };
        let mut skip_to = skip_to.min(bytes.len());
        while !line.is_char_boundary(skip_to) {
            skip_to += 1;
        }
        code.extend(std::iter::repeat_n(' ', skip_to - index));
        index = skip_to;
    }
    Some(code)
}

/// `line` without its visibility qualifier (`pub`, `pub(crate)`, ...).
fn without_visibility(line: &str) -> &str {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("pub ") {
        rest.trim_start()
    } else if line.starts_with("pub(") {
        line.split_once(')').map_or(line, |(_, rest)| rest.trim_start())
    } else {
        line
    }
}

/// `line` without a trailing `//` comment. A `//` inside a string literal is
/// not a comment.
fn without_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if in_string => index += 1,
            b'"' => in_string = !in_string,
            b'/' if !in_string && bytes.get(index + 1) == Some(&b'/') => {
                return line[..index].trim_end();
            }
            _ => {}
        }
        index += 1;
    }
    line.trim_end()
}

/// The name in a one-line `mod NAME;` declaration.
fn module_declaration(line: &str) -> Option<&str> {
    let name = without_visibility(line)
        .strip_prefix("mod ")?
        .trim()
        .strip_suffix(';')?
        .trim();
    let name = name.strip_prefix("r#").unwrap_or(name);
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(name)
}

/// The name in an inline `mod NAME {` opening line.
fn inline_module(line: &str) -> Option<String> {
    let opening = line.strip_suffix('{')?.trim_end();
    module_declaration(&format!("{opening};")).map(str::to_string)
}

fn attribute_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let inner = line.trim().strip_prefix("#[")?.strip_suffix(']')?;
    let (key, value) = inner.split_once('=')?;
    if key.trim() != name {
        return None;
    }
    value.trim().strip_prefix('"')?.strip_suffix('"')
}

/// Every file reachable from `root_file` through `mod` declarations. Modules
/// declared under `#[cfg(test)]` and `mod $name {` lines in a `macro_rules!`
/// body are skipped. A line that starts like a `mod` declaration and ends with
/// `;` or `{` but does not parse is an error, and so is `mod name;` or
/// `mod $name;` in a macro body, so a layout this function does not model cannot drop a
/// file unnoticed.
fn module_tree(root_file: &Path) -> Result<Vec<PathBuf>, String> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut pending = vec![(root_file.to_path_buf(), true)];
    while let Some((file, is_root)) = pending.pop() {
        if seen.contains(&file) {
            continue;
        }
        seen.push(file.clone());
        let text = fs::read_to_string(&file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        let directory = file.parent().unwrap_or(Path::new(".")).to_path_buf();
        let stem = file.file_stem().and_then(|stem| stem.to_str()).unwrap_or("");
        let children_directory = if is_root || stem == "mod" {
            directory.clone()
        } else {
            directory.join(stem)
        };
        let mut path_attribute = None;
        let mut test_only = false;
        // Enclosing inline `mod NAME {` blocks: the line that closes each
        // (rustfmt puts it at the opening line's indentation), its name, and
        // whether it is test-only.
        let mut inline: Vec<(String, String, bool)> = Vec::new();
        let lines: Vec<&str> = text.lines().collect();
        let mut index = 0;
        while index < lines.len() {
            let line = lines[index];
            index += 1;
            if inline.last().is_some_and(|(close, ..)| line == close) {
                inline.pop();
                continue;
            }
            let trimmed = without_line_comment(line.trim());
            // A `mod $name {` in a macro body declares no file, and `scan`
            // skips the same body. A `mod name;` there, or `mod $name;`, does
            // declare one wherever the macro is invoked, which this function
            // cannot place, so it is an error.
            if without_visibility(trimmed).starts_with("macro_rules!") && trimmed.ends_with('{') {
                let span = balanced_span(&lines, index - 1, b'{', b'}').ok_or_else(|| {
                    format!(
                        "{}: the `macro_rules!` at line {index} never closes its braces",
                        file.display()
                    )
                })?;
                if let Some((offset, body)) =
                    macro_body_module_declaration(&lines[index..index - 1 + span])
                {
                    return Err(format!(
                        "{}:{}: `{body}` in a `macro_rules!` body declares a file \
                         that depends on where the macro is invoked, which this \
                         checker cannot follow",
                        file.display(),
                        index + offset + 1
                    ));
                }
                index += span - 1;
                path_attribute = None;
                test_only = false;
                continue;
            }
            if let Some(name) = inline_module(trimmed) {
                inline.push((format!("{}}}", &line[..indentation(line)]), name, test_only));
                path_attribute = None;
                test_only = false;
                continue;
            }
            if trimmed.starts_with("#[") {
                if trimmed == "#[cfg(test)]" {
                    test_only = true;
                }
                if let Some(value) = attribute_value(trimmed, "path") {
                    path_attribute = Some(value.to_string());
                }
                continue;
            }
            if trimmed.is_empty() {
                continue;
            }
            let declared: Vec<Option<&str>> = trimmed
                .split_inclusive(';')
                .map(module_declaration)
                .collect();
            // A declaration ends with `;` or `{`. That leaves out `mod tests
            // {}` on one line, which declares no file, and a line of a
            // multi-line string that happens to start with `mod `.
            if without_visibility(trimmed).starts_with("mod ")
                && (trimmed.ends_with(';') || trimmed.ends_with('{'))
                && declared.iter().any(Option::is_none)
            {
                return Err(format!(
                    "{}: cannot parse the module declaration `{trimmed}`",
                    file.display()
                ));
            }
            for name in declared.into_iter().flatten() {
                if !test_only && !inline.iter().any(|(.., test)| *test) {
                    let mut parent = children_directory.clone();
                    for (_, inline_name, ..) in &inline {
                        parent.push(inline_name);
                    }
                    let child = match &path_attribute {
                        Some(path) => directory.join(path),
                        None => {
                            let flat = parent.join(format!("{name}.rs"));
                            if flat.is_file() {
                                flat
                            } else {
                                parent.join(name).join("mod.rs")
                            }
                        }
                    };
                    if !child.is_file() {
                        return Err(format!(
                            "{} declares `mod {name};` but {} does not exist",
                            file.display(),
                            child.display()
                        ));
                    }
                    pending.push((child, false));
                }
            }
            path_attribute = None;
            test_only = false;
        }
        if let Some((close, name, _)) = inline.last() {
            return Err(format!(
                "{}: inline `mod {name}` is never closed by a line `{close}`",
                file.display()
            ));
        }
    }
    Ok(seen)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attached {
    Public,
    ExportedMacro,
    Private,
    /// Under `#[doc(hidden)]`. rustdoc strips a hidden item before it lints,
    /// but measured 2026-10-04 it still lints a hidden enum variant's doc.
    Hidden,
    /// A non-`pub` `use`, whose doc rustdoc never renders.
    Use,
    /// A `mod`. Measured 2026-10-04, rustdoc lints a module's outer doc even
    /// when the module is private or hidden.
    Module,
    /// A `#[cfg(test)]` `mod`, which rustdoc never compiles.
    TestModule,
}

/// The net count of `open` minus `close` bytes in `line`, outside string and
/// character literals and before a `//` comment.
fn bracket_balance(line: &str, open: u8, close: u8) -> isize {
    let bytes = without_line_comment(line).as_bytes();
    let mut balance = 0;
    let mut in_string = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            match byte {
                b'\\' => index += 1,
                b'"' => in_string = false,
                _ => {}
            }
        } else if byte == b'"' {
            in_string = true;
        } else if byte == b'\'' {
            // `'x'` and `'\x'` are character literals; anything else is a
            // lifetime.
            if bytes.get(index + 2) == Some(&b'\'') {
                index += 2;
            } else if bytes.get(index + 1) == Some(&b'\\') {
                index += bytes[index + 2..]
                    .iter()
                    .position(|b| *b == b'\'')
                    .map_or(0, |offset| offset + 2);
            }
        } else if byte == open {
            balance += 1;
        } else if byte == close {
            balance -= 1;
        }
        index += 1;
    }
    balance
}

/// The lines from `index` up to the one where `open` and `close` first
/// balance, or `None` when they never do. rustfmt lays a long attribute out
/// over several lines, for example a vertical `#[derive(`.
fn balanced_span(lines: &[&str], index: usize, open: u8, close: u8) -> Option<usize> {
    let mut balance = 0;
    for (offset, line) in lines[index..].iter().enumerate() {
        balance += bracket_balance(line, open, close);
        if balance <= 0 {
            return Some(offset + 1);
        }
    }
    None
}

/// The attribute that starts at `index`, joined without whitespace, and the
/// number of lines it spans. An attribute whose brackets never balance (a raw
/// string such as `r"C:\"` defeats the count) is an error: skipping to the end
/// of the file would pass everything after it unread.
fn attribute(lines: &[&str], index: usize) -> Result<(String, usize), String> {
    let span = balanced_span(lines, index, b'[', b']').ok_or_else(|| {
        format!(
            "the attribute at line {} never closes its brackets",
            index + 1
        )
    })?;
    let text = lines[index..index + span]
        .iter()
        .flat_map(|line| line.chars())
        .filter(|c| !c.is_whitespace())
        .collect();
    Ok((text, span))
}

/// The item a doc block documents: the first line after `index` that is not
/// blank, a comment or part of an attribute.
fn attached_item(lines: &[&str], mut index: usize) -> Result<Attached, String> {
    let mut exported = false;
    let mut hidden = false;
    let mut test_only = false;
    while index < lines.len() {
        let trimmed = lines[index].trim();
        if trimmed.starts_with("#[") {
            let (text, span) = attribute(lines, index)?;
            exported |= text == "#[macro_export]";
            hidden |= text.starts_with("#[doc(hidden");
            test_only |= text == "#[cfg(test)]";
            index += span;
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("//") {
            index += 1;
            continue;
        }
        return Ok(if trimmed.starts_with("use ") {
            Attached::Use
        } else if without_visibility(trimmed).starts_with("mod ") {
            if test_only {
                Attached::TestModule
            } else {
                Attached::Module
            }
        } else if hidden {
            Attached::Hidden
        } else if trimmed.starts_with("pub ") {
            Attached::Public
        } else if exported && trimmed.starts_with("macro_rules!") {
            Attached::ExportedMacro
        } else {
            Attached::Private
        });
    }
    Ok(Attached::Private)
}

/// A brace block that changes which doc comments rustdoc documents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Block {
    /// An inline `mod NAME {`. Its privacy does not matter: see
    /// `library_item_is_documented`.
    Module,
    /// A function body: rustdoc documents no item declared inside one.
    Function,
    /// The body of a `#[doc(hidden)]` item other than a module, which rustdoc
    /// strips with everything in it. rustdoc still lints the items of a
    /// hidden inline module, so that stays a `Module`.
    Hidden,
    /// A `struct`, `enum` or `union` body, and whether it is `pub`. rustdoc
    /// documents every field and variant of a public one, hidden ones
    /// included.
    Members(bool),
    /// A `trait` body, and whether it is `pub`. rustdoc documents every trait
    /// item of a public one except a hidden one.
    Trait(bool),
    /// An `impl` body, and whether it implements a trait.
    Impl(bool),
}

/// The block a source line opens, if it opens one this checker tracks. The
/// block ends at the first later line that is exactly the opening line's
/// indentation followed by `}`, which is where rustfmt puts the brace.
fn block_opened(trimmed: &str) -> Option<Block> {
    if trimmed.ends_with(';') || trimmed.ends_with('}') {
        return None;
    }
    let public = trimmed.starts_with("pub ");
    let mut rest = without_visibility(trimmed);
    loop {
        let before = rest;
        for qualifier in ["async ", "const ", "unsafe ", "default "] {
            rest = rest.strip_prefix(qualifier).unwrap_or(rest);
        }
        if let Some(abi) = rest.strip_prefix("extern \"") {
            rest = abi.split_once("\" ").map_or(rest, |(_, after)| after);
        }
        if rest == before {
            break;
        }
    }
    let opens_brace = trimmed.ends_with('{');
    if rest.starts_with("fn ") {
        Some(Block::Function)
    } else if rest.starts_with("mod ") && opens_brace {
        Some(Block::Module)
    } else if ["struct ", "enum ", "union "]
        .iter()
        .any(|keyword| rest.starts_with(keyword))
        && opens_brace
    {
        Some(Block::Members(public))
    } else if rest.starts_with("trait ") && opens_brace {
        Some(Block::Trait(public))
    } else if rest.starts_with("impl ") || rest.starts_with("impl<") {
        Some(Block::Impl(rest.contains(" for ")))
    } else {
        None
    }
}

/// Whether this checker lints a `///` block in a library, given the blocks
/// that enclose it (outermost first) and the item it documents.
///
/// Module privacy is deliberately ignored. rustdoc documents a `pub` item of a
/// private module when a `pub use` re-exports it (`mod tool_local; pub use
/// tool_local::Detcore;` in detcore), and deciding that needs name
/// resolution. Linting every `pub` item, every member of a `pub` type and
/// every `pub` inherent method is stricter than rustdoc for an item nothing
/// re-exports, and the fix there is the same one-line wrap.
fn library_item_is_documented(blocks: &[Block], item: Attached) -> bool {
    if item == Attached::ExportedMacro {
        return true;
    }
    match blocks.iter().rev().find(|block| **block != Block::Module) {
        None | Some(Block::Module) => item == Attached::Public,
        Some(Block::Members(public_type)) | Some(Block::Trait(public_type)) => *public_type,
        // The implemented type may be private, which this checker cannot
        // see, so trait impls and impl-block docs are left to doc.rustdoc.
        Some(Block::Impl(false)) => item == Attached::Public,
        Some(Block::Impl(true)) | Some(Block::Function) | Some(Block::Hidden) => false,
    }
}

/// The doc marker of one source line and the text after it.
fn doc_text(line: &str) -> Option<(&'static str, &str)> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("//!") {
        return Some(("//!", rest));
    }
    if trimmed.starts_with("////") {
        return None;
    }
    trimmed.strip_prefix("///").map(|rest| ("///", rest))
}

fn indentation(text: &str) -> usize {
    text.len() - text.trim_start_matches(' ').len()
}

/// Replace `[start, end)` with spaces, keeping newlines.
fn blank(bytes: &mut [u8], start: usize, end: usize) {
    for byte in &mut bytes[start..end] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
}

fn run_length(bytes: &[u8], start: usize, byte: u8) -> usize {
    bytes[start..].iter().take_while(|b| **b == byte).count()
}

/// Remove code spans, then autolinks and HTML tags, then Markdown links, from
/// one paragraph. What remains is the text rustdoc's lint reads.
fn strip_markdown(paragraph: &str) -> String {
    let mut bytes = paragraph.as_bytes().to_vec();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'`' {
            let run = run_length(&bytes, index, b'`');
            let mut close = index + run;
            let mut matched = None;
            while close < bytes.len() {
                if bytes[close] == b'`' {
                    let other = run_length(&bytes, close, b'`');
                    if other == run {
                        matched = Some(close + other);
                        break;
                    }
                    close += other;
                } else {
                    close += 1;
                }
            }
            match matched {
                Some(end) => {
                    blank(&mut bytes, index, end);
                    index = end;
                }
                None => index += run,
            }
            continue;
        }
        index += 1;
    }
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'<'
            && bytes
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_alphabetic() || *next == b'/' || *next == b'!')
        {
            if let Some(offset) = bytes[index..].iter().position(|b| *b == b'>') {
                blank(&mut bytes, index, index + offset + 1);
                index += offset + 1;
                continue;
            }
        }
        index += 1;
    }
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'[' {
            if let Some(close) = matching(&bytes, index, b'[', b']') {
                let next = bytes.get(close + 1).copied();
                let end = match next {
                    Some(b'(') => matching(&bytes, close + 1, b'(', b')'),
                    Some(b'[') => matching(&bytes, close + 1, b'[', b']'),
                    _ => None,
                };
                if let Some(end) = end {
                    let start = if index > 0 && bytes[index - 1] == b'!' {
                        index - 1
                    } else {
                        index
                    };
                    blank(&mut bytes, start, end + 1);
                    index = end + 1;
                    continue;
                }
            }
        }
        index += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn matching(bytes: &[u8], open_at: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, byte) in bytes[open_at..].iter().enumerate() {
        if *byte == open {
            depth += 1;
        } else if *byte == close {
            depth -= 1;
            if depth == 0 {
                return Some(open_at + offset);
            }
        }
    }
    None
}

fn host_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-@:%._+~#=".contains(&byte)
}

/// URLs that rustdoc's `bare_urls` pattern would match:
/// `https?://([-a-zA-Z0-9@:%._+~#=]{2,256}\.)+[a-zA-Z]{2,63}`, then a path.
fn bare_urls(text: &str) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while let Some(offset) = text[index..].find("http") {
        let start = index + offset;
        let after_scheme = ["https://", "http://"]
            .iter()
            .find(|scheme| text[start..].starts_with(**scheme))
            .map(|scheme| start + scheme.len());
        let Some(host_start) = after_scheme else {
            index = start + 4;
            continue;
        };
        let host_end = host_start
            + bytes[host_start..]
                .iter()
                .take_while(|byte| host_byte(**byte))
                .count();
        let host = &bytes[host_start..host_end];
        let has_domain = host.iter().enumerate().any(|(dot, byte)| {
            *byte == b'.'
                && dot >= 2
                && host[dot + 1..]
                    .iter()
                    .take_while(|b| b.is_ascii_alphabetic())
                    .count()
                    >= 2
        });
        if has_domain {
            let end = host_end
                + bytes[host_end..]
                    .iter()
                    .take_while(|byte| host_byte(**byte) || b"?&/".contains(byte))
                    .count();
            found.push((start, text[start..end].trim_end_matches('.').to_string()));
            index = end;
        } else {
            index = host_end.max(start + 4);
        }
    }
    found
}

/// The bare URLs that rustdoc would report in one source file, or an error
/// when a tracked block, a skipped test module or a `macro_rules!` body never
/// closes: the file is then not laid out the way this checker models, and any
/// verdict on it would be a guess.
fn scan(source: &str, kind: Kind) -> Result<Vec<Finding>, String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut findings = Vec::new();
    let mut index = 0;
    let mut skip_until: Option<String> = None;
    // Attributes seen since the last item line.
    let mut test_attribute = false;
    let mut hidden_attribute = false;
    // Enclosing tracked blocks: the line that closes each, and the block.
    let mut blocks: Vec<(String, Block)> = Vec::new();
    while index < lines.len() {
        let line = lines[index];
        if let Some(close) = &skip_until {
            if line == close {
                skip_until = None;
            }
            index += 1;
            continue;
        }
        if blocks.last().is_some_and(|(close, _)| line == close) {
            blocks.pop();
            index += 1;
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with("#[") {
            let (text, span) = attribute(&lines, index)?;
            test_attribute |= text == "#[cfg(test)]";
            hidden_attribute |= text.starts_with("#[doc(hidden");
            index += span;
            continue;
        }
        let item_line = !trimmed.is_empty() && !trimmed.starts_with("//");
        let (test_item, hidden_item) = (test_attribute, hidden_attribute);
        if item_line {
            test_attribute = false;
            hidden_attribute = false;
        }
        if test_item
            && without_visibility(trimmed).starts_with("mod ")
            && trimmed.ends_with('{')
        {
            skip_until = Some(format!("{}}}", &line[..indentation(line)]));
            index += 1;
            continue;
        }
        // rustfmt does not lay out a macro body, so it is skipped by
        // counting braces rather than by indentation.
        if without_visibility(trimmed).starts_with("macro_rules!") && trimmed.ends_with('{') {
            index += balanced_span(&lines, index, b'{', b'}').ok_or_else(|| {
                format!(
                    "the `macro_rules!` at line {} never closes its braces",
                    index + 1
                )
            })?;
            continue;
        }
        let Some((marker, _)) = doc_text(line) else {
            if item_line {
                // A signature split over several lines is a body only if it
                // reaches `{` before `;`.
                let has_body = trimmed.ends_with('{')
                    || lines[index + 1..]
                        .iter()
                        .map(|later| later.trim())
                        .find(|later| later.ends_with('{') || later.ends_with(';'))
                        .is_some_and(|later| later.ends_with('{'));
                if let Some(block) = block_opened(trimmed).filter(|_| has_body) {
                    let block = if hidden_item && block != Block::Module {
                        Block::Hidden
                    } else {
                        block
                    };
                    blocks.push((format!("{}}}", &line[..indentation(line)]), block));
                }
            }
            index += 1;
            continue;
        };
        let start = index;
        let mut block = Vec::new();
        while index < lines.len() {
            match doc_text(lines[index]) {
                Some((other, text)) if other == marker => block.push(text),
                _ => break,
            }
            index += 1;
        }
        let enclosing: Vec<Block> = blocks.iter().map(|(_, block)| *block).collect();
        let linted = if enclosing.contains(&Block::Function) || enclosing.contains(&Block::Hidden)
        {
            false
        } else if marker == "//!" {
            true
        } else {
            let in_members = matches!(
                enclosing.iter().rev().find(|block| **block != Block::Module),
                Some(Block::Members(_))
            );
            match (attached_item(&lines, index)?, kind) {
                (Attached::Use, _) => false,
                (Attached::Module, _) => true,
                (Attached::TestModule, _) => false,
                (Attached::Hidden, _) if !in_members => false,
                (_, Kind::Binary) => true,
                (item, Kind::Library) => library_item_is_documented(&enclosing, item),
            }
        };
        if linted {
            findings.extend(scan_block(&block, start));
        }
    }
    if let Some(close) = skip_until {
        return Err(format!(
            "a `#[cfg(test)]` module is never closed by a line `{close}`, so the \
             rest of the file was never read"
        ));
    }
    match blocks.first() {
        Some((close, block)) => Err(format!(
            "a {block:?} block is never closed by a line `{close}`, so which doc \
             comments rustdoc documents cannot be decided"
        )),
        None => Ok(findings),
    }
}

/// The bare URLs in one doc block whose first line is source line
/// `first + 1`.
fn scan_block(block: &[&str], first: usize) -> Vec<Finding> {
    let common = block
        .iter()
        .filter(|text| !text.trim().is_empty())
        .map(|text| indentation(text))
        .min()
        .unwrap_or(0);
    let mut findings = Vec::new();
    let mut paragraph: Vec<(usize, &str)> = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    let mut previous_blank = true;
    let mut in_indented_code = false;
    let flush = |paragraph: &mut Vec<(usize, &str)>, findings: &mut Vec<Finding>| {
        if paragraph.is_empty() {
            return;
        }
        let joined: String = paragraph
            .iter()
            .map(|(_, text)| *text)
            .collect::<Vec<_>>()
            .join("\n");
        let stripped = strip_markdown(&joined);
        for (offset, url) in bare_urls(&stripped) {
            let line_in_paragraph = stripped[..offset].matches('\n').count();
            findings.push(Finding {
                line: paragraph[line_in_paragraph].0 + 1,
                url,
            });
        }
        paragraph.clear();
    };
    for (position, raw) in block.iter().enumerate() {
        let text = if raw.len() >= common { &raw[common..] } else { raw.trim_start() };
        let source_line = first + position;
        let leading = indentation(text);
        let body = text.trim_start();
        if let Some((byte, length)) = fence {
            if leading <= 3 && run_length(body.as_bytes(), 0, byte) >= length {
                fence = None;
            }
            continue;
        }
        if leading <= 3 && (body.starts_with("```") || body.starts_with("~~~")) {
            flush(&mut paragraph, &mut findings);
            let byte = body.as_bytes()[0];
            fence = Some((byte, run_length(body.as_bytes(), 0, byte)));
            previous_blank = false;
            continue;
        }
        if body.is_empty() {
            flush(&mut paragraph, &mut findings);
            previous_blank = true;
            continue;
        }
        if leading >= 4 && (previous_blank || in_indented_code) && paragraph.is_empty() {
            in_indented_code = true;
            previous_blank = false;
            continue;
        }
        in_indented_code = false;
        previous_blank = false;
        if leading <= 3 && body.starts_with('[') {
            if let Some(close) = body.find("]:") {
                if !body[..close].contains(']') {
                    flush(&mut paragraph, &mut findings);
                    continue;
                }
            }
        }
        paragraph.push((source_line, text));
    }
    flush(&mut paragraph, &mut findings);
    findings
}

fn repository_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        return Err("not inside a git checkout".to_string());
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

fn cargo_metadata(root: &Path) -> Result<serde_json::Value, String> {
    let output = Command::new("cargo")
        .current_dir(root)
        .args(["metadata", "--offline", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|error| format!("cannot run cargo metadata: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cargo metadata output is not JSON: {error}"))
}

/// Every documented file under `root` and its findings, keyed by path.
fn check(root: &Path) -> Result<(usize, Vec<(PathBuf, Finding)>), String> {
    let metadata = cargo_metadata(root)?;
    // Keyed by canonical path: a `#[path = "../.."]` module shared by two
    // crates is one file and is reported once.
    let mut files: BTreeMap<PathBuf, Kind> = BTreeMap::new();
    for (target_root, kind) in documented_roots(&metadata)? {
        for file in module_tree(&target_root)? {
            let file = fs::canonicalize(&file)
                .map_err(|error| format!("cannot resolve {}: {error}", file.display()))?;
            let entry = files.entry(file).or_insert(kind);
            if kind == Kind::Binary {
                *entry = Kind::Binary;
            }
        }
    }
    let mut findings = Vec::new();
    for (file, kind) in &files {
        let source = fs::read_to_string(file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        let found =
            scan(&source, *kind).map_err(|error| format!("{}: {error}", file.display()))?;
        for finding in found {
            let relative = file.strip_prefix(root).unwrap_or(file).to_path_buf();
            findings.push((relative, finding));
        }
    }
    Ok((files.len(), findings))
}

fn main() {
    rust_script_prelude::init();
    let result = repository_root().and_then(|root| check(&root));
    match result {
        Ok((files, findings)) if findings.is_empty() => {
            println!(
                "check-doc-bare-urls: OK -- no bare URL in the doc comments rustdoc lints \
                 ({files} documented source files)"
            );
        }
        Ok((files, findings)) => {
            for (path, finding) in &findings {
                eprintln!(
                    "{}:{}: bare URL {} in a doc comment; write <{}> or [text]({})",
                    path.display(),
                    finding.line,
                    finding.url,
                    finding.url,
                    finding.url
                );
            }
            eprintln!(
                "check-doc-bare-urls: FAILED -- {} bare URL(s) in {files} documented source \
                 files. rustdoc's bare_urls lint rejects them, and validation node doc.rustdoc \
                 runs it with -D warnings.",
                findings.len()
            );
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("check-doc-bare-urls: ERROR -- {error}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(source: &str, kind: Kind) -> Vec<String> {
        scan(source, kind)
            .unwrap()
            .into_iter()
            .map(|finding| finding.url)
            .collect()
    }

    const ISSUE: &str = "https://github.com/rrnewton/hermit/issues/3606";

    #[test]
    fn the_landed_incident_is_refused() {
        // The exact line 6166181d8f added to a binary target.
        let source = "/// diff (https://github.com/rrnewton/hermit/issues/3606).\nfn main() {}\n";
        assert_eq!(
            scan(source, Kind::Binary).unwrap(),
            vec![Finding {
                line: 1,
                url: ISSUE.to_string()
            }]
        );
        let fixed = "/// diff (<https://github.com/rrnewton/hermit/issues/3606>).\nfn main() {}\n";
        assert!(scan(fixed, Kind::Binary).unwrap().is_empty());
    }

    #[test]
    fn rustdocs_documented_set_is_what_counts() {
        let private = "/// see https://example.com/a\nfn private() {}\n";
        let crate_only = "/// see https://example.com/a\npub(crate) fn crate_only() {}\n";
        let public = "/// see https://example.com/a\n#[inline]\npub fn public() {}\n";
        let module = "//! see https://example.com/a\n";
        assert!(urls(private, Kind::Library).is_empty());
        assert!(urls(crate_only, Kind::Library).is_empty());
        assert_eq!(urls(public, Kind::Library).len(), 1);
        assert_eq!(urls(module, Kind::Library).len(), 1);
        assert_eq!(urls(private, Kind::Binary).len(), 1);
    }

    #[test]
    fn markdown_that_is_not_bare_is_accepted() {
        for text in [
            "<https://example.com/a>",
            "[https://example.com/a](https://example.com/b)",
            "[text][https://example.com/a]",
            "`https://example.com/a`",
            "``https://example.com/a ` still code``",
            "<a href=\"https://example.com/a\">link</a>",
            "the https:// scheme",
            "a `multi\n/// line https://example.com/a` span",
        ] {
            let source = format!("/// {text}\nfn f() {{}}\n");
            assert!(urls(&source, Kind::Binary).is_empty(), "{text}");
        }
        let fenced = "/// ```text\n/// https://example.com/a\n/// ```\nfn f() {}\n";
        let indented = "/// x\n///\n///     https://example.com/a\nfn f() {}\n";
        let reference = "/// [x]: https://example.com/a\nfn f() {}\n";
        for source in [fenced, indented, reference] {
            assert!(urls(source, Kind::Binary).is_empty(), "{source}");
        }
    }

    #[test]
    fn bare_forms_are_refused_on_the_right_line() {
        let source = "/// first\n///   second (https://example.com/a,\n/// http://example.org/b).\nfn f() {}\n";
        let findings = scan(source, Kind::Binary).unwrap();
        assert_eq!(
            findings,
            vec![
                Finding {
                    line: 2,
                    url: "https://example.com/a".to_string()
                },
                Finding {
                    line: 3,
                    url: "http://example.org/b".to_string()
                },
            ]
        );
    }

    #[test]
    fn test_only_modules_are_not_documented() {
        let source = "fn main() {}\n#[cfg(test)]\nmod tests {\n    /// https://example.com/a\n    fn t() {}\n}\n/// https://example.com/b\nfn after() {}\n";
        assert_eq!(urls(source, Kind::Binary), vec!["https://example.com/b"]);
        // A test module that never closes is an error, not a pass for the
        // rest of the file.
        let unclosed = "#[cfg(test)]\nmod tests {\n  }\n/// https://example.com/a\npub fn f() {}\n";
        let error = scan(unclosed, Kind::Library).unwrap_err();
        assert!(error.contains("never closed"), "{error}");
    }

    #[test]
    fn a_multi_line_attribute_does_not_hide_the_item() {
        // rustfmt's layout of a long derive.
        let source = "\
/// https://example.com/a
#[derive(
    Clone,
    Debug,
)]
pub struct Shown {
    /// https://example.com/b
    pub field: u8,
}
/// https://example.com/c
#[cfg_attr(
    feature = \"x\",
    derive(Debug)
)]
struct Private;
";
        assert_eq!(
            urls(source, Kind::Library),
            vec!["https://example.com/a", "https://example.com/b"]
        );
        // A raw string defeats the bracket count. Skipping to the end of the
        // file would pass `g` unread, so it is an error, both for an attribute
        // between items and for one under a doc block.
        for unbalanced in [
            "#[doc = r\"C:\\\"]\npub fn f() {}\n/// https://example.com/g\npub fn g() {}\n",
            "/// x\n#[doc = r\"C:\\\"]\npub fn f() {}\n/// https://example.com/g\npub fn g() {}\n",
        ] {
            let error = scan(unbalanced, Kind::Library).unwrap_err();
            assert!(error.contains("never closes its brackets"), "{error}");
        }
    }

    #[test]
    fn hidden_items_and_use_lines_are_not_documented() {
        let source = "\
/// https://example.com/a
#[doc(hidden)]
pub fn hidden() {}
/// https://example.com/b
#[doc(hidden)]
pub struct Hidden {
    /// https://example.com/c
    pub field: u8,
}
/// https://example.com/d
use std::fmt;
/// https://example.com/e
pub fn shown() {}
pub enum Shown {
    /// https://example.com/f
    #[doc(hidden)]
    Variant,
}
#[doc(hidden)]
pub mod hidden_module {
    /// https://example.com/g
    pub fn linted() {}
}
pub trait Api {
    /// https://example.com/h
    #[doc(hidden)]
    fn hidden_method(&self);
}
/// https://example.com/i
#[doc(hidden)]
pub mod hidden_with_doc {}
/// https://example.com/j
mod private_with_doc {}
/// https://example.com/k
#[cfg(test)]
mod tests {}
";
        // rustdoc lints `g`, the item of a hidden inline module, and not `h`,
        // a hidden trait item (finding 3 of the third review of
        // https://github.com/rrnewton/hermit/pull/3748). It also lints the
        // outer doc of a hidden or private module, `i` and `j` (fourth
        // review), but not of a test module, which it never compiles.
        let expected = [
            "https://example.com/e",
            "https://example.com/f",
            "https://example.com/g",
            "https://example.com/i",
            "https://example.com/j",
        ];
        assert_eq!(urls(source, Kind::Library), expected);
        assert_eq!(urls(source, Kind::Binary), expected);
    }

    #[test]
    fn a_macro_body_is_skipped_by_its_braces() {
        // rustfmt leaves a macro body alone, so an `fn` in it need not close
        // at its own indentation.
        let source = "\
macro_rules! make {
    ($name:ident) => {
        fn $name() {
            let _ = '{';
  }
    };
}
/// https://example.com/a
pub fn after() {}
";
        assert_eq!(urls(source, Kind::Library), vec!["https://example.com/a"]);
        let unclosed = "macro_rules! make {\n    () => {};\n";
        let error = scan(unclosed, Kind::Library).unwrap_err();
        assert!(error.contains("never closes"), "{error}");
    }

    #[test]
    fn members_of_a_public_type_are_documented_and_function_bodies_are_not() {
        // Each case is one rustdoc verdict measured on the scratch crate.
        let source = "\
/// https://example.com/a
pub struct Public {
    /// https://example.com/b
    hidden: u8,
}
struct Private {
    /// https://example.com/c
    pub field: u8,
}
pub trait Shown {
    /// https://example.com/d
    fn declared(
        &self,
    ) -> u8;
    /// https://example.com/e
    fn defaulted(&self) {
        /// https://example.com/f
        fn inner() {}
    }
}
impl Public {
    /// https://example.com/g
    pub fn method(&self) {}
    /// https://example.com/h
    fn private_method(&self) {}
}
impl Shown for Public {
    /// https://example.com/i
    fn declared(&self) -> u8 {
        0
    }
}
mod hidden {
    /// https://example.com/j
    pub fn in_private_module() {}
    /// https://example.com/k
    #[macro_export]
    macro_rules! exported {
        () => {};
    }
}
/// https://example.com/l
pub fn after_everything(
    a: u8,
) {
}
";
        let found = urls(source, Kind::Library);
        // `j` is a `pub` item of a private module: rustdoc documents it only
        // when something re-exports it, which this checker cannot see, so it
        // is linted (finding 1 of the review of
        // https://github.com/rrnewton/hermit/pull/3748).
        let expected: Vec<String> = ["a", "b", "d", "e", "g", "j", "k", "l"]
            .iter()
            .map(|path| format!("https://example.com/{path}"))
            .collect();
        assert_eq!(found, expected);
        // In a binary everything is documented except items inside a body.
        let found = urls(source, Kind::Binary);
        assert!(found.contains(&"https://example.com/h".to_string()));
        assert!(!found.contains(&"https://example.com/f".to_string()));
    }

    #[test]
    fn a_block_that_never_closes_is_an_error_not_a_pass() {
        // Not rustfmt's layout: the body's brace is not at the opening line's
        // indentation, so nothing after it can be placed.
        let source = "pub fn f() {
    }
/// https://example.com/a
pub fn g() {}
";
        let error = scan(source, Kind::Library).unwrap_err();
        assert!(error.contains("never closed"), "{error}");
    }

    #[test]
    fn macro_body_module_declarations() {
        for line in [
            "mod $n;",
            "pub(crate) mod $n ;",
            "$(pub mod $n;)*",
            "($n:ident) => {mod $n; $($t)*};",
            "[mod $n;]",
            "mod r#x;",
            "#[path = \"é.rs\"] mod x;",
            // Strings this cannot delimit: raw ones, and the closing line of
            // one that opened earlier.
            "let s = r\"\\\"; mod x;",
            "let s = r#\"a\"b\"#; mod x;",
            "end\"; mod x;",
            "let s = r#\"a\"b\"#; mod x; let t = r#\"c\"d\"#;",
            "let s = r\"\\\"; mod x; let c = '\"';",
            "f(/* open; mod x;",
        ] {
            assert!(macro_body_module_declaration(&[line]).is_some(), "{line}");
        }
        // On the line that closes a string opened earlier, a `//` inside the
        // string starts no comment.
        assert_eq!(
            macro_body_module_declaration(&["let s = \"a", "b // c\"; mod x;"]),
            Some((1, "b // c\"; mod x;"))
        );
        for line in [
            "mod $n {",
            "$(mod $n { })*",
            "amod x;",
            "$mod x;",
            "r#mod x;",
            "module;",
            "use super::mod_x;",
            "const S: &str = \" mod x;\";",
            "f(/* mod x; */);",
            "let c = '\"'; let d = \"mod x;\";",
            "é mod é {",
        ] {
            assert!(macro_body_module_declaration(&[line]).is_none(), "{line}");
        }
    }

    #[test]
    fn module_paths_follow_declarations() {
        let directory = std::env::temp_dir().join(format!(
            "check-doc-bare-urls-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(directory.join("b")).unwrap();
        fs::write(
            directory.join("lib.rs"),
            "mod a; // trailing comment\npub mod b; mod e;\n#[path = \"other.rs\"]\npub mod c;\n#[cfg(test)]\nmod t;\n",
        )
        .unwrap();
        fs::write(directory.join("e.rs"), "").unwrap();
        fs::write(directory.join("a.rs"), "pub mod inner;\n").unwrap();
        fs::create_dir_all(directory.join("a")).unwrap();
        fs::write(directory.join("a/inner.rs"), "").unwrap();
        fs::write(directory.join("b/mod.rs"), "pub(crate) mod d;\n").unwrap();
        fs::write(directory.join("b/d.rs"), "").unwrap();
        fs::write(directory.join("other.rs"), "").unwrap();
        let mut tree: Vec<String> = module_tree(&directory.join("lib.rs"))
            .unwrap()
            .into_iter()
            .map(|path| path.strip_prefix(&directory).unwrap().display().to_string())
            .collect();
        tree.sort();
        let expected = [
            "a.rs",
            "a/inner.rs",
            "b/d.rs",
            "b/mod.rs",
            "e.rs",
            "lib.rs",
            "other.rs",
        ];
        assert_eq!(tree, expected);
        fs::write(directory.join("lib.rs"), "mod missing;\n").unwrap();
        let error = module_tree(&directory.join("lib.rs")).unwrap_err();
        assert!(error.contains("mod missing;"), "{error}");
        // A declaration this function cannot parse is an error, not a file
        // silently left out.
        fs::write(directory.join("lib.rs"), "pub mod a /* why */;\n").unwrap();
        let error = module_tree(&directory.join("lib.rs")).unwrap_err();
        assert!(error.contains("cannot parse"), "{error}");
        fs::write(directory.join("lib.rs"), "mod inline {\n    fn f() {}\n").unwrap();
        let error = module_tree(&directory.join("lib.rs")).unwrap_err();
        assert!(error.contains("never closed"), "{error}");
        // Valid Rust that only looks like a declaration is not an error: a
        // `mod $name {` in a macro body and a string line starting with `mod `
        // (finding 2 of the third review of
        // https://github.com/rrnewton/hermit/pull/3748). A raw identifier
        // still names its file.
        fs::write(
            directory.join("lib.rs"),
            "macro_rules! m {\n    ($m:ident) => {\n        pub mod $m {\n        }\n    };\n}\nconst S: &str = \"\nmod not a declaration\n\";\nmod r#e;\n",
        )
        .unwrap();
        let tree: Vec<PathBuf> = module_tree(&directory.join("lib.rs")).unwrap();
        assert_eq!(tree, [directory.join("lib.rs"), directory.join("e.rs")]);
        fs::write(directory.join("lib.rs"), "macro_rules! m {\n    () => {};\n").unwrap();
        let error = module_tree(&directory.join("lib.rs")).unwrap_err();
        assert!(error.contains("never closes"), "{error}");
        // A literal `mod name;` in a macro body declares a real file wherever
        // the macro is invoked, so skipping it would drop that file unread
        // (finding 1 of the fourth review).
        fs::write(
            directory.join("lib.rs"),
            "macro_rules! declare {\n    () => {\n        pub mod e; // comment\n    };\n}\ndeclare!();\n",
        )
        .unwrap();
        let error = module_tree(&directory.join("lib.rs")).unwrap_err();
        assert!(error.contains("lib.rs:3: `pub mod e;`"), "{error}");
        // The same holds when the name is a metavariable, alone or in a
        // repetition: rustdoc follows `decl!(e)` to e.rs (finding of the fifth
        // review).
        for (pattern, body) in [("$m:ident", "pub mod $m;"), ("$($m:ident),*", "$(mod $m;)*")] {
            fs::write(
                directory.join("lib.rs"),
                format!("macro_rules! decl {{\n    ({pattern}) => {{\n        {body}\n    }};\n}}\ndecl!(e);\n"),
            )
            .unwrap();
            let error = module_tree(&directory.join("lib.rs")).unwrap_err();
            assert!(error.contains(&format!("lib.rs:3: `{body}`")), "{error}");
        }
        // rustfmt leaves a one-line arm with a repetition as it is, so a
        // declaration can follow `{` (finding 1 of the sixth review).
        fs::write(
            directory.join("lib.rs"),
            "macro_rules! decl {\n    ($n:ident $(, $t:item)*) => {mod $n; $($t)*};\n}\ndecl!(e);\n",
        )
        .unwrap();
        let error = module_tree(&directory.join("lib.rs")).unwrap_err();
        assert!(error.contains("lib.rs:2: "), "{error}");
        // A `mod x;` inside a string in a macro body declares nothing, but one
        // after a raw string or after the end of a string opened on an earlier
        // line is not lost to it (review of
        // https://github.com/rrnewton/hermit/pull/3758).
        fs::write(
            directory.join("lib.rs"),
            "macro_rules! m {\n    () => { const S: &str = \" mod x;\"; };\n}\n",
        )
        .unwrap();
        let tree: Vec<PathBuf> = module_tree(&directory.join("lib.rs")).unwrap();
        assert_eq!(tree, [directory.join("lib.rs")]);
        // After a string closes on a later line, quotes in the rest of that
        // line or a later one no longer pair as written (second review of
        // the same pull request).
        for body in [
            "const S: &str = r#\"a\"b\"#; mod $n;",
            "const S: &str = \"one\nline\"; mod $n;",
            "const S: &str = \"one\nline\"; mod $n; const C: char = '\"';",
            "const S: &str = \"one\nline\"; mod $n; const U: &str = \"https://example.com\";",
            "const S: &str = \"one\nline\"; mod $n; const T: &str = \"a\\\"b\";",
            // A `//` inside the string, on the line that closes it, starts no
            // comment (third review of the same pull request).
            "const S: &str = \"see\nhttps://example.com\"; mod $n;",
            "const S: &str = \"a\nb // c\"; mod $n;",
            "const S: &str = r#\"see\nhttps://example.com\"#; mod $n;",
        ] {
            fs::write(
                directory.join("lib.rs"),
                format!("macro_rules! decl {{\n    ($n:ident) => {{\n        {body}\n    }};\n}}\ndecl!(e);\n"),
            )
            .unwrap();
            let error = module_tree(&directory.join("lib.rs")).unwrap_err();
            assert!(error.contains("declares a file"), "{body}: {error}");
        }
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_binary_named_like_its_library_is_not_documented() {
        let metadata: serde_json::Value = serde_json::from_str(
            r#"{"packages":[{"targets":[
                {"name":"hermit","src_path":"/r/lib.rs","kind":["rlib","cdylib"],"doc":true},
                {"name":"hermit","src_path":"/r/main.rs","kind":["bin"],"doc":true},
                {"name":"test-harness","src_path":"/r/harness.rs","kind":["bin"],"doc":true},
                {"name":"quiet","src_path":"/r/quiet.rs","kind":["bin"],"doc":false},
                {"name":"it","src_path":"/r/tests/it.rs","kind":["test"],"doc":false}
            ]}]}"#,
        )
        .unwrap();
        assert_eq!(
            documented_roots(&metadata).unwrap(),
            vec![
                (PathBuf::from("/r/lib.rs"), Kind::Library),
                (PathBuf::from("/r/harness.rs"), Kind::Binary),
            ]
        );
    }

    #[test]
    fn the_repository_has_no_bare_url_in_linted_docs() {
        // This file lives in `scripts/`, so its parent's parent is the
        // checkout the tests describe, whatever the working directory.
        let root = Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .canonicalize()
            .unwrap();
        let (files, findings) = check(&root).unwrap();
        assert!(files > 0);
        assert!(findings.is_empty(), "{findings:?}");
    }
}
