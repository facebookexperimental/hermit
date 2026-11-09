// The Rust programs detcore/tests/lit's fixtures run as `%me`, built from
// detcore/tests/lit-guests for hermit-cli/tests/lit.rs. Keep the exact names
// shared by the producer (ci/manifest-plan/src/nextest_binaries.rs) and the
// consumer.
pub const LIT_GUEST_PACKAGE: &str = "hermit-lit-guests";
pub const LIT_GUEST_BINARIES: [&str; 21] = [
    "lit_chaos_schedule_replay",
    "lit_child_should_inherit_fds",
    "lit_close_on_exec",
    "lit_dup2_existing_fd",
    "lit_dup_should_create_a_valid_fd",
    "lit_fcntl_dupfd",
    "lit_fcntl_getfd_setfd",
    "lit_file_race_openwrite",
    "lit_file_write_race",
    "lit_fstat",
    "lit_hello_world_rs",
    "lit_no_close_on_exec",
    "lit_open_null_ptr",
    "lit_openat_lowest_fd",
    "lit_openat_next_lowest_fd",
    "lit_pipe_creates_valid_fds",
    "lit_print_race",
    "lit_read_badfd",
    "lit_sched_getaffinity",
    "lit_utime",
    "lit_utimes",
];
