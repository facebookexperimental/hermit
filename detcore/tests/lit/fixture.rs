/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Cargo entry point for Rust programs used by the Detcore lit tests.

macro_rules! fixture {
    ($module:ident, $path:literal) => {
        mod $module {
            include!($path);

            pub fn run() {
                main();
            }
        }
    };
}

fixture!(child_should_inherit_fds, "child_should_inherit_fds/main.rs");
fixture!(close_on_exec, "close_on_exec/main.rs");
fixture!(dup2_existing_fd, "dup2_existing_fd/main.rs");
fixture!(
    dup_should_create_a_valid_fd,
    "dup_should_create_a_valid_fd/main.rs"
);
fixture!(fcntl_dupfd, "fcntl_dupfd/main.rs");
fixture!(fcntl_getfd_setfd, "fcntl_getfd_setfd/main.rs");
fixture!(file_race_openwrite, "file_race_openwrite/main.rs");
fixture!(file_write_race, "file_write_race/main.rs");
fixture!(fstat, "fstat/main.rs");
fixture!(hello_world_rs, "hello_world_rs/main.rs");
fixture!(no_close_on_exec, "no_close_on_exec/main.rs");
fixture!(open_null_ptr, "open_null_ptr/main.rs");
fixture!(openat_lowest_fd, "openat_lowest_fd/main.rs");
fixture!(openat_next_lowest_fd, "openat_next_lowest_fd/main.rs");
fixture!(pipe_creates_valid_fds, "pipe_creates_valid_fds/main.rs");
fixture!(print_race, "print_race/main.rs");
fixture!(read_badfd, "read_badfd/main.rs");
fixture!(sched_getaffinity, "sched_getaffinity/main.rs");
fixture!(utime, "utime/main.rs");
fixture!(utimes, "utimes/main.rs");

fn main() {
    let fixture = std::env::var("HERMIT_LIT_FIXTURE")
        .expect("HERMIT_LIT_FIXTURE must name the lit fixture to run");

    match fixture.as_str() {
        "child_should_inherit_fds" => child_should_inherit_fds::run(),
        "close_on_exec" => close_on_exec::run(),
        "dup2_existing_fd" => dup2_existing_fd::run(),
        "dup_should_create_a_valid_fd" => dup_should_create_a_valid_fd::run(),
        "fcntl_dupfd" => fcntl_dupfd::run(),
        "fcntl_getfd_setfd" => fcntl_getfd_setfd::run(),
        "file_race_openwrite" => file_race_openwrite::run(),
        "file_write_race" => file_write_race::run(),
        "fstat" => fstat::run(),
        "hello_world_rs" => hello_world_rs::run(),
        "no_close_on_exec" => no_close_on_exec::run(),
        "open_null_ptr" => open_null_ptr::run(),
        "openat_lowest_fd" => openat_lowest_fd::run(),
        "openat_next_lowest_fd" => openat_next_lowest_fd::run(),
        "pipe_creates_valid_fds" => pipe_creates_valid_fds::run(),
        "print_race" => print_race::run(),
        "read_badfd" => read_badfd::run(),
        "sched_getaffinity" => sched_getaffinity::run(),
        "utime" => utime::run(),
        "utimes" => utimes::run(),
        other => panic!("unknown Detcore lit fixture: {other}"),
    }
}
