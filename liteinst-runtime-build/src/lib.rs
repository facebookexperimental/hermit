//! Build-only root that enables the LiteInst preload constructor.
//!
//! `reverie-liteinst` is outside Hermit's Cargo workspace, so Hermit's normal
//! dependency edge cannot select features when Cargo builds that package as a
//! cdylib. This standalone locked graph makes the constructor-bearing runtime
//! an explicit artifact without linking its constructor into the Hermit host.

//! The build script compiles the runtime member in an isolated target directory
//! and stages the exact cdylib reported by that Cargo invocation.

#[cfg(test)]
#[path = "../artifact.rs"]
mod artifact;
