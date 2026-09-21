/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Bound the gdbserver accept on the LIVENESS OF THE CLIENT HERMIT SPAWNED.
//!
//! ⚠️ WHY THIS EXISTS: HERMIT WAITS FOREVER FOR A GDB THAT IS ALREADY GONE.
//! `hermit record --verify-with-gdbex` and `hermit replay` under gdb both spawn a
//! `gdb` client and then start a container whose gdbserver waits for it. That
//! wait is an unbounded `listener.accept().await` in
//! `reverie-ptrace/src/gdbstub/server.rs`; nothing bounds it, and
//! `--record-timeout` arms the recording only. If the client exits or dies
//! WITHOUT completing its connection, the accept never returns and hermit never
//! returns with it.
//!
//! Reproduced with no kill and no signal: put a `gdb` on `PATH` that exits 0
//! without connecting. Observed live, the outer process alive in
//! `anon_pipe_read`, the container child alive in `epoll_wait` owning the LISTEN
//! socket, and gdb a zombie holding nothing.
//!
//! ⚠️ AND HERMIT SETS UP THE RACE ITSELF, which is why this is not exotic. The
//! client is spawned BEFORE the container that binds the port, so gdb can fail
//! its own `target remote` connect and exit on its own. Under load that window
//! widens; three such wedges were observed during one parallel test run while a
//! quiet run passed in a second.
//!
//! ⚠️ A BOUND ON THE ACCEPT, NOT A TIMEOUT ON THE RUN. The difference is the
//! whole design:
//!
//! * a bound keyed on the client asks an OBSERVABLE FACT — hermit spawned that
//!   process and holds its [`Child`], so "my client is gone" is a `wait`, not an
//!   estimate. It is correct under any load, and it cannot fire while a healthy
//!   session is in progress;
//! * a timeout guesses how long a connection ought to take, and kills a run that
//!   may be perfectly healthy. It is wrong precisely under load, which is when
//!   this fires.
//!
//! ⚠️ AND IT MUST NOT BE APPLIED WHERE THE WAIT IS CORRECT. `hermit run
//! --gdbserver` uses the same accept and SHOULD wait indefinitely: a human
//! attaches later, and there is no spawned client whose death could be observed.
//! The distinction is not which call site it is, but whether there is a client
//! whose liveness we own — which a timeout cannot tell apart and a [`Child`] can.

use std::net::SocketAddr;
use std::net::TcpStream;
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

/// Context added to a container failure when the spawned client had already
/// exited, so the error names the cause rather than the symptom.
pub const CLIENT_EXITED_BEFORE_CONNECTING: &str = "the gdb client hermit spawned exited before it finished connecting to the \
     gdbserver, so the replay had no debugger to serve";

/// How often to retry the release connection while the client is gone and the
/// container is still running.
///
/// ⚠️ THIS IS NOT A TIMEOUT AND IT BOUNDS NOTHING. The loop it paces exits on the
/// container finishing or the connection succeeding, both facts; the interval
/// only decides how promptly a released accept is noticed. The port may not be
/// bound yet when the client dies — hermit spawns the client first — so the
/// release cannot be a single attempt.
const RELEASE_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// How often the watcher asks whether the client has exited.
///
/// ⚠️ THIS EXISTS BECAUSE A BLOCKING `wait()` CANNOT BE INTERRUPTED, and that is
/// what made the first version of this file able to hang. Polling costs one
/// `waitpid(WNOHANG)` per interval and buys the ability to stop watching, which
/// a blocking wait does not sell at any price.
const CLIENT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Bound on ONE release-connect attempt.
///
/// ⚠️ THIS IS NOT A TIMEOUT ON ANYTHING THAT MATTERS. The loop retries, and it
/// exits on the container finishing or on a connect succeeding -- both facts.
/// This only stops a single `connect` to a saturated accept queue from stalling
/// for minutes on the path `finish()` now joins. Localhost either refuses
/// instantly or completes instantly; a stall here means the peer is not the one
/// we are looking for, so retrying is the right response to it.
const RELEASE_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to pause after a successful release connect before trying again.
///
/// ⚠️ THIS IS A RATE LIMIT AND NOTHING ELSE. IT DISCRIMINATES NOTHING. An earlier
/// revision of this comment claimed it was "the difference between WE CONNECTED
/// and WE RELEASED OURS", on the strength of a `done` check inside the grace
/// loop. That check is gone — see the loop body for why `done` is CORRELATION,
/// NOT AUTHENTICATION — but the claim outlived the mechanism and sat here
/// describing code that no longer existed. A doc comment asserting a property
/// the code dropped is worse than no comment: it is the first thing a reader
/// trusts.
///
/// What it actually buys: one connect per grace period to a peer that may not be
/// ours, instead of one every [`RELEASE_RETRY_INTERVAL`]. Without it the loop
/// spins as fast as `connect` returns — measured 16714 attempts in a window that
/// admits ~2 with the pause.
///
/// 25 × 20ms is a FLOOR of 500ms, not a bound: `thread::sleep` may overshoot, so
/// there is no upper limit on the pause. Long enough that a stranger is
/// contacted rarely, short enough that it is re-probed rather than abandoned. It
/// does not extend the saturated-listener bound, because that path never gets a
/// successful connect to start a window from.
const RELEASE_GRACE_TICKS: u32 = 25;

/// Watches the gdb client hermit spawned, and releases the gdbserver's accept if
/// that client dies while the container is still waiting for it.
///
/// Reaping the client is this type's job too: the previous code called
/// `gdb_client.wait()` AFTER the container result was propagated with `?`, so
/// every error path left an unreaped `gdb`. Owning the [`Child`] here means the
/// reap happens on every path, including the early returns.
pub struct GdbClientWatch {
    container_done: Arc<AtomicBool>,
    client_exited_early: Arc<AtomicBool>,
    // Test synchronization: set only after the watcher has reaped the client
    // and confirmed that the container has not finished.
    #[cfg(test)]
    client_exited_before_container_finished: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
}

impl GdbClientWatch {
    /// Take ownership of the spawned client and start watching it.
    ///
    /// `port` is the gdbserver port the container is about to listen on. The
    /// watcher connects to it ONLY after observing the client exit, and only
    /// while the container is still running.
    pub fn spawn(mut client: Child, port: u16) -> Self {
        let container_done = Arc::new(AtomicBool::new(false));
        let client_exited_early = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&container_done);
        let exited_early = Arc::clone(&client_exited_early);
        #[cfg(test)]
        let client_exited_before_container_finished = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let observed_exit_before_finish = Arc::clone(&client_exited_before_container_finished);

        let watcher = thread::spawn(move || {
            // ⚠️ POLL, DO NOT BLOCK. This was `client.wait()`, which blocks until
            // the client exits and cannot be woken by `container_done`. That made
            // `finish()` — which joined this thread — unable to return whenever
            // gdb outlived the container, WHICH IS THE ORDINARY ORDERING. A
            // watcher written to stop a hang could hang, exactly when the thing
            // it watches for happened.
            //
            // `try_wait` reaps on the same call, so the client is still reaped on
            // every path; the loop simply also gets to notice the container.
            loop {
                match client.try_wait() {
                    // Exited and reaped. Fall through to the report/release
                    // decision below.
                    Ok(Some(_)) => break,
                    Ok(None) => {}
                    // We can no longer observe this child, so we can say nothing
                    // about it. Claiming an early exit here would be inventing a
                    // cause; leave the flag false and let the container speak.
                    Err(_) => return,
                }
                if done.load(Ordering::SeqCst) {
                    // ⚠️ THE SUCCESS-PATH WAIT, RESTORED HERE. The container
                    // finished while the client is still alive: not an early
                    // exit, nothing to release. Before the watcher existed, both
                    // call sites ended with `let _ = gdb_client.wait();` at
                    // exactly this point, so hermit did not return while the gdb
                    // it spawned was still running. Two rewrites of this file
                    // dropped that -- the first silently, the second by detaching
                    // -- and neither argued for the change.
                    //
                    // Waiting HERE rather than in `finish()` is what keeps it
                    // compatible with the defect this file exists to prevent: the
                    // release decision is already made (there is nothing to
                    // release), so nothing downstream is gated on the client. See
                    // `finish` and `drop` for which of the two joins.
                    let _ = client.wait();
                    return;
                }
                thread::sleep(CLIENT_POLL_INTERVAL);
            }

            // ⚠️ RE-READ `done` AT THE MOMENT OF THE DECISION, NOT BEFORE IT.
            // A gdb that finishes normally a moment before the container returns
            // is a HEALTHY teardown. The first version set the flag on the
            // strength of a `done` read taken at client-exit time and then
            // attached "the client exited before connecting" to whatever error
            // the container produced — a false cause on a real failure, which
            // sends the next reader hunting a bug that does not exist.
            if done.load(Ordering::SeqCst) {
                return;
            }
            #[cfg(test)]
            observed_exit_before_finish.store(true, Ordering::SeqCst);

            // ⚠️ THE CLIENT IS GONE AND THE CONTAINER IS NOT. Whatever the
            // container is doing, no debugger will ever attach to it, so a
            // gdbserver blocked in accept() is blocked forever. One connection
            // releases it; the peer closing immediately is what tells the
            // gdbstub the session is over.
            // ⚠️ THE FLAG IS SET WHEN WE ACTUALLY RELEASE A BLOCKED ACCEPT, NOT
            // WHEN THE CLIENT EXITS. This is the correction that closes defect 2,
            // and re-reading `done` a moment later does NOT close it: if gdb
            // genuinely exits a moment before the container returns — a HEALTHY
            // teardown — then `done` is false at client-exit time and false again
            // an instant later, so any check taken at that moment flags it.
            //
            // "Did the client exit?" is the wrong question. "Was the container
            // still waiting for a client that will never come?" is the right one,
            // and a successful release connect is the evidence for it: it means
            // an accept was pending with nobody coming. If instead the container
            // finishes on its own while we retry, the loop below exits on `done`
            // and we say nothing — which is exactly the healthy case that used to
            // be reported as "exited before connecting".
            while !done.load(Ordering::SeqCst) {
                // ⚠️ THIS CONNECT IS UNAUTHENTICATED AND THE PORT IS GUESSABLE:
                // the call sites use `16384 + gettid() % 1024`. If the container's
                // listener is gone and an unrelated process has taken that port,
                // this connects to a stranger and drops it.
                //
                // ⚠️ AND MOVING THE FLAG ONTO THIS CONNECT RAISED THE STAKES, which
                // is new: a foreign listener now also produces a FALSE "the client
                // exited before connecting" line, not merely a stray connection.
                // Judged worth it — the alternative was reporting that false cause
                // on EVERY healthy teardown rather than on a port collision — but
                // it is a real trade and not a free win. The fix is to identify
                // the listener rather than to guess the port, which changes an
                // interface; tracked as `gdb_watcher_release_probe`.
                //
                // Refused simply means the container has not bound the port yet
                // — expected, because the client is spawned before the container
                // exists. Retry until it binds or the container finishes.
                // ⚠️ AND IT IS BOUNDED BECAUSE `finish()` NOW JOINS THIS THREAD.
                // A bare `TcpStream::connect` has no timeout: against a listener
                // whose accept queue is full the SYN is dropped and the call
                // stalls for minutes. While this thread was detached that only
                // delayed a background thread; a join puts it on hermit's own
                // return path, so the stall has to be bounded. This is a bound on
                // ONE CONNECT ATTEMPT inside a loop that already retries -- not a
                // timeout on the run, and not on the accept.
                let peer = SocketAddr::from(([127, 0, 0, 1], port));
                if let Ok(stream) = TcpStream::connect_timeout(&peer, RELEASE_CONNECT_TIMEOUT) {
                    drop(stream);
                    // ⚠️ UNCHANGED FROM MAIN, AND THIS PR DOES NOT CLOSE THE
                    // FALSE REPORT IT CAN PRODUCE. Main's comment here said "now
                    // the report is earned"; that is the claim the block below
                    // refutes, so it is gone rather than restated. A stranger's
                    // accept still latches this flag, and nothing lowers it --
                    // `store(false)` appears nowhere in this file. If the
                    // container then fails for its own reasons, the operator is
                    // told the client exited before connecting, which is defect 2
                    // of hermit#2654 reached through a collision instead of a
                    // race.
                    //
                    // It is left exactly as main has it because every fix
                    // available inside this watcher is worse than the disease:
                    // see the block below for why `done` cannot authenticate the
                    // peer. Closing it needs the listener IDENTIFIED rather than
                    // the port guessed -- an interface change, tracked as
                    // `gdb_watcher_release_probe`. THIS HEAD FIXES THE HANG AND
                    // LEAVES THE REPORT EXACTLY AS IT FOUND IT.
                    exited_early.store(true, Ordering::SeqCst);

                    // ⚠️ BUT A SUCCESSFUL CONNECT IS AN ATTEMPT, NOT A CONCLUSION,
                    // AND RETURNING HERE RESTORES THE HANG THIS TYPE PREVENTS.
                    // The port is guessable and shared -- `record_start` derives
                    // `16384 + tid % 1024` and `replay` defaults to 1234 -- so in
                    // exactly the window this loop exists for, between the client
                    // dying and the container binding, an unrelated local process
                    // can own it. Connecting there proves we reached A listener,
                    // never that we reached OURS. Returning left our gdbserver
                    // blocked in accept() with nobody coming, and stopped the one
                    // thread that would have released it -- the original defect,
                    // now with the watcher reporting success.
                    //
                    // ⚠️ SO KEEP TRYING -- AND DO NOT READ THIS PAUSE AS A
                    // DISCRIMINATOR. It is a RATE LIMIT and nothing more: one
                    // connect per grace period to a peer that may not be ours,
                    // instead of one every RELEASE_RETRY_INTERVAL. The stranger
                    // test pins it from both sides, a floor on the retries and a
                    // ceiling on the rate.
                    //
                    // ⚠️ AN EARLIER REVISION CHECKED `done` INSIDE THIS LOOP AND
                    // PRESENTED IT AS EVIDENCE THE REPORT WAS EARNED. Removed,
                    // because it was neither. `agent(hermit-001)` measured that
                    // deleting only that check left all seven cells green, so the
                    // line the comment block existed to justify was untested; and
                    // `agent(codex-rev-2678)` gave the reason it could never have
                    // carried that weight -- `done` IS CORRELATION, NOT
                    // AUTHENTICATION. A local process can bind the predictable
                    // port, accept our probe, and thereby make reverie's real bind
                    // FAIL; that container failure sets `done`. One adversary
                    // produces both halves of "we connected and then it finished",
                    // so no arrangement of a `done` check distinguishes a release
                    // from a collision. Waiting for it only made the false report
                    // look earned.
                    // ⚠️ CHECK `done` BETWEEN TICKS, OR `finish()` JOINS UP TO A
                    // FULL GRACE PERIOD LATE. `finish()` sets `done` and then
                    // joins this thread, so a flat 25-tick sleep put up to ~499ms
                    // of dead wait on hermit's own return path for a container
                    // that finished 1ms after a probe. The outer `while !done`
                    // already exits on the same fact; this only stops the sleep
                    // from outliving it, bounding the overshoot to one
                    // RELEASE_RETRY_INTERVAL.
                    //
                    // ⚠️ THIS IS NOT THE `done` CHECK THAT WAS REMOVED. That one
                    // read `done` to decide whether the report was EARNED, which
                    // it cannot do. This one only decides when to stop sleeping;
                    // the flag above is already set either way, so no report
                    // depends on it.
                    for _ in 0..RELEASE_GRACE_TICKS {
                        if done.load(Ordering::SeqCst) {
                            break;
                        }
                        thread::sleep(RELEASE_RETRY_INTERVAL);
                    }
                    continue;
                }
                thread::sleep(RELEASE_RETRY_INTERVAL);
            }
            // `done` won the race: the container finished under its own power, so
            // the client exiting first was an ordinary teardown. Say nothing.
        });

        Self {
            container_done,
            client_exited_early,
            #[cfg(test)]
            client_exited_before_container_finished,
            watcher: Some(watcher),
        }
    }

    /// Stop watching, WAIT FOR THE CLIENT, reap it, and report whether it had
    /// exited while the container was still running.
    ///
    /// Call this once the container run has returned, BEFORE propagating its
    /// result, so a failure can be given the cause rather than the symptom.
    ///
    /// ⚠️ THE WAIT IS A RESTORATION, NOT AN ADDITION, AND IT IS THE POINT OF THIS
    /// CHANGE. Before the watcher existed, both call sites ended with `let _ =
    /// gdb_client.wait();` — hermit did not return while the gdb it had spawned
    /// was still running. The first watcher deleted that silently; the rewrite
    /// that followed replaced the join with a detach, which deletes it again by a
    /// different route. Neither argued for the change, and it is observable: the
    /// shell prompt comes back with gdb still writing to the same terminal, and
    /// the client reparents to init.
    ///
    /// ⚠️ AND THE JOIN CANNOT REINTRODUCE THE BLOCK IT REPLACED, which is the
    /// distinction the whole file turns on. The old block was a wait taken BEFORE
    /// the release decision: the watcher's first act was `client.wait()`, so it
    /// could not notice the container, could not release the accept, and
    /// `finish()` inherited all of it — on error paths too, via `Drop`. The wait
    /// this joins is taken AFTER that decision, on the branch where the container
    /// has already finished and there is by construction nothing to release.
    ///
    /// ⚠️ "IT GATES NOTHING" WOULD BE FALSE, SO IT IS NOT CLAIMED. The join also
    /// covers the release loop, and `agent(hermit-dbgrev14)` measured the case:
    /// client dead AND reaped, watcher parked in `connect_timeout` against a
    /// saturated stranger, `finish()` blocked anyway — 741ms. It is BOUNDED, at
    /// one `RELEASE_CONNECT_TIMEOUT` plus a poll interval, because `done` is
    /// published before the join and the loop re-reads it every iteration; the
    /// same schedule against a bare `connect` measured 135.5s. Bounded is the
    /// claim. Not gated is not.
    ///
    /// ⚠️ AND ON `record_start.rs` THIS IS AN EXPANSION, NOT PURE RESTORATION —
    /// stated because the word "restore" would otherwise cover it. Pre-watcher,
    /// that file read `… .classified()?;` and THEN `let _ = gdb_client.wait();`,
    /// so a container FAILURE propagated past the wait and hermit never waited at
    /// all. `finish()` is called above the match, so it now waits on both
    /// outcomes. That is deliberate — a gdb outliving a failed container is no
    /// less hermit's child than one outliving a successful container — and it
    /// terminates because `record_start.rs` forces `-batch`, which is load-bearing
    /// and was previously unstated. `replay.rs` bound its result rather than
    /// propagating it, so its wait already ran on both paths: there this is exact
    /// parity, and it is parity with an INTERACTIVE gdb, since `replay.rs` passes
    /// no `-batch`. Hermit waiting for a human's debugger session to end is the
    /// behaviour that file always had.
    ///
    /// The flag read is unchanged in meaning: the watcher only sets it while
    /// `done` is false, and re-reads `done` at the moment of decision, so a
    /// client exiting as the container returns is still treated as the healthy
    /// teardown it is.
    ///
    /// ⚠️ `Drop` DELIBERATELY DOES NOT CALL THIS. See below.
    pub fn finish(&mut self) -> bool {
        self.container_done.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        self.client_exited_early.load(Ordering::SeqCst)
    }

    /// Stop watching WITHOUT waiting for anything.
    fn abandon(&mut self) {
        self.container_done.store(true, Ordering::SeqCst);
        // Detach. Dropping the handle does not stop the thread; it stops US
        // waiting on it, which is the whole point.
        self.watcher.take();
    }
}

impl Drop for GdbClientWatch {
    /// ⚠️ THE REAP MUST SURVIVE AN EARLY `?`. Building the container can fail
    /// between the spawn and the run, and the original code's `gdb_client.wait()`
    /// sat below that point, so such a path left an orphan. Dropping this signals
    /// the watcher, which owns the client and reaps it.
    ///
    /// ⚠️ AND IT DETACHES WHERE `finish` JOINS, WHICH IS THE WHOLE DIFFERENCE
    /// BETWEEN THE TWO PATHS. `finish` runs when the container has returned, so
    /// waiting for the client is what hermit always did. `Drop` runs when it has
    /// NOT — an early `?` means there may be no container coming at all, so a
    /// client still trying to connect to a port nobody will ever bind would never
    /// exit, and joining here would hang the error exit. That is the original
    /// defect rebuilt one level down, on the paths that were merely returning an
    /// error. The detached watcher still reaps the client; nobody waits for it.
    fn drop(&mut self) {
        self.abandon();
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Instant;

    use super::*;

    /// `finish()` must not sit out a grace period it no longer needs.
    ///
    /// ⚠️ THE GRACE SLEEP IS ON HERMIT'S OWN RETURN PATH, because `finish()` sets
    /// `container_done` and then JOINS this thread. A flat `for _ in
    /// 0..RELEASE_GRACE_TICKS { sleep }` cannot notice the container finishing, so
    /// hermit waited out the remainder of a pause whose only purpose is to slow
    /// down contacting a stranger. MEASURED on this test, entering the sleep and
    /// then calling `finish()`:
    ///
    ///     without the `done` check .... 381.7ms
    ///     with it ..................... 0.52ms
    ///
    /// The bound is one `RELEASE_RETRY_INTERVAL` (20ms) plus scheduling, so 100ms
    /// is generous while still an order of magnitude below the regression.
    ///
    /// ⚠️ THIS IS NOT THE `done` CHECK THAT WAS REMOVED FROM THIS LOOP. That one
    /// read `done` to decide whether the early-exit report was EARNED, which it
    /// cannot do -- `done` is correlation, not authentication. This one decides
    /// only when to stop sleeping, and no report depends on it.
    #[test]
    fn finish_does_not_wait_out_a_grace_period_after_the_container_is_done() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
        let port = listener.local_addr().expect("no local addr").port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => drop(stream),
                    Err(_) => break,
                }
            }
        });

        let client = std::process::Command::new("/bin/true")
            .spawn()
            .expect("failed to spawn the stand-in client");
        // Let the client exit, so the watcher reaches the release loop.
        thread::sleep(Duration::from_millis(200));
        let mut watch = GdbClientWatch::spawn(client, port);
        // Long enough for one successful connect to land the watcher INSIDE the
        // grace sleep, short enough that the sleep is still running.
        thread::sleep(Duration::from_millis(120));

        let started = std::time::Instant::now();
        let _ = watch.finish();
        let lag = started.elapsed();

        assert!(
            lag < Duration::from_millis(100),
            "finish() took {lag:?} to join after setting container_done; the grace \
             sleep is ignoring `done` and is charging hermit's return path for a \
             pause that only exists to rate-limit contacting a stranger"
        );
    }

    /// Connecting to a listener that is not ours must not end the watch.
    ///
    /// ⚠️ THIS IS THE HANG, RESTORED BY A SUCCESSFUL CONNECT, and it is live on
    /// `main` without the grace window: the release port is guessable
    /// (`16384 + tid % 1024`, or 1234 for `replay`), so an unrelated process can
    /// own it in exactly the window the release loop exists for. A watcher that
    /// concludes on the first successful connect then stops watching while our
    /// own gdbserver is still blocked in `accept()` with nobody coming.
    ///
    /// ⚠️ THE DISCRIMINATOR IS THE RETRY, NOT THE FLAG, and that is deliberate.
    /// An earlier version of this fix tried to tell "we released ours" from "a
    /// stranger accepted" by reading the socket for a prompt EOF. Measured, that
    /// probe cannot do it: our probe reads BEFORE it disconnects, so a gdbstub
    /// that has just accepted has no reason to close, and the genuine-release and
    /// already-served cases BOTH read as not-accepted. Asserting a retry count
    /// asks only what this loop actually controls.
    ///
    /// ⚠️ AND THE NAME NOW SAYS "STILL LATCHES THE REPORT", BECAUSE IT DOES. This
    /// was called `a_connect_to_a_stranger_is_not_treated_as_a_release` while
    /// discarding `watch.finish()` and never reading the flag — so the test named
    /// for the defect did not assert it, and `assert!(!watch.finish())` fails
    /// here. A stranger IS still treated as a release for reporting purposes.
    /// Naming the test for the property it has, and asserting the gap it does
    /// not close, is the difference between coverage and the appearance of it.
    ///
    /// Bounded, and asserts a count rather than blocking, so a regression is a
    /// named red and not a wedged runner.
    #[test]
    fn a_stranger_does_not_end_the_release_attempts_but_still_latches_the_report() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
        let port = listener.local_addr().expect("no local addr").port();

        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        drop(stream);
                    }
                    Err(_) => break,
                }
            }
        });

        let client = std::process::Command::new("/bin/true")
            .spawn()
            .expect("failed to spawn the stand-in client");
        // Let it exit first, so the watcher observes the exit rather than racing.
        thread::sleep(Duration::from_millis(200));

        let mut watch = GdbClientWatch::spawn(client, port);
        // The container is DELIBERATELY never marked done: the stranger did not
        // release our accept, because it never had it.
        //
        // ⚠️ A FIXED WALL-CLOCK WINDOW, NOT ONE DERIVED FROM THE CONSTANT UNDER
        // TEST. This slept `RELEASE_RETRY_INTERVAL * (RELEASE_GRACE_TICKS + 10)`,
        // so the observation window SCALED WITH THE PACE IT WAS MEASURING and the
        // attempt count stayed ~2 for every setting. Measured: RELEASE_GRACE_TICKS
        // of 2, 5, 25, 50 and 100 ALL PASSED — a 40ms grace hammers a stranger
        // 12.5x harder than a 500ms one and the test could not see it. A test
        // whose window moves with its subject pins nothing.
        //
        // 1600ms admits 1 + floor(1600/500) = 4 attempts at the current pace. The
        // bounds below are set to catch a 2x move in either direction while
        // leaving room for `thread::sleep` overshoot.
        const OBSERVATION_WINDOW: Duration = Duration::from_millis(1600);
        thread::sleep(OBSERVATION_WINDOW);
        let attempts = accepted.load(Ordering::SeqCst);

        // Let the watcher finish before asserting, so a failure reports a count
        // rather than leaving a thread running under the harness.
        watch.container_done.store(true, Ordering::SeqCst);
        let reported_early = watch.finish();

        assert!(
            attempts >= 3,
            "the watcher made {attempts} connection(s) to a peer that never released \
             anything; a successful connect must be an ATTEMPT, not a conclusion, or a \
             stranger on a guessable port silently restores the hang"
        );

        // ⚠️ AND A CEILING, BECAUSE THE FLOOR ALONE LEFT THE PAUSE UNTESTED.
        // `agent(hermit-001)` measured that the grace loop's body could be gutted
        // with all seven cells still green: the floor pins "it retried" and says
        // nothing about the rate, which is the only property the pause actually
        // has. Without the pause the loop spins as fast as connect returns:
        // MEASURED 16714 attempts in a comparable window.
        assert!(
            attempts <= 6,
            "the watcher made {attempts} connections in {OBSERVATION_WINDOW:?}; the \
             pause is a RATE LIMIT on contacting a peer that may not be ours, and an \
             unpaced retry loop hammers a stranger every RELEASE_RETRY_INTERVAL"
        );

        // ⚠️ THE OPEN DEFECT, PINNED RATHER THAN LEFT UNSTATED. This test is named
        // for a release and until now discarded `finish()` entirely, so it never
        // touched the report at all: `assert!(!reported_early)` FAILS here, and
        // the test named for the defect did not assert it.
        //
        // A stranger's accept still latches `client_exited_early` at the connect
        // site, and nothing lowers it — `store(false)` appears nowhere in this
        // file. So hermit tells the operator "the client exited before
        // connecting" on a port collision. THIS HEAD DOES NOT FIX THAT; it fixes
        // the hang, and this assertion states the remaining gap in executable
        // form so it cannot change unnoticed in either direction.
        //
        // Closing it needs the listener IDENTIFIED rather than the port guessed —
        // `done` is correlation not authentication, `peer_addr` cannot separate
        // two loopback listeners, and an EOF probe was measured non-
        // discriminating. Tracked as `gdb_watcher_release_probe`.
        //
        // ⚠️ WHOEVER FIXES IT: this assertion is expected to fail, and the fix is
        // to INVERT it, not to delete it.
        assert!(
            reported_early,
            "a stranger no longer latches the false early-exit report -- if that is \
             deliberate, invert this assertion and retire `gdb_watcher_release_probe`; \
             if it is accidental, the report has become silent instead of correct"
        );
    }

    /// A client that exits without connecting must release an accept that is
    /// already waiting.
    ///
    /// This is the hang, in miniature: a listener with nobody coming. Without
    /// the watcher, `accept()` here never returns.
    #[test]
    fn a_client_that_exits_without_connecting_releases_a_waiting_accept() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        let client = std::process::Command::new("/bin/true")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);

        // ⚠️ BOUNDED, BECAUSE A HANGING TEST IS WORSE THAN A RED ONE. The first
        // version called the blocking `accept()` and argued that hanging was "the
        // honest failure for this property, since the defect under test IS a
        // hang". That reasoning is wrong in a suite: a red test names itself in
        // one line, while a wedged one consumes the whole run's budget and is
        // reported as a timeout somewhere else entirely.
        listener
            .set_nonblocking(true)
            .expect("failed to set the test listener non-blocking");
        let deadline = Instant::now() + Duration::from_secs(30);
        let accepted = loop {
            match listener.accept() {
                Ok(pair) => break Some(pair),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        };
        assert!(
            accepted.is_some(),
            "the watcher did not release a waiting accept within 30s after the client exited; \\
             the release is broken and the gdbserver would block forever"
        );

        assert!(
            watch.finish(),
            "the client exited while the container was still running, so that must be reported"
        );
    }

    /// The watcher must NOT report an early exit when the container finished
    /// first, or every healthy gdb session would be labelled a failure -- and
    /// `finish()` must WAIT for that client rather than walking away from it.
    ///
    /// ⚠️ THIS TEST ASSERTED THE OPPOSITE ONE COMMIT AGO, and the reversal is the
    /// substance of this change. It read:
    ///
    /// ```text
    /// assert!(elapsed < Duration::from_secs(2),
    ///     "finish() took {elapsed:?}; it must not wait on a client that outlives the container");
    /// ```
    ///
    /// That encoded "hermit must not wait for the gdb it spawned" as a
    /// requirement. It never was one: before this file existed, both call sites
    /// ended with `let _ = gdb_client.wait();` and hermit waited. The requirement
    /// the assertion was reaching for is real, but it belongs on `Drop` -- see
    /// `dropping_a_watch_never_waits_for_the_client` -- because the defect was a
    /// wait taken BEFORE the release decision, on paths that were merely
    /// returning an error. Telling those two waits apart is the whole change.
    #[test]
    fn finish_waits_for_a_client_that_outlives_the_container() {
        // Bound but never accepted: the container "finishes" without the client
        // having exited, which is the ordinary ordering.
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        // Long enough that "did it wait?" is unambiguous, short enough that a
        // stray cannot outlive the suite. An earlier version left `sleep 30`
        // alive after `cargo test` returned 0.
        let client = std::process::Command::new("/bin/sleep")
            .arg("1")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);

        // ⚠️ THIS CALLS `finish()`, WHICH IS THE POINT. An earlier version poked
        // `container_done` and read `client_exited_early` by hand and then
        // detached the thread -- because calling `finish()` would have hit the
        // blocking join and hung. So the test AVOIDED the defect instead of
        // catching it. Going through the real entry point is what makes this a
        // regression test rather than a description.
        let started = Instant::now();
        let reported_early = watch.finish();
        let elapsed = started.elapsed();

        assert!(
            !reported_early,
            "a client still running when the container finished must not be reported as an early exit"
        );
        assert!(
            elapsed >= Duration::from_millis(900),
            "finish() returned after {elapsed:?}, before a 1-second client could have exited; \
             hermit is not waiting for the gdb it spawned"
        );
        drop(listener);
    }

    /// ⚠️ THE ONLY NEW BEHAVIOUR WITH TEETH: `finish()` JOINS, SO THE RELEASE
    /// PROBE IS NOW ON HERMIT'S RETURN PATH. A bare `TcpStream::connect` has no
    /// timeout, and against a listener whose accept queue is full the SYN is
    /// dropped rather than refused, so the call stalls. While the watcher was
    /// detached that only delayed a background thread. `agent(hermit-dbgrev14)`
    /// measured the same schedule both ways: **741ms with `connect_timeout`,
    /// 135.5s with main's bare `connect`.**
    ///
    /// ⚠️ AND THIS TEST RETURNS EARLY RATHER THAN FAILING IF IT CANNOT SATURATE
    /// THE QUEUE, WHICH IS A SILENT PASS AND IS THE DELIBERATE CHOICE. Saturation
    /// depends on the host's backlog behaviour; a test that goes RED when the
    /// environment will not cooperate teaches people to ignore it, which costs
    /// more than the coverage is worth. The bound is therefore ALSO carried by an
    /// old-fails/new-passes mutation recorded on the pull request, not by this
    /// cell alone. Saturation is verified before anything is asserted, so the pass
    /// is never taken as evidence when the setup did not hold.
    #[test]
    fn finish_is_bounded_when_the_release_probe_meets_a_saturated_listener() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();
        let peer = SocketAddr::from(([127, 0, 0, 1], port));

        // Shrink the accept queue to its minimum and never accept from it.
        // SAFETY: `listen` on an owned listening descriptor; writes no memory.
        unsafe {
            libc::listen(std::os::fd::AsRawFd::as_raw_fd(&listener), 0);
        }
        let mut held = Vec::new();
        for _ in 0..8 {
            match TcpStream::connect_timeout(&peer, Duration::from_millis(250)) {
                Ok(stream) => held.push(stream),
                Err(_) => break,
            }
        }

        // Verify the setup rather than assuming it: if a fresh connect still
        // completes, the queue is not full and this cell would prove nothing.
        if TcpStream::connect_timeout(&peer, Duration::from_millis(250)).is_ok() {
            return;
        }

        // A client that has already exited sends the watcher into the release
        // loop, where it meets the stalling peer.
        let client = std::process::Command::new("/bin/true")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);
        thread::sleep(Duration::from_millis(200));

        let started = Instant::now();
        watch.finish();
        let elapsed = started.elapsed();

        drop(held);
        drop(listener);

        assert!(
            elapsed < Duration::from_secs(5),
            "finish() blocked {elapsed:?} against a listener that never answers; the release \
             probe must be bounded now that finish() joins the watcher"
        );
    }

    /// ⚠️ THE PROMPTNESS GUARANTEE, ON THE PATH THAT ACTUALLY NEEDS IT. An early
    /// `?` between the spawn and the run drops the watch without a container ever
    /// having existed, so a client still trying to connect to a port nobody will
    /// bind would never exit. The original defect was a `Drop` that inherited a
    /// blocking wait and hung paths that were merely returning an error --
    /// measured at 30.001s. Restoring the success-path wait must not bring that
    /// back with it, and this is what says so.
    #[test]
    fn dropping_a_watch_never_waits_for_the_client() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        let client = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let pid = client.id();
        let watch = GdbClientWatch::spawn(client, port);

        let started = Instant::now();
        drop(watch);
        let elapsed = started.elapsed();

        // The detached watcher owns the client and reaps it. Kill the stand-in so
        // the suite leaves nothing running: a reviewer measured a `/bin/sleep 30`
        // alive after `cargo test` returned 0, reparented to the user subreaper,
        // and a leak like that is invisible in a green run.
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();

        assert!(
            elapsed < Duration::from_secs(5),
            "dropping the watch blocked {elapsed:?} on a live client; the error path must never \
             wait for one"
        );
        drop(listener);
    }

    /// A client that exits AFTER the container finished is a healthy teardown and
    /// must not be reported as "exited before connecting".
    ///
    /// ⚠️ THIS IS DEFECT 2, AND IT IS THE INVERSE OF A HANG: a correct outcome
    /// described as a failure. The first version read `done` only once, after a
    /// blocking `wait()`, so ANY client that exited before the container returned
    /// was flagged — and the flag adds a context line saying the client never
    /// connected. A false cause attached to a real failure sends the next reader
    /// hunting a bug that does not exist.
    #[test]
    fn a_client_that_exits_after_the_container_finished_is_not_reported_as_early() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        // Still alive when the container finishes, gone shortly after.
        let client = std::process::Command::new("/bin/sleep")
            .arg("1")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);

        // The container returns while the client is still running.
        let reported_early = watch.finish();
        assert!(
            !reported_early,
            "reported early while the client was still alive"
        );

        // ⚠️ THIS TAIL USED TO SLEEP 2500ms AND RE-READ THE FLAG, AND THE JOIN MADE
        // IT DEAD CODE — recorded rather than deleted quietly, because it is the
        // one behavioural gain of joining that is easy to miss. It read "the
        // watcher is detached and still polling; it must observe the exit and stay
        // silent", which was true when `finish()` detached: the flag could still
        // move after `finish()` returned, so re-reading it later was a real second
        // observation. `finish()` now joins, so the watcher has already run to
        // completion and nothing can set the flag afterwards — the re-read could
        // not fail for ANY implementation, and the sleep was 2.5s of dead time.
        // `agent(hermit-dbgrev14)` caught it.
        //
        // The property is unchanged and is now decided entirely by the assertion
        // above: joining turned `finish()`'s answer from a snapshot of a running
        // thread into a settled fact.
        assert!(
            !watch.client_exited_early.load(Ordering::SeqCst),
            "the flag moved after `finish()` returned, so joining did not settle it"
        );
        drop(listener);
    }

    /// The client exits FIRST, and the container then finishes on its own. There
    /// was no blocked accept to release, so this is a healthy teardown and must
    /// not be reported as "exited before connecting".
    ///
    /// ⚠️ THIS IS THE CASE RE-READING `done` CANNOT CATCH, and pinning it is why
    /// the flag moved. The client is gone before the container returns, so a
    /// check taken at client-exit time sees `done == false` — and so does a check
    /// taken an instant later. Only asking "did we have to release an accept?"
    /// distinguishes a stranded container from an ordinary shutdown.
    ///
    /// No listener is bound on `port`, so the release connect can never succeed:
    /// exactly the shape of a container that needed no rescuing.
    #[test]
    fn a_client_that_exits_first_but_strands_nobody_is_not_reported_as_early() {
        // Bind and immediately drop, so the port is plausible but nothing listens.
        let port = {
            let l = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
            l.local_addr().expect("no local addr").port()
        };

        let client = std::process::Command::new("/bin/true")
            .spawn()
            .expect("failed to spawn the stand-in client");
        let mut watch = GdbClientWatch::spawn(client, port);

        // Let the watcher observe the exit and spin on the release a few times.
        thread::sleep(Duration::from_millis(200));

        // The container now finishes under its own power.
        assert!(
            !watch.finish(),
            "a client that exited without stranding the container was reported as having \
             exited before connecting -- a healthy teardown described as a failed connect"
        );
        thread::sleep(Duration::from_millis(100));
        assert!(
            !watch.client_exited_early.load(Ordering::SeqCst),
            "the flag was set after the fact for a container that was never blocked"
        );
    }
    /// A client that CONNECTED, completed its session and exited must not be
    /// reported as having exited before connecting.
    ///
    /// [`CLIENT_EXITED_BEFORE_CONNECTING`] says the client "exited before it
    /// finished connecting", so the negation -- connected, served, quit -- is what
    /// must never be flagged. The other cells pin clients that never connect at
    /// all; this is the path a real `gdb -batch ... quit` takes on every
    /// successful run, and it is named here so the record has it.
    ///
    /// ⚠️ THIS IS A SCENARIO CELL, NOT INDEPENDENT COVERAGE, AND SAYING SO IS THE
    /// POINT. Reverting the defect-2 correction fails this cell AND
    /// `a_client_that_exits_first_but_strands_nobody`, because both end with
    /// NOTHING LISTENING on the port -- that cell binds nothing, this one drops
    /// the listener after the accept -- so both exercise the same branch: connect
    /// fails, loop spins, `done` arrives, no flag. Fail-on-revert therefore does
    /// not establish that this cell catches anything the suite would otherwise
    /// miss. Raised by `agent(hermit-001)` on the review of this change.
    ///
    /// ⚠️ AND IT CANNOT DETECT A REVERIE CONTRACT CHANGE, WHICH AN EARLIER VERSION
    /// OF THIS COMMENT CLAIMED. The `drop(listener)` below is this test's OWN, not
    /// `reverie-ptrace`'s. If `wait_for_tcp_connection` ever kept its listener
    /// bound, this cell would go on passing, because it drops the listener itself
    /// either way. The reverie side is pinned where the fact lives, by
    /// `the_listener_is_closed_once_the_client_is_accepted` in that repository.
    ///
    /// ⚠️ THE DISCRIMINATING SHAPE IS NOT WRITABLE GREEN TODAY. It would be a
    /// listener that stays BOUND and answering after the accept -- the only case
    /// where the release connect succeeds without our accept having been released.
    /// MEASURED on hermit#2678's head a826d51a2116: in exactly that scenario the
    /// flag comes out TRUE (`accepts_after_session=1 total_accepts=3
    /// flag_reported=true`), so a cell asserting the correct answer would be RED.
    /// The store at the successful connect precedes the grace loop, and moving it
    /// after would suppress the flag in the TRUE-positive case too, because a
    /// released accept also lets the container finish inside the grace window.
    /// This is the conceded port-collision exposure, tracked as
    /// `gdb_watcher_release_probe`; it is not reachable in production only because
    /// reverie drops the listener.
    #[test]
    fn a_client_that_connected_and_finished_its_session_is_not_reported_as_early() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        // A stand-in for `gdb -batch ... quit`: connect, wait for the server to
        // finish the session, then exit. Waiting on the accepted connection makes
        // the ordering a fact rather than a delay: the client cannot exit before
        // the accept and listener drop have happened.
        let client = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "exec 3<>/dev/tcp/127.0.0.1/{port} || exit 1; IFS= read -r -n 1 <&3; exec 3>&-"
            ))
            .spawn()
            .expect("failed to spawn the stand-in client");

        let mut watch = GdbClientWatch::spawn(client, port);

        // The gdbserver accepts. Bounded, because a hanging test is worse than a
        // red one: it names itself in a line, a wedged one eats the whole run.
        listener
            .set_nonblocking(true)
            .expect("failed to set the test listener non-blocking");
        let deadline = Instant::now() + Duration::from_secs(30);
        let accepted = loop {
            match listener.accept() {
                Ok(pair) => break Some(pair),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        };
        let (mut accepted, _) = accepted.expect("the stand-in client never connected within 30s");

        // ⚠️ THE REVERIE CONTRACT, REPRODUCED. `wait_for_tcp_connection` returns
        // the stream and drops the listener; from here on the port answers
        // nothing, while the session itself stays open.
        drop(listener);

        // End the accepted session, then wait for the peer to close it. EOF is
        // direct evidence that the client completed the session; no elapsed time
        // is used as a substitute for that fact.
        accepted
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("failed to bound the wait for the stand-in client to close");
        accepted
            .write_all(b"q")
            .expect("failed to finish the stand-in client's session");
        let mut byte = [0_u8; 1];
        assert_eq!(
            accepted
                .read(&mut byte)
                .expect("the stand-in client did not close its session within 30s"),
            0,
            "the stand-in client sent unexpected data instead of closing its session"
        );

        // `finish()` publishes container completion. Wait until the watcher has
        // reaped the client and confirmed that the container is still running,
        // so this cell cannot silently take the container-finished-first branch
        // and pass without exercising its case.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !watch
            .client_exited_before_container_finished
            .load(Ordering::SeqCst)
            && Instant::now() < deadline
        {
            thread::sleep(CLIENT_POLL_INTERVAL);
        }
        assert!(
            watch
                .client_exited_before_container_finished
                .load(Ordering::SeqCst),
            "the watcher did not observe the stand-in client exit while the container was still running"
        );

        assert!(
            !watch.finish(),
            "a client that connected, finished its session and exited was reported as having \
             exited before connecting -- the flag's own documented meaning, inverted"
        );
        drop(accepted);
    }
}
