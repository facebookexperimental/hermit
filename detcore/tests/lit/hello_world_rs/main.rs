// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// RUN: %hermit run -- %me | FileCheck %s
// CHECK: Hello world!

fn main() {
    println!("Hello world!");
}
