#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Refuse a cargo feature that a Buck target enables and Reindeer does not
//! resolve.
//!
//! bootstrap/regenerate-rust-deps generates shim/third-party/rust/BUCK with
//! Reindeer. Reindeer resolves every workspace member with each platform's
//! `features` list in shim/third-party/rust/reindeer.toml, or with `default`
//! alone when a platform names none, and generates only the third-party crates
//! and crate features that resolution reaches. The members' BUCK files are
//! hand-written, so a Buck target can enable a cargo feature that Reindeer
//! never resolved. The optional dependencies (`dep:x`) and dependency features
//! (`x/feature`) that feature turns on can then be missing from the generated
//! BUCK. That happened when hermit-cli's LiteInst backend became an optional
//! cargo feature while hermit-cli/BUCK kept enabling it: liteinst2 and
//! reverie-liteinst left the generated BUCK and regeneration failed
//! (https://github.com/rrnewton/hermit/issues/3567).
//!
//! The rule is therefore that Reindeer resolves each workspace member with
//! every one of its cargo features that a Buck target enables, directly or
//! through another feature, on every platform. A feature whose dependencies
//! Reindeer reaches anyway (`sabre = []` has none) is held to the same rule,
//! because the next dependency added under it would go missing without a word.
//!
//! The features are read from the tracked Starlark build files themselves
//! (every BUCK, PACKAGE and .bzl file) rather than from `buck2 uquery`, so the
//! check needs no Buck installation. It reads each `features = [...]` keyword
//! argument (or parameter default) and each `"features": [...]` dict entry,
//! which a macro can pass to a rule as `**kwargs`. It does not work out which
//! target a list belongs to: a name counts against every member with a cargo
//! feature of that name, and a name that is no member's cargo feature (a
//! Buck-only cfg such as `buck-release-provenance`) is ignored. What it cannot
//! read it refuses, with the file and line, instead of skipping: a `features`
//! value other than a literal list of plain string literals ending at `,`, `)`
//! or `}` (a variable, `select()`, a concatenation, a conditional, and so a
//! top-level `features = [...]` assignment, which makes a variable) or the
//! table described next, the string `"features"` used anywhere but as such a
//! dict key (`kwargs["features"]`) or as an entry of a loop that only copies
//! keyword arguments unchanged into another call (described below), a
//! string that sets a cfg feature the way a rustc flag does
//! (`--cfg=feature="x"`), and an f-string with a replacement field
//! (`f"{...}"`, a `{` that is neither doubled nor escaped by a backslash,
//! after an `f`, `fr` or `rf` prefix in any case), whose expression
//! open-source Buck2 evaluates and this check cannot read. A spelling that builds the name `features` at run time, or a
//! build file of another kind, is outside what it reads.
//!
//! The one loop read is a pure forward, as the shim's routed unittests copy
//! their base target's arguments: `for NAME in ("crate_root", "features",
//! ...):`, a literal tuple or list of plain strings, whose whole body is
//! `DEST[NAME] = kwargs[NAME]`, optionally guarded by
//! `if kwargs.get(NAME) != None:`, inside a function (the nearest `def` at the
//! start of a line before it, with no other statement at the start of a line
//! between), where NAME occurs nowhere else: not before the loop, not after it
//! (where it still names the last string), and not in a nested `def`, lambda
//! or comprehension (and no f-string anywhere can name it: each one is
//! refused). Such a copy holds what a caller passed as
//! `features = [...]`, which the scan reads at that call, so it enables
//! nothing new. Any other loop body, or a value built from the copy, is
//! refused like any other use of the string.
//!
//! One `features` value of another form is read and enables nothing: the
//! `"features": {...}` table of a target's `autocargo` `cargo_toml_config`,
//! from which autocargo writes the member's Cargo.toml `[features]` table. It
//! declares each cargo feature and what that feature turns on; it enables none
//! on the target. The shim discards `autocargo` (shim/shims.bzl), and a rule's
//! `features` attribute is a list, so a table could not set it. Its names are
//! therefore not counted, and the features the check compares are the same
//! with or without one. The table is read as strictly as a list: only as
//! an entry of the dict that is a `"cargo_toml_config"` value, holding only
//! distinct plain string literal keys, each mapped to a literal list of plain
//! string literals, and ending at `,`, `)` or `}`. A `features` table anywhere
//! else, or of any other form, is refused.
//!
//! Reindeer's resolution also reads three fields of a crate's fixups
//! (src/index.rs at the pinned revision): `omit_features` drops a feature
//! whatever the platform's list says, `omit_deps` drops a dependency, the
//! optional one a `dep:` feature turns on included, and `cfgs` changes which
//! `[target.'cfg(...)'.dependencies]` apply. This check models none of them, so
//! a workspace member's fixups that set one, at the top level or in a
//! `['cfg(...)']` platform table, are refused. They are read where Reindeer
//! reads them, `<fixups_dir>/<package name>/fixups.toml`, with reindeer.toml's
//! `fixups_dir` resolved against shim/third-party/rust, the directory that
//! holds reindeer.toml and that regenerate-rust-deps passes as
//! `--third-party-dir`, and `fixups` there when it is not set.
//!
//! It reads this repository's build files only. Reverie's crates are not
//! workspace members: their cargo features come from Cargo's resolution of
//! the members' dependencies, not from reindeer.toml, so a feature that
//! Reverie's own BUCK files enable (`nightly` on reverie-process, for example)
//! is not checked here.
//!
//! Run with no argument inside the checkout; `--help` or `-h` prints the usage.
//! Exit status: 0 when every such feature is resolved on every platform, 1 when
//! one is not or a file holds something the check refuses to read, 2 on a
//! usage or load error.
//!
//! ```cargo
//! [dependencies]
//! serde_json = "1"
//! toml = "0.8"
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;

/// The prefix of each line this check prints on standard error.
const NAME: &str = "check-buck-reindeer-features.rs";
const USAGE: &str = "usage: check-buck-reindeer-features.rs [-h | --help]";
const HELP: &str = "Refuse a cargo feature that a Buck target in this checkout enables and \
     Reindeer does not resolve under shim/third-party/rust/reindeer.toml. Run it with no \
     argument from inside a Hermit checkout. Exit status: 0 when every such feature is \
     resolved on every platform, 1 when one is not or a file holds something the check \
     refuses to read, 2 on a usage or load error.";
/// The directory Reindeer reads its configuration from, and whose `fixups`
/// directory it reads fixups from unless `fixups_dir` names another.
const THIRD_PARTY_DIR: &str = "shim/third-party/rust";
const REINDEER_TOML: &str = "shim/third-party/rust/reindeer.toml";
/// The remedy for a run whose Git repository is not a Hermit checkout's.
const CHECKOUT_REMEDY: &str = "run this inside a Hermit checkout, outside its submodules";
/// What to do about a TOML file that does not parse.
const SYNTAX_REMEDY: &str = "correct the syntax error, which stops Reindeer too";
/// The `[platform]` shape Reindeer reads, which each shape error names.
const PLATFORM_FORM: &str = "Reindeer expects a `[platform.<name>]` table for each platform, \
     with an optional `features` list of strings such as `features = [\"default\"]`";
/// The platform entry used when reindeer.toml has no `[platform]` table.
/// Reindeer then uses its built-in list (src/default_platforms.toml at the
/// pinned revision), none of which names features, so every one of them
/// resolves `default` only.
const BUILT_IN_PLATFORMS: &str = "Reindeer's built-in platforms";
/// The tracked Starlark build files the features are read from.
const BUILD_FILES: [&str; 3] = [":(glob)**/BUCK", ":(glob)**/PACKAGE", ":(glob)**/*.bzl"];
/// The fields of a crate's fixups that Reindeer's resolution reads, and what
/// each does there.
const RESOLVER_FIXUPS: [(&str, &str); 3] = [
    (
        "omit_features",
        "with which Reindeer drops a feature whatever the platform's list names",
    ),
    (
        "omit_deps",
        "with which Reindeer drops a dependency, the optional one a `dep:` feature turns on \
         included",
    ),
    (
        "cfgs",
        "which Reindeer adds when it decides which `[target.'cfg(...)'.dependencies]` apply",
    ),
];
/// Why a workspace member's fixups that set a resolver field are refused.
const FIXUPS_REMEDY: &str = "this check does not model that, so remove it from the workspace \
     member's fixups or teach this check to apply it";
/// The only `features` values the scan reads; anything else is refused.
const READABLE_VALUE: &str = "this check reads only a literal list of plain string literals \
     ending at `,`, `)` or `}`, or, as an entry of a `\"cargo_toml_config\"` dict, a literal \
     table of such lists under distinct plain string literal keys";
/// The dict key whose value is the only place the scan reads a `features`
/// table: autocargo's configuration of the Cargo.toml it writes.
const CARGO_TOML_CONFIG: &str = "cargo_toml_config";
/// Why a file the tokenizer stops in yields no features at all.
const UNREADABLE_FILE: &str = "so this check cannot read the file";
/// The string prefixes, compared without regard to case, that make a string
/// literal an f-string. Starlark reads them as part of the literal; the
/// tokenizer reads them as a name directly before it.
const F_STRING_PREFIXES: [&str; 3] = ["f", "fr", "rf"];
/// Why an f-string with a replacement field is refused.
const UNREADABLE_F_STRING: &str = "open-source Buck2 evaluates the expression in an f-string's \
     replacement field, which this check cannot read, so it could name a `features` value or \
     call a rule; write the value without an f-string";
/// Why an unresolved feature is refused, printed once under the violations.
const RESOLUTION_RULE: &str = "Reindeer must resolve each workspace member with every cargo \
     feature its Buck targets enable, or the optional dependencies and dependency features \
     those features turn on can be missing from the generated shim/third-party/rust/BUCK; \
     add the features to each platform's `features` list in shim/third-party/rust/reindeer.toml";
/// Operators and brackets, each listed before any shorter prefix of it, so
/// that `==` and `+=` are never read as `=`.
const PUNCTUATION: &[&str] = &[
    "//=", "**=", "<<=", ">>=", "==", "!=", "<=", ">=", "+=", "-=", "*=", "/=", "%=", "&=", "|=",
    "^=", "//", "**", "<<", ">>", "->", "(", ")", "[", "]", "{", "}", ",", ":", ";", ".", "=", "+",
    "-", "*", "/", "%", "&", "|", "^", "~", "<", ">", "@",
];

/// Each platform's Reindeer feature list, `None` when it names none.
type Platforms = BTreeMap<String, Option<Vec<String>>>;
/// A cargo feature and the features or dependencies it enables.
type CargoFeatures = BTreeMap<String, Vec<String>>;

struct Member {
    name: String,
    cargo_features: CargoFeatures,
}

#[derive(Debug, PartialEq)]
enum Token {
    /// An identifier or keyword.
    Name(String),
    /// A string literal's text between its quotes, escapes left as written.
    Str(String),
    /// An operator or bracket.
    Punct(&'static str),
    /// A number or any other character.
    Other,
}

struct Lexeme {
    token: Token,
    line: usize,
    /// The token's column, 0 at the start of its line, so the end of an
    /// indented block can be found.
    column: usize,
}

/// What one build file says about cargo features.
#[derive(Debug, Default)]
struct Scan {
    /// Every name a literal `features` list holds.
    features: BTreeSet<String>,
    /// How many literal lists were read.
    lists: usize,
    /// How many autocargo feature tables were read. They declare cargo
    /// features rather than enable them, so they add nothing to `features`.
    tables: usize,
    /// How many loops forward a `features` keyword argument unchanged
    /// ([`forwards_kwargs_entry`]). A forwarded value is a copy of one some
    /// caller set, which the scan reads where that caller sets it.
    forwards: usize,
    /// The line of each value the scan refuses to read, and why.
    refusals: Vec<(usize, String)>,
}

/// A string literal at the start of `text`, which begins with `quote`: its
/// text between the quotes, escapes left as written, and its length including
/// the quotes. `None` when it is not terminated.
fn string_literal(text: &str, quote: char) -> Option<(&str, usize)> {
    let triple = if quote == '"' { "\"\"\"" } else { "'''" };
    let delimiter = if text.starts_with(triple) {
        triple
    } else {
        &triple[..1]
    };
    let body = &text[delimiter.len()..];
    let mut chars = body.char_indices();
    while let Some((index, character)) = chars.next() {
        if character == '\\' {
            chars.next();
        } else if character == '\n' && delimiter.len() == 1 {
            return None;
        } else if body[index..].starts_with(delimiter) {
            return Some((&body[..index], 2 * delimiter.len() + index));
        }
    }
    None
}

/// The Starlark tokens of `text`, with comments dropped and each string
/// literal kept whole, so that neither a `#` comment nor a bracket inside a
/// string is read as code. The error is an unterminated string's line.
fn tokenize(text: &str) -> Result<Vec<Lexeme>, (usize, String)> {
    let mut tokens = Vec::new();
    let mut line = 1;
    let mut line_start = 0;
    let mut rest = text;
    while let Some(character) = rest.chars().next() {
        let column = text.len() - rest.len() - line_start;
        let length = if character == '\n' {
            line += 1;
            line_start = text.len() - rest.len() + 1;
            1
        } else if character.is_whitespace() {
            character.len_utf8()
        } else if character == '#' {
            rest.find('\n').unwrap_or(rest.len())
        } else if character == '"' || character == '\'' {
            let (literal, length) = string_literal(rest, character).ok_or_else(|| {
                (
                    line,
                    format!("the string literal here is unterminated, {UNREADABLE_FILE}"),
                )
            })?;
            tokens.push(Lexeme {
                token: Token::Str(literal.to_owned()),
                line,
                column,
            });
            line += rest[..length].matches('\n').count();
            if let Some(newline) = rest[..length].rfind('\n') {
                line_start = text.len() - rest.len() + newline + 1;
            }
            length
        } else if character.is_alphanumeric() || character == '_' {
            let length = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
                .unwrap_or(rest.len());
            let word = &rest[..length];
            let token = if character.is_ascii_digit() {
                Token::Other
            } else {
                // A name ends at a `.`; only a number continues through one.
                let name = word.split('.').next().unwrap_or(word);
                tokens.push(Lexeme {
                    token: Token::Name(name.to_owned()),
                    line,
                    column,
                });
                rest = &rest[name.len()..];
                continue;
            };
            tokens.push(Lexeme {
                token,
                line,
                column,
            });
            length
        } else {
            let (token, length) = match PUNCTUATION.iter().find(|punct| rest.starts_with(**punct)) {
                Some(punct) => (Token::Punct(punct), punct.len()),
                None => (Token::Other, character.len_utf8()),
            };
            tokens.push(Lexeme {
                token,
                line,
                column,
            });
            length
        };
        rest = &rest[length..];
    }
    Ok(tokens)
}

/// Whether the string literal at `tokens[index]` is an f-string with a
/// replacement field: it directly follows, on the same line with nothing
/// between, a name that is one of [`F_STRING_PREFIXES`], and holds a `{` that
/// is not doubled (`{{` is a literal brace). As in Starlark's f-string lexer,
/// raw or not, a backslash takes the next character as text, so `\{` opens
/// no field and in `\{{a}` the `{a}` is one. The longer escapes (`\x7b`,
/// `\u....`, octal) go on with digits only, so skipping one character is
/// enough.
fn is_f_string_with_field(tokens: &[Lexeme], index: usize) -> bool {
    let Some(Lexeme {
        token: Token::Str(text),
        line,
        column,
    }) = tokens.get(index)
    else {
        return false;
    };
    let Some(Lexeme {
        token: Token::Name(prefix),
        line: prefix_line,
        column: prefix_column,
    }) = index.checked_sub(1).and_then(|before| tokens.get(before))
    else {
        return false;
    };
    if prefix_line != line
        || prefix_column + prefix.len() != *column
        || !F_STRING_PREFIXES
            .iter()
            .any(|known| prefix.eq_ignore_ascii_case(known))
    {
        return false;
    }
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            characters.next();
        } else if character == '{' && characters.next() != Some('{') {
            return true;
        }
    }
    false
}

/// How a refusal names the token it stopped at.
fn describe(token: Option<&Token>) -> String {
    match token {
        None => "the end of the file".to_owned(),
        Some(Token::Name(name)) => format!("the name `{name}`"),
        Some(Token::Str(text)) => format!("the string {text:?}"),
        Some(Token::Punct(punct)) => format!("`{punct}`"),
        Some(Token::Other) => "a number or other token".to_owned(),
    }
}

/// The names in the literal list at `tokens[start]` and the index of the
/// token after it, which is a `,`, `)` or `}`, or why it is not one this check
/// reads.
fn literal_list(tokens: &[Lexeme], start: usize) -> Result<(Vec<String>, usize), String> {
    let token = |index: usize| tokens.get(index).map(|lexeme| &lexeme.token);
    if token(start) != Some(&Token::Punct("[")) {
        return Err(format!("is {} rather than a list", describe(token(start))));
    }
    let mut names = Vec::new();
    let mut index = start + 1;
    loop {
        match token(index) {
            Some(Token::Punct("]")) => break,
            Some(Token::Str(name)) if !name.contains('\\') => names.push(name.clone()),
            other => {
                return Err(format!(
                    "holds {} where a string without escapes belongs",
                    describe(other)
                ));
            }
        }
        index += 1;
        match token(index) {
            Some(Token::Punct(",")) => index += 1,
            Some(Token::Punct("]")) => break,
            other => {
                return Err(format!(
                    "holds {} after a string rather than `,` or `]`",
                    describe(other)
                ));
            }
        }
    }
    match token(index + 1) {
        Some(Token::Punct(",") | Token::Punct(")") | Token::Punct("}")) => Ok((names, index + 1)),
        other => Err(format!("continues past its list with {}", describe(other))),
    }
}

/// Nothing when `tokens[start]` begins a literal Cargo feature table this
/// check reads: plain string literal keys, each named once and mapped to a
/// literal list of plain string literals, the table ending at `,`, `)` or
/// `}`. Otherwise why it is not one.
fn feature_table(tokens: &[Lexeme], start: usize) -> Result<(), String> {
    let token = |index: usize| tokens.get(index).map(|lexeme| &lexeme.token);
    if token(start) != Some(&Token::Punct("{")) {
        return Err(format!("is {} rather than a table", describe(token(start))));
    }
    let mut keys = BTreeSet::new();
    let mut index = start + 1;
    loop {
        let key = match token(index) {
            Some(Token::Punct("}")) => break,
            Some(Token::Str(key)) if !key.contains('\\') => key,
            other => {
                return Err(format!(
                    "holds {} where a feature name without escapes belongs",
                    describe(other)
                ));
            }
        };
        if !keys.insert(key) {
            return Err(format!("declares the feature {key:?} twice"));
        }
        if token(index + 1) != Some(&Token::Punct(":")) {
            return Err(format!(
                "holds {} after the feature name {key:?} rather than `:`",
                describe(token(index + 1))
            ));
        }
        let (_, end) = literal_list(tokens, index + 2)
            .map_err(|why| format!("maps the feature {key:?} to a value that {why}"))?;
        match token(end) {
            Some(Token::Punct(",")) => index = end + 1,
            Some(Token::Punct("}")) => {
                index = end;
                break;
            }
            other => {
                return Err(format!(
                    "holds {} after the feature {key:?} rather than `,` or `}}`",
                    describe(other)
                ));
            }
        }
    }
    match token(index + 1) {
        Some(Token::Punct(",") | Token::Punct(")") | Token::Punct("}")) => Ok(()),
        other => Err(format!("continues past its table with {}", describe(other))),
    }
}

/// The key of the dict entry whose value begins at `tokens[index]`, when the
/// two tokens before it are a string literal and `:`.
fn entry_key(tokens: &[Lexeme], index: usize) -> Option<&str> {
    match tokens.get(index.checked_sub(2)?..index)? {
        [
            Lexeme {
                token: Token::Str(key),
                ..
            },
            Lexeme {
                token: Token::Punct(":"),
                ..
            },
        ] => Some(key),
        _ => None,
    }
}

/// Whether the string `"features"` at `tokens[index]` is an entry of a loop
/// that only copies keyword arguments unchanged into another call's
/// arguments. That is the one form, besides a literal dict key, in which the
/// string is read:
///
/// ```text
/// for NAME in ("crate_root", "features", ...):   # a literal tuple or list of plain strings
///     if kwargs.get(NAME) != None:               # optional
///         DEST[NAME] = kwargs[NAME]
/// ```
///
/// The copy holds exactly what a caller passed as `features = [...]`, which
/// the scan reads at that call. So the loop must be exactly this: its body
/// ends at the copy (the next token is on a later line, no further right than
/// `for`); it is inside a function (the nearest `def` at the start of a line
/// before it, with only punctuation, such as a parameter list's closing `):`,
/// at the start of a line between them); and NAME occurs nowhere else in that
/// function, which ends at the next `def` at the start of a line. NAME still
/// names the last string after the loop, and code before the loop can run
/// after it (an enclosing loop, a closure), so any other occurrence, nested
/// `def`s, lambdas and comprehensions included, is refused. Any other use of
/// the string, a built value (`kwargs[NAME] + [...]`), a construction, or a
/// run-time name, stays refused.
fn forwards_kwargs_entry(tokens: &[Lexeme], index: usize) -> bool {
    let token = |index: usize| tokens.get(index).map(|lexeme| &lexeme.token);
    let is_name =
        |index: usize, want: &str| matches!(token(index), Some(Token::Name(name)) if name == want);
    let plain =
        |index: usize| matches!(token(index), Some(Token::Str(text)) if !text.contains('\\'));
    // The literal tuple or list of plain strings around `index`.
    let mut open = index;
    loop {
        match open.checked_sub(1).and_then(&token) {
            Some(Token::Punct(",")) => open -= 1,
            Some(Token::Str(_)) if plain(open - 1) => open -= 1,
            Some(Token::Punct("(" | "[")) => {
                open -= 1;
                break;
            }
            _ => return false,
        }
    }
    let close_bracket = if token(open) == Some(&Token::Punct("(")) {
        ")"
    } else {
        "]"
    };
    let mut close = index;
    loop {
        close += 1;
        match token(close) {
            Some(Token::Punct(",")) => {}
            Some(Token::Str(_)) if plain(close) => {}
            Some(Token::Punct(punct)) if *punct == close_bracket => break,
            _ => return false,
        }
    }
    // `for NAME in` before it and `:` after it.
    let Some(for_index) = open.checked_sub(3) else {
        return false;
    };
    let Some(Token::Name(name)) = token(for_index + 1) else {
        return false;
    };
    if !is_name(for_index, "for")
        || !is_name(open - 1, "in")
        || token(close + 1) != Some(&Token::Punct(":"))
    {
        return false;
    }
    // The body: an optional `if kwargs.get(NAME) != None:`, then the copy.
    let mut body = close + 2;
    if is_name(body, "if") {
        let guard = [
            is_name(body + 1, "kwargs"),
            token(body + 2) == Some(&Token::Punct(".")),
            is_name(body + 3, "get"),
            token(body + 4) == Some(&Token::Punct("(")),
            is_name(body + 5, name),
            token(body + 6) == Some(&Token::Punct(")")),
            token(body + 7) == Some(&Token::Punct("!=")),
            is_name(body + 8, "None"),
            token(body + 9) == Some(&Token::Punct(":")),
        ];
        if !guard.iter().all(|ok| *ok) {
            return false;
        }
        body += 10;
    }
    let destination =
        matches!(token(body), Some(Token::Name(dest)) if dest != "kwargs" && dest != name);
    let copy = [
        destination,
        token(body + 1) == Some(&Token::Punct("[")),
        is_name(body + 2, name),
        token(body + 3) == Some(&Token::Punct("]")),
        token(body + 4) == Some(&Token::Punct("=")),
        is_name(body + 5, "kwargs"),
        token(body + 6) == Some(&Token::Punct("[")),
        is_name(body + 7, name),
        token(body + 8) == Some(&Token::Punct("]")),
    ];
    if !copy.iter().all(|ok| *ok) {
        return false;
    }
    // The loop ends at the copy.
    let end = body + 9;
    let for_column = tokens[for_index].column;
    let copy_line = tokens[body + 8].line;
    if let Some(next) = tokens.get(end) {
        if next.line == copy_line || next.column > for_column {
            return false;
        }
    }
    // Inside a function: the nearest `def` at the start of a line before the
    // loop, with no name, string or number at the start of a line between them
    // (a parameter list's closing `):` may be there).
    let starts_line = |index: usize| index == 0 || tokens[index - 1].line != tokens[index].line;
    let mut def_index = None;
    for before in (0..for_index).rev() {
        let lexeme = &tokens[before];
        if lexeme.column != 0 || !starts_line(before) {
            continue;
        }
        match &lexeme.token {
            Token::Name(word) if word == "def" => {
                def_index = Some(before);
                break;
            }
            Token::Punct(_) => {}
            _ => return false,
        }
    }
    let Some(def_index) = def_index else {
        return false;
    };
    if for_column == 0 {
        return false;
    }
    // NAME occurs nowhere else in that function, which ends at the next `def`
    // at the start of a line.
    let allowed = [
        Some(for_index + 1),
        (body > close + 2).then_some(close + 7),
        Some(body + 2),
        Some(body + 7),
    ];
    let mut at = def_index + 1;
    while let Some(lexeme) = tokens.get(at) {
        match &lexeme.token {
            Token::Name(word) if word == "def" && lexeme.column == 0 && starts_line(at) => break,
            Token::Name(word) if word == name && !allowed.contains(&Some(at)) => return false,
            _ => {}
        }
        at += 1;
    }
    true
}

/// Whether a string literal sets a cfg feature the way a rustc flag does
/// (`--cfg=feature="x"`), which enables a cargo feature with no `features`
/// list.
fn sets_cfg_feature(text: &str) -> bool {
    text.match_indices("feature").any(|(index, word)| {
        let after = text[index + word.len()..].trim_start();
        !text[..index]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
            && after.starts_with('=')
            && !after.starts_with("==")
    })
}

/// Every feature name one build file's literal `features` lists hold, and a
/// refusal for each `features` value it cannot read.
fn scan(text: &str) -> Scan {
    let mut scan = Scan::default();
    let tokens = match tokenize(text) {
        Ok(tokens) => tokens,
        Err(refusal) => {
            scan.refusals.push(refusal);
            return scan;
        }
    };
    let token = |index: usize| tokens.get(index).map(|lexeme| &lexeme.token);
    let list = |start| {
        literal_list(&tokens, start)
            .map(|(names, _)| names)
            .map_err(|why| format!("a `features` value {why}"))
    };
    // For each bracket still open, innermost last, the key of the dict entry
    // whose value it begins, or `None` when it begins no entry's value.
    let mut open = Vec::new();
    for (index, lexeme) in tokens.iter().enumerate() {
        let next = token(index + 1);
        let value = match &lexeme.token {
            Token::Str(_) if is_f_string_with_field(&tokens, index) => {
                scan.refusals.push((
                    lexeme.line,
                    format!(
                        "this string is an f-string with a replacement field; {UNREADABLE_F_STRING}"
                    ),
                ));
                continue;
            }
            Token::Punct("(" | "[" | "{") => {
                open.push(entry_key(&tokens, index));
                continue;
            }
            Token::Punct(")" | "]" | "}") => {
                open.pop();
                continue;
            }
            // A read or comparison of a variable named `features` sets
            // nothing; whatever reaches a rule's attribute passes through a
            // `features =` or a `"features":` checked here.
            Token::Name(name) if name == "features" && next == Some(&Token::Punct("=")) => {
                list(index + 2)
            }
            // A table declares cargo features for the Cargo.toml autocargo
            // writes and enables none, so its names are not added.
            Token::Str(text)
                if text == "features"
                    && next == Some(&Token::Punct(":"))
                    && token(index + 2) == Some(&Token::Punct("{")) =>
            {
                if open.last() != Some(&Some(CARGO_TOML_CONFIG)) {
                    Err(format!(
                        "a `features` value is a table outside a `\"{CARGO_TOML_CONFIG}\"` \
                         dict, the one place where a table declares cargo features"
                    ))
                } else {
                    match feature_table(&tokens, index + 2) {
                        Ok(()) => {
                            scan.tables += 1;
                            continue;
                        }
                        Err(why) => Err(format!("a `features` table {why}")),
                    }
                }
            }
            Token::Str(text) if text == "features" && next == Some(&Token::Punct(":")) => {
                list(index + 2)
            }
            Token::Str(text) if text == "features" && forwards_kwargs_entry(&tokens, index) => {
                scan.forwards += 1;
                continue;
            }
            Token::Str(text) if text == "features" => Err(format!(
                "the string \"features\" is followed by {} rather than `:`, as in \
                 `kwargs[\"features\"]`, through which a macro can set the attribute",
                describe(next)
            )),
            Token::Str(text) if sets_cfg_feature(text) => Err(format!(
                "the string {text:?} sets a cfg feature the way a rustc flag does, not through \
                 a `features` list"
            )),
            _ => continue,
        };
        match value {
            Ok(names) => {
                scan.features.extend(names);
                scan.lists += 1;
            }
            Err(why) => scan
                .refusals
                .push((lexeme.line, format!("{why}; {READABLE_VALUE}"))),
        }
    }
    scan
}

fn platform_features(config: &toml::Table) -> Result<Platforms, String> {
    let Some(table) = config.get("platform") else {
        return Ok(Platforms::from([(BUILT_IN_PLATFORMS.to_owned(), None)]));
    };
    let table = table
        .as_table()
        .ok_or_else(|| format!("{REINDEER_TOML}: [platform] is not a table; {PLATFORM_FORM}"))?;
    if table.is_empty() {
        return Err(format!(
            "{REINDEER_TOML}: [platform] names no platform; {PLATFORM_FORM}"
        ));
    }
    table
        .iter()
        .map(|(name, entry)| {
            let entry = entry.as_table().ok_or_else(|| {
                format!("{REINDEER_TOML}: platform.{name} is not a table; {PLATFORM_FORM}")
            })?;
            let features = match entry.get("features") {
                None => None,
                Some(list) => Some(
                    list.as_array()
                        .and_then(|list| {
                            list.iter()
                                .map(|value| value.as_str().map(str::to_owned))
                                .collect::<Option<Vec<_>>>()
                        })
                        .ok_or_else(|| {
                            format!(
                                "{REINDEER_TOML}: platform.{name}.features is not a list of \
                                 strings; {PLATFORM_FORM}"
                            )
                        })?,
                ),
            };
            Ok((format!("[platform.{name}]"), features))
        })
        .collect()
}

/// Where Reindeer reads fixups: reindeer.toml's `fixups_dir` resolved against
/// the directory that holds it, or that directory's `fixups` when it is not set.
fn fixups_dir(config: &toml::Table) -> Result<PathBuf, String> {
    let dir = match config.get("fixups_dir") {
        None => "fixups",
        Some(value) => value.as_str().ok_or_else(|| {
            format!(
                "{REINDEER_TOML}: fixups_dir is not a string; Reindeer expects a path relative \
                 to {THIRD_PARTY_DIR}, such as `fixups_dir = \"fixups\"`"
            )
        })?,
    };
    Ok(Path::new(THIRD_PARTY_DIR).join(dir))
}

/// The member's cargo features Reindeer enables for one platform.
fn resolved(cargo_features: &CargoFeatures, requested: Option<&[String]>) -> BTreeSet<String> {
    let default = ["default".to_owned()];
    let mut pending = requested
        .unwrap_or(&default)
        .iter()
        .filter(|name| cargo_features.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    let mut enabled = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if !enabled.insert(name.clone()) {
            continue;
        }
        pending.extend(
            cargo_features[&name]
                .iter()
                .filter(|value| cargo_features.contains_key(*value))
                .cloned(),
        );
    }
    enabled
}

/// The member's cargo features that some Buck file names.
fn wanted(member: &Member, named: &BTreeSet<String>) -> BTreeSet<String> {
    named
        .iter()
        .filter(|name| member.cargo_features.contains_key(*name))
        .cloned()
        .collect()
}

/// One line per (member, platform) that leaves unresolved a cargo feature of
/// the member that a Buck file names.
fn violations(platforms: &Platforms, members: &[Member], named: &BTreeSet<String>) -> Vec<String> {
    let mut found = Vec::new();
    for member in members {
        let wanted = wanted(member, named);
        for (platform, requested) in platforms {
            let enabled = resolved(&member.cargo_features, requested.as_deref());
            let missing = wanted.difference(&enabled).collect::<Vec<_>>();
            if !missing.is_empty() {
                let requested = match requested {
                    Some(list) => format!("{list:?}"),
                    None => "none, so only `default`".to_owned(),
                };
                found.push(format!(
                    "{}: Buck files enable its cargo feature(s) {missing:?}, left unresolved \
                     on {platform} (Reindeer features: {requested})",
                    member.name
                ));
            }
        }
    }
    found
}

/// The pass line: what was read, and what that obliged Reindeer to resolve.
fn summary(
    files: usize,
    lists: usize,
    tables: usize,
    forwards: usize,
    named: &BTreeSet<String>,
    platforms: &Platforms,
    members: &[Member],
) -> String {
    let checked = members
        .iter()
        .filter_map(|member| {
            let wanted = wanted(member, named);
            (!wanted.is_empty()).then(|| format!("{} {wanted:?}", member.name))
        })
        .collect::<Vec<_>>();
    let featureless = members
        .iter()
        .filter(|member| member.cargo_features.is_empty())
        .count();
    let scope = if platforms.contains_key(BUILT_IN_PLATFORMS) {
        BUILT_IN_PLATFORMS.to_owned()
    } else {
        format!("all {} platforms in {REINDEER_TOML}", platforms.len())
    };
    let outcome = if checked.is_empty() {
        "none of them is a workspace member's cargo feature".to_owned()
    } else {
        format!(
            "Reindeer resolves the cargo features among them, {}, on {scope}",
            checked.join(" and ")
        )
    };
    let loops = if forwards == 1 {
        "loop forwards"
    } else {
        "loops forward"
    };
    format!(
        "check-buck-reindeer-features.rs: {lists} literal `features` lists in {files} tracked \
         build files name {named:?}; {outcome}; {tables} autocargo `features` tables declare \
         cargo features and enable none; {forwards} {loops} a `features` argument unchanged; \
         {featureless} of {} workspace members have no cargo features",
        members.len()
    )
}

/// The program's standard output. `remedy` follows what the program reports
/// when it runs and fails; a program that cannot be started gets only the OS
/// error, which no remedy for the program's own report fits.
fn command_stdout(
    root: &Path,
    program: &str,
    args: &[&str],
    remedy: Option<&str>,
) -> Result<Vec<u8>, String> {
    let output = Command::new(program)
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|error| format!("failed to run {program}: {error}"))?;
    if !output.status.success() {
        let remedy = remedy
            .map(|remedy| format!("; {remedy}"))
            .unwrap_or_default();
        return Err(format!(
            "{program} {} failed with {}: {}{remedy}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn repository_root(cwd: &Path) -> Result<PathBuf, String> {
    let stdout = command_stdout(
        cwd,
        "git",
        &["rev-parse", "--show-toplevel"],
        Some(CHECKOUT_REMEDY),
    )?;
    let root = String::from_utf8(stdout)
        .map_err(|error| format!("repository root is not UTF-8: {error}"))?;
    Ok(PathBuf::from(root.trim_end()))
}

fn workspace_members(root: &Path) -> Result<Vec<Member>, String> {
    let args = [
        "metadata",
        "--locked",
        "--offline",
        "--format-version",
        "1",
        "--no-deps",
    ];
    let stdout = command_stdout(
        root,
        "cargo",
        &args,
        Some(
            "this check reads the workspace members' cargo features from it, so fix what Cargo \
             reports",
        ),
    )?;
    let metadata: serde_json::Value = serde_json::from_slice(&stdout)
        .map_err(|error| format!("cargo metadata output is not JSON: {error}"))?;
    metadata["packages"]
        .as_array()
        .ok_or("cargo metadata has no packages list")?
        .iter()
        .map(|package| {
            let name = package["name"]
                .as_str()
                .ok_or("cargo metadata package has no name")?;
            let cargo_features =
                serde_json::from_value::<CargoFeatures>(package["features"].clone())
                    .map_err(|error| format!("{name}: unreadable cargo features: {error}"))?;
            Ok(Member {
                name: name.to_owned(),
                cargo_features,
            })
        })
        .collect()
}

/// Each tracked Starlark build file's path and text.
fn build_files(root: &Path) -> Result<Vec<(String, String)>, String> {
    let mut args = vec!["ls-files", "-z", "--"];
    args.extend(BUILD_FILES);
    command_stdout(root, "git", &args, None)?
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path = String::from_utf8(path.to_vec())
                .map_err(|error| format!("tracked build file path is not UTF-8: {error}"))?;
            let text = fs::read_to_string(root.join(&path)).map_err(|error| {
                format!(
                    "failed to read the tracked build file {path}: {error}; this check reads \
                     every one, so restore it or stop tracking it"
                )
            })?;
            Ok((path, text))
        })
        .collect()
}

/// Each workspace member's fixups, where Reindeer reads them, with the path
/// they were read from.
fn member_fixups(
    root: &Path,
    dir: &Path,
    members: &[Member],
) -> Result<Vec<(String, toml::Table)>, String> {
    let mut found = Vec::new();
    for member in members {
        let path = dir.join(&member.name).join("fixups.toml");
        let path = path.display().to_string();
        let text = match fs::read_to_string(root.join(&path)) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "failed to read {path}: {error}; make it a readable file or remove it"
                ));
            }
        };
        let fixups = text
            .parse::<toml::Table>()
            .map_err(|error| format!("{path} is not TOML; {SYNTAX_REMEDY}: {error}"))?;
        found.push((path, fixups));
    }
    Ok(found)
}

/// A refusal for each field Reindeer's resolution reads that one member's
/// fixups set, at the top level or in a `['cfg(...)']` platform table.
fn fixups_refusals(path: &str, fixups: &toml::Table) -> Vec<String> {
    let platform_tables = fixups.iter().filter_map(|(key, value)| {
        let table = value.as_table().filter(|_| key.starts_with("cfg("))?;
        Some((format!("in ['{key}']"), table))
    });
    std::iter::once(("at the top level".to_owned(), fixups))
        .chain(platform_tables)
        .flat_map(|(place, table)| {
            RESOLVER_FIXUPS
                .iter()
                .filter(|(field, _)| table.contains_key(*field))
                .map(move |(field, effect)| {
                    format!("{path}: sets `{field}` {place}, {effect}; {FIXUPS_REMEDY}")
                })
        })
        .collect()
}

/// What the check reads from the repository.
struct Inputs {
    platforms: Platforms,
    members: Vec<Member>,
    /// Each tracked Starlark build file's path and text.
    files: Vec<(String, String)>,
    /// Each workspace member's fixups and the path they were read from.
    fixups: Vec<(String, toml::Table)>,
}

fn load(root: &Path) -> Result<Inputs, String> {
    let path = root.join(REINDEER_TOML);
    let text = fs::read_to_string(&path).map_err(|error| {
        format!(
            "failed to read {}: {error}; {CHECKOUT_REMEDY}",
            path.display()
        )
    })?;
    let config = text
        .parse::<toml::Table>()
        .map_err(|error| format!("{REINDEER_TOML} is not TOML; {SYNTAX_REMEDY}: {error}"))?;
    let files = build_files(root)?;
    if files.is_empty() {
        return Err(format!(
            "git ls-files found no tracked build file ({BUILD_FILES:?}); {CHECKOUT_REMEDY}"
        ));
    }
    let members = workspace_members(root)?;
    Ok(Inputs {
        platforms: platform_features(&config)?,
        fixups: member_fixups(root, &fixups_dir(&config)?, &members)?,
        members,
        files,
    })
}

/// The verdict on what was read: the pass line, or each refusal and each
/// unresolved feature, every one of which fails the check.
fn check(inputs: &Inputs) -> Result<String, Vec<String>> {
    let mut named = BTreeSet::new();
    let mut lists = 0;
    let mut tables = 0;
    let mut forwards = 0;
    let mut refused = Vec::new();
    for (path, text) in &inputs.files {
        let scan = scan(text);
        named.extend(scan.features);
        lists += scan.lists;
        tables += scan.tables;
        forwards += scan.forwards;
        refused.extend(
            scan.refusals
                .into_iter()
                .map(|(line, why)| format!("{path}:{line}: {why}")),
        );
    }
    refused.extend(
        inputs
            .fixups
            .iter()
            .flat_map(|(path, fixups)| fixups_refusals(path, fixups)),
    );
    let found = violations(&inputs.platforms, &inputs.members, &named);
    if refused.is_empty() && found.is_empty() {
        return Ok(summary(
            inputs.files.len(),
            lists,
            tables,
            forwards,
            &named,
            &inputs.platforms,
            &inputs.members,
        ));
    }
    let rule = (!found.is_empty()).then(|| RESOLUTION_RULE.to_owned());
    Err(refused.into_iter().chain(found).chain(rule).collect())
}

/// What a run prints and the status it exits with.
struct Outcome {
    status: u8,
    stdout: Vec<String>,
    stderr: Vec<String>,
}

impl Outcome {
    fn new(status: u8, stdout: Vec<String>, stderr: Vec<String>) -> Self {
        Self {
            status,
            stdout,
            stderr,
        }
    }
}

/// The run with these arguments in the checkout containing `cwd`.
fn run(args: &[OsString], cwd: &Path) -> Outcome {
    match args {
        [] => {}
        [arg] if arg == "--help" || arg == "-h" => {
            return Outcome::new(0, vec![USAGE.to_owned(), HELP.to_owned()], vec![]);
        }
        _ => {
            return Outcome::new(
                2,
                vec![],
                vec![
                    USAGE.to_owned(),
                    format!("{NAME}: takes no argument; run it with none, or with --help"),
                ],
            );
        }
    }
    let inputs = match repository_root(cwd).and_then(|root| load(&root)) {
        Ok(inputs) => inputs,
        Err(error) => return Outcome::new(2, vec![], vec![format!("{NAME}: {error}")]),
    };
    match check(&inputs) {
        Ok(line) => Outcome::new(0, vec![line], vec![]),
        Err(lines) => Outcome::new(
            1,
            vec![],
            lines.iter().map(|line| format!("{NAME}: {line}")).collect(),
        ),
    }
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let outcome = run(&args, Path::new("."));
    for line in &outcome.stdout {
        println!("{line}");
    }
    for line in &outcome.stderr {
        eprintln!("{line}");
    }
    ExitCode::from(outcome.status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn names(values: &[&str]) -> BTreeSet<String> {
        strings(values).into_iter().collect()
    }

    /// The lines `scan` refuses, after checking that every refusal says what
    /// the check does read or that it could not read the file at all.
    fn refused_lines(text: &str) -> Vec<usize> {
        let scan = scan(text);
        for (_, why) in &scan.refusals {
            assert!(
                why.contains(READABLE_VALUE)
                    || why.contains(UNREADABLE_FILE)
                    || why.contains(UNREADABLE_F_STRING),
                "{why}"
            );
        }
        scan.refusals.iter().map(|(line, _)| *line).collect()
    }

    fn member(name: &str, cargo_features: &[(&str, &[&str])]) -> Member {
        Member {
            name: name.to_owned(),
            cargo_features: cargo_features
                .iter()
                .map(|(feature, values)| ((*feature).to_owned(), strings(values)))
                .collect(),
        }
    }

    fn hermit() -> Member {
        member(
            "hermit",
            &[
                ("default", &[]),
                ("liteinst", &["dep:reverie-liteinst"]),
                ("dbt", &["dep:reverie-dbt"]),
                ("third-party-backends", &["dbt", "liteinst"]),
            ],
        )
    }

    fn platforms(entries: &[(&str, Option<&[&str]>)]) -> Platforms {
        entries
            .iter()
            .map(|(name, features)| ((*name).to_owned(), features.map(strings)))
            .collect()
    }

    const BACKENDS: &[&str] = &["default", "third-party-backends"];

    /// A Git repository in a fresh temporary directory, with one workspace
    /// member, `member`, whose cargo features are `default` and `liteinst`, a
    /// reindeer.toml whose one platform resolves `features`, the `tracked`
    /// files added to the index and the `untracked` files only written.
    fn repository(
        name: &str,
        features: &[&str],
        tracked: &[(&str, &str)],
        untracked: &[(&str, &str)],
    ) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "check-buck-reindeer-features-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let manifest = "[package]\nname = \"member\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
                        [features]\ndefault = []\nliteinst = []\n\n[workspace]\n";
        let platform = format!("[platform.linux-x86_64]\nfeatures = {features:?}\n");
        let setup = [
            ("Cargo.toml", manifest),
            ("src/lib.rs", ""),
            (REINDEER_TOML, platform.as_str()),
        ];
        for (path, text) in setup.iter().chain(tracked).chain(untracked) {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
        }
        command_stdout(&root, "git", &["init", "-q"], None).unwrap();
        let mut add = vec!["add", "--"];
        add.extend(tracked.iter().map(|(path, _)| *path));
        command_stdout(&root, "git", &add, None).unwrap();
        root
    }

    /// Requires a run in `root` to exit with `status`, printing `lines` on
    /// standard error and nothing on standard output.
    fn assert_run_fails(root: &Path, status: u8, lines: &[String]) {
        let outcome = run(&[], root);
        let stderr = lines
            .iter()
            .map(|line| format!("{NAME}: {line}"))
            .collect::<Vec<_>>();
        assert_eq!(
            (outcome.status, outcome.stdout, outcome.stderr),
            (status, vec![], stderr)
        );
    }

    #[test]
    fn reads_keyword_assignment_and_dict_entry_lists() {
        let text = "rust_library(\n    features = [\"liteinst\"],\n)\n\
                    rust_library(\n    features = [\n        'buck-release-provenance',\n        \
                    \"dbt\",\n    ],\n    rustc_features = [\"ignored\"],\n)\n\
                    ARGS = {\"features\": [\"sabre\"]}\n";
        let scan = scan(text);
        assert!(scan.refusals.is_empty(), "{:?}", scan.refusals);
        assert_eq!(scan.lists, 3);
        assert_eq!(
            scan.features,
            names(&["buck-release-provenance", "dbt", "liteinst", "sabre"])
        );
    }

    /// A target in the shape hermit-cli/BUCK gives libhermit: an autocargo
    /// `cargo_toml_config` whose `features` table is `table`, beside a
    /// `features` list that enables liteinst on the target.
    fn autocargo_target(table: &str) -> String {
        format!(
            "rust_library(\n    name = \"libhermit\",\n    autocargo = {{\n        \
             \"cargo_toml_dir\": \".\",\n        \"cargo_toml_config\": {{\n            \
             \"package\": {{\"version\": \"0.4.1\", \"keywords\": [\"linux\", \"hermit\"]}},\n            \
             \"dependencies_override\": {{\n                \"dependencies\": \
             {{\"hermit-test-workdir\": {{\"optional\": True}}}},\n            }},\n            \
             \"features\": {table},\n        }},\n    }},\n    features = [\"liteinst\"],\n)\n"
        )
    }

    // The table declares the cargo features of the Cargo.toml that autocargo
    // writes; only the target's own list enables one.
    #[test]
    fn reads_an_autocargo_feature_table_as_declaring_and_enabling_nothing() {
        let table = "{\n                \"default\": [],\n                \
                     \"dbt\": [\"dep:detcore-dbt\", \"dep:reverie-dbt\"],\n                \
                     \"sabre\": [\"dep:reverie-rpc-transport\"],\n                \
                     \"third-party-backends\": [\"dbt\", \"sabre\", \"liteinst\"],\n                \
                     \"kvm-native-test-support\": [\"reverie-kvm/native-test-support\"],\n            }";
        for table in [
            table,
            "{}",
            "{\"dbt\": []}",
            "{\"dbt\": [\"dep:reverie-dbt\",],}",
        ] {
            let read = scan(&autocargo_target(table));
            assert!(read.refusals.is_empty(), "{table}: {:?}", read.refusals);
            assert_eq!((read.lists, read.tables), (1, 1), "{table}");
            assert_eq!(read.features, names(&["liteinst"]), "{table}");
        }
    }

    #[test]
    fn refuses_a_feature_table_anywhere_but_a_cargo_toml_config_entry() {
        for text in [
            "ARGS = {\"features\": {\"dbt\": []}}\n",
            "rust_library(autocargo = {\"features\": {\"dbt\": []}})\n",
            "C = {\"cargo_toml_config\": {\"package\": {\"features\": {\"dbt\": []}}}}\n",
            "rust_library(features = {\"dbt\": []})\n",
            "C = {\"cargo_toml_config\": [{\"features\": {\"dbt\": []}}]}\n",
        ] {
            assert_eq!(refused_lines(text), [1], "{text}");
            let scan = scan(text);
            assert_eq!((scan.tables, scan.features.len()), (0, 0), "{text}");
        }
    }

    #[test]
    fn refuses_a_feature_table_that_is_not_literal() {
        for table in [
            "DBT_TABLE",
            "{\"dbt\": DBT}",
            "{NAME: []}",
            "{name: [] for name in NAMES}",
            "{\"dbt\": select({\"DEFAULT\": []})}",
            "{\"dbt\": [\"a\"] + [\"b\"]}",
            "{\"dbt\": {}}",
            "{\"d\\x62t\": []}",
            "{\"dbt\": [\"dep:reverie\\x2ddbt\"]}",
            "{\"dbt\": [], \"dbt\": [\"dep:reverie-dbt\"]}",
            "{\"dbt\" []}",
            "{\"dbt\": [] \"sabre\": []}",
            "{\"dbt\": [])",
            "{\"dbt\": []} | EXTRA",
            "{\"dbt\": []} if FULL else {}",
        ] {
            let text = autocargo_target(table);
            assert_eq!(
                refused_lines(&text),
                [10],
                "{table}: {:?}",
                scan(&text).refusals
            );
            assert_eq!(scan(&text).tables, 0, "{table}");
        }
    }

    #[test]
    fn a_comment_neither_ends_a_list_nor_adds_one() {
        let text = "rust_library(\n    features = [\n        \"dbt\",  # not ] the end\n        \
                    \"liteinst\",\n    ],\n    # features = [\"sabre\"],\n)\n";
        let scan = scan(text);
        assert!(scan.refusals.is_empty(), "{:?}", scan.refusals);
        assert_eq!(scan.features, names(&["dbt", "liteinst"]));
    }

    #[test]
    fn a_string_is_not_read_as_code() {
        let text = "def hermit_test(name):\n    \"\"\"Not features = FEATS.\n    # not a comment\n    \
                    \"\"\"\n    summary = \"one escaped \\\" quote\"\n    \
                    rust_test(name = name, labels = [\"#1\"], features = [\"dbt\"])\n";
        let scan = scan(text);
        assert!(scan.refusals.is_empty(), "{:?}", scan.refusals);
        assert_eq!(scan.features, names(&["dbt"]));
    }

    #[test]
    fn a_read_or_comparison_of_a_features_variable_sets_nothing() {
        let text = "def hermit_test(name, features = []):\n    if features == []:\n        \
                    pass\n    other(features)\n";
        let scan = scan(text);
        assert!(scan.refusals.is_empty(), "{:?}", scan.refusals);
        assert_eq!((scan.lists, scan.features.len()), (1, 0));
    }

    #[test]
    fn refuses_a_variable() {
        let text = "LIBHERMIT_FEATURES = [\"liteinst\"]\nrust_library(\n    features = \
                    LIBHERMIT_FEATURES,\n)\n";
        assert_eq!(refused_lines(text), [3]);
    }

    #[test]
    fn refuses_select() {
        let text = "rust_library(\n    features = select({\n        \"DEFAULT\": [\"liteinst\"],\n    \
                    }),\n)\n";
        assert_eq!(refused_lines(text), [2]);
    }

    #[test]
    fn refuses_a_concatenation() {
        let text = "rust_library(features = [\"dbt\"] + [\"liteinst\"])\n\
                    rust_library(features = BASE + [\"sabre\"])\n";
        assert_eq!(refused_lines(text), [1, 2]);
        assert!(scan(text).features.is_empty());
    }

    #[test]
    fn refuses_a_conditional_or_an_index() {
        assert_eq!(
            refused_lines("rust_library(features = [\"dbt\"] if FULL else [])\n"),
            [1]
        );
        assert_eq!(
            refused_lines("rust_library(features = [[\"dbt\"]][0])\n"),
            [1]
        );
    }

    #[test]
    fn refuses_a_list_item_that_is_not_a_plain_string() {
        for text in [
            "rust_library(features = [\"dbt\", NAME])\n",
            "rust_library(features = [\"lite\\x69nst\"])\n",
            "rust_library(features = [r\"liteinst\"])\n",
            "rust_library(features = [\"dbt\" \"liteinst\"])\n",
        ] {
            assert_eq!(refused_lines(text), [1], "{text}");
        }
    }

    #[test]
    fn refuses_the_string_features_outside_a_literal_dict_entry() {
        assert_eq!(
            refused_lines("def m(**kwargs):\n    kwargs[\"features\"] = [\"liteinst\"]\n"),
            [2]
        );
        assert_eq!(
            refused_lines("def m(**kwargs):\n    f = kwargs.get('features', [])\n"),
            [2]
        );
        assert_eq!(refused_lines("ARGS = {\"features\": FEATS}\n"), [1]);
    }

    /// The shim's routed-unittest forward (shim/shims.bzl), as it is written.
    // As the shim writes it: the parameter list's closing `):` at the start
    // of a line, and later functions with loops over the same name.
    const SHIM_FORWARD: &str = "def _inline_unittest(\n    name, kwargs, deps, mapped_srcs = None\n):\n    test_kwargs = {\n        \"visibility\": [\"PUBLIC\"],\n    }\n    for attr in (\"crate_root\", \"features\", \"resources\", \"target_compatible_with\"):\n        if kwargs.get(attr) != None:\n            test_kwargs[attr] = kwargs[attr]\n    if mapped_srcs:\n        test_kwargs[\"mapped_srcs\"] = mapped_srcs\n    prelude.rust_test(name = name, **test_kwargs)\n\ndef rust_library(name, **kwargs):\n    for attr in (\"compatible_with\",):\n        kwargs[attr] = kwargs.get(attr)\n";

    #[test]
    fn reads_a_loop_that_forwards_keyword_arguments_unchanged() {
        let read = scan(SHIM_FORWARD);
        assert_eq!(read.refusals, Vec::new());
        assert_eq!((read.forwards, read.lists, read.features.len()), (1, 0, 0));
        // Unguarded, and as a list, it is the same forward.
        let unguarded = "def m(**kwargs):\n    out = {}\n    for a in [\"features\"]:\n        out[a] = kwargs[a]\n    return out\n";
        assert_eq!(refused_lines(unguarded), Vec::<usize>::new());
        assert_eq!(scan(unguarded).forwards, 1);
    }

    #[test]
    fn refuses_a_loop_that_does_more_than_forward() {
        let cases = [
            // The copy is built on.
            "def m(**kwargs):\n    out = {}\n    for a in (\"features\",):\n        out[a] = kwargs[a] + [\"x\"]\n",
            // The body goes on past the copy.
            "def m(**kwargs):\n    out = {}\n    for a in (\"features\",):\n        out[a] = kwargs[a]\n        out[a].append(\"x\")\n",
            // The guard tests some other name.
            "def m(other, **kwargs):\n    out = {}\n    for a in (\"features\",):\n        if kwargs.get(other) != None:\n            out[a] = kwargs[a]\n",
            // The guard's body has an else branch.
            "def m(**kwargs):\n    out = {}\n    for a in (\"features\",):\n        if kwargs.get(a) != None:\n            out[a] = kwargs[a]\n        else:\n            out[a] = [\"x\"]\n",
            // The loop name, still \"features\" after the loop, is read again.
            "def m(**kwargs):\n    out = {}\n    for a in (\"crate_root\", \"features\"):\n        out[a] = kwargs[a]\n    out[a] = [\"x\"]\n",
            // A tuple holding something other than plain strings.
            "def m(**kwargs):\n    out = {}\n    for a in (\"features\", NAME):\n        out[a] = kwargs[a]\n",
            // A tuple not iterated by a loop.
            "ATTRS = (\"crate_root\", \"features\")\n",
            // A copy from somewhere other than kwargs.
            "def m(other, **kwargs):\n    out = {}\n    for a in (\"features\",):\n        out[a] = other[a]\n",
        ];
        for text in cases {
            let read = scan(text);
            assert_eq!(read.forwards, 0, "{text}");
            assert_eq!(read.refusals.len(), 1, "{text}");
        }
    }

    // NAME may occur nowhere else in the function: after the loop it still
    // names the last string, code before the loop can run after it, and a
    // later loop, comprehension or nested scope binding NAME does not end that
    // (a comprehension is its own scope; an empty or untaken loop binds
    // nothing). A loop outside a function is refused too: NAME is a global.
    #[test]
    fn refuses_a_forward_whose_name_occurs_elsewhere_or_outside_a_function() {
        let head = "def m(name, **kwargs):\n    out = {}\n    for a in (\"features\",):\n        out[a] = kwargs[a]\n";
        let cases = [
            // A later loop rebinding NAME.
            (format!("{head}    for a in (\"labels\",):\n        out[a] = kwargs.get(a)\n"), 3),
            // E1: a later comprehension binding NAME.
            (format!("{head}    [None for a in ()]\n    out[a] = out[a] + [\"liteinst\"]\n"), 3),
            // E2: an empty later loop.
            (format!("{head}    for a in ():\n        pass\n    out[a] = [\"liteinst\"]\n"), 3),
            // E3: the rebinding loop under an untaken branch.
            (format!("{head}    if False:\n        for a in (\"labels\",):\n            pass\n    out[a] = [\"liteinst\"]\n"), 3),
            // E4: the rebinding loop's iterable reads NAME.
            (format!("{head}    for a in [out.update({{a: [\"liteinst\"]}})]:\n        pass\n"), 3),
            // A nested def after the loop, then a read of NAME.
            (format!("{head}    def g():\n        pass\n    out[a] = [\"liteinst\"]\n"), 3),
            // A lambda reading NAME.
            (format!("{head}    f = lambda: out.update({{a: [\"liteinst\"]}})\n"), 3),
            // E5: an enclosing loop runs code written before the forward after it.
            ("def m(**kwargs):\n    out = {}\n    for i in range(2):\n        if i == 1:\n            kwargs[a] = [\"liteinst\"]\n        for a in (\"features\",):\n            if kwargs.get(a) != None:\n                out[a] = kwargs[a]\n".to_string(), 6),
            // E6: a closure written before the loop, called after it.
            ("def m(**kwargs):\n    out = {}\n    def put():\n        out[a] = [\"liteinst\"]\n    for a in (\"features\",):\n        out[a] = kwargs[a]\n    put()\n".to_string(), 5),
            // E7: a module-level forward; NAME is a global.
            ("kwargs = {}\nOUT = {}\nfor a in (\"features\",):\n    if kwargs.get(a) != None:\n        OUT[a] = kwargs[a]\n\ndef m(name):\n    d = {}\n    d[a] = [\"liteinst\"]\n    native.rust_library(name = name, **d)\n".to_string(), 3),
            // N6: a module-level loop right after a function.
            ("def f():\n    pass\nfor a in (\"features\",):\n    if kwargs.get(a) != None:\n        OUT[a] = kwargs[a]\n".to_string(), 3),
            // A forward under a module-level block after a function.
            ("def f():\n    pass\nif True:\n    for a in (\"features\",):\n        OUT[a] = kwargs[a]\n".to_string(), 4),
            // The body goes on, deeper, without reading NAME.
            (format!("{head}        x = 1\n"), 3),
            // A comprehension over the tuple, then a copy: no `:` after it.
            ("def m(**kwargs):\n    out = {}\n    [None for a in (\"features\",)]\n    out[a] = kwargs[a]\n".to_string(), 3),
        ];
        for (text, line) in &cases {
            let read = scan(text);
            assert_eq!(read.forwards, 0, "{text}");
            assert_eq!(refused_lines(text), vec![*line], "{text}");
        }
    }

    // Open-source Buck2 evaluates an f-string's replacement field, which the
    // check cannot read: it can name a forward's NAME where the scan does not
    // see it (N1 after the loop, N1b in a closure before it), or call a rule
    // with a `features` list (N3, with no loop at all).
    #[test]
    fn refuses_an_f_string_with_a_replacement_field() {
        let head = "def m(name, **kwargs):\n    out = {}\n    for a in (\"features\",):\n        out[a] = kwargs[a]\n";
        let cases = [
            // N1: NAME read in an f-string after the loop.
            (format!("{head}    out[f\"{{a}}\"] = [\"liteinst\"]\n"), 5),
            // N1b: the same in a closure written before the loop.
            ("def m(**kwargs):\n    out = {}\n    def put():\n        out[f\"{a}\"] = [\"liteinst\"]\n    for a in (\"features\",):\n        out[a] = kwargs[a]\n    put()\n".to_string(), 4),
            // N3: a rule called inside an f-string.
            ("def m(name):\n    _ = f\"{native.rust_library(name = name, features = ['liteinst'])}\"\n".to_string(), 2),
            // Each prefix, in any case, raw or not, with either quote, triple-quoted.
            ("X = F'{a}'\n".to_string(), 1),
            ("X = fr\"{a}\"\n".to_string(), 1),
            ("X = Rf\"{a}\"\n".to_string(), 1),
            ("X = fR'''\n{a}'''\n".to_string(), 1),
            // A field after a doubled brace.
            ("X = f\"{{{a}\"\n".to_string(), 1),
            // A field after an escaped brace, which pairs with no other:
            // NAME read through a forward, raw or not, after an escaped
            // backslash; a rule called; a field holding a nested quote.
            (head.to_string() + "    out[f\"\\{{a}\"[2:]] = [\"liteinst\"]\n", 5),
            (head.to_string() + "    out[fr\"\\{{a}\"[2:]] = [\"liteinst\"]\n", 5),
            (head.to_string() + "    out[f\"\\\\\\{{a}\"[3:]] = [\"liteinst\"]\n", 5),
            ("def m(name):\n    _ = f\"\\{{native.rust_library(name = name, features = ['liteinst'])}\"\n".to_string(), 2),
            ("X = f\"\\{{d[\"k\"]}\"\n".to_string(), 1),
        ];
        for (text, line) in &cases {
            let read = scan(text);
            assert_eq!(refused_lines(text), vec![*line], "{text}");
            assert!(
                read.refusals
                    .iter()
                    .all(|(_, why)| why.contains(UNREADABLE_F_STRING)),
                "{text}: {:?}",
                read.refusals
            );
        }
        // Not f-strings with a field: only doubled braces (after an escaped
        // backslash too), only an escaped brace, no braces, a function named
        // `f` called with a string, the name `f` before a string it is not
        // part of, a plain string holding braces.
        for text in [
            "X = f\"{{a}}\"\n",
            "X = f\"\\\\{{a}}\"\n",
            "X = f\"\\{\"\n",
            "X = f\"plain\"\n",
            "X = f(\"{a}\")\n",
            "X = [f, \"{a}\"]\n",
            "X = f\n\"{a}\"\n",
            "X = \"{a}\".format(a = 1)\n",
        ] {
            assert_eq!(refused_lines(text), Vec::<usize>::new(), "{text}");
        }
    }

    #[test]
    fn refuses_a_cfg_feature_set_through_rustc_flags() {
        assert_eq!(
            refused_lines("rust_library(rustc_flags = [\"--cfg\", 'feature=\"liteinst\"'])\n"),
            [1]
        );
        assert_eq!(
            refused_lines("rust_library(rustc_flags = [\"--cfg=feature=\\\"dbt\\\"\"])\n"),
            [1]
        );
        assert_eq!(
            refused_lines(
                "rust_library(rustc_flags = [\"--cfg=sanitized_feature=1\", \"feature == on\"])\n"
            ),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn refuses_an_unterminated_string() {
        assert_eq!(
            refused_lines("rust_library(\n    features = [\"liteinst],\n)\n"),
            [2]
        );
        // A single-quoted string ends at its line, so the refusal names the
        // line it starts on rather than wherever the next quote happens to be.
        assert_eq!(
            refused_lines("rust_library(\n    name = \"a,\n    features = [\"dbt\"],\n)\n"),
            [2]
        );
    }

    #[test]
    fn a_feature_enabled_through_another_one_is_resolved() {
        let found = violations(
            &platforms(&[("[platform.linux-x86_64]", Some(BACKENDS))]),
            &[hermit()],
            &names(&["liteinst", "dbt"]),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    // The regression in https://github.com/rrnewton/hermit/issues/3567: a
    // platform that names no features resolves `default` only.
    #[test]
    fn default_only_resolution_refuses_an_optional_backend() {
        let found = violations(
            &platforms(&[("[platform.linux-x86_64]", None)]),
            &[hermit()],
            &names(&["liteinst"]),
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("\"liteinst\""), "{found:?}");
    }

    #[test]
    fn one_platform_without_the_feature_refuses() {
        let found = violations(
            &platforms(&[
                ("[platform.linux-x86_64]", Some(BACKENDS)),
                ("[platform.linux-arm64]", Some(&["default"])),
            ]),
            &[hermit()],
            &names(&["liteinst"]),
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("linux-arm64"), "{found:?}");
    }

    #[test]
    fn a_name_that_is_no_cargo_feature_is_ignored() {
        let found = violations(
            &platforms(&[("[platform.linux-x86_64]", None)]),
            &[hermit()],
            &names(&["buck-release-provenance"]),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_name_counts_against_every_member_with_that_cargo_feature() {
        let other = member("other", &[("default", &[]), ("dbt", &["dep:x"])]);
        let found = violations(
            &platforms(&[("[platform.linux-x86_64]", Some(BACKENDS))]),
            &[hermit(), other],
            &names(&["dbt"]),
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("other:"), "{found:?}");
    }

    #[test]
    fn no_platform_table_resolves_default_only() {
        let built_in = platform_features(&toml::Table::new()).unwrap();
        assert_eq!(built_in, platforms(&[(BUILT_IN_PLATFORMS, None)]));
        let found = violations(&built_in, &[hermit()], &names(&["liteinst"]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains(BUILT_IN_PLATFORMS), "{found:?}");
    }

    #[test]
    fn a_platform_that_names_no_features_still_resolves_default() {
        let member = member(
            "member",
            &[("default", &["liteinst"]), ("liteinst", &["dep:x"])],
        );
        let found = violations(
            &platforms(&[(BUILT_IN_PLATFORMS, None)]),
            &[member],
            &names(&["liteinst"]),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    // With no platform there would be nothing to check, and so nothing to
    // refuse.
    #[test]
    fn an_empty_platform_table_is_an_error() {
        let config = "[platform]\n".parse::<toml::Table>().unwrap();
        assert!(
            platform_features(&config)
                .unwrap_err()
                .contains("names no platform")
        );
    }

    #[test]
    fn each_shape_error_names_the_form_reindeer_expects() {
        for (text, condition) in [
            ("platform = 1\n", "[platform] is not a table"),
            ("[platform]\n", "[platform] names no platform"),
            ("[platform]\nlinux = 1\n", "platform.linux is not a table"),
            (
                "[platform.linux]\nfeatures = \"default\"\n",
                "platform.linux.features is not a list of strings",
            ),
            (
                "[platform.linux]\nfeatures = [1]\n",
                "platform.linux.features is not a list of strings",
            ),
        ] {
            let config = text.parse::<toml::Table>().unwrap();
            assert_eq!(
                platform_features(&config),
                Err(format!("{REINDEER_TOML}: {condition}; {PLATFORM_FORM}")),
                "{text}"
            );
        }
        let config = "fixups_dir = [\"fixups\"]\n"
            .parse::<toml::Table>()
            .unwrap();
        let error = fixups_dir(&config).unwrap_err();
        assert!(
            error.starts_with(&format!("{REINDEER_TOML}: fixups_dir is not a string; ")),
            "{error}"
        );
        assert!(
            error.ends_with("such as `fixups_dir = \"fixups\"`"),
            "{error}"
        );
    }

    // Reindeer resolves `fixups_dir` against the directory holding
    // reindeer.toml (src/config.rs at the pinned revision).
    #[test]
    fn fixups_dir_is_resolved_against_the_third_party_dir() {
        let read = |text: &str| fixups_dir(&text.parse::<toml::Table>().unwrap()).unwrap();
        assert_eq!(read(""), Path::new("shim/third-party/rust/fixups"));
        assert_eq!(
            read("fixups_dir = \"elsewhere\"\n"),
            Path::new("shim/third-party/rust/elsewhere")
        );
        assert_eq!(read("fixups_dir = \"/abs\"\n"), Path::new("/abs"));
    }

    #[test]
    fn a_platform_table_replaces_the_built_in_platforms() {
        let config = "[platform.linux-x86_64]\ntarget = \"x\"\nfeatures = [\"default\", \"dbt\"]\n\
                      [platform.windows-gnu]\ntarget = \"y\"\n"
            .parse::<toml::Table>()
            .unwrap();
        assert_eq!(
            platform_features(&config).unwrap(),
            platforms(&[
                ("[platform.linux-x86_64]", Some(&["default", "dbt"])),
                ("[platform.windows-gnu]", None),
            ])
        );
    }

    #[test]
    fn reads_every_tracked_buck_package_and_bzl_file_and_nothing_else() {
        let root = repository(
            "files",
            &["default", "liteinst"],
            &[
                ("BUCK", "rust_library(features = [\"liteinst\"])\n"),
                ("a/b/BUCK", "rust_library(features = [\"default\"])\n"),
                ("defs/rules.bzl", "ARGS = {\"features\": [\"dbt\"]}\n"),
                ("sub/PACKAGE", "package(features = [\"buck-only\"])\n"),
                ("README.md", "features = FEATURES\n"),
                (
                    "shim/third-party/rust/fixups/member/fixups.toml",
                    "extra_srcs = [\"src/**\"]\n",
                ),
                (
                    "shim/third-party/rust/fixups/clap/fixups.toml",
                    "omit_features = [\"deprecated\"]\n",
                ),
            ],
            &[("untracked/BUCK", "rust_library(features = FEATURES)\n")],
        );
        let inputs = load(&root).unwrap();
        let paths = inputs
            .files
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["BUCK", "a/b/BUCK", "defs/rules.bzl", "sub/PACKAGE"]);
        let line = check(&inputs).unwrap();
        assert!(
            line.contains("4 literal `features` lists in 4 tracked build files"),
            "{line}"
        );
        let outcome = run(&[], &root);
        assert_eq!(
            (outcome.status, outcome.stdout, outcome.stderr),
            (0, vec![line], vec![])
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn load_refuses_a_repository_with_no_tracked_build_file() {
        let root = repository(
            "none",
            &["default", "liteinst"],
            &[("README.md", "rust_library(features = [\"liteinst\"])\n")],
            &[("BUCK", "rust_library(features = [\"liteinst\"])\n")],
        );
        let error = load(&root).err().unwrap();
        assert_eq!(
            error,
            "git ls-files found no tracked build file ([\":(glob)**/BUCK\", \
             \":(glob)**/PACKAGE\", \":(glob)**/*.bzl\"]); run this inside a Hermit checkout, \
             outside its submodules"
        );
        assert_run_fails(&root, 2, &[error]);
        fs::remove_dir_all(&root).unwrap();
    }

    // A git ls-files that fails, here on a corrupt index that git rev-parse
    // does not read, gets git's report and no remedy.
    #[test]
    fn a_failed_git_ls_files_gets_no_remedy() {
        let root = repository(
            "corrupt-index",
            &["default", "liteinst"],
            &[("BUCK", "rust_library(features = [\"liteinst\"])\n")],
            &[],
        );
        fs::write(root.join(".git/index"), "not an index").unwrap();
        let output = Command::new("git")
            .current_dir(&root)
            .args(["ls-files", "-z", "--"])
            .args(BUILD_FILES)
            .output()
            .unwrap();
        let error = load(&root).err().unwrap();
        assert_eq!(
            error,
            format!(
                "git ls-files -z -- {} failed with {}: {}",
                BUILD_FILES.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )
        );
        assert_run_fails(&root, 2, &[error]);
        fs::remove_dir_all(&root).unwrap();
    }

    // A value the scan refuses fails the check even when every feature it did
    // read is resolved.
    #[test]
    fn a_refusal_alone_fails_the_check() {
        let root = repository(
            "refusal",
            &["default", "liteinst"],
            &[(
                "defs/rules.bzl",
                "LITEINST = [\"liteinst\"]\nrust_library(features = LITEINST)\n",
            )],
            &[],
        );
        let lines = check(&load(&root).unwrap()).unwrap_err();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("defs/rules.bzl:2: "), "{lines:?}");
        assert_run_fails(&root, 1, &lines);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unresolved_feature_fails_the_check_and_states_the_rule() {
        let root = repository(
            "unresolved",
            &["default"],
            &[("BUCK", "rust_library(features = [\"liteinst\"])\n")],
            &[],
        );
        let lines = check(&load(&root).unwrap()).unwrap_err();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("member: "), "{lines:?}");
        assert_eq!(lines[1], RESOLUTION_RULE);
        assert_run_fails(&root, 1, &lines);
        fs::remove_dir_all(&root).unwrap();
    }

    // A table's names are not counted, so the comparison is the one the
    // target's own list makes: with no list it passes, and with one naming
    // liteinst, which this platform leaves unresolved, it still fails.
    #[test]
    fn a_feature_table_leaves_the_comparison_to_the_targets_list() {
        let table = "{\"default\": [], \"liteinst\": []}";
        let declared = autocargo_target(table).replace("    features = [\"liteinst\"],\n", "");
        let root = repository(
            "table-declared",
            &["default"],
            &[("BUCK", declared.as_str())],
            &[],
        );
        let line = check(&load(&root).unwrap()).unwrap();
        assert!(
            line.contains("0 literal `features` lists in 1 tracked build files name {};"),
            "{line}"
        );
        assert!(
            line.contains(
                "; 1 autocargo `features` tables declare cargo features and enable none;"
            ),
            "{line}"
        );
        assert!(
            line.contains("; 0 loops forward a `features` argument unchanged;"),
            "{line}"
        );
        fs::remove_dir_all(&root).unwrap();

        let enabled = autocargo_target(table);
        let root = repository(
            "table-enabled",
            &["default"],
            &[("BUCK", enabled.as_str())],
            &[],
        );
        let lines = check(&load(&root).unwrap()).unwrap_err();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("member: "), "{lines:?}");
        assert!(lines[0].contains("[\"liteinst\"]"), "{lines:?}");
        assert_eq!(lines[1], RESOLUTION_RULE);
        assert_run_fails(&root, 1, &lines);
        fs::remove_dir_all(&root).unwrap();
    }

    /// The fields and places `fixups_refusals` names for fixups `text`, after
    /// checking that each refusal names the file, what the field does and the
    /// remedy.
    fn refused_fixups(text: &str) -> Vec<(String, String)> {
        let fixups = text.parse::<toml::Table>().unwrap();
        fixups_refusals("f.toml", &fixups)
            .iter()
            .map(|line| {
                let (field, effect) = RESOLVER_FIXUPS
                    .iter()
                    .find(|(field, _)| line.starts_with(&format!("f.toml: sets `{field}` ")))
                    .unwrap_or_else(|| panic!("{line}"));
                let place = line
                    .strip_prefix(&format!("f.toml: sets `{field}` "))
                    .and_then(|rest| rest.strip_suffix(&format!(", {effect}; {FIXUPS_REMEDY}")))
                    .unwrap_or_else(|| panic!("{line}"));
                ((*field).to_owned(), place.to_owned())
            })
            .collect()
    }

    // Reindeer's resolution reads `omit_features`, `omit_deps` and `cfgs` from
    // a crate's top-level fixups and from each `cfg(...)` table that applies to
    // the platform (src/index.rs and src/fixups.rs at the pinned revision), so
    // the resolution this check computes would be wrong with any of them.
    #[test]
    fn each_field_the_resolution_reads_is_refused_where_it_applies() {
        let top = |field: &str| vec![(field.to_owned(), "at the top level".to_owned())];
        let linux = "in ['cfg(target_os = \"linux\")']".to_owned();
        for (text, expected) in [
            ("omit_features = [\"liteinst\"]\n", top("omit_features")),
            ("omit_deps = [\"reverie-liteinst\"]\n", top("omit_deps")),
            ("cfgs = [\"liteinst\"]\n", top("cfgs")),
            // A quoted or escaped key is the same key to Reindeer.
            ("\"omit_features\" = []\n", top("omit_features")),
            ("\"omit\\u005Fdeps\" = []\n", top("omit_deps")),
            (
                "['cfg(target_os = \"linux\")']\nomit_features = [\"liteinst\"]\n",
                vec![("omit_features".to_owned(), linux.clone())],
            ),
            (
                "'cfg(target_os = \"linux\")'.cfgs = []\nomit_deps = []\n",
                vec![
                    ("omit_deps".to_owned(), "at the top level".to_owned()),
                    ("cfgs".to_owned(), linux.clone()),
                ],
            ),
        ] {
            assert_eq!(refused_fixups(text), expected, "{text}");
        }
    }

    // `features` applies only when a generated rule compiles the crate, and a
    // build script's `omit_deps` only to its generated rule (src/fixups.rs at
    // the pinned revision); neither changes the resolution. Only a key that
    // starts with `cfg(` names a platform table (src/fixups/config.rs); any
    // other table is a field, such as `env`, whose keys are variable names.
    #[test]
    fn fixups_that_leave_the_resolution_alone_are_accepted() {
        for text in [
            "extra_srcs = [\"src/**\"]\nfeatures = [\"liteinst\"]\n",
            "[buildscript.build]\nomit_deps = [\"cc\"]\n",
            "['cfg(target_os = \"linux\")']\nrustc_flags = [\"-g\"]\n",
            "[env]\nomit_features = \"1\"\n",
        ] {
            assert_eq!(
                refused_fixups(text),
                Vec::<(String, String)>::new(),
                "{text}"
            );
        }
    }

    // A member's fixups refusal fails the run. They are read from the
    // `fixups_dir` reindeer.toml names, or from `fixups` when it names none, as
    // Reindeer reads them, and only from there.
    #[test]
    fn a_members_fixups_are_read_from_fixups_dir_and_fail_the_check() {
        for (name, setting, read, field) in [
            ("fixups-default", "", "fixups", "omit_features"),
            (
                "fixups-dir",
                "fixups_dir = \"elsewhere\"\n",
                "elsewhere",
                "omit_deps",
            ),
        ] {
            let config = format!(
                "{setting}[platform.linux-x86_64]\nfeatures = [\"default\", \"liteinst\"]\n"
            );
            let root = repository(
                name,
                &["default", "liteinst"],
                &[
                    (REINDEER_TOML, config.as_str()),
                    ("BUCK", "rust_library(features = [\"liteinst\"])\n"),
                    (
                        "shim/third-party/rust/elsewhere/member/fixups.toml",
                        "omit_deps = [\"x\"]\n",
                    ),
                    (
                        "shim/third-party/rust/fixups/member/fixups.toml",
                        "omit_features = [\"liteinst\"]\n",
                    ),
                ],
                &[],
            );
            let (_, effect) = RESOLVER_FIXUPS
                .iter()
                .find(|(known, _)| *known == field)
                .unwrap();
            let lines = check(&load(&root).unwrap()).unwrap_err();
            assert_eq!(
                lines,
                [format!(
                    "shim/third-party/rust/{read}/member/fixups.toml: sets `{field}` at the top \
                     level, {effect}; {FIXUPS_REMEDY}"
                )],
                "{name}"
            );
            assert_run_fails(&root, 1, &lines);
            fs::remove_dir_all(&root).unwrap();
        }
    }

    // Each load error exits 2 with the whole message: what failed, what the
    // OS, the TOML parser or Cargo said about it, and what to do.
    #[test]
    fn each_load_error_names_what_failed_and_the_remedy() {
        let fixups = "shim/third-party/rust/fixups/member/fixups.toml";
        let cargo_metadata = [
            "metadata",
            "--locked",
            "--offline",
            "--format-version",
            "1",
            "--no-deps",
        ];
        let cases: [(&str, &[(&str, &str)], Option<&str>, &str); 6] = [
            (
                "no-reindeer-toml",
                &[],
                Some(REINDEER_TOML),
                "failed to read {root}/shim/third-party/rust/reindeer.toml: No such file or \
                 directory (os error 2); run this inside a Hermit checkout, outside its \
                 submodules",
            ),
            (
                "bad-reindeer-toml",
                &[(REINDEER_TOML, "[platform\n")],
                None,
                "shim/third-party/rust/reindeer.toml is not TOML; correct the syntax error, \
                 which stops Reindeer too: {parser}",
            ),
            (
                "bad-fixups",
                &[(fixups, "omit_features = [\n")],
                None,
                "shim/third-party/rust/fixups/member/fixups.toml is not TOML; correct the \
                 syntax error, which stops Reindeer too: {parser}",
            ),
            (
                "fixups-directory",
                &[("shim/third-party/rust/fixups/member/fixups.toml/x", "")],
                None,
                "failed to read shim/third-party/rust/fixups/member/fixups.toml: Is a \
                 directory (os error 21); make it a readable file or remove it",
            ),
            (
                "missing-build-file",
                &[("defs/BUCK", "")],
                Some("defs/BUCK"),
                "failed to read the tracked build file defs/BUCK: No such file or directory \
                 (os error 2); this check reads every one, so restore it or stop tracking it",
            ),
            (
                "bad-manifest",
                &[("Cargo.toml", "[package\n")],
                None,
                "cargo metadata --locked --offline --format-version 1 --no-deps failed with \
                 {cargo}; this check reads the workspace members' cargo features from it, so \
                 fix what Cargo reports",
            ),
        ];
        for (name, files, removed, expected) in cases {
            let mut tracked = vec![("BUCK", "rust_library(features = [\"liteinst\"])\n")];
            tracked.extend(files);
            let root = repository(name, &["default", "liteinst"], &tracked, &[]);
            if let Some(path) = removed {
                fs::remove_file(root.join(path)).unwrap();
            }
            // `{root}` stands for the fixture checkout, `{parser}` for the TOML
            // parser's own message about the case's file, and `{cargo}` for the
            // status and standard error of the same cargo metadata run.
            let mut expected = expected.replace("{root}", &root.display().to_string());
            if expected.contains("{parser}") {
                let parser = files[0].1.parse::<toml::Table>().unwrap_err();
                expected = expected.replace("{parser}", &parser.to_string());
            }
            if expected.contains("{cargo}") {
                let output = Command::new("cargo")
                    .current_dir(&root)
                    .args(cargo_metadata)
                    .output()
                    .unwrap();
                let stderr = String::from_utf8_lossy(&output.stderr);
                let cargo = format!("{}: {}", output.status, stderr.trim());
                expected = expected.replace("{cargo}", &cargo);
            }
            let error = load(&root).err().unwrap();
            assert_eq!(error, expected, "{name}");
            assert_run_fails(&root, 2, &[error]);
            fs::remove_dir_all(&root).unwrap();
        }
    }

    // A remedy follows what a program reports when it runs and fails. A
    // program that cannot be started gets the OS error alone: git or cargo
    // missing is not fixed by running somewhere else or fixing the manifest.
    #[test]
    fn a_program_that_cannot_start_gets_no_remedy() {
        let missing = "check-buck-reindeer-features-no-such-program";
        assert_eq!(
            command_stdout(Path::new("/"), missing, &[], Some(CHECKOUT_REMEDY)),
            Err(format!(
                "failed to run {missing}: No such file or directory (os error 2)"
            ))
        );
        // A program that runs and fails gets the remedy, if any, after its
        // status and standard error.
        let output = Command::new("git")
            .current_dir("/")
            .arg("--bogus")
            .output()
            .unwrap();
        let report = format!(
            "git --bogus failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        assert_eq!(
            command_stdout(Path::new("/"), "git", &["--bogus"], Some(CHECKOUT_REMEDY)),
            Err(format!("{report}; {CHECKOUT_REMEDY}"))
        );
        assert_eq!(
            command_stdout(Path::new("/"), "git", &["--bogus"], None),
            Err(report)
        );
    }

    // Outside any Git repository the run fails before it reads a file, and says
    // where to run it.
    #[test]
    fn a_run_outside_a_checkout_says_where_to_run() {
        let outcome = run(&[], Path::new("/"));
        assert_eq!((outcome.status, outcome.stdout.len()), (2, 0));
        assert_eq!(outcome.stderr.len(), 1, "{:?}", outcome.stderr);
        let line = &outcome.stderr[0];
        assert!(
            line.starts_with(&format!(
                "{NAME}: git rev-parse --show-toplevel failed with "
            )),
            "{line}"
        );
        assert!(line.ends_with(CHECKOUT_REMEDY), "{line}");
    }

    // The arguments are read before the checkout, which a directory outside any
    // Git repository would fail to load with status 2.
    #[test]
    fn help_prints_the_usage_and_any_other_argument_is_a_usage_error() {
        for arg in ["--help", "-h"] {
            let outcome = run(&[OsString::from(arg)], Path::new("/"));
            assert_eq!(
                (outcome.status, outcome.stdout, outcome.stderr),
                (0, strings(&[USAGE, HELP]), vec![])
            );
        }
        for args in [&["extra"][..], &["--help", "extra"], &["-h", "-h"]] {
            let args = args.iter().map(OsString::from).collect::<Vec<_>>();
            let outcome = run(&args, Path::new("/"));
            assert_eq!((outcome.status, outcome.stdout.len()), (2, 0), "{args:?}");
            assert_eq!(outcome.stderr.len(), 2, "{args:?}");
            assert_eq!(outcome.stderr[0], USAGE);
            assert!(outcome.stderr[1].ends_with("or with --help"), "{args:?}");
        }
    }
}
