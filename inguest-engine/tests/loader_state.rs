// Compile the exact transport/state implementation in an ordinary test harness,
// without loading the engine DSO or replacing libtest's allocator/TLS owner.
#[path = "../src/loader_state.rs"]
mod loader_state;
