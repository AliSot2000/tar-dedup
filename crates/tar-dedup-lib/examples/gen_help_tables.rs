//! Emit JSON rows of every CLI option, grouped by subcommand + `help_heading`,
//! for the Readme table generator (`scripts/gen_readme.py`).
//!
//! The output is the single source of truth for the option tables: `description`
//! is the clap help text verbatim (doc comments joined onto one line), so the
//! Readme can never drift from `--help`.

use clap::CommandFactory;
use clap::builder::Arg;

use tar_dedup_lib::cli::Cli;

fn row_json(arg: &Arg) -> serde_json::Value {
    let help = arg
        .get_help()
        .map(|h| h.to_string())
        .unwrap_or_default();
    // Join multi-line doc comments onto one line (single cell line).
    let description = help
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    serde_json::json!({
        "short": arg.get_short().map(|c| format!("-{c}")),
        "long": arg.get_long().map(|l| format!("--{l}")),
        "required": arg.is_required_set(),
        "args": arg_range(arg),
        "description": description,
    })
}

/// Human-readable value range for the Args column, e.g. `0`, `1`, `0..1`, `1+`.
fn arg_range(arg: &Arg) -> String {
    if !arg.get_action().takes_values() {
        return "0".into();
    }
    match arg.get_num_args() {
        Some(r) => {
            let (min, max) = (r.min_values(), r.max_values());
            if max == usize::MAX {
                format!("{min}+")
            } else if min == max {
                format!("{min}")
            } else {
                format!("{min}..{max}")
            }
        }
        // No explicit num_args: infer 1 (`Option<T>`) / 1+ (repeatable).
        None => match arg.get_action() {
            clap::ArgAction::Append => "1+".into(),
            _ => "1".into(),
        },
    }
}

fn main() {
    let cli = Cli::command();
    let mut out = serde_json::Map::new();
    for sub in cli.get_subcommands() {
        out.insert(sub.get_name().to_string(), serde_json::json!(collect(sub, &cli)));
    }
    println!("{}", serde_json::to_string_pretty(&serde_json::Value::Object(out)).unwrap());
}

/// Group `sub`'s arguments by `help_heading`; `None` becomes `Options`.
fn collect(sub: &clap::Command, cli: &clap::Command) -> Vec<serde_json::Value> {
    let mut groups: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();

    let mut args: Vec<&Arg> = sub.get_arguments().collect();
    // The top-level global `--log-level` only renders in the archive tables
    // (see Readme layout); attribute it there.
    if sub.get_name() == "archive" {
        args.extend(cli.get_arguments());
    }

    for arg in args {
        let heading = arg
            .get_help_heading()
            .unwrap_or("Options")
            .to_string();
        groups
            .entry(heading)
            .or_default()
            .push(row_json(arg));
    }

    groups
        .into_iter()
        .map(|(heading, rows)| serde_json::json!({ "heading": heading, "rows": rows }))
        .collect()
}