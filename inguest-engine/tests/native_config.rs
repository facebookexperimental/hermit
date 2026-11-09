// Existing native configuration controls exercise the exact production
// selection/decoder source in a normal libtest harness. This target loads no
// engine DSO, allocator, callback table, SDK/client or synthetic coordinator.
#[path = "../src/native/config.rs"]
mod native_config;
