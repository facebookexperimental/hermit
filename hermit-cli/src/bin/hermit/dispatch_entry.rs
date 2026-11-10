/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! One executable boundary, before fbinit or any application/private role.
//! Selection uses only shared option grammar; the selected executable owns
//! its complete parser. Neither a probe nor a handoff grants runtime authority.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::sync::OnceLock;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use clap::CommandFactory;
use hermit::compatibility::ExecutableIdentity;

use super::dispatch_executable::HeldExecutable;
use super::dispatch_input;
use super::dispatch_input::Environment;

// Set only by a successful sealed-frame adoption, not by an argv/env value.
// Its sole authority is the original stdin reservation/error for this image.
static ADOPTED_BACKEND: OnceLock<&'static str> = OnceLock::new();

fn refusal(error: &anyhow::Error) -> Option<i32> {
    super::display_entry_error(
        "Executable dispatch refused; no backend was substituted",
        error,
    );
    Some(super::HERMIT_INTERNAL_FAILURE_EXIT)
}

fn has_private_marker(argv: &[OsString]) -> bool {
    argv.get(1)
        .is_some_and(|argument| argument.as_os_str().as_bytes().starts_with(b"__hermit"))
}

pub(super) fn core() -> Option<i32> {
    // Exact metadata requests never inspect/adopt caller input, parse options,
    // initialize logging/budgets, or construct a backend adapter.
    if let Some(status) = super::executable_probe::core() {
        return Some(status);
    }
    let result = (|| -> Result<()> {
        let environment = Environment::capture()?;
        ensure!(
            environment.locator()?.is_none(),
            "the core executable cannot adopt or forward a backend input handoff"
        );
        let argv: Vec<_> = std::env::args_os().collect();
        // Existing private role owners still validate their own exact protocol.
        // No dispatch selection or plain handoff can enter those protocols.
        if has_private_marker(&argv) {
            return Ok(());
        }
        let schema = super::Args::command();
        let selection = match super::run_config::external_selection(&schema, &argv) {
            Ok(selection) => selection,
            Err(error) => {
                // The longest fully parsed shared prefix can own a report
                // before an ambiguous option. Never scan its unknown tail for
                // --config/--verify-json or guess extension option arity.
                let stamp =
                    super::run_config::external_refusal_pending(&schema, &argv).and_then(|path| {
                        path.map_or(Ok(()), |path| super::write_pending_verification_json(&path))
                    });
                return Err(error)
                    .with_context(|| format!("shared selection failed; pending_report={stamp:?}"));
            }
        };
        let Some(selection) = selection else {
            return Ok(());
        };
        if let Some(path) = &selection.pending_verification {
            // Required before a missing stamp, lookup, probe or exec can fail:
            // a stale successful report must not survive a refused attempt.
            super::write_pending_verification_json(path)?;
        }
        environment.require_probe_environment()?;
        environment.require_unchanged()?;
        let core = hermit::compatibility::current()?;
        let executable = HeldExecutable::prepare(&selection.backend, core)?;
        match executable.execute(&argv, &environment)? {}
    })();
    match result {
        Ok(()) => None,
        Err(error) => refusal(&error),
    }
}

pub(super) fn backend<A: super::backend_hook::BackendAdapter>() -> Option<i32> {
    if let Some(status) = super::executable_probe::backend(A::NAME, true) {
        return Some(status);
    }
    let result = (|| -> Result<()> {
        super::backend_hook::validate_name(A::NAME)?;
        let environment = Environment::capture()?;
        let Some(value) = environment.locator()? else {
            return Ok(());
        };
        let fd = dispatch_input::locator(value)?;
        let argv: Vec<_> = std::env::args_os().collect();
        let compiled = ExecutableIdentity::backend(A::NAME)?;
        let core = hermit::compatibility::current()?;
        let original = dispatch_input::adopt(fd, &argv, &environment, &compiled, &core)?;
        original.restore_after_adoption()?;
        ADOPTED_BACKEND
            .set(A::NAME)
            .map_err(|_| anyhow::anyhow!("executable input handoff was adopted more than once"))?;
        Ok(())
    })();
    match result {
        Ok(()) => None,
        Err(error) => refusal(&error),
    }
}

pub(super) fn require_application_entry(selected: Option<&str>) -> Result<()> {
    let environment = Environment::capture()?;
    ensure!(
        environment.locator()?.is_none(),
        "unconsumed executable input handoff; call executable_entry_with_backend before fbinit"
    );
    if let Some(adopted) = ADOPTED_BACKEND.get() {
        ensure!(
            selected == Some(*adopted),
            "adopted input belongs to another static executable backend; fallback is refused"
        );
    }
    Ok(())
}
