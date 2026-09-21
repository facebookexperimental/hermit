#!/usr/bin/env -S rust-script --force
//! Produce a nextest configuration with every per-test wall bound scaled by
//! the machine-specific wall multiplier.
//!
//! ```cargo
//! [dependencies]
//! toml_edit = "0.22.27"
//! ```

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use toml_edit::value;
use toml_edit::Array;
use toml_edit::ArrayOfTables;
use toml_edit::DocumentMut;
use toml_edit::InlineTable;
use toml_edit::Item;
use toml_edit::Table;
use toml_edit::Value;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[allow(dead_code)]
#[path = "manifest-plan/src/timeouts.rs"]
mod timeouts;

const CPU_WRAPPER_BIN_ENV: &str = "HERMIT_NEXTEST_CPU_WRAPPER_BIN";
const CPU_WRAPPER_NAME: &str = "hermit-per-test-cpu";

fn usage() -> &'static str {
    "usage: nextest-timeout-config.rs SOURCE MULTIPLIER OUTPUT"
}

fn parse_period_seconds(period: &str) -> Result<u64, String> {
    let digits = period.strip_suffix('s').ok_or_else(|| {
        format!("slow-timeout.period must be a positive whole-second duration, got {period:?}")
    })?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "slow-timeout.period must be a positive whole-second duration, got {period:?}"
        ));
    }
    let seconds = digits
        .parse::<u64>()
        .map_err(|error| format!("invalid slow-timeout.period {period:?}: {error}"))?;
    if seconds == 0 {
        return Err("slow-timeout.period must be greater than zero".into());
    }
    Ok(seconds)
}

fn scale_period_value(period: &mut Value, multiplier: f64) -> Result<(), String> {
    let source = period
        .as_str()
        .ok_or_else(|| "slow-timeout.period must be a quoted duration string".to_string())?;
    let base_seconds = parse_period_seconds(source)?;
    let scaled = timeouts::scale_timeout_seconds(base_seconds, multiplier, "wall multiplier")?;
    *period = Value::from(format!("{scaled}s"));
    Ok(())
}

fn scale_inline_timeout(table: &mut InlineTable, multiplier: f64) -> Result<(), String> {
    let period = table
        .get_mut("period")
        .ok_or_else(|| "slow-timeout table is missing period".to_string())?;
    scale_period_value(period, multiplier)
}

fn scale_table_timeout(table: &mut Table, multiplier: f64) -> Result<(), String> {
    let period = table
        .get_mut("period")
        .ok_or_else(|| "slow-timeout table is missing period".to_string())?
        .as_value_mut()
        .ok_or_else(|| "slow-timeout.period must be a scalar value".to_string())?;
    scale_period_value(period, multiplier)
}

fn scale_timeout_item(item: &mut Item, multiplier: f64) -> Result<(), String> {
    match item {
        Item::Value(Value::InlineTable(table)) => scale_inline_timeout(table, multiplier),
        Item::Table(table) => scale_table_timeout(table, multiplier),
        _ => Err("slow-timeout must be a TOML table".into()),
    }
}

fn visit_table(table: &mut Table, multiplier: f64, scaled: &mut usize) -> Result<(), String> {
    for (key, item) in table.iter_mut() {
        if key == "slow-timeout" {
            scale_timeout_item(item, multiplier)?;
            *scaled += 1;
        } else {
            visit_item(item, multiplier, scaled)?;
        }
    }
    Ok(())
}

fn visit_array_of_tables(
    tables: &mut ArrayOfTables,
    multiplier: f64,
    scaled: &mut usize,
) -> Result<(), String> {
    for table in tables.iter_mut() {
        visit_table(table, multiplier, scaled)?;
    }
    Ok(())
}

fn visit_item(item: &mut Item, multiplier: f64, scaled: &mut usize) -> Result<(), String> {
    match item {
        Item::Table(table) => visit_table(table, multiplier, scaled),
        Item::ArrayOfTables(tables) => visit_array_of_tables(tables, multiplier, scaled),
        Item::None | Item::Value(_) => Ok(()),
    }
}

fn run_wrapper_count(document: &DocumentMut, profile: &str) -> Result<usize, String> {
    let Some(profile_table) = document
        .get("profile")
        .and_then(Item::as_table)
        .and_then(|profiles| profiles.get(profile))
        .and_then(Item::as_table)
    else {
        return Ok(0);
    };
    let Some(scripts) = profile_table.get("scripts") else {
        return Ok(0);
    };
    let scripts = scripts
        .as_array_of_tables()
        .ok_or_else(|| format!("profile.{profile}.scripts must be an array of tables"))?;
    Ok(scripts
        .iter()
        .filter(|script| script.contains_key("run-wrapper"))
        .count())
}

fn add_cpu_wrapper(document: &mut DocumentMut, wrapper_bin: &Path) -> Result<(), String> {
    if !wrapper_bin.is_absolute() {
        return Err("nextest CPU wrapper executable must be absolute".into());
    }
    for profile in ["default", "ci"] {
        let count = run_wrapper_count(document, profile)?;
        if count != 0 {
            return Err(format!(
                "profile.{profile} already defines {count} run-wrapper rule(s); refusing to make per-test CPU measurement partial or ambiguous"
            ));
        }
    }

    let experimental = document
        .entry("experimental")
        .or_insert_with(|| Item::Value(Value::Array(Array::new())))
        .as_array_mut()
        .ok_or_else(|| "experimental must be an array".to_string())?;
    let wrapper_feature_count = experimental
        .iter()
        .filter(|value| value.as_str() == Some("wrapper-scripts"))
        .count();
    if wrapper_feature_count > 1 {
        return Err("experimental contains wrapper-scripts more than once".into());
    }
    if wrapper_feature_count == 0 {
        experimental.push("wrapper-scripts");
    }

    if document
        .get("scripts")
        .and_then(Item::as_table)
        .and_then(|scripts| scripts.get("wrapper"))
        .and_then(Item::as_table)
        .is_some_and(|wrappers| wrappers.contains_key(CPU_WRAPPER_NAME))
    {
        return Err(format!(
            "scripts.wrapper.{CPU_WRAPPER_NAME} is already defined"
        ));
    }
    let wrapper_path = wrapper_bin
        .to_str()
        .ok_or_else(|| "nextest CPU wrapper executable path is not UTF-8".to_string())?;
    let mut command = InlineTable::new();
    // An argv array preserves spaces and shell punctuation in a target path.
    let mut argv = Array::new();
    argv.push(wrapper_path);
    command.insert("command-line", Value::Array(argv));
    command.insert("relative-to", Value::from("none"));
    let scripts = document
        .entry("scripts")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| "scripts must be a table".to_string())?;
    let wrappers = scripts
        .entry("wrapper")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| "scripts.wrapper must be a table".to_string())?;
    let mut wrapper = Table::new();
    wrapper["command"] = value(command);
    wrapper["target-runner"] = value("within-wrapper");
    wrappers.insert(CPU_WRAPPER_NAME, Item::Table(wrapper));

    let scripts = &mut document["profile"]["default"]["scripts"];
    if scripts.is_none() {
        *scripts = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let scripts = scripts
        .as_array_of_tables_mut()
        .ok_or_else(|| "profile.default.scripts must be an array of tables".to_string())?;
    let mut binding = Table::new();
    binding["filter"] = value("all()");
    binding["run-wrapper"] = value(CPU_WRAPPER_NAME);
    scripts.push(binding);

    if run_wrapper_count(document, "default")? != 1
        || run_wrapper_count(document, "ci")? != 0
    {
        return Err(
            "generated nextest config does not define exactly one inherited run wrapper for default and ci"
                .into(),
        );
    }
    Ok(())
}

fn scaled_config(
    source: &str,
    multiplier: f64,
    cpu_wrapper: Option<&Path>,
) -> Result<String, String> {
    timeouts::validate_timeout_multiplier(multiplier, "wall multiplier")?;
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| format!("cannot parse nextest TOML: {error}"))?;
    let mut scaled = 0;
    visit_table(document.as_table_mut(), multiplier, &mut scaled)?;
    if scaled == 0 {
        return Err("nextest config contains no slow-timeout.period values".into());
    }
    if let Some(wrapper_bin) = cpu_wrapper {
        add_cpu_wrapper(&mut document, wrapper_bin)?;
    }
    Ok(document.to_string())
}

fn cpu_wrapper_from_env() -> Option<std::path::PathBuf> {
    env::var_os(CPU_WRAPPER_BIN_ENV).map(Into::into)
}

fn run() -> Result<(), String> {
    let mut args = env::args_os();
    let _program = args.next();
    let source = args.next().ok_or_else(|| usage().to_string())?;
    let multiplier = args
        .next()
        .ok_or_else(|| usage().to_string())?
        .into_string()
        .map_err(|_| "MULTIPLIER must be UTF-8".to_string())?
        .parse::<f64>()
        .map_err(|error| format!("invalid wall multiplier: {error}"))?;
    let output = args.next().ok_or_else(|| usage().to_string())?;
    if args.next().is_some() {
        return Err(usage().into());
    }
    let source_text = fs::read_to_string(Path::new(&source))
        .map_err(|error| format!("cannot read {}: {error}", Path::new(&source).display()))?;
    let cpu_wrapper = cpu_wrapper_from_env();
    let rendered = scaled_config(
        &source_text,
        multiplier,
        cpu_wrapper.as_deref(),
    )?;
    fs::write(Path::new(&output), rendered)
        .map_err(|error| format!("cannot write {}: {error}", Path::new(&output).display()))
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nextest-timeout-config: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_every_profile_and_override_with_ceil_rounding() {
        let source = concat!(
            "# retained comment\n",
            "[profile.default]\n",
            "slow-timeout = { period = \"57s\", terminate-after = 1, grace-period = \"2s\" }\n",
            "unrelated = \"kept\"\n",
            "[profile.ci]\n",
            "slow-timeout = { period = \"10s\", terminate-after = 2 }\n",
            "[[profile.ci.overrides]]\n",
            "filter = \"test(example)\"\n",
            "slow-timeout = { period = \"3s\", terminate-after = 1 }\n",
        );
        let rendered = scaled_config(source, 1.25, None).unwrap();
        assert!(rendered.contains("# retained comment"));
        assert!(rendered.contains("unrelated = \"kept\""));
        assert!(rendered.contains("filter = \"test(example)\""));
        assert!(rendered.contains("period = \"72s\""));
        assert!(rendered.contains("period = \"13s\""));
        assert!(rendered.contains("period = \"4s\""));
        assert_eq!(rendered.matches("slow-timeout").count(), 3);
    }

    #[test]
    fn refuses_malformed_or_missing_periods() {
        for period in ["0s", "1.5s", "5m", "-1s", "s"] {
            let source = format!(
                "[profile.default]\nslow-timeout = {{ period = {period:?}, terminate-after = 1 }}\n"
            );
            assert!(
                scaled_config(&source, 1.0, None).is_err(),
                "accepted {period}"
            );
        }
        assert!(
            scaled_config(
                "[profile.default]\nslow-timeout = { terminate-after = 1 }\n",
                1.0,
                None,
            )
            .unwrap_err()
            .contains("missing period")
        );
        assert!(
            scaled_config("[profile.default]\nfail-fast = false\n", 1.0, None)
                .unwrap_err()
                .contains("no slow-timeout.period")
        );
    }

    #[test]
    fn refuses_nonpositive_or_nonfinite_multiplier() {
        let source =
            "[profile.default]\nslow-timeout = { period = \"57s\", terminate-after = 1 }\n";
        for multiplier in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(scaled_config(source, multiplier, None).is_err());
        }
    }

    #[test]
    fn adds_one_default_wrapper_inherited_by_ci_without_changing_wall_scaling() {
        let source = concat!(
            "nextest-version = \"0.9.100\"\n",
            "[profile.default]\n",
            "slow-timeout = { period = \"57s\", terminate-after = 1, grace-period = \"2s\" }\n",
            "[profile.ci]\n",
            "status-level = \"slow\"\n",
        );
        let rendered = scaled_config(
            source,
            1.5,
            Some(Path::new("/tmp/wrapper")),
        )
        .unwrap();
        let document = rendered.parse::<DocumentMut>().unwrap();
        assert_eq!(run_wrapper_count(&document, "default").unwrap(), 1);
        assert_eq!(run_wrapper_count(&document, "ci").unwrap(), 0);
        assert_eq!(rendered.matches("run-wrapper").count(), 1);
        assert!(rendered.contains("period = \"86s\""));
        assert!(rendered.contains("wrapper-scripts"));
        assert!(rendered.contains("target-runner = \"within-wrapper\""));
    }

    #[test]
    fn refuses_a_second_default_or_ci_run_wrapper() {
        for profile in ["default", "ci"] {
            let source = format!(
                "[profile.default]\nslow-timeout = {{ period = \"57s\" }}\n[[profile.{profile}.scripts]]\nfilter = \"all()\"\nrun-wrapper = \"other\"\n"
            );
            let error = scaled_config(
                &source,
                1.0,
                Some(Path::new("/tmp/wrapper")),
            )
            .unwrap_err();
            assert!(error.contains(&format!("profile.{profile} already defines")));
        }
    }

    #[test]
    fn wrapper_path_is_one_literal_argument() {
        let path = "/tmp/a path/it's-$(literal)-wrapper";
        let rendered = scaled_config(
            "[profile.default]\nslow-timeout = { period = \"57s\" }\n",
            1.0, Some(Path::new(path)),
        ).unwrap();
        let document = rendered.parse::<DocumentMut>().unwrap();
        let argv = document["scripts"]["wrapper"][CPU_WRAPPER_NAME]["command"]["command-line"].as_array().unwrap();
        assert_eq!(argv.len(), 1);
        assert_eq!(argv.get(0).unwrap().as_str(), Some(path));
    }
}
