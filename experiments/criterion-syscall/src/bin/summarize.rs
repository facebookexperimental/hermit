/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use plotters::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct BenchmarkMetadata {
    group_id: String,
    function_id: Option<String>,
}

#[derive(Deserialize)]
struct Estimates {
    mean: Estimate,
    slope: Option<Estimate>,
}

#[derive(Clone, Deserialize)]
struct Estimate {
    confidence_interval: ConfidenceInterval,
    point_estimate: f64,
    standard_error: f64,
}

#[derive(Clone, Deserialize)]
struct ConfidenceInterval {
    confidence_level: f64,
    lower_bound: f64,
    upper_bound: f64,
}

#[derive(Clone)]
struct Row {
    syscall: String,
    backend: String,
    estimate: Estimate,
    statistic: &'static str,
    normalization_scale: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("criterion summary failed: {error:#}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let criterion_home = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/criterion"));
    let output = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("results/latest"));
    if args.next().is_some() {
        bail!("usage: summarize [CRITERION_HOME] [OUTPUT_DIR]");
    }
    if !criterion_home.is_dir() {
        bail!(
            "Criterion output directory does not exist: {}",
            criterion_home.display()
        );
    }
    fs::create_dir_all(&output)?;

    let mut rows = Vec::new();
    visit(&criterion_home, &mut |path| {
        if path.file_name().and_then(|name| name.to_str()) != Some("benchmark.json") {
            return Ok(());
        }
        if path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            != Some("new")
        {
            return Ok(());
        }
        let metadata: BenchmarkMetadata = read_json(path)?;
        let Some(syscall) = metadata.group_id.strip_prefix("marginal/") else {
            return Ok(());
        };
        let Some(backend) = metadata.function_id else {
            return Ok(());
        };
        let (backend, normalization_scale) = normalized_backend(backend)?;
        let estimates_path = path.with_file_name("estimates.json");
        let estimates: Estimates = read_json(&estimates_path)?;
        let (mut estimate, statistic) = match estimates.slope {
            Some(slope) => (slope, "slope"),
            None => (estimates.mean, "mean"),
        };
        normalize_estimate(&mut estimate, normalization_scale as f64);
        rows.push(Row {
            syscall: syscall.to_owned(),
            backend,
            estimate,
            statistic,
            normalization_scale,
        });
        Ok(())
    })?;
    if rows.is_empty() {
        bail!(
            "no marginal syscall estimates found below {}",
            criterion_home.display()
        );
    }
    rows.sort_by(|left, right| {
        left.syscall
            .cmp(&right.syscall)
            .then_with(|| backend_rank(&left.backend).cmp(&backend_rank(&right.backend)))
            .then_with(|| left.backend.cmp(&right.backend))
    });

    write_tsv(&output.join("summary.tsv"), &rows)?;
    write_markdown(&output.join("SUMMARY.md"), &criterion_home, &rows, &output)?;
    for (syscall, syscall_rows) in grouped(&rows) {
        draw_plot(
            &output.join(format!("{syscall}.svg")),
            syscall,
            &syscall_rows,
        )?;
    }
    copy_if_exists(
        &criterion_home.join("capabilities.tsv"),
        &output.join("capabilities.tsv"),
    )?;
    copy_if_exists(
        &criterion_home.join("fixed-counts.tsv"),
        &output.join("fixed-counts.tsv"),
    )?;

    println!("wrote {} estimates to {}", rows.len(), output.display());
    println!(
        "Criterion HTML: {}",
        criterion_home.join("report/index.html").display()
    );
    Ok(())
}

fn visit(directory: &Path, callback: &mut impl FnMut(&Path) -> Result<()>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            visit(&path, callback)?;
        } else {
            callback(&path)?;
        }
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    serde_json::from_reader(file).with_context(|| format!("parsing {}", path.display()))
}

fn write_tsv(path: &Path, rows: &[Row]) -> Result<()> {
    let mut output = BufWriter::new(File::create(path)?);
    writeln!(
        output,
        "syscall\tbackend\tstatistic\tnormalization_scale\tns_per_syscall\tci_lower_ns\tci_upper_ns\tconfidence\tstandard_error"
    )?;
    for row in rows {
        let interval = &row.estimate.confidence_interval;
        writeln!(
            output,
            "{}\t{}\t{}\t{}\t{:.6}\t{:.6}\t{:.6}\t{:.3}\t{:.6}",
            row.syscall,
            row.backend,
            row.statistic,
            row.normalization_scale,
            row.estimate.point_estimate,
            interval.lower_bound,
            interval.upper_bound,
            interval.confidence_level,
            row.estimate.standard_error
        )?;
    }
    Ok(())
}

fn write_markdown(
    path: &Path,
    criterion_home: &Path,
    rows: &[Row],
    output_directory: &Path,
) -> Result<()> {
    let mut output = BufWriter::new(File::create(path)?);
    writeln!(output, "# Marginal syscall cost")?;
    writeln!(output)?;
    writeln!(
        output,
        "Criterion linear-regression estimates. Units are nanoseconds per additional syscall; intervals are 95% confidence intervals."
    )?;
    writeln!(output)?;
    writeln!(
        output,
        "Full Criterion HTML: `{}`.",
        criterion_home.join("report/index.html").display()
    )?;
    writeln!(output)?;

    for (syscall, syscall_rows) in grouped(rows) {
        writeln!(output, "## `{syscall}`")?;
        writeln!(output)?;
        writeln!(output, "![{syscall} backend comparison]({syscall}.svg)")?;
        writeln!(output)?;
        writeln!(output, "| Backend | ns/syscall | 95% CI | Statistic |")?;
        writeln!(output, "| --- | ---: | ---: | --- |")?;
        for row in syscall_rows {
            let interval = &row.estimate.confidence_interval;
            writeln!(
                output,
                "| {} | {:.3} | {:.3}-{:.3} | {}{} |",
                row.backend,
                row.estimate.point_estimate,
                interval.lower_bound,
                interval.upper_bound,
                row.statistic,
                if row.normalization_scale == 1 {
                    String::new()
                } else {
                    format!(" / {} calls", row.normalization_scale)
                }
            )?;
        }
        writeln!(output)?;
    }
    writeln!(output, "Generated files: `{}`.", output_directory.display())?;
    Ok(())
}

fn draw_plot(path: &Path, syscall: &str, rows: &[&Row]) -> Result<()> {
    let positive: Vec<&Row> = rows
        .iter()
        .copied()
        .filter(|row| row.estimate.confidence_interval.lower_bound > 0.0)
        .collect();
    if positive.is_empty() {
        bail!("no positive confidence intervals for {syscall}");
    }
    let minimum = positive
        .iter()
        .map(|row| row.estimate.confidence_interval.lower_bound)
        .fold(f64::INFINITY, f64::min)
        / 1.5;
    let maximum = positive
        .iter()
        .map(|row| row.estimate.confidence_interval.upper_bound)
        .fold(f64::NEG_INFINITY, f64::max)
        * 1.5;
    let width = 1_200;
    let height = 700;
    let root = SVGBackend::new(path, (width, height)).into_drawing_area();
    root.fill(&WHITE)?;
    let count = positive.len();
    let mut chart = ChartBuilder::on(&root)
        .caption(
            format!("Marginal {syscall} cost by backend"),
            ("sans-serif", 28),
        )
        .margin(24)
        .x_label_area_size(150)
        .y_label_area_size(90)
        .build_cartesian_2d(0_usize..count, (minimum..maximum).log_scale())?;

    let names: Vec<&str> = positive.iter().map(|row| row.backend.as_str()).collect();
    chart
        .configure_mesh()
        .x_desc("Backend")
        .y_desc("Nanoseconds per syscall (log scale)")
        .x_labels(count)
        .x_label_formatter(&|value| names.get(*value).copied().unwrap_or_default().to_owned())
        .axis_desc_style(("sans-serif", 20))
        .label_style(("sans-serif", 15))
        .draw()?;

    for (index, row) in positive.iter().enumerate() {
        let x = index;
        let interval = &row.estimate.confidence_interval;
        let color = Palette99::pick(index).mix(0.9);
        chart.draw_series(std::iter::once(PathElement::new(
            vec![(x, interval.lower_bound), (x, interval.upper_bound)],
            ShapeStyle::from(&color).stroke_width(3),
        )))?;
        chart.draw_series(std::iter::once(Circle::new(
            (x, row.estimate.point_estimate),
            7,
            color.filled(),
        )))?;
    }
    root.present()?;
    Ok(())
}

fn grouped(rows: &[Row]) -> BTreeMap<&str, Vec<&Row>> {
    let mut groups: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for row in rows {
        groups.entry(&row.syscall).or_default().push(row);
    }
    groups
}

fn backend_rank(name: &str) -> usize {
    match name {
        "native" => 0,
        "gvisor-systrap" => 1,
        "gvisor-kvm" => 2,
        "reverie-ptrace" => 3,
        "reverie-dbi" => 4,
        "reverie-kvm" => 5,
        "reverie-sabre" => 6,
        _ => usize::MAX,
    }
}

fn normalized_backend(value: String) -> Result<(String, u64)> {
    let Some((backend, scale)) = value.rsplit_once("__scale_") else {
        return Ok((value, 1));
    };
    let scale: u64 = scale
        .parse()
        .with_context(|| format!("invalid normalization suffix in {value:?}"))?;
    if scale == 0 {
        bail!("normalization scale cannot be zero in {value:?}");
    }
    Ok((backend.to_owned(), scale))
}

fn normalize_estimate(estimate: &mut Estimate, scale: f64) {
    estimate.point_estimate /= scale;
    estimate.standard_error /= scale;
    estimate.confidence_interval.lower_bound /= scale;
    estimate.confidence_interval.upper_bound /= scale;
}

fn copy_if_exists(source: &Path, destination: &Path) -> Result<()> {
    if source.is_file() {
        fs::copy(source, destination)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scaled_backend_name() {
        assert_eq!(
            normalized_backend("reverie-kvm__scale_1000".to_owned()).unwrap(),
            ("reverie-kvm".to_owned(), 1_000)
        );
        assert_eq!(
            normalized_backend("native".to_owned()).unwrap(),
            ("native".to_owned(), 1)
        );
    }

    #[test]
    fn rejects_zero_normalization_scale() {
        assert!(normalized_backend("backend__scale_0".to_owned()).is_err());
    }
}
