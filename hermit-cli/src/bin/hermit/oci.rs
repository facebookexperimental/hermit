/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `hermit oci` — discover and run OCI images from the local image store.
//!
//! `hermit run --image <ref>` already materializes an image rootfs, and because
//! `buildah` and `podman` share one containers/storage graph root it has always
//! read the *same* store podman writes. What was missing was a way to see what
//! is in that store, to ask what hermit would do with a reference, and — most
//! importantly — to resolve a reference to a stable image **identity** before
//! anything is keyed against it.
//!
//! `hermit oci run` is deliberately a thin front over the existing `--image`
//! path. The new behavior is the resolution step: a tag is a mutable pointer, so
//! running (and caching) against the resolved image id is what makes "the file
//! inputs are pinned" true rather than aspirational.

use clap::Parser;
use hermit::Error;
use hermit::ExitStatus;

use crate::global_opts::GlobalOpts;
use crate::podman_store::PodmanStore;
use crate::podman_store::ResolvedImage;
use crate::run::RunOpts;

#[derive(Debug, Parser)]
pub struct OciOpts {
    #[clap(subcommand)]
    command: OciSubcommand,
}

#[derive(Debug, Parser)]
enum OciSubcommand {
    /// List OCI images available in the local image store.
    #[clap(name = "ls")]
    Ls(LsOpts),

    /// Show what hermit would do with an image reference.
    #[clap(name = "inspect")]
    Inspect(InspectOpts),

    /// Download an image into the local store (the only networked subcommand).
    #[clap(name = "pull")]
    Pull(PullOpts),

    /// Run a program deterministically against an image's filesystem.
    #[clap(name = "run", trailing_var_arg = true)]
    Run(Box<OciRunOpts>),
}

#[derive(Debug, Parser)]
struct LsOpts {
    /// Include untagged intermediate images.
    #[clap(long, short = 'a')]
    all: bool,
}

#[derive(Debug, Parser)]
struct InspectOpts {
    /// Image reference: a tag, a short or full image id, or `name@sha256:...`.
    image: String,
}

#[derive(Debug, Parser)]
struct PullOpts {
    /// Image reference to download.
    image: String,
}

#[derive(Debug, Parser)]
struct OciRunOpts {
    /// Image reference to run against. Must already be in the local store
    /// unless `--pull` is given.
    #[clap(value_name = "IMAGE")]
    image_ref: String,

    /// Download the image first if it is not already present locally.
    #[clap(long)]
    pull: bool,

    /// Options forwarded to `hermit run`, followed by the guest program.
    #[clap(flatten)]
    run: RunOpts,
}

/// Obtain the local store, turning "no podman" into an actionable error.
///
/// `hermit oci` is *about* the local store, so unlike `hermit run --image` there
/// is no meaningful fallback: say plainly what is missing.
fn require_store() -> Result<PodmanStore, Error> {
    PodmanStore::probe()?.ok_or_else(|| {
        Error::msg(
            "No local OCI image store found. `hermit oci` reads the \
             containers/storage store that podman and buildah share, so it \
             needs a rootless `podman` on PATH.",
        )
    })
}

impl OciOpts {
    pub fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        match &mut self.command {
            OciSubcommand::Ls(opts) => opts.main(),
            OciSubcommand::Inspect(opts) => opts.main(),
            OciSubcommand::Pull(opts) => opts.main(),
            OciSubcommand::Run(opts) => opts.main(global),
        }
    }
}

impl LsOpts {
    fn main(&self) -> Result<ExitStatus, Error> {
        let store = require_store()?;
        let images = store.list(self.all)?;
        if images.is_empty() {
            // An empty store is a legitimate state, not a failure. Say where we
            // looked so the user can tell "empty" from "wrong store".
            println!("No images in the local store at {}", store.graph_root());
            return Ok(ExitStatus::SUCCESS);
        }
        println!("{:<14}  {:<48}  {:>10}", "IMAGE ID", "NAME", "SIZE");
        for image in &images {
            println!(
                "{:<14}  {:<48}  {:>10}",
                short_id(&image.id),
                image.display_name(),
                human_size(image.size),
            );
        }
        Ok(ExitStatus::SUCCESS)
    }
}

impl InspectOpts {
    fn main(&self) -> Result<ExitStatus, Error> {
        let store = require_store()?;
        let image = store.resolve(&self.image)?;
        report(&store, &image)?;
        Ok(ExitStatus::SUCCESS)
    }
}

impl PullOpts {
    fn main(&self) -> Result<ExitStatus, Error> {
        let store = require_store()?;
        let image = store.pull(&self.image)?;
        report(&store, &image)?;
        Ok(ExitStatus::SUCCESS)
    }
}

/// Print the resolution result and the parts of the image config that change
/// how hermit runs the guest.
fn report(store: &PodmanStore, image: &ResolvedImage) -> Result<(), Error> {
    println!("reference:  {}", image.reference);
    println!("image id:   {}", image.id);
    // A locally built image has no manifest digest until it is pushed or
    // pulled. Distinguish that from "we failed to look it up".
    println!(
        "digest:     {}",
        image
            .digest
            .as_deref()
            .unwrap_or("<none> (locally built; not pushed or pulled)")
    );
    println!("store:      {}", store.graph_root());

    let config = store.config(image)?;
    println!(
        "workdir:    {}",
        config
            .workdir
            .as_deref()
            .unwrap_or("/ (image declares none)")
    );
    if config.env.is_empty() {
        println!("env:        <none declared>");
    } else {
        println!("env:");
        for (key, value) in &config.env {
            println!("  {key}={value}");
        }
    }
    if !config.entrypoint.is_empty() {
        println!("entrypoint: {:?}", config.entrypoint);
    }
    if !config.cmd.is_empty() {
        // hermit always requires an explicit guest program, so say so rather
        // than let the reader assume `hermit oci run <ref>` would use this.
        println!(
            "cmd:        {:?}  (informational; hermit requires an explicit program)",
            config.cmd
        );
    }

    match store.overlay_dirs(image) {
        Ok(dirs) => {
            println!("layers:     {} (topmost first)", dirs.len());
            for dir in &dirs {
                println!("  {dir}");
            }
        }
        Err(e) => println!("layers:     <unavailable> ({e})"),
    }
    Ok(())
}

impl OciRunOpts {
    fn main(&mut self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // `hermit oci run` takes its image as a positional; the inherited
        // `--image` flag would be a second, unresolved way to say the same
        // thing. Reject the ambiguity rather than silently picking one.
        if let Some(flag) = self.run.image() {
            return Err(Error::msg(format!(
                "`hermit oci run` takes the image as a positional argument; drop \
                 `--image {flag}` and pass {:?} once. `--image` on `hermit run` \
                 bypasses reference resolution, which is what `hermit oci run` \
                 exists to do.",
                self.image_ref
            )));
        }
        let store = require_store()?;
        // Resolution never contacts a registry unless --pull was asked for, so
        // an ordinary `hermit oci run` cannot silently download.
        let image = if self.pull {
            store.pull(&self.image_ref)?
        } else {
            store.resolve(&self.image_ref)?
        };
        tracing::info!(
            "Resolved OCI reference {} to image id {}",
            image.reference,
            image.id
        );
        // Hand the *resolved id* to the existing --image path. Passing the raw
        // reference would let a moved tag alias an older rootfs.
        self.run.set_image(image.id.clone());
        self.run.main(global)
    }
}

fn short_id(id: &str) -> &str {
    // podman's own display width for an image id.
    id.get(..12).unwrap_or(id)
}

/// Human-readable byte count, matching the decimal (MB = 10^6) convention
/// podman uses so the two tools' listings agree.
fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("kB", 1_000),
        ("B", 1),
    ];
    for (suffix, scale) in UNITS {
        if bytes >= scale {
            return if scale == 1 {
                format!("{bytes} {suffix}")
            } else {
                format!("{:.1} {suffix}", bytes as f64 / scale as f64)
            };
        }
    }
    "0 B".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_truncates_to_podman_width_without_panicking() {
        let id = "1ee23df5e47b5b7331304af929f5461f9f516f5c166c3031832d2adfd22f323c";
        assert_eq!(short_id(id), "1ee23df5e47b");
        // Never panic on an unexpectedly short id.
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn human_size_uses_the_decimal_convention_podman_prints() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1_500), "1.5 kB");
        assert_eq!(human_size(80_659_122), "80.7 MB");
        assert_eq!(human_size(2_500_000_000), "2.5 GB");
    }

    // `hermit oci run` must accept the same trailing guest command shape as
    // `hermit run`, with the image reference first.
    #[test]
    fn oci_run_parses_image_then_forwarded_run_args() {
        let opts = OciOpts::try_parse_from([
            "oci",
            "run",
            "restored-ubuntu:24.04",
            "--strict",
            "--",
            "/bin/echo",
            "hi",
        ])
        .expect("oci run should parse an image followed by run options");
        match opts.command {
            OciSubcommand::Run(run) => {
                assert_eq!(run.image_ref, "restored-ubuntu:24.04");
                assert!(!run.pull, "--pull must default off so run never downloads");
            }
            other => panic!("expected the run subcommand, got {other:?}"),
        }
    }

    #[test]
    fn oci_ls_defaults_to_hiding_intermediate_images() {
        let opts = OciOpts::try_parse_from(["oci", "ls"]).expect("oci ls should parse");
        match opts.command {
            OciSubcommand::Ls(ls) => assert!(!ls.all),
            other => panic!("expected the ls subcommand, got {other:?}"),
        }
        let opts = OciOpts::try_parse_from(["oci", "ls", "--all"]).expect("oci ls --all");
        match opts.command {
            OciSubcommand::Ls(ls) => assert!(ls.all),
            other => panic!("expected the ls subcommand, got {other:?}"),
        }
    }
}
