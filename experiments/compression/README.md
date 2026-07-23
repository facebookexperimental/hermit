# Compression Determinism Experiment

This experiment exercises system `bzip2`, `gzip`, and `bzip2recover` under
strict Hermit. It generates a fixed multi-block text corpus, compresses and
decompresses it with both codecs three times, and requires SHA-256-identical
compressed output across every run.

`gzip` runs with `-n` to omit the original file name and timestamp from its
format header. This isolates compression and Hermit execution determinism from
deliberately variable archive metadata.

## Requirements

- x86-64 Linux with the Hermit runtime prerequisites
- system `bzip2`, `bzip2recover`, and `gzip`
- `awk`, `cmp`, and `sha256sum`

## Run

From the repository root:

```bash
experiments/compression/run.sh
```

The runner builds release Hermit when `HERMIT_BIN` is not already executable.
Override its artifact directory or input size with environment variables:

```bash
HERMIT_BIN=target/debug/hermit \
COMPRESSION_ARTIFACT_ROOT=target/compression-local \
COMPRESSION_INPUT_LINES=12000 \
  experiments/compression/run.sh
```

The corpus must remain large enough for `bzip2recover` to find at least two
bzip2 blocks. The default 24,000 lines produce roughly 2 MiB of input.

## Evidence

Each invocation creates `target/compression/evidence-<UTC timestamp>/` with:

- the generated input and its SHA-256;
- per-run compressed and decompressed files;
- per-run `bzip2recover` output and recovered-block manifests;
- `results.tsv`, `metadata.txt`, and `summary.txt`.

The command exits successfully only if all decompressed data matches the input
and the bzip2, gzip, and recovered-block SHA-256 values match across three runs.
