//! Genuine map/cancel controls plus an explicit bounded initializer experiment.
// H-private root component wired below; receipts record compilation and tests.
// Uses existing retained-owner APIs. It grants no production source authority.
use std::io;
// H-private map/cancel data only. No execution/source/admission capability.
// It authorizes mapping only; execution qualification is separate.
// Control body, placed as a child of packet_held_tests so it reuses
// that module's genuine admit/Admission/exact_storage/Config/watch helpers.
// Requires the reviewed custody bridge and paired reverie-loader dev dependency.
// Ordinary controls stop after mapping; Initialize separately observes real return.
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::panic::resume_unwind;
use std::sync::atomic::Ordering;

use reverie_inguest::launcher::exec_stop::CleanupReceipt;
use reverie_inguest::launcher::exec_stop::CopiedInputRequest;
use reverie_inguest::launcher::exec_stop::ExecStop;
use reverie_inguest::launcher::exec_stop::PreparedExec;
use reverie_inguest::launcher::exec_stop::physical::Preparation;
use reverie_inguest::launcher::exec_stop::register_words;
use sha2::Digest;
use sha2::Sha256;

use super::*;
use crate::mapped_first_admission::runtime_support::WAIT;
use crate::mapped_first_admission::runtime_support::need;
use crate::mapped_first_admission::runtime_support::rpc;
use crate::mapped_first_admission::runtime_support::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum MappedCase {
    Positive,
    OtherBirth,
    Poison,
    Initialize,
}

fn same_error(error: io::Error, expected: &str) -> io::Result<()> {
    need(
        error.kind() == io::ErrorKind::InvalidData && error.to_string() == expected,
        "negative reached a different exact lease predicate",
    )
}
fn launch_into<'fd>(
    root: &mut RetainedPhysical<'fd>,
    a: &'fd PendingPacketLease,
    b: &'fd PendingPacketLease,
    detlog: &'fd UnixStream,
    runtime: &'fd File,
    handoff: &'fd File,
) -> io::Result<()> {
    let request = PreparedExec::new(
        std::path::Path::new("/bin/true"),
        &["mapped-genuine-packet-cancel-only".into()],
        &[("ORIGINAL".into(), "mapped-packet".into())],
        None,
        // BOTH packet OFDs really cross BOTH births. No membership failure can
        // stand in for the intended negative watched-birth comparison.
        vec![
            a.inherited_fd(),
            b.inherited_fd(),
            detlog.as_fd(),
            runtime.as_fd(),
            handoff.as_fd(),
        ],
    )
    .map_err(rpc)?;
    root.retain_spawn(ExecStop::spawn(request, root.receipt.clone()));
    root.initial().map(|_| ())
}
/// Safety: called only by an exclusively bounded physical-control runner on the
/// original OS thread, with compatible stable CPUID/XCR0 domain for XState::pkru,
/// genuine sole-leader/unshared-FD initial stop and known seccomp/signal lineage.
/// The existing prepare_mapping contract remains unchanged. Ordinary cases only
/// map. Initialize also requires the reviewed exact-image valid-path, signal and
/// concurrent-RPC contract at its unsafe observer call; never Guest release.
unsafe fn mapped_packet_control(
    case: MappedCase,
    image_bytes: &[u8],
    observed: &ObservedImage,
    placement: &ProposedPlacement,
) -> io::Result<()> {
    // All fallible pre-birth setup may use '?'. After birth every outcome reaches
    // same-owner cancellation before watch teardown or host assertion unwinds.
    let (image, descriptor) = checked_image(image_bytes, observed).map_err(rpc)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let directory = tempfile::tempdir()?;
    let directory_fd = File::open(directory.path())?;
    let socket = |name: &str| {
        // Real initialization uses the existing rpc.sock/stats.sock companion
        // contract. Map/cancel-only controls keep their distinct a/b endpoints.
        let name = if case == MappedCase::Initialize && name == "a" {
            "rpc"
        } else {
            name
        };
        std::path::PathBuf::from(format!(
            "/proc/{}/fd/{}/{name}.sock",
            std::process::id(),
            directory_fd.as_raw_fd()
        ))
    };
    let config = detcore::Config {
        no_rcb_time: true,
        max_timeslice: None,
        target_timeslice: None,
        sequentialize_threads: false,
        in_guest_detlog_forward_policy: Some(detcore::detlog::ForwardPolicy::all().encode()),
        ..Default::default()
    };
    need(
        packet::config_violation(&config).is_none(),
        "Config outside ordinary domain",
    )?;
    let global = Arc::new(runtime.block_on(
        <detcore::GlobalState as GlobalTool>::init_global_state(&config),
    ));
    let lease_a = PendingPacketLease::new()?;
    let lease_b = PendingPacketLease::new()?;
    let runtime_file = memfd_bytes(image_bytes, true)?;
    let handoff_a = memfd_bytes(&[], false)?;
    let handoff_b = memfd_bytes(&[], false)?;
    let (sender_a, receiver_a) = if case == MappedCase::Initialize {
        let (sender, receiver) = crate::run::RunOpts::open_forwarded_detlog_channel()
            .map_err(rpc)?
            .ok_or_else(|| rpc("actual DETLOG channel absent"))?;
        (UnixStream::from(sender), UnixStream::from(receiver))
    } else {
        UnixStream::pair()?
    };
    let mut trial = if case == MappedCase::Initialize {
        Some(init_trial::Trial::register(
            receiver_a.as_fd().try_clone_to_owned()?,
        ))
    } else {
        None
    };
    let (sender_b, receiver_b) = UnixStream::pair()?;
    receiver_a.set_nonblocking(true)?;
    receiver_b.set_nonblocking(true)?;
    // Tokio's sockets are registered under the same current-thread runtime.
    let (mut receiver_a, mut receiver_b) = {
        let _entered = runtime.enter();
        (
            tokio::net::UnixStream::from_std(receiver_a)?,
            tokio::net::UnixStream::from_std(receiver_b)?,
        )
    };
    let mut admissions = Vec::new();
    // Stable optional wire storage is declared before the borrowing physical
    // holders; explicit cleanup drops physical holders first, then this storage.
    let mut bundle = None;
    let mut a = RetainedPhysical::empty(CleanupReceipt::default());
    let mut b = RetainedPhysical::empty(CleanupReceipt::default());
    let mut panic_payload = None;
    let setup = catch_unwind(AssertUnwindSafe(|| -> io::Result<_> {
        launch_into(
            &mut a,
            &lease_a,
            &lease_b,
            &sender_a,
            &runtime_file,
            &handoff_a,
        )?;
        if matches!(case, MappedCase::OtherBirth | MappedCase::Poison) {
            launch_into(
                &mut b,
                &lease_a,
                &lease_b,
                &sender_b,
                &runtime_file,
                &handoff_b,
            )?;
        }
        let stop = a.initial()?;
        let captured = stop.capture_initial_inputs().map_err(rpc)?;
        let admission_a = runtime.block_on(admit(
            &mut admissions,
            &global,
            &config,
            &socket("a"),
            stop,
            &sender_a,
        ))?;
        let admission_b = if matches!(case, MappedCase::OtherBirth | MappedCase::Poison) {
            let actual = runtime.block_on(admit(
                &mut admissions,
                &global,
                &config,
                &socket("b"),
                b.initial()?,
                &sender_b,
            ))?;
            need(
                admission_a.watched_process().birth_identity()
                    != actual.watched_process().birth_identity(),
                "controls did not create distinct real births",
            )?;
            Some(actual)
        } else {
            None
        };
        let sealed_a = lease_a.fill(stop, &admission_a)?;
        let sealed_b = match admission_b.as_ref() {
            Some(admission) if case == MappedCase::OtherBirth => {
                Some(lease_b.fill(b.initial()?, admission)?)
            }
            _ => None,
        };
        exact_storage(&sealed_a, &admission_a.initial_packet()?)?;
        sealed_a.verify(stop)?;
        let control = placement.roles[4]
            .start
            .checked_add(PAGE)
            .ok_or_else(|| rpc("control overflow"))?;
        let relay = aligned(
            control
                .checked_add(descriptor[6])
                .ok_or_else(|| rpc("relay overflow"))?,
            descriptor[9],
        )
        .map_err(rpc)?;
        let address = placement.roles[3]
            .start
            .checked_add(PAGE)
            .ok_or_else(|| rpc("handoff address"))?;
        let copied_address = address
            .checked_add(2 * PAGE)
            .ok_or_else(|| rpc("copied address"))?;
        let copied_capacity = usize::try_from(
            placement.roles[3]
                .end
                .checked_sub(PAGE)
                .and_then(|end| end.checked_sub(copied_address))
                .ok_or_else(|| rpc("copied capacity"))?,
        )
        .map_err(rpc)?;
        let copied = stop
            .prepare_copied_inputs(
                &captured,
                &image,
                &CopiedInputRequest {
                    roles: placement.roles,
                    free_band: placement.free_band,
                    layout_address: observed.layout_address,
                    copied_address,
                    copied_capacity,
                    control_address: control,
                    relay_address: relay,
                    filter_scratch: placement.filter_scratch,
                },
            )
            .map_err(rpc)?;
        // SAFETY: actual unchanged stopped XState; runner's compatible CPU/XCR0
        // contract is explicit above. No host RDPKRU, guessed offset or zero.
        let pkru = unsafe { stop.entry().xstate().pkru() }.map_err(rpc)?;
        let wire = PacketMappingWire::build(
            &image,
            WireInputs {
                runtime: image_bytes,
                observed,
                roles: &placement.roles,
                descriptor: &descriptor,
                reference: observed.reference_address,
                copied: &copied,
                pkru,
                free_band: placement.free_band,
                packet: &sealed_a,
            },
            Context {
                pid: stop.pid().as_raw() as u64,
                regs: register_words(stop.entry().regs()),
                mask: stop.entry().signal_mask(),
                input_fd: sealed_a.fd().as_raw_fd() as u64,
            },
        )
        .map_err(rpc)?;
        handoff_a.write_all_at(&wire.bytes, 0)?;
        handoff_a.set_len(wire.bytes.len() as u64)?;
        handoff_a.sync_all()?;
        seal_memfd(&handoff_a)?;
        Ok((wire, sealed_a, sealed_b, admission_a, admission_b))
    }));
    let mut primary = match setup {
        Ok(Ok(value)) => {
            bundle = Some(value);
            Ok(())
        }
        Ok(Err(error)) => Err(error),
        Err(payload) => {
            panic_payload = Some(payload);
            Err(rpc("setup panicked; retained root cleanup follows"))
        }
    };
    if let Some((wire, sealed_a, sealed_b, admission_a, admission_b)) = bundle.as_ref() {
        let inputs = Preparation {
            runtime_fd: runtime_file.as_fd(),
            runtime_bytes: image_bytes,
            layout_address: observed.layout_address,
            handoff_fd: handoff_a.as_fd(),
            handoff_bytes: &wire.bytes,
            handoff_address: wire.address,
            input_fd: lease_a.inherited_fd(),
            free_band: placement.free_band,
            filter_scratch: placement.filter_scratch,
        };
        let observed = catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
            // SAFETY: root retains exactly the outer leases named above; only
            // existing synchronous mapping syscall/INT3 at this step.
            unsafe { a.prepare(inputs) }?;
            sealed_a.verify_prepared(a.mapped()?)?;
            exact_storage(sealed_a, &admission_a.initial_packet()?)?;
            need(
                !admission_a.failed.load(Ordering::Acquire)
                    && !admission_a.initial_used.load(Ordering::Acquire),
                "positive poisoned admission or consumed initial nonce",
            )?;
            match case {
                MappedCase::Positive => {}
                MappedCase::Initialize => {
                    let trial = trial
                        .as_mut()
                        .ok_or_else(|| rpc("initializer trial absent"))?;
                    runtime.block_on(async {
                        trial.start(
                            admissions
                                .first_mut()
                                .ok_or_else(|| rpc("actual admission absent"))?,
                            &directory_fd,
                        )?;
                        // SAFETY: exact immutable V11 image and decoded PC list,
                        // independent valid-path pre-TLS source audit, original
                        // ptracer/current-thread runtime, retained real leases,
                        // concurrent genuine RPC and outer bounded cancellation.
                        // This experiment stops at the real initializer INT3 and
                        // never claims full qualification or releases a Guest.
                        unsafe {
                            trial
                                .observe(
                                    a.mapped
                                        .as_mut()
                                        .ok_or_else(|| rpc("actual mapping absent"))?,
                                    &config,
                                )
                                .await
                        }
                    })?;
                }
                MappedCase::OtherBirth => {
                    let sealed_b = sealed_b
                        .as_ref()
                        .ok_or_else(|| rpc("actual sealed B absent"))?;
                    let admission_b = admission_b
                        .as_ref()
                        .ok_or_else(|| rpc("actual admission B absent"))?;
                    let error = sealed_b
                        .verify_prepared(a.mapped()?)
                        .err()
                        .ok_or_else(|| rpc("other actual birth accepted after mapping"))?;
                    same_error(
                        error,
                        "native peer/watch/source belongs to a different or failed birth",
                    )?;
                    need(
                        admission_b.failed.load(Ordering::Acquire)
                            && !admission_b.initial_used.load(Ordering::Acquire),
                        "wrong-birth admission not poisoned or nonce used",
                    )?;
                    sealed_a.verify_prepared(a.mapped()?)?;
                    need(
                        !admission_a.failed.load(Ordering::Acquire),
                        "wrong-birth B poisoned A",
                    )?;
                }
                MappedCase::Poison => {
                    let admission_b = admission_b
                        .as_ref()
                        .ok_or_else(|| rpc("actual admission B absent"))?;
                    let error = match lease_a.fill(b.initial()?, admission_b) {
                        Err(error) => error,
                        Ok(_) => return Err(rpc("second fill accepted")),
                    };
                    same_error(error, "mapped packet lease fill was already attempted")?;
                    need(
                        admission_b.failed.load(Ordering::Acquire)
                            && !admission_a.failed.load(Ordering::Acquire),
                        "second-fill B did not poison exactly its actual admission",
                    )?;
                    let error = sealed_a
                        .verify_prepared(a.mapped()?)
                        .err()
                        .ok_or_else(|| rpc("old sealed A survived permanent poison"))?;
                    same_error(error, "mapped packet lease is permanently poisoned")?;
                    need(
                        admission_a.failed.load(Ordering::Acquire),
                        "old A verification failed without poisoning A",
                    )?;
                    // Raw immutable bytes remain equal; storage equality cannot
                    // substitute for the permanently failed lease predicate.
                    exact_storage(sealed_a, sealed_a.bytes())?;
                }
            }
            Ok(())
        }));
        primary = match observed {
            Ok(result) => result,
            Err(payload) => {
                panic_payload = Some(payload);
                Err(rpc("mapping/observation panicked; cleanup follows"))
            }
        };
    }
    // Neither assertions nor a Result error may unwind the owners before both
    // actual children have confirmed same-generation original-owner retirement.
    cleanup_all(&mut a, &mut b, &mut panic_payload);
    let transcript = a.cancelled.as_ref().map(|cancelled| {
        need(
            cancelled.cleanup.confirmed
                && cancelled.filter_bytes.len() == 23 * 8
                && cancelled
                    .injected_calls
                    .last()
                    .is_some_and(|(nr, args, result)| {
                        *nr == libc::SYS_seccomp as u64
                            && args[0] == libc::SECCOMP_SET_MODE_FILTER as u64
                            && args[1] == libc::SECCOMP_FILTER_FLAG_TSYNC
                            && *result == 0
                    }),
            "actual map/cancel transcript lost successful final filter",
        )
    });
    let mut cleanup_errors = Vec::new();
    match transcript {
        Some(Err(error)) => cleanup_errors.push(format!("mapping transcript: {error}")),
        None if primary.is_ok() => {
            cleanup_errors.push("no actual mapping cancellation transcript".to_owned())
        }
        _ => {}
    }
    // End every descriptor/backing borrow before dropping parent DETLOG senders.
    drop(a);
    drop(b);
    drop(bundle);
    drop(sender_a);
    drop(sender_b);
    runtime.block_on(async {
        if let Some(trial) = trial.as_mut() {
            match trial.close().await {
                Ok(Some(owner)) => match admissions
                    .iter_mut()
                    .find(|slot| Arc::ptr_eq(&slot.owner, &owner))
                {
                    Some(slot) => slot.drained = true,
                    None => cleanup_errors
                        .push("drained RPC has no matching retained admission".to_owned()),
                },
                Ok(None) => {}
                Err(error) => cleanup_errors.push(format!("actual initializer services: {error}")),
            }
        }
        for slot in &mut admissions {
            if primary.is_err() {
                slot.owner.fail(&"mapped packet observation failed");
            }
            let drain = if slot.drained {
                Ok(())
            } else {
                slot.owner.exit_admission().drain(WAIT).await
            };
            match drain {
                Ok(()) => {
                    slot.drained = true;
                    if slot.owner.exit_admission().reported_exits()
                        != [slot.owner.watched_process().process_id()]
                    {
                        cleanup_errors.push(
                            "actual watch did not report its own child exactly once".to_owned(),
                        );
                    }
                }
                Err(error) => cleanup_errors.push(format!("actual watch drain: {error:?}")),
            }
            if slot
                .issues
                .as_ref()
                .is_some_and(|issues| !issues.snapshot().is_empty())
            {
                cleanup_errors.push("actual RPC listener acquired an issue".to_owned());
            }
        }
        for receiver in [&mut receiver_a, &mut receiver_b] {
            let outcome = super::bounded(async {
                let mut byte = [0];
                need(
                    receiver.read(&mut byte).await? == 0,
                    "actual inherited DETLOG not EOF",
                )
            })
            .await;
            if let Err(error) = outcome {
                cleanup_errors.push(format!("DETLOG EOF: {error}"));
            }
        }
    });
    drop(admissions); // Directory and GlobalState still live here.
    // Input digest closure is separate from packet/birth/cleanup observations.
    if <[u8; 32]>::from(Sha256::digest(image_bytes)) != observed.image_sha256 {
        cleanup_errors.push("retained input image changed across control".to_owned());
    }
    if let Some(payload) = panic_payload {
        if !cleanup_errors.is_empty() {
            // Cleanup is confirmed now; diagnostic failure still must not replace
            // the original setup/observation/retirement panic being rethrown.
            if let Err(diagnostic) = catch_unwind(AssertUnwindSafe(|| {
                eprintln!("independent cleanup failures: {cleanup_errors:?}")
            })) {
                std::mem::forget(diagnostic);
            }
        }
        resume_unwind(payload);
    }
    if !cleanup_errors.is_empty() {
        return Err(rpc(format!(
            "cleanup={cleanup_errors:?}; primary={primary:?}"
        )));
    }
    // Map controls executed no image entry. Initialize may have observed the
    // genuine initializer return, but no case releases Guest or claims Ready.
    primary
}

// Ordinary child module: no path includes or duplicate codec/owner.
mod measured_v11;

#[test]
fn mapped_packet_prepared_same_birth() -> io::Result<()> {
    measured_v11::run(MappedCase::Positive)
}

#[test]
fn mapped_packet_prepared_other_birth_refuses() -> io::Result<()> {
    measured_v11::run(MappedCase::OtherBirth)
}

#[test]
fn mapped_packet_prepared_poison_refuses_old_lease() -> io::Result<()> {
    measured_v11::run(MappedCase::Poison)
}

#[cfg(test)]
mod retirement_unwind_tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::PanicPayload;
    use super::RetirementStep;
    use super::guarded_retirement;

    // Pure host-control model: these booleans are NOT a CleanupReceipt, child,
    // wait result or kernel confirmation. No test can enter the retention loop.
    struct Model {
        created: bool,
        confirmed: bool,
        capability: Option<()>,
    }
    impl Model {
        fn snapshot(&self) -> (bool, bool, bool) {
            (self.created, self.confirmed, self.capability.is_some())
        }
    }
    struct OuterLease(Rc<Cell<usize>>);
    impl Drop for OuterLease {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn consumed_capability_unwind_retains_outer_frame_without_confirmation() {
        let drops = Rc::new(Cell::new(0));
        let outer = OuterLease(drops.clone());
        let mut owner = Model {
            created: true,
            confirmed: false,
            capability: Some(()),
        };
        let mut panic = None;
        let step = guarded_retirement(
            &mut owner,
            &mut panic,
            |owner| {
                let _consumed = owner.capability.take();
                std::panic::panic_any("consumed before cancel returned");
            },
            Model::snapshot,
        );
        assert_eq!(step, RetirementStep::RetainFrame);
        assert!(
            panic
                .as_ref()
                .is_some_and(|payload| payload.downcast_ref::<&str>()
                    == Some(&"consumed before cancel returned"))
        );
        assert!(owner.capability.is_none());
        assert_eq!(drops.get(), 0);
        // The real root spins here. A data-only test drops its simulated lease
        // explicitly after checking the production decision; no reap is claimed.
        drop(outer);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn borrowed_retry_unwind_preserves_only_retained_capability() {
        let mut owner = Model {
            created: true,
            confirmed: false,
            capability: Some(()),
        };
        let mut panic = None;
        assert_eq!(
            guarded_retirement(
                &mut owner,
                &mut panic,
                |_| {
                    std::panic::panic_any("borrowed retry panicked");
                },
                Model::snapshot
            ),
            RetirementStep::RetryRetained
        );
        assert!(owner.capability.is_some());
        assert!(panic.is_some());
    }

    #[test]
    fn confirmation_observed_after_unwind_is_not_lost() {
        let mut owner = Model {
            created: true,
            confirmed: false,
            capability: Some(()),
        };
        let mut panic = None;
        assert_eq!(
            guarded_retirement(
                &mut owner,
                &mut panic,
                |owner| {
                    let _ = owner.capability.take();
                    owner.confirmed = true;
                    std::panic::panic_any("panic after modeled confirmation");
                },
                Model::snapshot
            ),
            RetirementStep::Complete
        );
        assert!(panic.is_some());
    }

    #[test]
    fn snapshot_unwind_cannot_authorize_frame_release() {
        let mut owner = Model {
            created: true,
            confirmed: true,
            capability: Some(()),
        };
        let mut panic = None;
        assert_eq!(
            guarded_retirement(
                &mut owner,
                &mut panic,
                |_| {},
                |_| {
                    std::panic::panic_any("receipt snapshot unavailable");
                }
            ),
            RetirementStep::RetainFrame
        );
        assert!(panic.is_some());
    }

    #[test]
    fn original_panic_survives_secondary_payload_with_panicking_drop() {
        struct Secondary(Arc<AtomicUsize>);
        impl Drop for Secondary {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("secondary payload Drop must not run during cleanup");
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut owner = Model {
            created: true,
            confirmed: false,
            capability: Some(()),
        };
        let mut first: Option<PanicPayload> = Some(Box::new("original setup panic"));
        assert_eq!(
            guarded_retirement(
                &mut owner,
                &mut first,
                |_| {
                    std::panic::panic_any(Secondary(dropped.clone()));
                },
                Model::snapshot
            ),
            RetirementStep::RetryRetained
        );
        assert_eq!(
            first.as_ref().unwrap().downcast_ref::<&str>(),
            Some(&"original setup panic")
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn no_created_child_completes_without_fabricating_confirmation() {
        let mut owner = Model {
            created: false,
            confirmed: false,
            capability: None,
        };
        let mut panic = None;
        assert_eq!(
            guarded_retirement(&mut owner, &mut panic, |_| {}, Model::snapshot),
            RetirementStep::Complete
        );
        assert!(!owner.confirmed);
        assert!(panic.is_none());
    }
}

#[test]
fn mapped_genuine_initializer_returns_before_guest() -> io::Result<()> {
    measured_v11::run(MappedCase::Initialize)
}
