---
name: presenting-quantitative-data
description: "Present benchmark, coverage, reliability, and performance numbers so readers can audit every claim. Use when writing tables, ratios, percentages, scorecards, charts, experiment summaries, or quantitative comparisons."
---

# Presenting Quantitative Data

Make every number traceable to a population, measurement, and primary source.
Prefer compact tables backed by durable raw artifacts over unsupported prose.

## Anchor Every Result

- Give ratios as `numerator / denominator` before or alongside percentages.
  Define the denominator, including exclusions, failures, and missing rows.
- Bind results to an immutable measurement anchor: run ID, full source SHA in
  metadata, and a linked CSV/TSV/JSON artifact. Never combine numbers from
  different anchors without labeling the join.
- Define every column, unit, baseline, aggregation, and status value. State
  whether a value is absolute, normalized, cumulative, or a delta.
- Link primary sources: raw result rows, metadata, producing script, and pinned
  code or documentation. A narrative summary is not a primary source.

## Report With Honest Precision

- Show sample count and the chosen statistic. Preserve failed and timed-out
  samples as statuses; do not silently remove them from the denominator.
- Use only as many significant figures as the measurement supports. Two or
  three significant figures are usually enough for timings and slowdowns.
  Avoid precision created only by a calculator.
- Pair normalized ratios with representative absolute measurements. A `2.0x`
  change is uninterpretable without the baseline scale and unit.
- Keep measurement, derived value, and interpretation visibly separate.
  Label causal explanations as hypotheses unless the experiment isolates the
  mechanism.

## Enable Reader Self-Audit

For each headline number, a reader should be able to recover:

1. the exact source rows and run anchor;
2. the numerator, denominator, formula, unit, and baseline;
3. the sample count and treatment of failures or exclusions;
4. the measurement context: host class, workload, configuration, and relevant
   constraints;
5. the primary-source links needed to recompute the claim.

Before publishing, recompute summaries from the raw artifact and spot-check at
least one row by hand. If the data cannot support that audit, weaken or remove
the claim.
