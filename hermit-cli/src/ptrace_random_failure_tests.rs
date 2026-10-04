/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// This is a host-permission/lifecycle diagnostic, not a determinism claim.
// The C guest uses only real syscalls; no production failure injection is used.
mod real_random {
    use std::io::Write;
    use std::num::NonZeroU64;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU32;

    use super::*;

    include!("ptrace_random_permissions.rs");

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>);
    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut log = self.0.lock().unwrap();
            if log.len() + bytes.len() > 2 * 1024 * 1024 {
                self.1.store(true, Ordering::SeqCst);
                return Err(std::io::Error::other("real random test log exceeded 2 MiB"));
            }
            log.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Log {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    struct Mapping(*mut AtomicU32);
    impl Mapping {
        fn new(file: &File) -> Self {
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    4096,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(ptr, libc::MAP_FAILED);
            Self(ptr.cast())
        }
        fn field(&self, n: usize) -> &AtomicU32 {
            assert!(n < 6);
            unsafe { &*self.0.add(n) }
        }
    }
    impl Drop for Mapping {
        fn drop(&mut self) {
            assert_eq!(unsafe { libc::munmap(self.0.cast(), 4096) }, 0);
        }
    }
    fn pidfd(pid: i32) -> OwnedFd {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(
            fd >= 0,
            "pidfd_open({pid}): {}",
            std::io::Error::last_os_error()
        );
        unsafe { OwnedFd::from_raw_fd(fd as i32) }
    }
    fn ready(fd: &OwnedFd) -> bool {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, 0) };
        assert!(
            result >= 0,
            "pidfd poll: {}",
            std::io::Error::last_os_error()
        );
        result == 1 && poll.revents & (libc::POLLIN | libc::POLLHUP) != 0
    }
    #[derive(Default)]
    struct Facts {
        root: i32,
        child: i32,
        root_fd: Option<OwnedFd>,
        child_fd: Option<OwnedFd>,
        complete: bool,
        primary_address: usize,
        released_after_timer: bool,
        drop_count: usize,
        retired_before_drop: bool,
    }
    struct Guard {
        directory: tempfile::TempDir,
        facts: Rc<RefCell<Facts>>,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let mut facts = self.facts.borrow_mut();
            facts.retired_before_drop = facts.complete
                && facts.root_fd.as_ref().is_some_and(ready)
                && facts.child_fd.as_ref().is_some_and(ready);
            facts.drop_count += 1;
            // TempDir destruction follows the original-pidfd sample above.
            assert!(self.directory.path().exists());
        }
    }
    fn copy_failure(failure: &PtraceRunFailure) -> &detcore::random::RandomCopyFailure {
        let reverie::Error::Tool(inner) = failure.primary() else {
            panic!("wrong primary: {failure:?}")
        };
        inner.downcast_ref().expect("original RandomCopyFailure")
    }

    #[derive(Clone, Copy)]
    pub(super) enum Operation {
        Getrandom,
        Readv,
    }
    impl Operation {
        fn name(self) -> &'static str {
            match self {
                Self::Getrandom => "getrandom",
                Self::Readv => "readv",
            }
        }
    }

    pub(super) fn case(name: &str, fatal: bool, trace_diagnostics: bool, operation: Operation) {
        if std::env::var("HERMIT_OWNER_TEST_ROLE").as_deref() == Ok(name) {
            isolated(name, || exercise(fatal, trace_diagnostics, operation));
            return;
        }
        let parent_tid = unsafe { libc::syscall(libc::SYS_gettid) };
        let _parent = credentials(
            &format!("/proc/self/task/{parent_tid}"),
            "outer-before-isolation",
        );
        let fixture = tempfile::tempdir().unwrap();
        let compile = |name: &str, bytes: &[u8]| {
            let source = fixture.path().join(format!("{name}.c"));
            let binary = fixture.path().join(name);
            std::fs::write(&source, bytes).unwrap();
            let status = Command::new("timeout")
                .args([
                    "--kill-after=1s",
                    "10s",
                    "cc",
                    "-O2",
                    "-std=c11",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .status()
                .unwrap();
            assert!(status.success(), "{name} fixture compile failed: {status}");
            binary
        };
        let binary = match std::env::var_os("HERMIT_RANDOM_FAILURE_GUEST") {
            Some(binary) => {
                let binary = PathBuf::from(binary);
                assert!(binary.is_absolute() && binary.is_file());
                binary
            }
            None => compile(
                "guest",
                include_bytes!("../tests/fixtures/random_copy_fatal.c"),
            ),
        };
        let probe = compile(
            "permission-probe",
            include_bytes!("../tests/fixtures/ptrace_copy_permission.c"),
        );
        isolated_with_child_setup(
            name,
            &[
                ("HERMIT_RANDOM_FAILURE_GUEST", &binary),
                ("HERMIT_RANDOM_PERMISSION_PROBE", &probe),
            ],
            restrict_ptrace_copy,
            || unreachable!(),
        );
    }

    fn exercise(fatal: bool, trace_diagnostics: bool, operation: Operation) {
        let deadline: u64 = std::env::var("HERMIT_OWNER_TEST_DEADLINE")
            .unwrap()
            .parse()
            .unwrap();
        let (original_tid, original_credentials) = restricted_thread_credentials("before-runtime");
        let probe = Command::new(std::env::var_os("HERMIT_RANDOM_PERMISSION_PROBE").unwrap())
            .status()
            .unwrap();
        assert!(
            probe.success(),
            "native copy-permission discriminator failed: {probe}"
        );
        assert!(
            monotonic_ns() < deadline,
            "native discriminator exceeded original deadline"
        );
        // This isolated process owns natural-parent waits after product facts
        // are sealed. It never creates a second ptrace waiter.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let storage = tempfile::tempdir().unwrap();
        let backing = tempfile::tempdir().unwrap();
        let backing_path = backing.path().to_path_buf();
        let shared_path = backing.path().join("shared");
        let gate_path = backing.path().join("gate");
        let gate_c = std::ffi::CString::new(gate_path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(gate_c.as_ptr(), 0o600) }, 0);
        let mut gate = File::options()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&gate_path)
            .unwrap();
        let file = File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&shared_path)
            .unwrap();
        file.set_len(4096).unwrap();
        let mapping = Rc::new(Mapping::new(&file));
        let recording = storage.path().join("preemptions.json");
        let summary = storage.path().join("success-summary.json");
        let facts = Rc::new(RefCell::new(Facts::default()));
        let log = Log::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(if trace_diagnostics {
                "detcore=debug,reverie_ptrace=debug,detcore::tool_global=trace"
            } else {
                "detcore=debug,reverie_ptrace=debug"
            })
            .with_writer(move || writer.clone())
            .finish();
        let future_facts = facts.clone();
        let future_mapping = mapping.clone();
        let future_log = log.clone();
        let future_recording = recording.clone();
        let future_summary = summary.clone();
        let limit = Duration::from_nanos(deadline.saturating_sub(monotonic_ns()));
        let result = tracing::subscriber::with_default(subscriber, || {
            run(Some(limit), move |control| async move {
                let _guard = Guard {
                    directory: backing,
                    facts: future_facts.clone(),
                };
                let mut command = reverie::process::Command::new(
                    std::env::var_os("HERMIT_RANDOM_FAILURE_GUEST").unwrap(),
                );
                command
                    .arg(shared_path)
                    .arg(if fatal { "0" } else { "1" })
                    .arg(gate_path)
                    .arg(operation.name())
                    .stdout(reverie::process::Stdio::piped())
                    .stderr(reverie::process::Stdio::piped());
                let mut config = detcore::Config {
                    sequentialize_threads: true,
                    deterministic_io: true,
                    has_uts_namespace: false,
                    max_timeslice: NonZeroU64::new(1_000_000),
                    record_preemptions_to: Some(future_recording),
                    detlog_io_buffers: true,
                    ..Default::default()
                };
                config.validate();
                let config = crate::prepare_backend_config(config, crate::Backend::Ptrace);
                let tracer = reverie_ptrace::TracerBuilder::<detcore::Detcore>::new(command)
                    .config(config)
                    .spawn()
                    .await?;
                let root = tracer.guest_pid().as_raw();
                {
                    let mut facts = future_facts.borrow_mut();
                    facts.root = root;
                    facts.root_fd = Some(pidfd(root));
                }
                control.register(tracer.termination_handle());
                let observer = async {
                    loop {
                        if future_mapping.field(0).load(Ordering::SeqCst) == 1
                            && future_mapping.field(1).load(Ordering::SeqCst) == 1
                        {
                            let child = future_mapping.field(5).load(Ordering::SeqCst) as i32;
                            assert!(child > 0 && child != root);
                            let status =
                                std::fs::read_to_string(format!("/proc/{child}/status")).unwrap();
                            assert!(status.lines().any(|line| line == format!("PPid:\t{root}")));
                            let tracer_tid = unsafe { libc::syscall(libc::SYS_gettid) };
                            assert!(
                                status
                                    .lines()
                                    .any(|line| line == format!("TracerPid:\t{tracer_tid}"))
                            );
                            let text = future_log.text();
                            let event =
                                format!("[detcore, dtid {child}] inbound timer preemption event");
                            if text.contains(&event) {
                                let (tid, observed) =
                                    restricted_thread_credentials("actual-tracer-before-gate");
                                assert_eq!(tid, original_tid);
                                assert_eq!(observed, original_credentials);
                                assert_tracee_credentials(root, tid, &observed);
                                assert_tracee_credentials(child, tid, &observed);
                                let mut facts = future_facts.borrow_mut();
                                facts.child = child;
                                facts.child_fd = Some(pidfd(child));
                                assert!(!ready(facts.root_fd.as_ref().unwrap()));
                                assert!(!ready(facts.child_fd.as_ref().unwrap()));
                                facts.released_after_timer = true;
                                future_mapping.field(2).store(1, Ordering::SeqCst);
                                gate.write_all(&[1]).unwrap();
                                break;
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                };
                let (outcome, ()) = tokio::join!(tracer.wait_with_output_completion(), observer);
                if let ToolRunOutcome::Complete(completion) = &outcome {
                    let mut facts = future_facts.borrow_mut();
                    facts.complete = true;
                    if let Err(failure) = &completion.result
                        && let reverie::Error::Tool(inner) = failure.primary()
                        && let Some(copy) =
                            inner.downcast_ref::<detcore::random::RandomCopyFailure>()
                    {
                        facts.primary_address = copy as *const _ as usize;
                    }
                }
                let (output, global) = consume(outcome, control).await?;
                global.clean_up(false, &Some(future_summary)).await;
                Ok(output)
            })
        });
        let text = log.text();
        eprintln!("REAL_RANDOM_LOG fatal={fatal}\n{text}\nEND_REAL_RANDOM_LOG");
        // These observations precede any recovery or natural-parent reaping.
        let before = facts.borrow();
        eprintln!(
            "REAL_RANDOM_OBSERVATION fatal={fatal} result={result:?} root={} child={} complete={} timer={} drops={} retired_before_drop={} backing_exists={} after={}",
            before.root,
            before.child,
            before.complete,
            before.released_after_timer,
            before.drop_count,
            before.retired_before_drop,
            backing_path.exists(),
            mapping.field(4).load(Ordering::SeqCst)
        );
        drop(before);
        if let Err(error) = &result
            && let Some(pending) = error.downcast_ref::<HermitCleanupUnconfirmed>()
        {
            let rescue = Instant::now() + Duration::from_secs(2);
            let original = pending.clone();
            let rescued = original.resume::<reverie::process::Output>();
            eprintln!(
                "REAL_RANDOM separate rescue elapsed_within_2s={} result={rescued:?}",
                Instant::now() < rescue
            );
            panic!("original completion was pending; rescue is not qualification");
        }
        assert!(monotonic_ns() < deadline);
        assert!(
            !log.1.load(Ordering::SeqCst),
            "log overflow invalidates observations"
        );
        let facts = facts.borrow();
        assert!(facts.released_after_timer && facts.complete);
        assert_eq!(facts.drop_count, 1);
        assert!(facts.retired_before_drop && !backing_path.exists());
        assert_eq!(
            text.matches("thread exit hook, deregistering from scheduler.")
                .count(),
            2
        );
        let record: detcore::preemptions::PreemptionRecord =
            serde_json::from_slice(&std::fs::read(&recording).unwrap()).unwrap();
        assert_eq!(
            record.extract_all().len(),
            2,
            "actual initialized thread recordings"
        );
        if trace_diagnostics {
            assert!(
                text.contains("next two instruction bytes Err(EIO)"),
                "actual failed optional memory observation must run under TRACE"
            );
        }
        if fatal {
            let failure = result
                .unwrap_err()
                .downcast::<HermitPtraceFailure>()
                .unwrap();
            let primary = copy_failure(failure.failure());
            assert_eq!(primary.errno(), reverie::Errno::EPERM);
            assert!(primary.to_string().contains("omit --no-namespace"));
            assert_eq!(primary as *const _ as usize, facts.primary_address);
            assert_eq!(failure.failure().origin().pid.as_raw(), facts.root);
            assert_eq!(failure.failure().origin().phase, "ptrace syscall callback");
            assert!(
                failure.cleanup().scheduler.is_ok(),
                "{:?}",
                failure.cleanup().scheduler
            );
            assert!(
                failure.cleanup().preemption_recording.is_ok(),
                "{:?}",
                failure.cleanup().preemption_recording
            );
            eprintln!(
                "REAL_RANDOM_FAILED_G scheduler={:?} preemption_recording={:?} primary_address={} errno={:?}",
                failure.cleanup().scheduler,
                failure.cleanup().preemption_recording,
                facts.primary_address,
                primary.errno()
            );
            assert!(!failure.timeout_during_cleanup());
            assert!(failure.failure().secondary().is_empty());
            assert!(failure.callback_diagnostics().is_empty());
            assert!(
                failure
                    .failure()
                    .captured_prefix()
                    .unwrap()
                    .stdout()
                    .is_empty()
            );
            assert_eq!(mapping.field(4).load(Ordering::SeqCst), 0);
            assert!(!summary.exists());
            let inbound = format!("inbound syscall: {}(", operation.name());
            let tail = text.rsplit_once(&inbound).unwrap().1;
            let arguments = tail
                .lines()
                .next()
                .unwrap()
                .split_once(" DETLOG_RECORD=")
                .unwrap()
                .0;
            match operation {
                Operation::Getrandom => assert!(arguments.contains(", 7, 0)")),
                Operation::Readv => {
                    let (fd, vector_and_count) = arguments.split_once(", ").unwrap();
                    assert!(fd.parse::<i32>().unwrap() >= 0);
                    let (vector, count) = vector_and_count.split_once(", ").unwrap();
                    assert!(
                        usize::from_str_radix(vector.strip_prefix("0x").unwrap(), 16).unwrap() > 0
                    );
                    assert_eq!(count, "1) = ?");
                }
            }
            assert!(tail.contains("backend failure"));
            assert!(!tail.lines().any(|line| line.contains("finish syscall #")
                && line.contains(&format!("{}(", operation.name()))));
            assert!(
                !tail
                    .lines()
                    .any(|line| line.contains("[iobuf]") && line.contains(operation.name()))
            );
            for pid in [facts.root, facts.child] {
                assert!(tail.contains(&format!(
                    "guest terminated by signal tid={pid} pid={pid} signal=SIGKILL"
                )));
            }
            // A terminal ptrace wait may leave the natural parent's zombie.
            // Retain the same child pidfd for that separate actual wait.
            let fd = facts.child_fd.as_ref().unwrap();
            let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let observed = unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    fd.as_raw_fd() as u32,
                    &mut status,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            let wait_errno = if observed < 0 {
                std::io::Error::last_os_error().raw_os_error()
            } else {
                None
            };
            let proc_present = std::path::Path::new(&format!("/proc/{}", facts.child))
                .try_exists()
                .unwrap();
            eprintln!(
                "REAL_RANDOM_NATURAL_WAIT rc={observed} immediate_errno={wait_errno:?} pid={} code={} status={} proc_present={proc_present} original_pidfd_ready={}",
                unsafe { status.si_pid() },
                status.si_code,
                unsafe { status.si_status() },
                ready(fd)
            );
            if observed == 0 {
                assert_eq!(unsafe { status.si_pid() }, facts.child);
                assert_eq!(status.si_code, libc::CLD_KILLED);
                assert_eq!(unsafe { status.si_status() }, libc::SIGKILL);
                assert_eq!(
                    unsafe {
                        libc::waitid(
                            libc::P_PIDFD,
                            fd.as_raw_fd() as u32,
                            &mut status,
                            libc::WEXITED | libc::WNOHANG,
                        )
                    },
                    0
                );
                assert_eq!(unsafe { status.si_pid() }, facts.child);
            } else {
                // The original notifier may already be both ptracer and natural
                // parent after reparenting. ECHILD alone is never proof: require
                // the already sealed Complete/hooks/original pidfd evidence AND
                // actual absence. A present zombie must take the wait branch.
                assert_eq!(wait_errno, Some(libc::ECHILD));
                assert!(!proc_present && ready(fd));
            }
            assert!(
                !std::path::Path::new(&format!("/proc/{}", facts.child))
                    .try_exists()
                    .unwrap()
            );
        } else {
            let output = result.unwrap();
            assert_eq!(output.status, reverie::ExitStatus::Exited(0));
            assert_eq!(
                output.stdout,
                b"AFTER_RANDOM result=7 errno=0 sentinel=165\nAFTER_CHILD status=1792\n"
            );
            assert_eq!(mapping.field(4).load(Ordering::SeqCst), 1);
            assert!(summary.exists());
        }
        assert!(monotonic_ns() < deadline);
        eprintln!("REAL_RANDOM_PREDICATES fatal={fatal} all=true before_original_deadline=true");
    }
}

#[test]
fn real_random_copy_failure_retires_initialized_sibling_and_timer() {
    real_random::case(
        "real_random_copy_failure_retires_initialized_sibling_and_timer",
        true,
        false,
        real_random::Operation::Getrandom,
    );
}
#[test]
fn real_random_live_copy_preserves_sibling_and_timer_success() {
    real_random::case(
        "real_random_live_copy_preserves_sibling_and_timer_success",
        false,
        false,
        real_random::Operation::Getrandom,
    );
}

#[test]
fn real_random_copy_failure_with_trace_diagnostics() {
    real_random::case(
        "real_random_copy_failure_with_trace_diagnostics",
        true,
        true,
        real_random::Operation::Getrandom,
    );
}

#[test]
fn real_random_readv_import_failure_retires_initialized_sibling_and_timer() {
    real_random::case(
        "real_random_readv_import_failure_retires_initialized_sibling_and_timer",
        true,
        false,
        real_random::Operation::Readv,
    );
}

#[test]
fn real_random_readv_live_import_preserves_sibling_and_timer_success() {
    real_random::case(
        "real_random_readv_live_import_preserves_sibling_and_timer_success",
        false,
        false,
        real_random::Operation::Readv,
    );
}
