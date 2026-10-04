# Directory actions

Treat a data-bearing directory as an object and co-locate its action scripts as
the methods that inspect or transform that data:

- `tests/` owns test-corpus actions such as `tests/manifest-cli.rs`; callers
  should not put test-manifest-specific tools in `scripts/`.
- `scripts/` contains only repository-wide utilities and shared CLI support.
- `debug/` and `experiments/` data belongs in the outer `dev-hermit` workspace,
  where each directory owns the actions that operate on its stored sessions or
  experiment artifacts.

Keep reusable product logic in normal Rust crates. A directory action may call
that logic, but should remain a small interface over the directory's on-disk
schema.

Follow-on: if the outer workspace's debug session and automatic experiment
tracking actions become repository-independent, generalize them in
`agent-utils/` rather than copying them into Hermit.
