/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hermit's KVM run, and the one place its Detcore instantiation is rooted.
//!
//! `detcore::Detcore` under `reverie_kvm`'s tool runtime is a large
//! instantiation. In its own crate it builds in a separate rustc process,
//! beside the other backends' instantiations. The entry point,
//! [`run_detcore`], is not generic and returns a boxed future, so its callers
//! instantiate nothing of it (see `hermit-record` for why it is boxed).
//!
//! `reverie_kvm` is an x86-64 VMM and empty elsewhere, and so is this crate.
#![cfg(target_arch = "x86_64")]

use std::future::Future;
use std::pin::Pin;

use reverie_kvm::KvmBackend;
use reverie_kvm::ToolRunCompletion;

/// The future [`run_detcore`] returns. It borrows the backend it runs on.
pub type RunDetcore<'a> = Pin<
    Box<dyn Future<Output = reverie_kvm::Result<ToolRunCompletion<detcore::GlobalState>>> + 'a>,
>;

/// Runs the static ELF already installed in BACKEND to completion under
/// Detcore with CONFIG. With CAPTURE_OUTPUT the guest's stdout and stderr are
/// returned in the completion instead of written through.
///
/// The future is boxed so the async body, and everything it instantiates, is
/// compiled in this crate rather than in its caller.
///
/// A CONFIG that leaves the pass-through syscalls untrapped
/// (`untrap_passthrough_syscalls`) is refused before the backend is touched,
/// as `detcore::UntrapPassthroughRefusal::Backend("kvm")`: untrapping is the
/// ptrace backend's alone (`detcore::untrap_passthrough_off_ptrace`). This
/// entry is public, so it asks that rule itself (`admitted`) rather than
/// relying on the hermit library's launch dispatchers, which ask it too.
pub fn run_detcore(
    backend: &mut KvmBackend,
    config: detcore::Config,
    capture_output: bool,
) -> RunDetcore<'_> {
    admitted(config, |config| {
        Box::pin(
            backend.run_static_elf_with_tool_completion::<detcore::Detcore>(config, capture_output),
        )
    })
}

/// RUN's future for CONFIG, unless KVM refuses CONFIG first: RUN, which
/// holds the backend, is called only once CONFIG is admitted, so a refused
/// run never reaches the backend or the guest.
fn admitted<'a>(
    config: detcore::Config,
    run: impl FnOnce(detcore::Config) -> RunDetcore<'a>,
) -> RunDetcore<'a> {
    match detcore::untrap_passthrough_off_ptrace(&config, "kvm") {
        Ok(()) => run(config),
        Err(refusal) => Box::pin(std::future::ready(Err(reverie_kvm::Error::Reverie(
            refusal.into(),
        )))),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;

    use super::*;

    /// The result of RUN, which must be ready at once.
    fn ready(
        mut run: RunDetcore<'_>,
    ) -> reverie_kvm::Result<ToolRunCompletion<detcore::GlobalState>> {
        match run.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("the run was not decided at once"),
        }
    }

    /// REFUSAL as the tool error a KVM run returns, written out.
    fn as_tool_error<E>(_: &E, refusal: detcore::UntrapPassthroughRefusal) -> String
    where
        E: std::fmt::Display + From<detcore::UntrapPassthroughRefusal>,
    {
        E::from(refusal).to_string()
    }

    /// `run_detcore` refuses a configuration that leaves the pass-through
    /// syscalls untrapped before its run, which holds the backend, is called,
    /// though the configuration is otherwise admitted (RCB time, no record)
    /// and has KVM's declared capabilities. Here the run is a stand-in that
    /// fails the test if called, so the test needs no /dev/kvm and opens no
    /// VM. The same configuration without the mode reaches the run, whose
    /// result is returned unchanged.
    #[test]
    fn untrapping_is_refused_before_the_backend_runs() {
        let qualified = detcore::Config {
            untrap_passthrough_syscalls: true,
            sequentialize_threads: true,
            max_timeslice: std::num::NonZeroU64::new(10_000),
            backend: KvmBackend::capabilities(),
            ..Default::default()
        };
        assert_eq!(detcore::untrap_passthrough_admission(&qualified), Ok(()));
        match ready(admitted(qualified.clone(), |_| {
            panic!("the refused run reached the backend")
        })) {
            Err(reverie_kvm::Error::Reverie(error)) => assert_eq!(
                error.to_string(),
                as_tool_error(&error, detcore::UntrapPassthroughRefusal::Backend("kvm"))
            ),
            Err(other) => panic!("not the untrap refusal: {other}"),
            Ok(_) => panic!("the untrapped calls were admitted on KVM"),
        }

        let ran = Cell::new(false);
        let without = detcore::Config {
            untrap_passthrough_syscalls: false,
            ..qualified
        };
        match ready(admitted(without, |config| {
            assert!(!config.untrap_passthrough_syscalls);
            ran.set(true);
            Box::pin(std::future::ready(Err(reverie_kvm::Error::RunAborted)))
        })) {
            Err(reverie_kvm::Error::RunAborted) => {}
            _ => panic!("the admitted run's own result was not returned"),
        }
        assert!(ran.get(), "the admitted run was not called");
    }

    /// The public entry itself, on a real backend: `run_detcore` answers a
    /// configuration that leaves the pass-through syscalls untrapped with the
    /// refusal at once, before the backend runs anything. No program is
    /// installed, so a run that reached the backend would not end with this
    /// refusal. Requires /dev/kvm, as the backend's own unit tests do.
    #[test]
    fn run_detcore_refuses_untrapping_before_the_backend_runs() {
        let mut backend = KvmBackend::new(0x10000).expect("this refusal test requires /dev/kvm");
        let config = detcore::Config {
            untrap_passthrough_syscalls: true,
            sequentialize_threads: true,
            max_timeslice: std::num::NonZeroU64::new(10_000),
            backend: KvmBackend::capabilities(),
            ..Default::default()
        };
        assert_eq!(detcore::untrap_passthrough_admission(&config), Ok(()));
        match ready(run_detcore(&mut backend, config, false)) {
            Err(reverie_kvm::Error::Reverie(error)) => assert_eq!(
                error.to_string(),
                as_tool_error(&error, detcore::UntrapPassthroughRefusal::Backend("kvm"))
            ),
            Err(other) => panic!("not the untrap refusal: {other}"),
            Ok(_) => panic!("the untrapped calls were admitted on KVM"),
        }
    }
}
