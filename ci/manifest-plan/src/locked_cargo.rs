/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! No validation step may rewrite a committed `Cargo.lock`.
//!
//! Cargo rewrites a stale lock file in place unless the call carries
//! `--locked` or `--frozen`. The lock freshness gate, `pre.cargo_lock`, reads
//! the lock file in the same checkout the other nodes build in, and no node
//! waits for it, so one unlocked `cargo build` that ran before it would
//! rewrite a stale committed lock file and the gate would then pass. Every
//! Cargo call of a validation node that resolves a workspace therefore carries
//! one of the two flags, and a stale lock file fails the run at its first
//! Cargo call, in a full run and in a focused one alike
//! (<https://github.com/facebookexperimental/hermit/issues/112>).
//!
//! [`cargo_calls`] finds the `cargo` calls in a node's shell command,
//! including calls nested in quoted strings: the pinned-root wrapper's
//! payload, `bash -c` scripts and command substitutions.
//! [`script_cargo_calls`] does the same for a script a node runs. The tests
//! refuse an unlocked call in the committed DAG or in the scripts its nodes
//! run unless [`UNLOCKED_CARGO_EXCEPTIONS`] names its node or script with the
//! reason. [`stale_lock_twin`] runs one call's exact arguments in a fixture
//! workspace whose committed lock file is stale. This module's tests apply it
//! to the first Cargo calls of every committed plan, and scripts/validate.rs's
//! tests apply it to those of its focused plans.
//!
//! The scan reads every node command and a fixed list of scripts that nodes
//! run (`SCRIPTS` in the tests). It discovers no scripts, so a Cargo call in
//! any other program a node runs, whether a script, a Rust tool, a build script
//! or a test, is not seen. Nor is a command that names Cargo through a variable
//! (`"$CARGO" build`); no node command does that.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use dagrun::model::Step;

/// Nodes or scripts whose lock-resolving Cargo call cannot carry `--locked`
/// or `--frozen`, by node tag or script path, each with the reason it cannot.
/// Empty: every lock-resolving Cargo call of the committed DAG and of the
/// scripts the tests scan carries one.
pub const UNLOCKED_CARGO_EXCEPTIONS: &[(&str, &str)] = &[];

/// How one Cargo call treats the lock file of the workspace it runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockUse {
    /// `--locked` or `--frozen` comes before any `--`: Cargo refuses a lock
    /// file that needs updating and leaves it as it is.
    Locked,
    /// The call resolves the workspace without either flag, so Cargo rewrites
    /// a stale lock file.
    Unlocked,
    /// The call resolves no workspace lock file, for the reason given.
    Unresolved(&'static str),
}

/// One `cargo` call in a shell command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CargoCall {
    /// The words after `cargo`, with shell quoting removed and redirections
    /// dropped.
    pub argv: Vec<String>,
    pub lock: LockUse,
}

impl CargoCall {
    /// Whether the call resolves a workspace lock file, with or without the
    /// flags.
    pub fn resolves_lock(&self) -> bool {
        !matches!(self.lock, LockUse::Unresolved(_))
    }

    /// The call as a command line, for messages.
    pub fn render(&self) -> String {
        render(&self.argv)
    }
}

fn render(argv: &[String]) -> String {
    std::iter::once("cargo")
        .chain(argv.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

const PRINTS_ONLY: &str = "prints Cargo's version or help";

/// Measured on 2026-11-15 under strace with cargo-nextest 0.9.100, the version
/// setup.nextest installs, in a workspace whose lock file was stale: the only
/// Cargo call of `cargo nextest show-config version` was `cargo locate-project
/// --workspace --message-format=plain`, and the lock file stayed
/// byte-identical. The tests repeat the measurement. Only that exact call is
/// exempt. No other show-config subcommand was measured, and `show-config
/// test-groups` builds the test list, which resolves the lock file, so every
/// other form counts as resolving it.
const NEXTEST_SHOW_CONFIG_VERSION: &str = "`nextest show-config version` runs only `cargo \
     locate-project --workspace`, which resolves no dependency graph";

/// Every `cargo` call in the shell command `command`.
///
/// A word that is itself a script (it holds `cargo` and white space, such as
/// the pinned-root wrapper's payload or a `bash -c` argument) is scanned as a
/// script, and so is the body of every command or process substitution. The
/// arguments of a command that never runs one, such as `echo`, `printf` or
/// `test`, are not scanned. Each call is found once.
pub fn cargo_calls(command: &str) -> Vec<CargoCall> {
    let mut calls = Vec::new();
    collect(command, Origin::Verbatim(0), &mut calls);
    calls.into_iter().map(|(_, call)| call).collect()
}

/// The lock-resolving Cargo calls without `--locked` or `--frozen`, rendered,
/// by node.
pub fn unlocked_cargo_calls(steps: &[Step]) -> BTreeMap<String, Vec<String>> {
    steps
        .iter()
        .filter_map(|step| {
            let unlocked = cargo_calls(&step.cmd)
                .into_iter()
                .filter(|call| call.lock == LockUse::Unlocked)
                .map(|call| call.render())
                .collect::<Vec<_>>();
            (!unlocked.is_empty()).then(|| (step.tag(), unlocked))
        })
        .collect()
}

/// One Cargo call in a script that a validation node runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptCargoCall {
    /// The script's line the call is on; for a call inside a quoted string,
    /// the line the string starts on.
    pub line: usize,
    pub lock: LockUse,
    /// The call as the scan read it: a shell call's command line, or the
    /// first source line of a Rust `Command::new("cargo")`.
    pub shown: String,
}

/// Every Cargo call in the script at `path` (for its language) with the
/// contents `text`.
///
/// A shell script is scanned as [`cargo_calls`] scans a command. In a Rust
/// script each `Command::new("cargo")` is a call, locked when a `"--locked"`
/// or `"--frozen"` literal comes after it and before the next `Command::new(`
/// or the end of its enclosing block, which the formatted source indents less.
pub fn script_cargo_calls(path: &str, text: &str) -> Vec<ScriptCargoCall> {
    if path.ends_with(".rs") {
        return rust_cargo_calls(text);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut calls = Vec::new();
    collect(text, Origin::Verbatim(0), &mut calls);
    calls
        .into_iter()
        .map(|(offset, call)| ScriptCargoCall {
            line: 1 + chars[..offset.min(chars.len())]
                .iter()
                .filter(|c| **c == '\n')
                .count(),
            lock: call.lock,
            shown: call.render(),
        })
        .collect()
}

fn rust_cargo_calls(text: &str) -> Vec<ScriptCargoCall> {
    const SITE: &str = "Command::new(\"cargo\")";
    let lines = text.lines().collect::<Vec<_>>();
    let indent = |line: &str| line.len() - line.trim_start().len();
    let mut calls = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some((_, after)) = line.split_once(SITE) else {
            continue;
        };
        let mut region = String::from(after);
        for next in &lines[index + 1..] {
            if next.contains("Command::new(")
                || (next.trim_start().starts_with('}') && indent(next) < indent(line))
            {
                break;
            }
            region.push('\n');
            region.push_str(next);
        }
        let locked = region.contains("\"--locked\"") || region.contains("\"--frozen\"");
        calls.push(ScriptCargoCall {
            line: index + 1,
            lock: if locked {
                LockUse::Locked
            } else {
                LockUse::Unlocked
            },
            shown: line.trim().to_string(),
        });
    }
    calls
}

/// The steps of a plan that run a lock-resolving Cargo call while no ancestor
/// in the same plan runs one, with those calls: the Cargo calls that can run
/// first. Dependencies outside `steps` are not part of the plan.
pub fn first_lock_resolving_steps(steps: &[Step]) -> BTreeMap<String, Vec<CargoCall>> {
    let resolving: BTreeMap<String, Vec<CargoCall>> = steps
        .iter()
        .filter_map(|step| {
            let calls = cargo_calls(&step.cmd)
                .into_iter()
                .filter(CargoCall::resolves_lock)
                .collect::<Vec<_>>();
            (!calls.is_empty()).then(|| (step.tag(), calls))
        })
        .collect();
    let deps: BTreeMap<String, &[String]> = steps
        .iter()
        .map(|step| (step.tag(), step.deps.as_slice()))
        .collect();
    let has_resolving_ancestor = |tag: &str| {
        let mut stack: Vec<&str> = deps
            .get(tag)
            .map(|direct| direct.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let mut seen = BTreeSet::new();
        while let Some(dep) = stack.pop() {
            if !seen.insert(dep) {
                continue;
            }
            if resolving.contains_key(dep) {
                return true;
            }
            if let Some(next) = deps.get(dep) {
                stack.extend(next.iter().map(String::as_str));
            }
        }
        false
    };
    resolving
        .iter()
        .filter(|(tag, _)| !has_resolving_ancestor(tag.as_str()))
        .map(|(tag, calls)| (tag.clone(), calls.clone()))
        .collect()
}

/// One shell token.
#[derive(Debug)]
enum Token {
    /// A word with its quoting removed. A command or process substitution in
    /// it reads `$(...)`; its body is scanned separately.
    Word(String),
    /// `;`, `&`, `&&`, `|`, `||`, `|&`, `;;`, `(`, `)` or a newline.
    Separator,
    /// A redirection operator. The next word is its target, not an argument.
    Redirect,
}

/// Where the offsets of a scanned script fall in the text the scan began
/// with: verbatim from a start offset, or all at one offset for a script that
/// was a quoted word and so no longer has the original's characters.
#[derive(Clone, Copy)]
enum Origin {
    Verbatim(usize),
    Within(usize),
}

impl Origin {
    fn at(self, offset: usize) -> usize {
        match self {
            Origin::Verbatim(start) => start + offset,
            Origin::Within(at) => at,
        }
    }
}

/// The tokens of one script with their offsets, and the bodies of the
/// substitutions in it with theirs.
#[derive(Default)]
struct Lexed {
    tokens: Vec<(Token, usize)>,
    substitutions: Vec<(String, usize)>,
}

/// The word [`Lexer`] is reading.
#[derive(Default)]
struct Word {
    text: String,
    start: usize,
    /// A word has begun, even an empty one (`''`).
    started: bool,
    /// Some character of it was quoted or escaped.
    quoted: bool,
}

/// Splits a script into shell tokens the way bash reads them, as far as the
/// scan needs: quoting, escapes, operators, redirections, here-documents,
/// `${...}`, and command and process substitutions.
struct Lexer<'a> {
    chars: &'a [char],
    i: usize,
    lexed: Lexed,
    word: Word,
    /// After `<<` or `<<-`, whether its here-document strips leading tabs,
    /// until its delimiter word ends.
    awaiting_delimiter: Option<bool>,
    /// Here-documents whose bodies start on the next line.
    heredocs: Vec<(String, bool)>,
}

fn lex(script: &str) -> Lexed {
    let chars: Vec<char> = script.chars().collect();
    let mut lexer = Lexer {
        chars: &chars,
        i: 0,
        lexed: Lexed::default(),
        word: Word::default(),
        awaiting_delimiter: None,
        heredocs: Vec::new(),
    };
    lexer.run();
    lexer.lexed
}

impl Lexer<'_> {
    fn next_is(&self, offset: usize, c: char) -> bool {
        self.chars.get(self.i + offset) == Some(&c)
    }

    fn begin_word(&mut self) {
        if !self.word.started {
            self.word.started = true;
            self.word.start = self.i;
        }
    }

    fn finish_word(&mut self) {
        let word = std::mem::take(&mut self.word);
        if !word.started {
            return;
        }
        if let Some(strip_tabs) = self.awaiting_delimiter.take() {
            self.heredocs.push((word.text.clone(), strip_tabs));
        }
        self.lexed.tokens.push((Token::Word(word.text), word.start));
    }

    fn separator(&mut self) {
        self.finish_word();
        self.lexed.tokens.push((Token::Separator, self.i));
    }

    fn run(&mut self) {
        while self.i < self.chars.len() {
            match self.chars[self.i] {
                ' ' | '\t' => {
                    self.finish_word();
                    self.i += 1;
                }
                '\n' => {
                    self.separator();
                    self.i += 1;
                    self.skip_heredoc_bodies();
                }
                '#' if !self.word.started => {
                    while self.i < self.chars.len() && self.chars[self.i] != '\n' {
                        self.i += 1;
                    }
                }
                '\\' if self.next_is(1, '\n') => self.i += 2,
                '\\' => {
                    self.begin_word();
                    self.word.quoted = true;
                    if let Some(&next) = self.chars.get(self.i + 1) {
                        self.word.text.push(next);
                    }
                    self.i += 2;
                }
                '\'' => {
                    self.begin_word();
                    self.word.quoted = true;
                    self.i += 1;
                    while self.i < self.chars.len() && self.chars[self.i] != '\'' {
                        self.word.text.push(self.chars[self.i]);
                        self.i += 1;
                    }
                    self.i += 1;
                }
                '$' if self.next_is(1, '\'') => self.ansi_c_quoted(),
                '"' => self.double_quoted(),
                '$' if self.next_is(1, '(') => {
                    self.begin_word();
                    self.substitution(self.i + 1);
                }
                '$' if self.next_is(1, '{') => {
                    self.begin_word();
                    let end = closing(self.chars, self.i + 1, '{', '}');
                    self.word.text.extend(&self.chars[self.i..end]);
                    self.i = end;
                }
                '`' => {
                    self.begin_word();
                    self.backquoted();
                }
                '<' | '>' if self.next_is(1, '(') => {
                    self.begin_word();
                    self.substitution(self.i + 1);
                }
                '<' | '>' => self.redirect(),
                '&' if self.next_is(1, '>') => {
                    self.finish_word();
                    self.i += 2;
                    while self.next_is(0, '>') {
                        self.i += 1;
                    }
                    self.lexed.tokens.push((Token::Redirect, self.i));
                }
                operator @ (';' | '&' | '|' | '(' | ')') => {
                    self.separator();
                    self.i += 1;
                    let doubled = matches!(
                        (operator, self.chars.get(self.i)),
                        (';', Some(';')) | ('&', Some('&')) | ('|', Some('|' | '&'))
                    );
                    if doubled {
                        self.i += 1;
                    }
                }
                other => {
                    self.begin_word();
                    self.word.text.push(other);
                    self.i += 1;
                }
            }
        }
        self.finish_word();
    }

    fn double_quoted(&mut self) {
        self.begin_word();
        self.word.quoted = true;
        self.i += 1;
        while self.i < self.chars.len() && self.chars[self.i] != '"' {
            match self.chars[self.i] {
                '\\' if matches!(
                    self.chars.get(self.i + 1),
                    Some('"' | '\\' | '$' | '`' | '\n')
                ) =>
                {
                    if !self.next_is(1, '\n') {
                        self.word.text.push(self.chars[self.i + 1]);
                    }
                    self.i += 2;
                }
                '$' if self.next_is(1, '(') => self.substitution(self.i + 1),
                '`' => self.backquoted(),
                other => {
                    self.word.text.push(other);
                    self.i += 1;
                }
            }
        }
        self.i += 1;
    }

    fn ansi_c_quoted(&mut self) {
        self.begin_word();
        self.word.quoted = true;
        self.i += 2;
        while self.i < self.chars.len() && self.chars[self.i] != '\'' {
            if self.chars[self.i] == '\\' {
                self.i += 1;
            }
            if let Some(&c) = self.chars.get(self.i) {
                self.word.text.push(c);
            }
            self.i += 1;
        }
        self.i += 1;
    }

    fn redirect(&mut self) {
        // Digits written against the operator name its file descriptor
        // (`2>&1`); they are not an argument.
        if self.word.started
            && !self.word.quoted
            && self.word.text.chars().all(|c| c.is_ascii_digit())
        {
            self.word = Word::default();
        } else {
            self.finish_word();
        }
        let start = self.i;
        self.i += 1;
        while matches!(self.chars.get(self.i), Some('<' | '>' | '&' | '|' | '-')) {
            self.i += 1;
        }
        let operator: String = self.chars[start..self.i].iter().collect();
        if operator == "<<" || operator == "<<-" {
            self.awaiting_delimiter = Some(operator == "<<-");
        }
        self.lexed.tokens.push((Token::Redirect, start));
    }

    /// Past the end of the line the last newline closed, skip the body of
    /// every here-document that line opened, up to its delimiter line.
    fn skip_heredoc_bodies(&mut self) {
        for (delimiter, strip_tabs) in std::mem::take(&mut self.heredocs) {
            while self.i < self.chars.len() {
                let end = self.chars[self.i..]
                    .iter()
                    .position(|c| *c == '\n')
                    .map_or(self.chars.len(), |at| self.i + at);
                let line: String = self.chars[self.i..end].iter().collect();
                self.i = (end + 1).min(self.chars.len());
                let line = if strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if line == delimiter {
                    break;
                }
            }
        }
    }

    /// Record the substitution whose `(` is at `open` and move past its `)`.
    /// The word keeps only a `$(...)` placeholder, so the body is scanned
    /// once, as its own script.
    fn substitution(&mut self, open: usize) {
        let end = closing(self.chars, open, '(', ')');
        let body_end = end.saturating_sub(1).max(open + 1);
        self.lexed.substitutions.push((
            self.chars[open + 1..body_end.min(self.chars.len())]
                .iter()
                .collect(),
            open + 1,
        ));
        self.word.text.push_str("$(...)");
        self.i = end;
    }

    /// Record the backquoted substitution that starts here and move past it.
    fn backquoted(&mut self) {
        let start = self.i + 1;
        let mut i = start;
        let mut body = String::new();
        while i < self.chars.len() && self.chars[i] != '`' {
            if self.chars[i] == '\\' && i + 1 < self.chars.len() {
                body.push(self.chars[i + 1]);
                i += 2;
            } else {
                body.push(self.chars[i]);
                i += 1;
            }
        }
        self.lexed.substitutions.push((body, start));
        self.word.text.push_str("$(...)");
        self.i = i + 1;
    }
}

/// The index after the `close` that balances the `open` at `start`, skipping
/// quoted text, or the end of input when it is unbalanced.
fn closing(chars: &[char], start: usize, open: char, close: char) -> usize {
    let mut depth = 0usize;
    let mut i = start;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '\'' => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

fn collect(script: &str, origin: Origin, calls: &mut Vec<(usize, CargoCall)>) {
    let lexed = lex(script);
    let mut command: Vec<(String, usize)> = Vec::new();
    let mut redirect_target = false;
    let end = (Token::Separator, script.chars().count());
    for (token, offset) in lexed.tokens.into_iter().chain([end]) {
        match token {
            Token::Word(_) if redirect_target => redirect_target = false,
            Token::Word(word) => command.push((word, offset)),
            Token::Redirect => redirect_target = true,
            Token::Separator => {
                simple_command(&command, origin, calls);
                command.clear();
                redirect_target = false;
            }
        }
    }
    for (body, offset) in lexed.substitutions {
        let inner = match origin {
            Origin::Verbatim(start) => Origin::Verbatim(start + offset),
            within @ Origin::Within(_) => within,
        };
        collect(&body, inner, calls);
    }
}

/// Options whose value can be the word `cargo`, which is then not a call:
/// `ci/nextest-binaries.rs prepare --build-mode cargo|buck`. Any other `cargo`
/// word is read as a call, so a wrapper the scan does not know (a shell
/// function such as `hermit_run_lock_check`) still exposes its Cargo call.
const CARGO_AS_VALUE: &[&str] = &["--build-mode"];

/// Commands whose arguments name no command to run: text, tests, variable
/// and function declarations.
const NO_COMMAND_ARGUMENTS: &[&str] = &[
    "echo", "printf", "function", "[", "[[", "test", "local", "declare", "readonly", "export",
    "unset", "type", "hash",
];

fn simple_command(words: &[(String, usize)], origin: Origin, calls: &mut Vec<(usize, CargoCall)>) {
    let command_word = words.iter().find(|(word, _)| !is_command_prefix(word));
    if command_word.is_some_and(|(word, _)| NO_COMMAND_ARGUMENTS.contains(&word.as_str())) {
        return;
    }
    let call_at = (0..words.len()).find(|&index| {
        words[index].0 == "cargo"
            && (index == 0 || !CARGO_AS_VALUE.contains(&words[index - 1].0.as_str()))
    });
    if let Some(at) = call_at {
        let argv = words[at + 1..]
            .iter()
            .map(|(word, _)| word.clone())
            .collect::<Vec<_>>();
        if let Some(call) = classify(&argv) {
            calls.push((origin.at(words[at].1), call));
        }
    }
    for (word, offset) in words {
        if word != "cargo" && word.contains("cargo") && word.contains(char::is_whitespace) {
            collect(word, Origin::Within(origin.at(*offset)), calls);
        }
    }
}

/// A word that comes before the command word of a simple command: a reserved
/// word, `!`, a brace, or a variable assignment.
fn is_command_prefix(word: &str) -> bool {
    if matches!(
        word,
        "if" | "then" | "else" | "elif" | "do" | "while" | "until" | "!" | "{" | "}" | "time"
    ) {
        return true;
    }
    word.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The index of the subcommand in Cargo's arguments, past a `+toolchain` and
/// Cargo's own options, and whether those options include `--locked` or
/// `--frozen`. `None` when no subcommand follows.
fn subcommand_index(argv: &[String]) -> Option<(usize, bool)> {
    let mut index = usize::from(argv.first().is_some_and(|word| word.starts_with('+')));
    let mut locked = false;
    loop {
        match argv.get(index)?.as_str() {
            "--locked" | "--frozen" => locked = true,
            "--config" | "-Z" | "-C" | "--color" => index += 1,
            option if option.starts_with('-') => {}
            _ => return Some((index, locked)),
        }
        index += 1;
    }
}

/// The arguments of a Cargo subcommand up to a `--`, after which they belong
/// to the program it runs.
fn own_arguments(arguments: &[String]) -> &[String] {
    let end = arguments
        .iter()
        .position(|word| word == "--")
        .unwrap_or(arguments.len());
    &arguments[..end]
}

/// The call that `cargo` followed by `argv` makes, or `None` when no
/// subcommand follows (`command -v cargo`).
fn classify(argv: &[String]) -> Option<CargoCall> {
    let leading_end = argv
        .iter()
        .position(|word| !word.starts_with('-') && !word.starts_with('+'))
        .unwrap_or(argv.len());
    if argv[..leading_end].iter().any(|word| {
        matches!(
            word.as_str(),
            "-V" | "--version" | "--list" | "-h" | "--help"
        )
    }) {
        return Some(CargoCall {
            argv: argv.to_vec(),
            lock: LockUse::Unresolved(PRINTS_ONLY),
        });
    }
    let (index, global_locked) = subcommand_index(argv)?;
    let own = own_arguments(&argv[index + 1..]);
    let locked = global_locked
        || own
            .iter()
            .any(|word| word == "--locked" || word == "--frozen");
    let lock = match unresolved_reason(&argv[index], own) {
        Some(reason) => LockUse::Unresolved(reason),
        None if locked => LockUse::Locked,
        None => LockUse::Unlocked,
    };
    Some(CargoCall {
        argv: argv.to_vec(),
        lock,
    })
}

/// Why a subcommand with these arguments resolves no workspace lock file, or
/// `None` when it does. A subcommand not named here resolves one.
fn unresolved_reason(subcommand: &str, arguments: &[String]) -> Option<&'static str> {
    let has = |flag: &str| {
        arguments
            .iter()
            .any(|word| word == flag || word.starts_with(&format!("{flag}=")))
    };
    match subcommand {
        "version" | "help" => Some(PRINTS_ONLY),
        "locate-project" | "verify-project" | "read-manifest" => {
            Some("reads one manifest and resolves no dependency graph")
        }
        "metadata" if has("--no-deps") => {
            Some("`--no-deps` lists the workspace members without resolving their dependencies")
        }
        "install" if !has("--path") => Some(
            "installs a published crate under that crate's own lock file; the workspace's lock \
             file is never read or written",
        ),
        // Exactly the measured call; any other show-config form resolves.
        "nextest" if arguments == ["show-config", "version"] => Some(NEXTEST_SHOW_CONFIG_VERSION),
        _ => None,
    }
}

/// The text Cargo's refusal of a lock file that needs updating contains for
/// both `--locked` and `--frozen`, in the older ("the lock file ... needs to be
/// updated but --locked was passed to prevent this") and the current ("cannot
/// update the lock file ... because --locked was passed to prevent this")
/// wording.
const LOCK_REFUSAL: &str = "was passed to prevent this";

/// Cargo's exit status when a command fails, the lock refusal among them.
const CARGO_FAILURE: i32 = 101;

/// What one Cargo call did in a fixture workspace whose committed lock file is
/// stale.
#[derive(Debug)]
pub struct StaleLockRun {
    pub argv: Vec<String>,
    /// The exit status, or `None` when a signal ended Cargo.
    pub code: Option<i32>,
    pub stderr: String,
    pub lock_before: Vec<u8>,
    pub lock_after: Vec<u8>,
}

impl StaleLockRun {
    pub fn lock_unchanged(&self) -> bool {
        self.lock_before == self.lock_after
    }

    /// Cargo refused the stale lock file, exiting 101, and left it
    /// byte-identical.
    pub fn refused(&self) -> bool {
        self.code == Some(CARGO_FAILURE)
            && self.lock_unchanged()
            && self.stderr.contains(LOCK_REFUSAL)
    }

    /// Cargo resolved the workspace again and rewrote the stale lock file.
    pub fn rewrote(&self) -> bool {
        self.code == Some(0) && !self.lock_unchanged()
    }

    fn describe(&self) -> String {
        format!(
            "`{}` exited {:?}, lock file {}: {}",
            render(&self.argv),
            self.code,
            if self.lock_unchanged() {
                "byte-identical"
            } else {
                "rewritten"
            },
            self.stderr.trim()
        )
    }
}

/// A lock-resolving call run twice against the same stale lock file: as
/// written, which must refuse it, and with `--locked` and `--frozen` removed,
/// which must rewrite it. The second run shows that the fixture is stale in a
/// way the call would otherwise rewrite.
#[derive(Debug)]
pub struct StaleLockTwin {
    pub locked: StaleLockRun,
    pub unlocked: StaleLockRun,
}

impl StaleLockTwin {
    pub fn check(&self) -> Result<(), String> {
        if !self.locked.refused() {
            return Err(format!(
                "the call did not refuse the stale lock file with exit status 101 and leave it \
                 byte-identical: {}",
                self.locked.describe()
            ));
        }
        if !self.unlocked.rewrote() {
            return Err(format!(
                "its mutant without --locked or --frozen did not rewrite the stale lock file, so \
                 the fixture cannot tell the two apart: {}",
                self.unlocked.describe()
            ));
        }
        Ok(())
    }
}

/// `argv` with every `--locked` and `--frozen` before a `--` removed.
pub fn without_lock_flags(argv: &[String]) -> Vec<String> {
    let end = argv
        .iter()
        .position(|word| word == "--")
        .unwrap_or(argv.len());
    argv[..end]
        .iter()
        .filter(|word| !matches!(word.as_str(), "--locked" | "--frozen"))
        .chain(&argv[end..])
        .cloned()
        .collect()
}

/// Run `cargo` with `argv`, a lock-resolving call's exact arguments, in a
/// fixture workspace under the empty directory `scratch` whose committed lock
/// file is stale, then run its mutant without `--locked` and `--frozen` there.
///
/// The fixture has a member for each package `argv` selects, with the
/// features, targets and custom profile it names, so that the call fails at
/// the lock file rather than at a missing package. After its lock file is
/// generated, each selected member gains a path dependency on one more member,
/// the change a manifest edit without its lock update makes. Nothing is
/// fetched: Cargo runs offline, with its target directory under `scratch`.
pub fn stale_lock_twin(argv: &[String], scratch: &Path) -> Result<StaleLockTwin, String> {
    let fixture = StaleFixture::new(argv, scratch)?;
    let locked = fixture.run(argv)?;
    let unlocked = fixture.run(&without_lock_flags(argv))?;
    Ok(StaleLockTwin { locked, unlocked })
}

/// Run [`stale_lock_twin`] for each call, concurrently, each in its own
/// directory under `scratch`, and name every call that fails its twin with the
/// plan nodes that make it, given as the values of `calls`.
pub fn check_stale_lock_twins(
    calls: &BTreeMap<Vec<String>, BTreeSet<String>>,
    scratch: &Path,
) -> Result<(), String> {
    let failures: Vec<String> = std::thread::scope(|scope| {
        let runs = calls
            .iter()
            .enumerate()
            .map(|(index, (argv, nodes))| {
                let directory = scratch.join(format!("twin-{index}"));
                scope.spawn(move || {
                    let checked = fs::create_dir_all(&directory)
                        .map_err(|error| format!("cannot create {}: {error}", directory.display()))
                        .and_then(|()| stale_lock_twin(argv, &directory))
                        .and_then(|twin| twin.check());
                    checked.err().map(|error| {
                        format!(
                            "{} (run first by {}): {error}",
                            render(argv),
                            nodes.iter().cloned().collect::<Vec<_>>().join(", ")
                        )
                    })
                })
            })
            .collect::<Vec<_>>();
        runs.into_iter()
            .filter_map(|run| run.join().expect("a stale-lock twin panicked"))
            .collect()
    });
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "a stale lock file does not fail these first Cargo calls:\n{}",
            failures.join("\n")
        ))
    }
}

/// The member each selected fixture member comes to depend on after the lock
/// file is generated.
const LATE_DEPENDENCY: &str = "fixture-late-dependency";

/// The fixture member when the call names no package.
const DEFAULT_PACKAGE: &str = "fixture-package";

/// One fixture member: the features and targets a call names for it.
#[derive(Default)]
struct PackageShape {
    features: BTreeSet<String>,
    bins: BTreeSet<String>,
    tests: BTreeSet<String>,
    examples: BTreeSet<String>,
}

/// A fixture workspace with a stale committed lock file.
struct StaleFixture {
    workspace: PathBuf,
    target: PathBuf,
}

impl StaleFixture {
    fn new(argv: &[String], scratch: &Path) -> Result<Self, String> {
        let (packages, profiles) = fixture_shape(argv)?;
        let workspace = scratch.join("workspace");
        let mut members = packages.keys().cloned().collect::<Vec<_>>();
        members.push(LATE_DEPENDENCY.to_string());
        let mut root = format!(
            "[workspace]\nresolver = \"2\"\nmembers = [{}]\n",
            members
                .iter()
                .map(|member| format!("\"{member}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        for profile in &profiles {
            root.push_str(&format!("\n[profile.{profile}]\ninherits = \"dev\"\n"));
        }
        write(&workspace.join("Cargo.toml"), &root)?;
        for (name, package) in &packages {
            write_package(&workspace, name, package)?;
        }
        write_package(&workspace, LATE_DEPENDENCY, &PackageShape::default())?;
        let fixture = StaleFixture {
            workspace,
            target: scratch.join("target"),
        };
        let generated = fixture
            .cargo()
            .args(["generate-lockfile", "--offline"])
            .output()
            .map_err(|error| format!("cannot run cargo generate-lockfile: {error}"))?;
        if !generated.status.success() {
            return Err(format!(
                "cargo generate-lockfile failed in the fixture: {}",
                String::from_utf8_lossy(&generated.stderr).trim()
            ));
        }
        for name in packages.keys() {
            let manifest = fixture.workspace.join(name).join("Cargo.toml");
            let mut text = fs::read_to_string(&manifest)
                .map_err(|error| format!("cannot read {}: {error}", manifest.display()))?;
            text.push_str(&format!(
                "{LATE_DEPENDENCY} = {{ path = \"../{LATE_DEPENDENCY}\" }}\n"
            ));
            write(&manifest, &text)?;
        }
        Ok(fixture)
    }

    fn cargo(&self) -> Command {
        let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        command
            .current_dir(&self.workspace)
            .env("CARGO_TARGET_DIR", &self.target)
            .env("CARGO_NET_OFFLINE", "true")
            .env_remove("CARGO_BUILD_TARGET");
        command
    }

    fn run(&self, argv: &[String]) -> Result<StaleLockRun, String> {
        let lock = self.workspace.join("Cargo.lock");
        let read_lock =
            || fs::read(&lock).map_err(|error| format!("cannot read {}: {error}", lock.display()));
        let lock_before = read_lock()?;
        let output = self
            .cargo()
            .args(argv)
            .output()
            .map_err(|error| format!("cannot run {}: {error}", render(argv)))?;
        Ok(StaleLockRun {
            argv: argv.to_vec(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            lock_before,
            lock_after: read_lock()?,
        })
    }
}

/// The members a fixture for `cargo` with `argv` needs, by package name, and
/// the custom profiles it names.
fn fixture_shape(
    argv: &[String],
) -> Result<(BTreeMap<String, PackageShape>, BTreeSet<String>), String> {
    let (index, _) =
        subcommand_index(argv).ok_or_else(|| format!("`{}` has no subcommand", render(argv)))?;
    let mut packages: BTreeMap<String, PackageShape> = BTreeMap::new();
    let mut profiles = BTreeSet::new();
    let mut features = BTreeSet::new();
    let mut targets = PackageShape::default();
    let mut words = own_arguments(&argv[index + 1..]).iter();
    while let Some(word) = words.next() {
        let (flag, inline) = match word.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (word.as_str(), None),
        };
        let mut value = || {
            inline
                .map(str::to_string)
                .or_else(|| words.next().cloned())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "-p" | "--package" => {
                packages.entry(value()?).or_default();
            }
            "-F" | "--features" => {
                for feature in value()?.split([',', ' ']).filter(|f| !f.is_empty()) {
                    match feature.split_once('/') {
                        Some((package, feature)) => {
                            packages
                                .entry(package.to_string())
                                .or_default()
                                .features
                                .insert(feature.to_string());
                        }
                        None => {
                            features.insert(feature.to_string());
                        }
                    }
                }
            }
            "--bin" => {
                targets.bins.insert(value()?);
            }
            "--test" => {
                targets.tests.insert(value()?);
            }
            "--example" => {
                targets.examples.insert(value()?);
            }
            "--profile" => {
                let profile = value()?;
                if !matches!(profile.as_str(), "dev" | "release" | "test" | "bench") {
                    profiles.insert(profile);
                }
            }
            "--manifest-path" => {
                let path = value()?;
                if path != "Cargo.toml" {
                    return Err(format!(
                        "the fixture runs in its workspace root, so it takes `--manifest-path \
                         Cargo.toml` only, not `{path}`"
                    ));
                }
            }
            "--target" => {
                return Err("the fixture builds for the host only, so it takes no --target".into());
            }
            "--message-format" | "--target-dir" | "-j" | "--jobs" | "--color" | "--config"
            | "-Z" | "--exclude" | "--artifact-dir" => {
                value()?;
            }
            _ => {}
        }
    }
    if packages.is_empty() {
        packages.insert(DEFAULT_PACKAGE.to_string(), PackageShape::default());
    }
    for package in packages.values_mut() {
        package.features.extend(features.iter().cloned());
    }
    if let Some(first) = packages.values_mut().next() {
        first.bins.extend(targets.bins);
        first.tests.extend(targets.tests);
        first.examples.extend(targets.examples);
    }
    Ok((packages, profiles))
}

fn write_package(workspace: &Path, name: &str, package: &PackageShape) -> Result<(), String> {
    let directory = workspace.join(name);
    let mut manifest = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n\
         autobins = false\nautoexamples = false\nautotests = false\nautobenches = false\n\n\
         [lib]\npath = \"src/lib.rs\"\n"
    );
    write(
        &directory.join("src/lib.rs"),
        "//! A stale-lock fixture member.\n\n/// Does nothing.\npub fn fixture() {}\n",
    )?;
    let default_bin = [format!("{name}-bin")];
    let bins = if package.bins.is_empty() {
        default_bin.iter().collect::<Vec<_>>()
    } else {
        package.bins.iter().collect()
    };
    for bin in bins {
        manifest.push_str(&format!(
            "\n[[bin]]\nname = \"{bin}\"\npath = \"src/bin/{bin}.rs\"\n"
        ));
        write(
            &directory.join(format!("src/bin/{bin}.rs")),
            "fn main() {}\n",
        )?;
    }
    for test in &package.tests {
        manifest.push_str(&format!(
            "\n[[test]]\nname = \"{test}\"\npath = \"tests/{test}.rs\"\n"
        ));
        write(&directory.join(format!("tests/{test}.rs")), "")?;
    }
    for example in &package.examples {
        manifest.push_str(&format!(
            "\n[[example]]\nname = \"{example}\"\npath = \"examples/{example}.rs\"\n"
        ));
        write(
            &directory.join(format!("examples/{example}.rs")),
            "fn main() {}\n",
        )?;
    }
    manifest.push_str("\n[features]\n");
    for feature in &package.features {
        manifest.push_str(&format!("{feature} = []\n"));
    }
    manifest.push_str("\n[dependencies]\n");
    write(&directory.join("Cargo.toml"), &manifest)
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    fs::write(path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scripts validation nodes run whose Cargo calls resolve the Hermit
    /// workspace, each with how a node reaches it. The invariant test scans
    /// each in full, beside every node command. The list is fixed, not
    /// discovered: a Cargo call in any other program a node runs is not seen.
    const SCRIPTS: [(&str, &str, &str); 6] = [
        (
            "ci/run-nextest-counted.sh",
            include_str!("../../run-nextest-counted.sh"),
            "71 nodes run it; its `cargo nextest` runs when HERMIT_PREPARED_NEXTEST_REQUIRED is \
             not 1",
        ),
        (
            "ci/kvm-gap-ledger.rs",
            include_str!("../../kvm-gap-ledger.rs"),
            "privileged-test.deadline_slice_cap_kvm runs it; its `cargo nextest list` runs when \
             HERMIT_PREPARED_NEXTEST_REQUIRED is not 1",
        ),
        (
            "ci/compat-envelope/scorecard.rs",
            include_str!("../../compat-envelope/scorecard.rs"),
            "the scorecard.compatibility nodes and selftest.scorecard_commands build and run \
             hermit-manifest-plan through it",
        ),
        (
            "ci/compat-envelope/pressure-test.rs",
            include_str!("../../compat-envelope/pressure-test.rs"),
            "selftest.pressure_test runs it",
        ),
        (
            "ci/check-musl-detcore.sh",
            include_str!("../../check-musl-detcore.sh"),
            "check.musl_detcore runs it, as does `make lint` through lint-musl-detcore",
        ),
        (
            "ci/stage-detcore-liteinst.sh",
            include_str!("../../stage-detcore-liteinst.sh"),
            "build.hermit_exec_stop_launcher_compile_in_pinned_root runs it; its `cargo build` \
             builds the detcore-liteinst runtime beside the cohort's test executables",
        ),
    ];

    fn committed() -> dagrun::model::DagConfig {
        dagrun::io::dag_from_json(include_str!("../../dag/validate.json")).unwrap()
    }

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    /// A directory under the temporary directory, removed when dropped, so
    /// a failing test leaves nothing behind either.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "locked-cargo-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn kind(lock: LockUse) -> &'static str {
        match lock {
            LockUse::Locked => "locked",
            LockUse::Unlocked => "unlocked",
            LockUse::Unresolved(_) => "unresolved",
        }
    }

    /// The scripts' lock-resolving Cargo calls without `--locked` or
    /// `--frozen`, as `path:line: call`.
    fn unlocked_script_calls(scripts: &[(&str, String)]) -> Vec<String> {
        scripts
            .iter()
            .flat_map(|(path, text)| {
                script_cargo_calls(path, text)
                    .into_iter()
                    .filter(|call| call.lock == LockUse::Unlocked)
                    .map(move |call| format!("{path}:{}: {}", call.line, call.shown))
            })
            .collect()
    }

    #[test]
    fn calls_are_found_in_nested_and_quoted_scripts_once_each() {
        let pinned_root = "./ci/hermetic/run-in-pinned-root.sh --src . --out o --src-rw \
             --cargo-home o/cargo -- bash -c 'guard && hermit_payload=$1 && shift && exec bash \
             -c \"$hermit_payload\"' bash 'export X=1; RUSTDOCFLAGS='\\''-D warnings'\\'' \
             ./ci/run-with-core-build.sh cargo doc --workspace --no-deps'";
        let cases: &[(&str, &[(&str, &str)])] = &[
            (pinned_root, &[("doc --workspace --no-deps", "unlocked")]),
            (
                "cargo build --locked -p a --bins",
                &[("build --locked -p a --bins", "locked")],
            ),
            (
                "cargo --locked build -p a",
                &[("--locked build -p a", "locked")],
            ),
            (
                "cargo +1.97.1 build --frozen",
                &[("+1.97.1 build --frozen", "locked")],
            ),
            // After `--` the flag is the test binary's, not Cargo's.
            (
                "cargo test -p a -- --locked",
                &[("test -p a -- --locked", "unlocked")],
            ),
            (
                "cargo build -p a && echo --locked",
                &[("build -p a", "unlocked")],
            ),
            (
                "cargo build; cargo test --locked",
                &[("build", "unlocked"), ("test --locked", "locked")],
            ),
            (
                "CARGO_BUILD_JOBS=8 cargo build --locked --message-format json > >(python3 \
                 ./ci/cargo-artifact-ready.py hermit-bin) 2>&1",
                &[("build --locked --message-format json", "locked")],
            ),
            (
                "cargo nextest show-config version >/dev/null 2>&1 || if command -v with-proxy \
                 >/dev/null 2>&1; then with-proxy cargo install cargo-nextest --locked --version \
                 0.9.100; else cargo install cargo-nextest --version 0.9.100; fi",
                &[
                    ("nextest show-config version", "unresolved"),
                    (
                        "install cargo-nextest --locked --version 0.9.100",
                        "unresolved",
                    ),
                    ("install cargo-nextest --version 0.9.100", "unresolved"),
                ],
            ),
            (
                "cargo install --path .",
                &[("install --path .", "unlocked")],
            ),
            (
                "printf 'build it with cargo build -p a\\n' >&2; echo cargo build -p a",
                &[],
            ),
            (
                "version=$(cargo metadata --format-version 1); test -n \"$(cargo metadata \
                 --no-deps --format-version 1)\"",
                &[
                    ("metadata --format-version 1", "unlocked"),
                    ("metadata --no-deps --format-version 1", "unresolved"),
                ],
            ),
            (
                "if ! cargo check -p a; then exit 1; fi",
                &[("check -p a", "unlocked")],
            ),
            (
                "bash -c \"cargo build \\\"--locked\\\" -p 'a b'\"",
                &[("build --locked -p [a b]", "locked")],
            ),
            ("command -v cargo >/dev/null 2>&1", &[]),
            (
                "./ci/nextest-binaries.rs prepare full --build-mode cargo --cohort native-exec",
                &[],
            ),
            (
                "hermit_wrap() { \"$@\"; }; hermit_wrap cargo build -p a",
                &[("build -p a", "unlocked")],
            ),
            ("cargo --version", &[("--version", "unresolved")]),
            (
                "./ci/run-nextest-counted.sh ${CI:+--profile ci} -p a --cargo-home x/cargo",
                &[],
            ),
            // A here-document's body is text, even one that reads as a call.
            (
                "python3 - <<'PYEOF'\nprint('cargo build -p a')\n# it's\nPYEOF\ncargo test",
                &[("test", "unlocked")],
            ),
            (
                "function cargo {\n    [[ $1 == nextest ]] || return 99\n}\nunset -f cargo\n\
                 [[ $x == cargo ]] && cargo build $'--locked'",
                &[("build --locked", "locked")],
            ),
            (
                "RUSTFLAGS=\"-A deprecated\" \\\n    cargo check --locked -p a",
                &[("check --locked -p a", "locked")],
            ),
        ];
        for (command, expected) in cases {
            // A word holding a space reads `[a b]`.
            let found = cargo_calls(command)
                .into_iter()
                .map(|call| {
                    let shown = call
                        .argv
                        .iter()
                        .map(|word| {
                            if word.contains(' ') {
                                format!("[{word}]")
                            } else {
                                word.clone()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    (shown, kind(call.lock))
                })
                .collect::<Vec<_>>();
            let expected = expected
                .iter()
                .map(|(argv, lock)| (argv.to_string(), *lock))
                .collect::<Vec<_>>();
            assert_eq!(found, expected, "{command}");
        }
    }

    #[test]
    fn script_calls_are_named_by_line() {
        let shell = "#!/bin/bash\n# cargo build in a comment\nif true; then\n    \
             cargo nextest --config-file x run \"$@\"\nfi\n";
        assert_eq!(
            script_cargo_calls("ci/x.sh", shell),
            [ScriptCargoCall {
                line: 4,
                lock: LockUse::Unlocked,
                shown: "cargo nextest --config-file x run $@".to_string(),
            }]
        );
        let rust = "fn a() {\n    let mut command = Command::new(\"cargo\");\n    \
             command.args([\"build\", \"--locked\"]);\n}\n\nfn b() {\n    let output = \
             Command::new(\"cargo\")\n        .args([\"build\"])\n        .output();\n}\n\
             fn c() {\n    \"--locked\"\n}\n";
        assert_eq!(
            script_cargo_calls("ci/x.rs", rust)
                .into_iter()
                .map(|call| (call.line, kind(call.lock)))
                .collect::<Vec<_>>(),
            [(2, "locked"), (7, "unlocked")]
        );
    }

    #[test]
    fn every_cargo_call_that_resolves_a_lock_carries_locked_or_frozen() {
        let committed = committed();
        let excepted: BTreeMap<&str, &str> = UNLOCKED_CARGO_EXCEPTIONS.iter().copied().collect();
        assert_eq!(
            excepted.len(),
            UNLOCKED_CARGO_EXCEPTIONS.len(),
            "UNLOCKED_CARGO_EXCEPTIONS names a node or script twice"
        );
        let unlocked = unlocked_cargo_calls(&committed.steps);
        let scripts = SCRIPTS
            .iter()
            .map(|(path, text, _)| (*path, text.to_string()))
            .collect::<Vec<_>>();
        let unlocked_in_scripts = unlocked_script_calls(&scripts);
        let mut refused = unlocked
            .iter()
            .filter(|(node, _)| !excepted.contains_key(node.as_str()))
            .map(|(node, calls)| format!("{node}: {}", calls.join("; ")))
            .collect::<Vec<_>>();
        refused.extend(
            unlocked_in_scripts
                .iter()
                .filter(|call| {
                    !excepted
                        .keys()
                        .any(|script| call.starts_with(&format!("{script}:")))
                })
                .cloned(),
        );
        assert!(
            refused.is_empty(),
            "no validation step may rewrite a committed Cargo.lock, but these Cargo calls \
             resolve a workspace without --locked or --frozen and UNLOCKED_CARGO_EXCEPTIONS \
             gives no reason:\n{}",
            refused.join("\n")
        );
        for (place, reason) in UNLOCKED_CARGO_EXCEPTIONS {
            assert!(
                !reason.trim().is_empty(),
                "{place} is excepted without a reason"
            );
            assert!(
                unlocked.contains_key(*place)
                    || unlocked_in_scripts
                        .iter()
                        .any(|call| call.starts_with(&format!("{place}:"))),
                "{place} is excepted but makes no unlocked call; remove its entry"
            );
        }

        // The scan reaches a call nested in the pinned-root wrapper's quoted
        // payload, one followed by a process substitution, and a call in each
        // script.
        let step = |tag: &str| {
            committed
                .steps
                .iter()
                .find(|step| step.tag() == tag)
                .unwrap_or_else(|| panic!("{tag} is not in the committed DAG"))
        };
        let nested = step("setup.manifest_plan_in_pinned_root");
        assert!(
            nested
                .cmd
                .starts_with("./ci/hermetic/run-in-pinned-root.sh ")
        );
        assert_eq!(
            cargo_calls(&nested.cmd),
            [CargoCall {
                argv: words("build --locked -p hermit-manifest-plan --bins"),
                lock: LockUse::Locked,
            }]
        );
        let compile = cargo_calls(&step("build.workspace_compile_in_pinned_root").cmd);
        assert!(
            compile.len() >= 2 && compile.iter().all(|call| call.lock == LockUse::Locked),
            "{compile:?}"
        );
        assert!(
            compile
                .iter()
                .all(|call| !call.argv.iter().any(|word| word.contains("python3"))),
            "a redirection target leaked into a Cargo call: {compile:?}"
        );
        for (path, text) in &scripts {
            assert!(
                script_cargo_calls(path, text)
                    .iter()
                    .any(|call| call.lock == LockUse::Locked),
                "the scan finds no locked Cargo call in {path}"
            );
        }

        // Planted mutants: one node's --locked removed, and each script
        // site's, is refused by name.
        let mut planted = committed.clone();
        let node = planted
            .steps
            .iter_mut()
            .find(|step| step.tag() == "setup.manifest_plan_in_pinned_root")
            .unwrap();
        node.cmd = node.cmd.replace("cargo build --locked ", "cargo build ");
        assert_eq!(
            unlocked_cargo_calls(&planted.steps),
            BTreeMap::from([(
                "setup.manifest_plan_in_pinned_root".to_string(),
                vec!["cargo build -p hermit-manifest-plan --bins".to_string()],
            )])
        );
        for (path, locked, unlocked) in [
            (
                "ci/run-nextest-counted.sh",
                "\"$operation\" --locked \"${forwarded[@]}\"",
                "\"$operation\" \"${forwarded[@]}\"",
            ),
            (
                "ci/kvm-gap-ledger.rs",
                "[\"nextest\", \"list\", \"--locked\"]",
                "[\"nextest\", \"list\"]",
            ),
            (
                "ci/compat-envelope/scorecard.rs",
                "\"--quiet\",\n            \"--locked\",\n",
                "\"--quiet\",\n",
            ),
        ] {
            let mut planted = scripts.clone();
            let text = &mut planted
                .iter_mut()
                .find(|(script, _)| *script == path)
                .unwrap()
                .1;
            assert_eq!(text.matches(locked).count(), 1, "{path}");
            *text = text.replacen(locked, unlocked, 1);
            let refused = unlocked_script_calls(&planted);
            assert!(
                refused.len() == 1 && refused[0].starts_with(&format!("{path}:")),
                "{path}: {refused:?}"
            );
        }
    }

    /// The plan of every label in the committed DAG (the full and portable
    /// lanes among them) and the Cargo calls in it that can run first: each,
    /// run as written against a stale committed lock file, must refuse it and
    /// leave it byte-identical, and its mutant without `--locked` must rewrite
    /// it. Focused plans are scripts/validate.rs's.
    #[test]
    fn a_stale_lock_fails_the_first_cargo_call_of_every_committed_plan() {
        let committed = committed();
        let labels = committed
            .steps
            .iter()
            .flat_map(|step| step.labels.iter().cloned())
            .collect::<BTreeSet<_>>();
        let mut first: BTreeMap<Vec<String>, BTreeSet<String>> = BTreeMap::new();
        for label in &labels {
            let plan =
                dagrun::select_steps_by_labels(&committed, std::slice::from_ref(label)).unwrap();
            for (node, calls) in first_lock_resolving_steps(&plan.steps) {
                for call in calls {
                    first
                        .entry(call.argv)
                        .or_default()
                        .insert(format!("{label}: {node}"));
                }
            }
        }
        for node in [
            "full: setup.manifest_plan",
            "hosted-privileged: setup.manifest_plan_on_host",
        ] {
            assert!(
                first.values().flatten().any(|first| first == node),
                "{node} no longer makes a first Cargo call: {first:?}"
            );
        }
        let scratch = Scratch::new("first-calls");
        check_stale_lock_twins(&first, &scratch.0).unwrap();
    }

    /// Repeats the measurement behind [`NEXTEST_SHOW_CONFIG_VERSION`] with the
    /// installed cargo-nextest, which the validation installs as setup.nextest.
    #[test]
    fn nextest_show_config_leaves_a_stale_lock_file_untouched() {
        let argv = words("nextest show-config version");
        assert_eq!(
            classify(&argv).map(|call| kind(call.lock)),
            Some("unresolved")
        );
        let scratch = Scratch::new("show-config");
        let run = StaleFixture::new(&argv, &scratch.0)
            .and_then(|fixture| fixture.run(&argv))
            .unwrap();
        assert!(
            run.code == Some(0) && run.lock_unchanged(),
            "`cargo nextest show-config version` must exit 0 and leave a stale lock file \
             byte-identical (the measurement needs cargo-nextest installed): {}",
            run.describe()
        );
    }

    /// Only the measured `show-config version` is exempt. `show-config
    /// test-groups` builds the test list, which resolves the lock file, so a
    /// node that runs it without `--locked` is refused by name, and with
    /// `--locked` it is accepted.
    #[test]
    fn a_node_running_show_config_test_groups_needs_locked() {
        let template = committed()
            .steps
            .into_iter()
            .find(|step| step.tag() == "setup.nextest")
            .unwrap();
        let node = |cmd: &str| {
            let mut step = template.clone();
            step.group = "synthetic".to_string();
            step.job = "show_config_test_groups".to_string();
            step.cmd = cmd.to_string();
            step
        };
        assert_eq!(
            unlocked_cargo_calls(&[node("cargo nextest show-config test-groups >/dev/null")]),
            BTreeMap::from([(
                "synthetic.show_config_test_groups".to_string(),
                vec!["cargo nextest show-config test-groups".to_string()],
            )]),
            "the invariant must refuse `cargo nextest show-config test-groups` without --locked \
             and name its node"
        );
        assert_eq!(
            unlocked_cargo_calls(&[node(
                "cargo nextest show-config test-groups --locked >/dev/null"
            )]),
            BTreeMap::new(),
            "the invariant must accept `cargo nextest show-config test-groups --locked`"
        );
    }

    #[test]
    fn the_fixture_takes_each_selected_package_feature_target_and_profile() {
        let (packages, profiles) = fixture_shape(&words(
            "build --locked --profile validate --workspace --all-targets --features \
             hermit-run/liteinst,shared -p hermit-manifest-plan --bin nextest-cpu-wrapper \
             --message-format json-render-diagnostics",
        ))
        .unwrap();
        assert_eq!(
            packages.keys().map(String::as_str).collect::<Vec<_>>(),
            ["hermit-manifest-plan", "hermit-run"]
        );
        assert_eq!(
            packages["hermit-run"].features,
            BTreeSet::from(["liteinst".to_string(), "shared".to_string()])
        );
        assert_eq!(
            packages["hermit-manifest-plan"].bins,
            BTreeSet::from(["nextest-cpu-wrapper".to_string()])
        );
        assert_eq!(profiles, BTreeSet::from(["validate".to_string()]));
        assert!(fixture_shape(&words("build --manifest-path reverie/Cargo.toml")).is_err());
        assert_eq!(
            without_lock_flags(&words("--frozen test --locked -p a -- --locked")),
            words("test -p a -- --locked")
        );
    }
}
