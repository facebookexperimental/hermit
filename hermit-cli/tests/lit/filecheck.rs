// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! The part of LLVM's FileCheck that detcore/tests/lit's fixtures use, and no
//! more:
//!
//! - the `PREFIX:`, `PREFIX-NEXT:` and `PREFIX-EMPTY:` directives, for one
//!   prefix (`CHECK` unless `--check-prefix` names another);
//! - patterns of literal text, `{{regex}}`, `[[NAME:regex]]` and `[[NAME]]`.
//!   A regex is compiled by the `regex` crate, not as FileCheck's POSIX
//!   extended syntax; the fixtures' seven distinct regexes (`.+`, `[0-9]+`,
//!   `([ab]+)`, `(1*2+|2*1+)` and the like) mean the same in both;
//! - FileCheck's default whitespace rule: a run of spaces and tabs, in the
//!   input and in a pattern's literal text, matches any other such run.
//!
//! Anything else FileCheck would accept (another directive suffix of the
//! prefix, a numeric or pseudo variable, a variable used in the directive that
//! defines it) is refused by [`parse`] as unsupported, so a fixture can never
//! pass by relying on syntax this matcher would misread.

use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Check,
    Next,
    Empty,
}

#[derive(Debug, PartialEq, Eq)]
enum Piece {
    Literal(String),
    Regex(String),
    Define(String, String),
    Use(String),
}

#[derive(Debug)]
pub struct Directive {
    kind: Kind,
    /// The 1-based line of the check file that holds the directive.
    line: usize,
    text: String,
    pieces: Vec<Piece>,
}

/// The directives of CHECK_TEXT for PREFIX, or why it uses something outside
/// the supported subset.
pub fn parse(check_text: &str, prefix: &str) -> Result<Vec<Directive>, String> {
    if !valid_prefix(prefix) {
        return Err(format!("check prefix {prefix:?} is not a valid prefix"));
    }
    let mut directives: Vec<Directive> = Vec::new();
    for (index, line) in check_text.lines().enumerate() {
        let number = index + 1;
        let Some((kind, rest)) = find_directive(line, prefix)
            .map_err(|error| format!("check line {number}: {error}"))?
        else {
            continue;
        };
        let text = rest.trim_matches([' ', '\t']).to_owned();
        match kind {
            Kind::Empty if !text.is_empty() => {
                return Err(format!(
                    "check line {number}: {prefix}-EMPTY carries a pattern"
                ));
            }
            Kind::Check | Kind::Next if text.is_empty() => {
                return Err(format!("check line {number}: empty check string"));
            }
            Kind::Next | Kind::Empty if directives.is_empty() => {
                return Err(format!(
                    "check line {number}: {kind:?} directive without a previous {prefix}:"
                ));
            }
            _ => {}
        }
        let pieces = pieces(&text).map_err(|error| format!("check line {number}: {error}"))?;
        let mut defined = Vec::new();
        for piece in &pieces {
            match piece {
                Piece::Define(name, _) => defined.push(name.as_str()),
                Piece::Use(name) if defined.contains(&name.as_str()) => {
                    return Err(format!(
                        "check line {number}: [[{name}]] is used in the directive that defines it"
                    ));
                }
                _ => {}
            }
        }
        directives.push(Directive {
            kind,
            line: number,
            text,
            pieces,
        });
    }
    if directives.is_empty() {
        return Err(format!("no check strings found with prefix {prefix}:"));
    }
    Ok(directives)
}

fn valid_prefix(prefix: &str) -> bool {
    let mut chars = prefix.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The first directive for PREFIX on LINE, as FileCheck finds it: the prefix
/// not preceded by a word character, a hyphen or an underscore.
fn find_directive<'a>(line: &'a str, prefix: &str) -> Result<Option<(Kind, &'a str)>, String> {
    let mut start = 0;
    while let Some(offset) = line[start..].find(prefix) {
        let at = start + offset;
        start = at + prefix.len();
        let preceded = line[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if preceded {
            continue;
        }
        let after = &line[start..];
        if let Some(rest) = after.strip_prefix(':') {
            return Ok(Some((Kind::Check, rest)));
        }
        if let Some(rest) = after.strip_prefix("-NEXT:") {
            return Ok(Some((Kind::Next, rest)));
        }
        if let Some(rest) = after.strip_prefix("-EMPTY:") {
            return Ok(Some((Kind::Empty, rest)));
        }
        if let Some(suffix) = after.strip_prefix('-') {
            let word: String = suffix
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '-')
                .collect();
            let rest = &suffix[word.len()..];
            if !word.is_empty() && (rest.starts_with(':') || word.starts_with("COUNT-")) {
                return Err(format!("{prefix}-{word} is not supported"));
            }
        }
    }
    Ok(None)
}

fn pieces(text: &str) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("{{") {
            let end = after
                .find("}}")
                .ok_or_else(|| format!("unterminated {{{{ in {text:?}"))?;
            flush(&mut literal, &mut pieces);
            let regex = &after[..end];
            if regex.is_empty() {
                return Err(format!("empty {{{{}}}} in {text:?}"));
            }
            pieces.push(Piece::Regex(regex.to_owned()));
            rest = &after[end + 2..];
        } else if let Some(after) = rest.strip_prefix("[[") {
            let end = after
                .find("]]")
                .ok_or_else(|| format!("unterminated [[ in {text:?}"))?;
            flush(&mut literal, &mut pieces);
            let body = &after[..end];
            let (name, regex) = match body.split_once(':') {
                Some((name, regex)) => (name, Some(regex)),
                None => (body, None),
            };
            if !valid_variable(name) {
                return Err(format!("[[{body}]] is not a supported variable"));
            }
            pieces.push(match regex {
                Some("") => return Err(format!("[[{body}]] defines an empty pattern")),
                Some(regex) => Piece::Define(name.to_owned(), regex.to_owned()),
                None => Piece::Use(name.to_owned()),
            });
            rest = &after[end + 2..];
        } else {
            let c = rest.chars().next().unwrap();
            literal.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    flush(&mut literal, &mut pieces);
    Ok(pieces)
}

fn flush(literal: &mut String, pieces: &mut Vec<Piece>) {
    if !literal.is_empty() {
        pieces.push(Piece::Literal(std::mem::take(literal)));
    }
}

fn valid_variable(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Collapse every run of spaces and tabs to one space.
fn canonical(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.chars() {
        if c == ' ' || c == '\t' {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// Check INPUT against DIRECTIVES, or say where it first fails to match.
pub fn check(directives: &[Directive], input: &str) -> Result<(), String> {
    let mut lines: Vec<String> = input.split('\n').map(canonical).collect();
    if input.ends_with('\n') {
        lines.pop();
    }
    let mut variables: Vec<(String, String)> = Vec::new();
    // The line and byte offset just past the previous match.
    let mut position = (0, 0);
    let mut previous: Option<usize> = None;
    for directive in directives {
        let fail = |what: String| {
            Err(format!(
                "check line {} `{}`: {what}\ninput:\n{input}",
                directive.line, directive.text
            ))
        };
        if directive.kind == Kind::Empty {
            // FileCheck matches CHECK-EMPTY as `\n$` from the previous match,
            // and `$` also matches at the end of the input, so the empty line
            // after the input's final newline counts as the next line.
            let next = previous.unwrap() + 1;
            let empty = match lines.get(next) {
                Some(line) => line.is_empty(),
                None => next == lines.len() && input.ends_with('\n'),
            };
            if !empty {
                return fail(format!("input line {} is not empty", next + 1));
            }
            position = (next, 0);
            previous = Some(next);
            continue;
        }
        let (regex, groups) = match compile(directive, &variables) {
            Ok(compiled) => compiled,
            Err(error) => return fail(error),
        };
        let found = (position.0..lines.len()).find_map(|index| {
            let from = if index == position.0 { position.1 } else { 0 };
            let line = &lines[index][from..];
            regex.captures(line).map(|captures| (index, from, captures))
        });
        let Some((index, from, captures)) = found else {
            return fail("no match in the rest of the input".into());
        };
        if directive.kind == Kind::Next {
            let previous = previous.unwrap();
            if index == previous {
                return fail(format!(
                    "the match is on input line {}, the same line as the previous match",
                    index + 1
                ));
            }
            if index != previous + 1 {
                return fail(format!(
                    "the first match is on input line {}, not on line {}",
                    index + 1,
                    previous + 2
                ));
            }
        }
        for (name, group) in groups {
            let value = captures.get(group).map_or("", |m| m.as_str());
            variables.retain(|(defined, _)| *defined != name);
            variables.push((name, value.to_owned()));
        }
        position = (index, from + captures.get(0).unwrap().end());
        previous = Some(index);
    }
    Ok(())
}

/// DIRECTIVE's pattern as one regex, with the capture group of each variable
/// it defines.
fn compile(
    directive: &Directive,
    variables: &[(String, String)],
) -> Result<(Regex, Vec<(String, usize)>), String> {
    let mut source = String::new();
    let mut groups = Vec::new();
    let mut group = 0;
    for piece in &directive.pieces {
        match piece {
            Piece::Literal(text) => source.push_str(&regex::escape(&canonical(text))),
            Piece::Regex(regex) => {
                group += Regex::new(regex)
                    .map_err(|error| format!("{{{{{regex}}}}} is not a valid regex: {error}"))?
                    .captures_len()
                    - 1;
                source.push_str(&format!("(?:{regex})"));
            }
            Piece::Define(name, regex) => {
                let inner = Regex::new(regex)
                    .map_err(|error| format!("[[{name}:{regex}]] is not a valid regex: {error}"))?;
                group += 1;
                groups.push((name.clone(), group));
                group += inner.captures_len() - 1;
                source.push_str(&format!("({regex})"));
            }
            Piece::Use(name) => {
                let value = variables
                    .iter()
                    .find(|(defined, _)| defined == name)
                    .map(|(_, value)| value)
                    .ok_or_else(|| format!("[[{name}]] is used before it is defined"))?;
                source.push_str(&regex::escape(value));
            }
        }
    }
    let regex = Regex::new(&source).map_err(|error| format!("bad pattern: {error}"))?;
    Ok((regex, groups))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(checks: &str, input: &str) -> Result<(), String> {
        check(&parse(checks, "CHECK").unwrap(), input)
    }

    #[test]
    fn check_matches_anywhere_after_the_previous_match() {
        run("CHECK: b\nCHECK: d", "a\nb c\nd\n").unwrap();
        run("CHECK: b\nCHECK: c", "a b c\n").unwrap();
        run("CHECK: d\nCHECK: b", "b\nd\n").unwrap_err();
        run("CHECK: x", "a\nb\n").unwrap_err();
    }

    #[test]
    fn check_next_needs_the_very_next_line() {
        run("CHECK: a\nCHECK-NEXT: b", "a\nb\n").unwrap();
        let skipped = run("CHECK: a\nCHECK-NEXT: c", "a\nb\nc\n").unwrap_err();
        assert!(skipped.contains("not on line 2"), "{skipped}");
        let same = run("CHECK: a\nCHECK-NEXT: b", "a b\nb\n").unwrap_err();
        assert!(same.contains("same line"), "{same}");
        run("CHECK: a\nCHECK-NEXT: b", "a\n").unwrap_err();
    }

    #[test]
    fn check_empty_needs_an_empty_next_line() {
        run("CHECK: a\nCHECK-EMPTY:\nCHECK-NEXT: b", "a\n\nb\n").unwrap();
        run("CHECK: a\nCHECK-EMPTY:", "a\nb\n").unwrap_err();
        // As in FileCheck, whose CHECK-EMPTY is `\n$`: the end of the input
        // after a final newline is an empty next line, and no newline is none.
        run("CHECK: a\nCHECK-EMPTY:", "a\n").unwrap();
        run("CHECK: a\nCHECK-EMPTY:", "a").unwrap_err();
        run("CHECK: a\nCHECK-EMPTY:", "a\n\n").unwrap();
        // Nothing follows that empty line.
        run("CHECK: a\nCHECK-EMPTY:\nCHECK: b", "a\n").unwrap_err();
    }

    #[test]
    fn regexes_and_variables_match_and_bind() {
        run("CHECK: x={{[0-9]+}};", "x=42;\n").unwrap();
        run("CHECK: x={{[0-9]+}};", "x=4a;\n").unwrap_err();
        run(
            "CHECK: dev: [[DEV:[0-9]+]],\nCHECK: dev: [[DEV]],",
            "dev: 7,\nother\ndev: 7,\n",
        )
        .unwrap();
        let changed = run(
            "CHECK: dev: [[DEV:[0-9]+]],\nCHECK: dev: [[DEV]],",
            "dev: 7,\ndev: 8,\n",
        )
        .unwrap_err();
        assert!(changed.contains("no match"), "{changed}");
        // A group inside a {{regex}} does not shift the variable's group.
        run(
            "CHECK: {{(a)(b)}} [[V:c+]]\nCHECK-NEXT: [[V]]",
            "ab ccc\nccc\n",
        )
        .unwrap();
        // Literal text is escaped, not read as a regex.
        run("CHECK: a.c", "abc\n").unwrap_err();
    }

    #[test]
    fn whitespace_runs_match_each_other() {
        run("CHECK: a  b", "a\tb\n").unwrap();
        run("CHECK: a b", "a     b\n").unwrap();
        run("CHECK:     indented", "indented\n").unwrap();
        run("CHECK: ab", "a b\n").unwrap_err();
    }

    #[test]
    fn only_the_named_prefix_counts() {
        let checks = "CHECK: never\nPARENT: a\nPARENT-NEXT: b";
        check(&parse(checks, "PARENT").unwrap(), "a\nb\n").unwrap();
        // Part of a longer word is not a directive.
        let directives = parse("XCHECK: no\nCHECK_X: no\nCHECKS: no\nCHECK: yes", "CHECK").unwrap();
        assert_eq!(directives.len(), 1);
        assert_eq!(directives[0].text, "yes");
    }

    #[test]
    fn unsupported_syntax_is_refused() {
        for checks in [
            "CHECK: a\nCHECK-NOT: b",
            "CHECK: a\nCHECK-SAME: b",
            "CHECK-DAG: a",
            "CHECK-LABEL: a",
            "CHECK-COUNT-2: a",
            "CHECK: [[#N]]",
            "CHECK: [[@LINE]]",
            "CHECK: [[V:a]] [[V]]",
            "CHECK: {{unterminated",
            "CHECK:",
            "CHECK-NEXT: a",
            "CHECK: a\nCHECK-EMPTY: b",
            "no directives at all",
        ] {
            assert!(
                parse(checks, "CHECK").is_err(),
                "{checks:?} must be refused"
            );
        }
        assert!(parse("CHECK: a", "1BAD").is_err());
        let bad = parse("CHECK: {{(}}", "CHECK").unwrap();
        assert!(
            check(&bad, "x\n")
                .unwrap_err()
                .contains("not a valid regex")
        );
        let undefined = parse("CHECK: [[V]]", "CHECK").unwrap();
        assert!(
            check(&undefined, "x\n")
                .unwrap_err()
                .contains("before it is defined")
        );
    }
}
