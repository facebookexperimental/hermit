//! Combined content identity for the declared native Detcore/runtime inputs.
//!
//! This is compiled input binding only. It does not replace the artifact
//! producer, native kernel/namespace admission or actual runtime controls.

use sha2::Digest;
use sha2::Sha256;

/// Combine independently compiled RV and Hermit native inputs identically in
/// the actual coordinator and the actual private runtime consumer.
pub fn digest(reverie_digest: [u8; 32]) -> [u8; 32] {
    let inputs: &[(&str, &[u8])] = &[
        (
            "h/model/native_sources.rs",
            include_bytes!("liteinst_native_exec_sources.rs"),
        ),
        (
            "h/model/native_input.rs",
            include_bytes!("liteinst_native_exec_input.rs"),
        ),
        (
            "h/model/native_runtime.rs",
            include_bytes!("liteinst_native_exec_runtime.rs"),
        ),
        (
            "h/model/native_catalog.rs",
            include_bytes!("liteinst_native_exec_catalog.rs"),
        ),
        (
            "h/model/native_state.rs",
            include_bytes!("liteinst_native_exec_state.rs"),
        ),
        ("h/model/lib.rs", include_bytes!("lib.rs")),
        ("h/model/fd.rs", include_bytes!("fd.rs")),
        ("h/model/futex.rs", include_bytes!("futex.rs")),
        (
            "h/detcore/config.rs",
            include_bytes!("../../detcore/src/config.rs"),
        ),
        (
            "h/detcore/native_exec.rs",
            include_bytes!("../../detcore/src/native_exec.rs"),
        ),
        (
            "h/detcore/native_registry.rs",
            include_bytes!("../../detcore/src/tool_global/native_exec.rs"),
        ),
        (
            "h/detcore/tool_global.rs",
            include_bytes!("../../detcore/src/tool_global.rs"),
        ),
        (
            "h/detcore/tool_local.rs",
            include_bytes!("../../detcore/src/tool_local.rs"),
        ),
        ("h/detcore/fd.rs", include_bytes!("../../detcore/src/fd.rs")),
        (
            "h/detcore/memory.rs",
            include_bytes!("../../detcore/src/memory.rs"),
        ),
        (
            "h/detcore/dispatch_observer.rs",
            include_bytes!("../../detcore/src/dispatch_observer.rs"),
        ),
        (
            "h/runtime/lib.rs",
            include_bytes!("../../detcore-liteinst/src/lib.rs"),
        ),
        (
            "h/runtime/exec_initial.rs",
            include_bytes!("../../detcore-liteinst/src/exec_initial.rs"),
        ),
        (
            "h/runtime/native_catalog.rs",
            include_bytes!("../../detcore-liteinst/src/exec_native/catalog.rs"),
        ),
        (
            "h/runtime/native_consumer.rs",
            include_bytes!("../../detcore-liteinst/src/exec_native/consumer.rs"),
        ),
        (
            "h/runtime/native_entry.rs",
            include_bytes!("../../detcore-liteinst/src/exec_native/entry.rs"),
        ),
        (
            "h/runtime/native_driver.rs",
            include_bytes!("../../detcore-liteinst/src/exec_native/driver.rs"),
        ),
        (
            "h/runtime/native_provider.rs",
            include_bytes!("../../detcore-liteinst/src/exec_native/provider.rs"),
        ),
    ];
    let mut digest = Sha256::new();
    digest.update(b"Hermit declared native inputs v1\0");
    digest.update(32_u64.to_le_bytes());
    digest.update(reverie_digest);
    for (name, bytes) in inputs {
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    digest.finalize().into()
}
