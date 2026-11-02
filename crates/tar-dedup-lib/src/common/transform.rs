//! GNU tar `--transform` / `--xform` sed-subset parser and applier.
//!
//! Mirrors the store-only model of `--mode`: the expression is validated at
//! config time and stored in meta at archive time; it is applied on extract in
//! `place_prologue`, before `--strip-components`.
//!
//! The pattern uses the `regex` crate (linear-time, consistent with `filter.rs`).
//! GNU BRE pattern backrefs are unsupported on purpose; replacement backrefs are
//! fine and are translated for the crate (`&` -> `$0`, `\1..\9` -> `$1..$9`).

use crate::error::{Error, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Where the path transform expression comes from on extract (mirrors `ModeSource`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TransformSource {
    /// No name transformation.
    #[default]
    None,
    /// Apply the expression recorded in the archive.
    Stored,
    /// Apply an explicitly provided sed expression (validated on the CLI).
    Cli(String),
}

/// A single `s` substitution step, applied in order like sed.
#[derive(Debug, Clone)]
struct TransformStep {
    re: Regex,
    replace: String,
    global: bool,
}

/// A parsed GNU tar `--transform` expression: ordered `s` clauses.
#[derive(Debug, Clone)]
pub struct TransformExpr {
    steps: Vec<TransformStep>,
}

impl TransformExpr {
    /// Apply the transform to a member name.
    pub fn apply(&self, member: &str) -> String {
        let mut out = member.to_string();
        for step in &self.steps {
            out = if step.global {
                step.re.replace_all(&out, step.replace.as_str()).into_owned()
            } else {
                step.re.replace(&out, step.replace.as_str()).into_owned()
            };
        }
        out
    }
}

/// Parse a GNU tar `--transform` string into a reusable expression.
pub fn parse_transform_expr(expr: &str) -> Result<TransformExpr> {
    let mut steps = Vec::new();
    for clause in expr.split(';') {
        let clause = clause.trim();
        if clause.is_empty() {
            continue;
        }
        steps.push(parse_clause(clause)?);
    }
    if steps.is_empty() {
        return Err(Error::Config(format!(
            "invalid --transform `{expr}`: expected at least one `s/DELIM...` clause"
        )));
    }
    Ok(TransformExpr { steps })
}

fn parse_clause(clause: &str) -> Result<TransformStep> {
    let bytes = clause.as_bytes();
    if bytes.first() != Some(&b's') {
        return Err(Error::Config(format!(
            "invalid --transform clause `{clause}`: only `s/DELIM` substitution is supported"
        )));
    }
    if bytes.len() < 3 {
        return Err(Error::Config(format!(
            "invalid --transform clause `{clause}`: missing delimiter"
        )));
    }
    let delim = bytes[1];

    let pat_end = find_unescaped(bytes, delim, 2).ok_or_else(|| {
        Error::Config(format!(
            "invalid --transform clause `{clause}`: unterminated pattern"
        ))
    })?;
    let repl_end = find_unescaped(bytes, delim, pat_end + 1).ok_or_else(|| {
        Error::Config(format!(
            "invalid --transform clause `{clause}`: unterminated replacement"
        ))
    })?;

    let pattern = &clause[2..pat_end];
    let replacement = translate_replacement(&clause[pat_end + 1..repl_end]);
    let flags = &clause[repl_end + 1..];

    let mut global = false;
    let mut case_insensitive = false;
    for flag in flags.chars() {
        match flag {
            'g' => global = true,
            'i' => case_insensitive = true,
            // regex-crate is already extended-style; GNU BRE/ERE distinction is moot.
            'x' => {}
            other => {
                return Err(Error::Config(format!(
                    "invalid --transform flag `{other}` in clause `{clause}` (supported: g, i, x)"
                )));
            }
        }
    }
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| Error::Config(format!("invalid --transform regex `{pattern}`: {e}")))?;

    Ok(TransformStep { re, replace: replacement, global })
}

/// Find the next delimiter byte that is not preceded by an odd run of backslashes.
fn find_unescaped(bytes: &[u8], delim: u8, start: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == delim {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Translate a GNU sed replacement into `regex`-crate replacement syntax.
fn translate_replacement(repl: &str) -> String {
    let mut out = String::with_capacity(repl.len());
    let mut chars = repl.chars();
    while let Some(c) = chars.next() {
        match c {
            '&' => out.push_str("$0"),
            '\\' => match chars.next() {
                Some('&') => out.push('&'),
                Some('\\') => out.push('\\'),
                Some(d @ '1'..='9') => {
                    out.push('$');
                    out.push(d);
                }
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            // `$` is literal in sed; escape it so the crate does not treat it
            // as a capture-group reference.
            '$' => out.push_str("$$"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_applies_first_match_only_by_default() {
        let t = parse_transform_expr("s/a/b/").unwrap();
        assert_eq!(t.apply("banana"), "bbnana");
    }

    #[test]
    fn parse_g_flag_replaces_all() {
        let t = parse_transform_expr("s/a/b/g").unwrap();
        assert_eq!(t.apply("banana"), "bbnbnb");
    }

    #[test]
    fn parse_i_flag_is_case_insensitive() {
        let t = parse_transform_expr("s/FOO/bar/i").unwrap();
        assert_eq!(t.apply("foo"), "bar");
    }

    #[test]
    fn parse_ampersand_is_whole_match() {
        let t = parse_transform_expr("s/x/<&>/").unwrap();
        assert_eq!(t.apply("axb"), "a<x>b");
    }

    #[test]
    fn parse_replacement_backrefs() {
        // BRE-style `\(` grouping is NOT supported by regex-crate; use ERE groups.
        let t = parse_transform_expr(r"s/(foo)/(\1)/").unwrap();
        assert_eq!(t.apply("foo"), "(foo)");
    }

    #[test]
    fn parse_multiple_clauses_apply_in_order() {
        let t = parse_transform_expr("s/a/x/;s/b/y/").unwrap();
        assert_eq!(t.apply("ab"), "xy");
    }

    #[test]
    fn parse_custom_delimiter() {
        let t = parse_transform_expr("s,^usr/,var/,").unwrap();
        assert_eq!(t.apply("usr/lib"), "var/lib");
        assert_eq!(t.apply("etc"), "etc");
    }

    #[test]
    fn parse_errors_on_bad_input() {
        assert!(parse_transform_expr("").is_err());
        assert!(parse_transform_expr("y/a/b/").is_err());
        assert!(parse_transform_expr("s/a/b/2").is_err()); // number flag unsupported
        assert!(parse_transform_expr("s/a/b/q").is_err()); // unknown flag
        assert!(parse_transform_expr("s/a/b").is_err()); // unterminated replacement
    }

    #[test]
    fn translate_replacement_escapes_dollar() {
        assert_eq!(translate_replacement("price $5"), "price $$5");
        assert_eq!(translate_replacement("a&b"), "a$0b");
        assert_eq!(translate_replacement(r"\1"), "$1");
        assert_eq!(translate_replacement(r"\&"), "&");
    }
}
