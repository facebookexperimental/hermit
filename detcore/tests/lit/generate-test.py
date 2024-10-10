#!/usr/bin/env python3
# @lint-ignore-every LICENSELINT
#
#  Copyright (c) Meta Platforms, Inc. and affiliates.
#  All rights reserved.
#
#  This source code is licensed under the BSD-style license found in the
#  LICENSE file in the root directory of this source tree.
#

"""
This program helps you quickly generate a new lit test.

Usage: ./generate-test.py foobar.rs

This example will generate a test called `foobar` with a `main.rs`.
"""

import argparse
from pathlib import Path

RUST_TEMPLATE = """\
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me | FileCheck %s
// CHECK: Hello world!

fn main() {
    println!("Hello world!");
}
"""

C_TEMPLATE = """\
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me | FileCheck %s
// CHECK: Hello world!

#include <stdio.h>

int main(int argc, char* argv[]) {
  printf("Hello world!\\n");
  return 0;
}
"""

GO_TEMPLATE = """\
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me | FileCheck %s
// CHECK: Hello world!
package main

import "fmt"

func main() {
\tfmt.Println("Hello world!")
}
"""

HERMIT_RUN_LIT_TEST = """\
RUN: %hermit run --no-sequentialize-threads --no-deterministic-io -- %me | FileCheck %s
CHECK: Hello world!
"""

HERMIT_RUN_STRICT_LIT_TEST = """\
RUN: %hermit run -- %me | FileCheck %s
CHECK: Hello world!
"""

HERMIT_RUN_STRICT_VERIFY_LIT_TEST = """\
RUN: %hermit run --verify -- %me
"""


def parse_args():
    parser = argparse.ArgumentParser(description="Generates a new lit test.")
    parser.add_argument("name", type=Path, help="The name of the test.")
    parser.add_argument(
        "--lang",
        choices=["rust", "c", "go"],
        default="rust",
        help="The language to use for the test. If not specified, will default to Rust.",
    )
    return parser.parse_args()


def yes_no(message):
    """
    Prompts the user for a boolean.
    """
    while True:
        s = input(f"{message}? (Y/n): ").lower()
        if not s or s == "y":
            return True
        if s == "n":
            return False

        print("Invalid input. Lets try that again...")


def gen_test(name, lang):
    try:
        name.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        print(f"The test '{name}' already exists.")
        return

    if lang == "rust":
        with (name / "main.rs").open("w") as f:
            f.write(RUST_TEMPLATE)
    elif lang == "c":
        with (name / "main.c").open("w") as f:
            f.write(C_TEMPLATE)
    elif lang == "go":
        with (name / "main.go").open("w") as f:
            f.write(GO_TEMPLATE)

    lit_tests = []

    if yes_no(
        "Generate lit test for `hermit run --no-sequentialize-threads --no-deterministic-io`"
    ):
        with (name / "hermit-run.lit").open("w") as f:
            f.write(HERMIT_RUN_LIT_TEST)
        lit_tests.append("hermit-run")

    if yes_no("Generate lit test for `hermit run`"):
        with (name / "hermit-run-strict.lit").open("w") as f:
            f.write(HERMIT_RUN_STRICT_LIT_TEST)
        lit_tests.append("hermit-run-strict")

        # Doesn't make sense to use --verify unless we're also running with
        # `--strict`.
        if yes_no("Generate lit test for `hermit run --verify`"):
            with (name / "hermit-run-strict-verify.lit").open("w") as f:
                f.write(HERMIT_RUN_STRICT_VERIFY_LIT_TEST)
            lit_tests.append("hermit-run-strict-verify")

    print("")
    print(f"Generated a new test suite in '{name}'! You can run these tests with:")
    print("")
    print(f"    buck test @mode/dev-nosan :{name}")

    for t in lit_tests:
        print(f"    buck test @mode/dev-nosan :{name}-{t}")

    print("")


def main():
    args = parse_args()

    lang = args.lang
    name = Path(args.name.stem)
    ext = args.name.suffix

    if ext == ".rs":
        lang = "rust"
    elif ext == ".c":
        lang = "c"
    elif ext == ".go":
        lang = "go"

    gen_test(name, lang)


if __name__ == "__main__":
    main()
