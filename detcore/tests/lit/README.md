# Lit Tests

This directory contains integration tests where a specific (or patterned)
stdout is expected. This is good for testing that a program's output is
deterministic.

Buck runs these tests through LLVM's [`lit`][] tool. Cargo uses the compatible
runner in `cargo.rs` and the Rust [`litcheck-filecheck`][] implementation.
Please reference the [`FileCheck`][] documentation to understand the
directives that can be used for checking test output.

[`lit`]: https://llvm.org/docs/CommandGuide/lit.html
[`FileCheck`]: https://llvm.org/docs/CommandGuide/FileCheck.html
[`litcheck-filecheck`]: https://crates.io/crates/litcheck-filecheck

## Test discovery

Buck will automatically discover new tests. Just add a new file and the test
will get compiled and executed automatically.

You can also use `generate-test.py` in the same directory as this README to
easily generate a new lit test:

    ./generate-test.py foobar.rs

This will generate a test called `foobar` with a `main.rs`.

## Running the tests

Cargo runs the hardware-independent tests by default. PMU-dependent strict,
chaos, and verify cases are ignored unless explicitly requested:

    cargo test -p hermit --test detcore_lit
    cargo test -p hermit --test detcore_lit -- --ignored --test-threads=1

Each Cargo test gets its own working directory and `TMPDIR`, so tests that
bind or preserve `/tmp` do not share state.

Buck continues to use LLVM lit directly:

    buck test //hermetic_infra/detcore/tests/lit/...
