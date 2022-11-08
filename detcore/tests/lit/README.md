# Lit Tests

This directory contains integration tests where a specific (or patterned)
stdout is expected. This is good for testing that a program's output is
deterministic.

These tests are run through LLVM's [`lit`][] tool. Please reference the
[`FileCheck`][] documentation to understand the directives that can be used
for checking on the output of tests.

[`lit`]: https://llvm.org/docs/CommandGuide/lit.html
[`FileCheck`]: https://llvm.org/docs/CommandGuide/FileCheck.html

## Test discovery

Buck will automatically discover new tests. Just add a new file and the test
will get compiled and executed automatically.

You can also use `generate-test.py` in the same directory as this README to
easily generate a new lit test:

    ./generate-test.py foobar.rs

This will generate a test called `foobar` with a `main.rs`.

## Running the tests

    buck test //hermetic_infra/detcore/tests/lit/...
