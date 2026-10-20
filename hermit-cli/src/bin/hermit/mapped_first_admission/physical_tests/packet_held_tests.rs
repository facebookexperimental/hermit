//! Genuine held-first-exec packet controls. Never enters a private runtime,
//! loader instruction, ordinary program instruction or an RPC handshake.
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use detcore::detlog::NativePhysicalWatchIdentity;
use reverie::GlobalTool;
use reverie_inguest::launcher::exec_stop::ChildOwner;
use reverie_inguest::launcher::exec_stop::CleanupReceipt;
use reverie_inguest::launcher::exec_stop::ExecStop;
use reverie_inguest::launcher::exec_stop::InitialExecStop;
use reverie_inguest::launcher::exec_stop::LaunchFailure;
use reverie_inguest::launcher::exec_stop::PreparedExec;
use tokio::io::AsyncReadExt;

use super::super::packet;
use super::super::packet_lease::PendingPacketLease;
use super::WAIT;
use super::bounded;
use super::need;
use super::rpc;

// The existing owner performs cancellation and bounded terminal waits. This
// guard retains failed cleanup authority rather than turning a failed cancel
// into a successful Drop observation. Explicit cleanup is required for PASS.
struct HeldExec<'fd> {
    stop: Option<InitialExecStop<'fd>>,
    launch_failure: Option<LaunchFailure<'fd>>,
    retired_owner: Option<Box<ChildOwner<'fd>>>,
    receipt: CleanupReceipt,
}
impl<'fd> HeldExec<'fd> {
    fn spawn(
        lease: &'fd PendingPacketLease,
        detlog: &'fd UnixStream,
        inherit_packet: bool,
    ) -> io::Result<Self> {
        let mut inherited = vec![detlog.as_fd()];
        if inherit_packet {
            inherited.push(lease.inherited_fd());
        }
        let request = PreparedExec::new(
            std::path::Path::new("/bin/true"),
            &["genuine-packet-held-control".into()],
            &[("ORIGINAL".into(), "packet-lease".into())],
            None,
            inherited,
        )
        .map_err(rpc)?;
        let receipt = CleanupReceipt::default();
        let mut held = Self {
            stop: None,
            launch_failure: None,
            retired_owner: None,
            receipt: receipt.clone(),
        };
        match ExecStop::spawn(request, receipt) {
            Ok(stop) => held.stop = Some(stop),
            Err(failure) => held.launch_failure = Some(failure),
        }
        Ok(held)
    }
    fn ready(&self) -> io::Result<&InitialExecStop<'fd>> {
        self.stop.as_ref().ok_or_else(|| {
            rpc(format!(
                "real first exec did not stop: {:?}",
                self.launch_failure
            ))
        })
    }
    fn cancel(&mut self) -> io::Result<()> {
        if let Some(stop) = self.stop.take() {
            match stop.cancel() {
                Ok(state) => need(state.confirmed, "actual stop cancel unconfirmed")?,
                Err(owner) => self.retired_owner = Some(owner),
            }
        }
        if let Some(failure) = self.launch_failure.as_mut() {
            // One existing bounded retry; never discard an unresolved owner.
            let _ = failure.retry_cleanup();
        }
        let state = self.receipt.snapshot();
        need(
            self.retired_owner.is_none() && (!state.child_created || state.confirmed),
            "real held child terminal wait remains unconfirmed",
        )
    }
}
impl Drop for HeldExec<'_> {
    fn drop(&mut self) {
        // Emergency fallback only. No PASS or watch-drain claim comes from Drop.
        let _ = self.cancel();
    }
}

use super::super::runtime_support::admit;
use super::super::runtime_support::exact_storage;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Positive,
    WrongBirth,
    OmittedPacket,
    RepeatedOtherAdmission,
}

async fn run(case: Case) -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    // Same retained-directory transport as existing admission tests; the PID
    // here is only a pathname coordinate, never a birth/ownership authority.
    let directory_fd = File::open(directory.path())?;
    let socket = |name: &str| {
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
        in_guest_detlog_forward_policy: Some("INFO".to_owned()),
        ..Default::default()
    };
    need(
        packet::config_violation(&config).is_none(),
        "real Config outside ordinary domain",
    )?;
    let global = Arc::new(<detcore::GlobalState as GlobalTool>::init_global_state(&config).await);
    let lease_a = PendingPacketLease::new()?;
    let lease_b = PendingPacketLease::new()?;
    let (sender_a, receiver_a) = UnixStream::pair()?;
    let (sender_b, receiver_b) = UnixStream::pair()?;
    receiver_a.set_nonblocking(true)?;
    receiver_b.set_nonblocking(true)?;
    let mut receiver_a = tokio::net::UnixStream::from_std(receiver_a)?;
    let mut receiver_b = tokio::net::UnixStream::from_std(receiver_b)?;
    let mut held_a = None;
    let mut held_b = None;
    let mut admissions = Vec::new();
    let observed: io::Result<()> = async {
        held_a = Some(HeldExec::spawn(
            &lease_a,
            &sender_a,
            case != Case::OmittedPacket,
        )?);
        let stop_a = held_a
            .as_ref()
            .ok_or_else(|| rpc("held A absent"))?
            .ready()?;
        let before = stop_a.capture_initial_inputs().map_err(rpc)?;
        let a = admit(
            &mut admissions,
            &global,
            &config,
            &socket("a"),
            stop_a,
            &sender_a,
        )
        .await?;
        let mut b = None;
        if matches!(case, Case::WrongBirth | Case::RepeatedOtherAdmission) {
            held_b = Some(HeldExec::spawn(&lease_b, &sender_b, true)?);
            let stop_b = held_b
                .as_ref()
                .ok_or_else(|| rpc("held B absent"))?
                .ready()?;
            let actual_b = admit(
                &mut admissions,
                &global,
                &config,
                &socket("b"),
                stop_b,
                &sender_b,
            )
            .await?;
            need(
                a.watched_process().birth_identity() != actual_b.watched_process().birth_identity(),
                "wrong-birth control did not create distinct real births",
            )?;
            b = Some(actual_b);
        }
        match case {
            Case::Positive | Case::RepeatedOtherAdmission => {
                let expected = a.initial_packet()?;
                let sealed = lease_a.fill(stop_a, &a)?;
                need(
                    sealed.fd().as_raw_fd() == lease_a.inherited_fd().as_raw_fd(),
                    "packet lease replaced original descriptor",
                )?;
                exact_storage(&sealed, &expected)?;
                sealed.verify(stop_a)?;
                let packet = packet::decode(sealed.bytes()).map_err(rpc)?;
                need(
                    packet.coordinator == socket("a").as_os_str().as_bytes(),
                    "actual packet lost bound listener",
                )?;
                // capture_initial_inputs performs fresh actual held-task register,
                // xstate/mask and original-stack readback internally. Comparing
                // public evidence is additional observation, not a raw Stopped escape.
                let after = stop_a.capture_initial_inputs().map_err(rpc)?;
                need(
                    before.registers() == after.registers()
                        && before.stack_start() == after.stack_start()
                        && before.stack_bytes() == after.stack_bytes()
                        && before.maps_snapshots() == after.maps_snapshots(),
                    "packet storage changed original held context/stack/maps",
                )?;
                need(
                    !a.failed.load(std::sync::atomic::Ordering::Acquire)
                        && !a.initial_used.load(std::sync::atomic::Ordering::Acquire),
                    "storage positive consumed nonce or poisoned admission",
                )?;
                if case == Case::RepeatedOtherAdmission {
                    let b = b.as_ref().ok_or_else(|| rpc("real B admission absent"))?;
                    let error = match lease_a.fill(stop_a, b) {
                        Err(error) => error,
                        Ok(_) => return Err(rpc("second fill unexpectedly succeeded")),
                    };
                    need(
                        error.to_string().contains("already attempted"),
                        "repeat failed at another predicate",
                    )?;
                    need(
                        b.failed.load(std::sync::atomic::Ordering::Acquire),
                        "B not poisoned",
                    )?;
                    need(
                        !a.failed.load(std::sync::atomic::Ordering::Acquire),
                        "repeat B error unexpectedly used A instead",
                    )?;
                    let error = sealed
                        .verify(stop_a)
                        .err()
                        .ok_or_else(|| rpc("old A lease survived failed B fill"))?;
                    need(
                        error.to_string().contains("permanently poisoned"),
                        "old A failed at another predicate",
                    )?;
                    need(
                        a.failed.load(std::sync::atomic::Ordering::Acquire),
                        "old A verify did not poison real A",
                    )?;
                }
            }
            Case::WrongBirth => {
                let b = b.as_ref().ok_or_else(|| rpc("real B admission absent"))?;
                let error = match lease_a.fill(stop_a, b) {
                    Err(error) => error,
                    Ok(_) => return Err(rpc("wrong actual birth accepted")),
                };
                need(
                    error.to_string().contains("different or failed birth"),
                    "wrong birth failed at another guard",
                )?;
                need(
                    b.failed.load(std::sync::atomic::Ordering::Acquire),
                    "wrong-birth B not poisoned",
                )?;
            }
            Case::OmittedPacket => {
                let error = match lease_a.fill(stop_a, &a) {
                    Err(error) => error,
                    Ok(_) => return Err(rpc("omitted inherited descriptor accepted")),
                };
                need(
                    error
                        .to_string()
                        .contains("exec_stop_descriptor_not_inherited"),
                    "omitted descriptor failed at another guard",
                )?;
                need(
                    a.failed.load(std::sync::atomic::Ordering::Acquire),
                    "omitted-inherit A not poisoned",
                )?;
            }
        }
        Ok(())
    }
    .await;

    // Evaluate every ordinary-error cleanup path without '?' short-circuit.
    let cancel_a = held_a.as_mut().map_or(Ok(()), HeldExec::cancel);
    let cancel_b = held_b.as_mut().map_or(Ok(()), HeldExec::cancel);
    let mut cleanup = Vec::new();
    for (name, outcome) in [("A", cancel_a), ("B", cancel_b)] {
        if let Err(error) = outcome {
            cleanup.push(format!("{name} terminal wait: {error}"));
        }
    }
    for slot in &mut admissions {
        if observed.is_err() {
            slot.owner.fail(&"held packet observation failed");
        }
        let drain = slot.owner.exit_admission().drain(WAIT).await;
        if let Err(error) = &drain {
            cleanup.push(format!("watch drain: {error:?}"));
        }
        if drain.is_ok() {
            if slot.owner.exit_admission().reported_exits()
                != [slot.owner.watched_process().process_id()]
            {
                cleanup.push("watch did not report its actual held child exactly once".into());
            }
            slot.drained = true;
            // The server was actually bound/configured, never served or replaced.
        }
        if slot
            .issues
            .as_ref()
            .is_some_and(|issues| !issues.snapshot().is_empty())
        {
            cleanup.push("unserved actual listener gained a transport issue".into());
        }
    }
    // HeldExec's type retains the borrowed PreparedExec lifetime even after
    // successful cancel. Drop these guards BEFORE dropping parent DETLOG copies.
    drop(held_a);
    drop(held_b);
    drop(sender_a);
    drop(sender_b);
    for (name, receiver) in [("A", &mut receiver_a), ("B", &mut receiver_b)] {
        let eof = bounded(async {
            let mut byte = [0];
            need(
                receiver.read(&mut byte).await? == 0,
                "actual inherited DETLOG OFD not at EOF",
            )
        })
        .await;
        if let Err(error) = eof {
            cleanup.push(format!("{name} DETLOG EOF: {error}"));
        }
    }
    // Admissions/server paths drop while the retained directory FD still exists.
    drop(admissions);
    need(
        cleanup.is_empty(),
        "held packet physical cleanup unconfirmed",
    )
    .map_err(|error| {
        rpc(format!(
            "{error}: {}; observation={observed:?}",
            cleanup.join("; ")
        ))
    })?;
    observed
}

#[tokio::test(flavor = "current_thread")]
async fn genuine_packet_at_first_exec_matches_admission_and_cancels() {
    run(Case::Positive).await.unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn genuine_packet_wrong_actual_birth_refuses_and_drains_both() {
    run(Case::WrongBirth).await.unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn genuine_packet_omitted_inherited_descriptor_refuses_and_cancels() {
    run(Case::OmittedPacket).await.unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn genuine_packet_failed_repeat_with_real_b_invalidates_prior_a() {
    run(Case::RepeatedOtherAdmission).await.unwrap();
}

// Retained V11 bytes are mapped/cancelled only; no image instruction executes.
mod mapped_cancel;
