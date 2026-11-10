//! Build-only root that stages the owned LiteInst preload.
//!
//! The published `reverie-liteinst-preload` leaf owns its allocator and
//! constructor. This standalone locked graph builds that leaf explicitly
//! without linking its constructor into the Hermit host.

//! The build script uses an isolated target directory and stages the exact
//! current leaf cdylib reported by that Cargo invocation.

#[cfg(test)]
#[path = "../artifact.rs"]
mod artifact;
