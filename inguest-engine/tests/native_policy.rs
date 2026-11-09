// Exercise the exact production helpers through an ordinary libtest ELF.
// No shared-engine DSO allocator, constructor, ToolHost or native Guest is
// instantiated by this target.
#[path = "../src/native/policy.rs"]
mod native_policy;
