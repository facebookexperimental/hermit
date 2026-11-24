/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Container setup against a host that renames a new file or directory over
//! a path Hermit binds over while the container is set up.
//!
//! The guest's `/etc` is a private stage: Hermit's frozen `/etc/ld.so.cache`
//! and `/etc/group` are bound over the stage's own entries, so a host rename
//! of the host's files detaches nothing, and setup neither retries nor lets
//! the guest see the host's new file. The empty directory Hermit hides the
//! host's nscd cache behind is still bound over the host's own path: a rename
//! over it detaches that bind in every namespace, and setup must find the
//! bind gone when it records the mount table and retry, up to its bound
//! (https://github.com/rrnewton/hermit/issues/4799).
//!
//! The fixture runs hermit inside an outer user+mount namespace whose `/etc`
//! and `/var/run` (as the host resolves it) are test-owned directories, so
//! the test renames over them without touching the host's. The fixture builds
//! what they hold itself: its `/etc/ld.so.cache` with `ldconfig`
//! (`common/loader_cache.rs`), never a copy of the host's, and, on a host with
//! no `/var/run` (the validation's pinned root has none), a test-owned `/var`
//! holding its run directory. The race probe
//! `setup_mounts_made` (docs/RACE_PROBES.md) holds the container's init after
//! setup made its mounts and before it records the mount table. The renames
//! occur at that after-mounts hold, once per affected path per held setup
//! attempt, so every rename lands in the window it means to test, and the
//! test then lets the init go on. `hold-each` holds every setup attempt.
//! Hermit runs with neither virtual CPUID nor a preemption timeslice, so no
//! case needs CPUID faulting or an exact branch counter and none is gated on
//! either. A host where the fixture cannot be built fails each case, naming
//! why.

#[path = "common/loader_cache.rs"]
mod loader_cache;

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;

/// The end of every retry line of a launch's or a `--verify` leg's setup.
const RETRY: &str = "retrying with a fresh snapshot";

/// The race probe point the tests hold at.
const POINT: &str = "setup_mounts_made";

/// What the host renames over the frozen files' paths.
const NEW_CACHE: &[u8] = b"a replacement cache\n";
const NEW_GROUP: &[u8] = b"renamedbyhost:x:4242:\n";

/// The start of the failure of a case whose fixture could not be built.
const UNBUILDABLE_FIXTURE: &str = "setup_retry could not build its fixture, so this case fails instead of passing having run nothing";

fn run(command: &mut Command) -> std::process::Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn")
}

fn read_lossy(path: &Path) -> String {
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// The line a held visit to [`POINT`] prints.
fn hold_line() -> String {
    format!("race_probe: {POINT} holding until")
}

/// The test-owned `/etc` and run directory, and the hermit launched over them.
struct Fixture {
    directory: tempfile::TempDir,
    etc: PathBuf,
    run: PathBuf,
    /// The test-owned directory bound over [`Fixture::cover_target`]: the run
    /// directory itself, or a `var` directory holding it.
    cover: PathBuf,
    /// The host's `/var/run`, resolved, or, on a host with no `/var/run`, its
    /// `/var`.
    cover_target: PathBuf,
    /// Where the fixture's run directory appears in the outer namespace, so
    /// that the guest's `/var/run/nscd` is the fixture's: the host's
    /// `/var/run`, resolved, or `/var/run` itself.
    var_run: PathBuf,
    /// The loader cache the fixture built, which Hermit freezes at
    /// `/etc/ld.so.cache`.
    cache: Vec<u8>,
    hermit: PathBuf,
}

/// The fixture over the hermit under test. A host where it cannot be built
/// fails the case, naming why.
fn fixture() -> Fixture {
    fixture_with(Path::new(env!("CARGO_BIN_EXE_hermit")))
}

/// [`fixture`] over `hermit`.
fn fixture_with(hermit: &Path) -> Fixture {
    build_fixture(hermit).unwrap_or_else(|reason| panic!("{UNBUILDABLE_FIXTURE}: {reason}"))
}

/// The fixture, or why it cannot be built: no outer namespace, no loader
/// cache built (`loader_cache::build` names why), neither a `/var/run` nor a
/// `/var` to hold one, or a nested `hermit` that does not run on this host.
fn build_fixture(hermit: &Path) -> Result<Fixture, String> {
    let probe = run(Command::new("unshare").args(["-rm", "true"]));
    if !probe.status.success() {
        return Err(format!("unshare -rm unavailable: {probe:?}"));
    }
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let etc = directory.path().join("etc");
    std::fs::create_dir(&etc).unwrap();
    let cache = loader_cache::build(&etc.join("ld.so.cache"))?;
    // A group file Hermit freezes, the host's or, where it has none, one of
    // its own, so that a rename over `/etc/group` always meets a frozen file.
    let group = std::fs::read("/etc/group").unwrap_or_else(|_| b"root:x:0:\n".to_vec());
    std::fs::write(etc.join("group"), group).unwrap();
    // The run directory covers the host's `/var/run`, resolved. A host with
    // no `/var/run` gets one in the outer namespace instead: a test-owned
    // `var` directory holding the run directory covers its `/var`.
    let (run_dir, cover, cover_target, var_run) = match std::fs::canonicalize("/var/run") {
        Ok(var_run) => {
            let run_dir = directory.path().join("run");
            (run_dir.clone(), run_dir, var_run.clone(), var_run)
        }
        Err(_) if Path::new("/var").is_dir() => {
            let var = directory.path().join("var");
            let run_dir = var.join("run");
            (
                run_dir,
                var,
                PathBuf::from("/var"),
                PathBuf::from("/var/run"),
            )
        }
        Err(error) => {
            return Err(format!(
                "the host's /var/run does not resolve ({error}), and it has no /var to hold one"
            ));
        }
    };
    std::fs::create_dir_all(run_dir.join("nscd")).unwrap();
    let fixture = Fixture {
        directory,
        etc,
        run: run_dir,
        cover,
        cover_target,
        var_run,
        cache,
        hermit: hermit.to_path_buf(),
    };
    let (status, _, stderr) = fixture.launch("probe", "", "true", None).wait();
    if !status.success() {
        return Err(format!("nested hermit probe failed: {status}\n{stderr}"));
    }
    Ok(fixture)
}

impl Fixture {
    /// Launches `hermit run --strict <extra>` of the shell command `guest`
    /// over the fixture, holding at [`POINT`] in `hold` mode (`hold` or
    /// `hold-each`) if given. Hermit runs with neither virtual CPUID nor a
    /// preemption timeslice: the cases test container setup, before any guest
    /// runs, so they need neither CPUID faulting nor an exact branch counter.
    /// The CPU profile is named, x86-64-v2, because `auto` reads what the
    /// host's `/bin/sh` declares: where that is x86-64-v3, as on CentOS Stream
    /// 10, auto presents x86-64-v3, which a run without virtual CPUID refuses.
    fn launch(&self, label: &str, extra_args: &str, guest: &str, hold: Option<&str>) -> Held {
        let probe = hold.map(|mode| format!("{POINT}={mode}"));
        self.launch_with_probe(label, extra_args, guest, probe.as_deref(), hold.is_some())
    }

    /// [`Fixture::launch`] with `HERMIT_RACE_PROBE` set to `probe` if given,
    /// and the hold directory armed for the container's init if `armed`.
    fn launch_with_probe(
        &self,
        label: &str,
        extra_args: &str,
        guest: &str,
        probe: Option<&str>,
        armed: bool,
    ) -> Held {
        let root = self.directory.path();
        let hold_dir = root.join(format!("{label}-hold"));
        std::fs::create_dir(&hold_dir).unwrap();
        let stdout = root.join(format!("{label}-stdout"));
        let stderr = root.join(format!("{label}-stderr"));
        let script = format!(
            "mount --bind {} /etc && mount --bind {} {} && exec {} run --strict --no-virtualize-cpuid --cpu-profile=x86-64-v2 --max-timeslice=disabled {} {} -- /bin/sh -c '{guest}'",
            self.etc.display(),
            self.cover.display(),
            self.cover_target.display(),
            self.hermit.display(),
            detcore_model::config::CANONICAL_TEST_EPOCH_ARG,
            extra_args,
        );
        let mut command = Command::new("timeout");
        command
            .args([
                "--kill-after",
                "5s",
                "60s",
                "unshare",
                "-rm",
                "sh",
                "-c",
                &script,
            ])
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&stdout).expect("create stdout file"))
            .stderr(std::fs::File::create(&stderr).expect("create stderr file"));
        if armed {
            // A point in the container's init needs this beside the release.
            std::fs::write(hold_dir.join("launcher-holds-armed"), b"").unwrap();
        }
        if let Some(probe) = probe {
            command
                .env("HERMIT_RACE_PROBE", probe)
                .env("HERMIT_RACE_PROBE_HOLD_DIR", &hold_dir);
        }
        Held {
            child: command.spawn().expect("spawn launch"),
            stdout,
            stderr,
            hold_dir,
        }
    }

    /// Renames a new file holding `bytes` over the fixture's `/etc/<name>`.
    fn rename_over_etc(&self, name: &str, bytes: &[u8]) {
        let new = self.etc.join(format!("{name}.new"));
        std::fs::write(&new, bytes).unwrap();
        std::fs::rename(&new, self.etc.join(name)).unwrap();
    }

    /// Renames a new directory over the fixture's nscd directory (an empty
    /// one can be renamed over), holding a file `host-nscd-state` if
    /// `with_state`, as nscd's own cache would.
    fn rename_over_nscd(&self, with_state: bool) {
        let new = self.run.join("nscd.new");
        std::fs::create_dir(&new).unwrap();
        if with_state {
            std::fs::write(new.join("host-nscd-state"), b"").unwrap();
        }
        std::fs::rename(&new, self.run.join("nscd")).unwrap();
    }

    /// Where Hermit binds its empty directory over the fixture's nscd
    /// directory.
    fn nscd_target(&self) -> PathBuf {
        self.var_run.join("nscd")
    }
}

/// A launch from [`Fixture::launch`].
struct Held {
    child: std::process::Child,
    stdout: PathBuf,
    stderr: PathBuf,
    hold_dir: PathBuf,
}

impl Held {
    /// Waits for the `visit`th setup attempt to hold at [`POINT`], does `act`,
    /// and lets the attempt go on.
    fn at_hold(&mut self, visit: usize, act: impl FnOnce()) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(55);
        while read_lossy(&self.stderr).matches(&hold_line()).count() < visit {
            if let Some(status) = self.child.try_wait().expect("poll launch") {
                panic!(
                    "the launch ended ({status}) before setup attempt {visit} held\n{}",
                    read_lossy(&self.stderr)
                );
            }
            assert!(
                std::time::Instant::now() < deadline,
                "setup attempt {visit} never held\n{}",
                read_lossy(&self.stderr)
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        act();
        std::fs::write(self.hold_dir.join(POINT), b"").unwrap();
    }

    /// The launch's status, stdout and stderr.
    fn wait(mut self) -> (ExitStatus, String, String) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(55);
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll launch") {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "launch never finished\n{}",
                read_lossy(&self.stderr)
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        (status, read_lossy(&self.stdout), read_lossy(&self.stderr))
    }
}

/// The guest's `/etc` is a private stage, so the host renaming a new
/// `/etc/ld.so.cache` and a new `/etc/group` over the frozen ones while setup
/// holds after its mounts detaches nothing: setup records its mount table at
/// its first attempt, with no retry, and the guest reads the frozen copies,
/// not the host's new files.
#[test]
fn a_host_rename_over_the_frozen_cache_and_group_during_setup_detaches_nothing() {
    let fixture = fixture();
    let mut held = fixture.launch(
        "staged",
        "",
        "wc -c < /etc/ld.so.cache; grep -c renamedbyhost /etc/group || true",
        Some("hold"),
    );
    held.at_hold(1, || {
        fixture.rename_over_etc("ld.so.cache", NEW_CACHE);
        fixture.rename_over_etc("group", NEW_GROUP);
    });
    let (status, stdout, stderr) = held.wait();

    assert_eq!(
        std::fs::read(fixture.etc.join("ld.so.cache")).unwrap(),
        NEW_CACHE
    );
    assert_eq!(std::fs::read(fixture.etc.join("group")).unwrap(), NEW_GROUP);
    assert!(status.success(), "launch refused: {status}\n{stderr}");
    assert!(
        !stderr.contains(RETRY),
        "a rename over a staged path must not detach a setup attempt\n{stderr}"
    );
    assert_eq!(
        stderr.matches(&hold_line()).count(),
        1,
        "one setup attempt, held once\n{stderr}"
    );
    let cache_size = fixture.cache.len().to_string();
    assert_eq!(
        stdout.split_whitespace().collect::<Vec<_>>(),
        [cache_size.as_str(), "0"],
        "the guest reads the frozen cache ({cache_size} bytes) and the frozen group \
         file (no line of the host's new one)\n{stderr}"
    );
}

/// The host renaming a new nscd directory over the one Hermit hides once setup
/// has made its mounts (held there, before setup records its mount table)
/// detaches Hermit's bind over it: the record finds the bind gone, and the
/// launch retries setup once and runs. The guest still finds the directory
/// empty, though the host's new one holds a file. A replacement earlier in
/// setup, between the bind's lookup and the bind or between the bind and its
/// read-only remount, is not this case's.
#[test]
fn launches_survive_nscd_dir_replacement_once_setup_has_mounted() {
    let fixture = fixture();
    let mut held = fixture.launch(
        "launch",
        "",
        "ls -A /var/run/nscd && echo end",
        Some("hold"),
    );
    held.at_hold(1, || fixture.rename_over_nscd(true));
    let (status, stdout, stderr) = held.wait();

    assert!(
        status.success(),
        "launch refused across an nscd directory replacement: {status}\n{stderr}"
    );
    let retries: Vec<&str> = stderr.lines().filter(|line| line.contains(RETRY)).collect();
    assert_eq!(
        retries.len(),
        1,
        "one replacement during setup, one setup retry\n{stderr}"
    );
    assert!(
        retries[0].contains("hermit: container setup attempt 1 of 5"),
        "the retry names its attempt: {}\n{stderr}",
        retries[0]
    );
    assert_eq!(
        stdout.trim(),
        "end",
        "the guest finds {} empty\n{stderr}",
        fixture.nscd_target().display()
    );
}

/// The same race through `--verify`, held at the same point: run 1's setup
/// retries the same way (the pair rerun cannot cover a leg that never
/// started), and the report verifies with no discarded pair.
#[test]
fn verify_legs_survive_nscd_dir_replacement_once_setup_has_mounted() {
    let fixture = fixture();
    let report_path = fixture.directory.path().join("verify.json");
    let extra = format!(
        "--verify --verify-strict --verify-json {}",
        report_path.display()
    );
    let mut held = fixture.launch(
        "verify",
        &extra,
        "ls -A /var/run/nscd && echo end",
        Some("hold"),
    );
    held.at_hold(1, || fixture.rename_over_nscd(true));
    let (status, stdout, stderr) = held.wait();

    assert!(
        status.success(),
        "verify launch refused across an nscd directory replacement: {status}\n{stderr}"
    );
    let retries: Vec<&str> = stderr.lines().filter(|line| line.contains(RETRY)).collect();
    assert_eq!(
        retries.len(),
        1,
        "one replacement during setup, one setup retry\n{stderr}"
    );
    assert!(
        retries[0].contains("hermit: verification run 1 setup attempt 1 of 5"),
        "the retry names run 1's attempt: {}\n{stderr}",
        retries[0]
    );
    let report = hermit::canonical_verdict::VerificationReport::from_current_json_slice(
        &std::fs::read(&report_path).expect("read verify json"),
    )
    .expect("parse verify json");
    assert!(report.verified, "{stderr}");
    assert!(report.bitwise_parity, "{stderr}");
    assert!(report.discarded_attempts.is_empty(), "{stderr}");
    assert!(
        stdout.lines().next().is_some() && stdout.lines().all(|line| line == "end"),
        "the guest finds {} empty, and the run shows nothing else\n{stdout}\n{stderr}",
        fixture.nscd_target().display()
    );
}

/// The bound itself: with a new nscd directory renamed over the old one in
/// every setup attempt, every attempt is detached and the launch refuses,
/// naming the attempts, instead of retrying forever.
#[test]
fn setup_retry_exhaustion_refuses_naming_the_attempts() {
    let fixture = fixture();
    let mut held = fixture.launch("exhaustion", "", "true", Some("hold-each"));
    for visit in 1..=5 {
        held.at_hold(visit, || fixture.rename_over_nscd(false));
    }
    let (status, _, stderr) = held.wait();

    assert_eq!(
        status.code(),
        Some(hermit::HERMIT_INTERNAL_FAILURE_EXIT),
        "{stderr}"
    );
    assert!(
        stderr.contains("container setup was still detached after 5 attempts"),
        "exhaustion must name the attempts\n{stderr}"
    );
    assert_eq!(
        stderr.matches(RETRY).count(),
        4,
        "four retries precede the fifth, refusing attempt\n{stderr}"
    );
    assert_eq!(
        stderr.matches(&hold_line()).count(),
        5,
        "each of the five attempts held once\n{stderr}"
    );
}

/// Malformed or extreme `HERMIT_RACE_PROBE` settings for the setup point
/// (empty, inverted, full-width, out-of-range probability, garbage, a hold),
/// inherited by a launch whose hold directory is not armed: the setup point
/// reads none of them, and every launch runs.
#[test]
fn an_unarmed_setup_point_ignores_any_race_probe_setting() {
    let fixture = fixture();
    let full = format!("{POINT}=0-{}", u64::MAX);
    let settings = [
        format!("{POINT}="),
        format!("{POINT}=9-1"),
        full.clone(),
        format!("{full}@0.5"),
        format!("{POINT}=1-2@7"),
        format!("{POINT}=garbage"),
        format!("{POINT}=hold"),
        format!("{POINT}=hold-each"),
    ];
    for (index, setting) in settings.iter().enumerate() {
        let held = fixture.launch_with_probe(
            &format!("unarmed-{index}"),
            "",
            "true",
            Some(setting),
            false,
        );
        let (status, _, stderr) = held.wait();
        assert!(
            status.success(),
            "an unarmed launch under HERMIT_RACE_PROBE={setting} refused: {status}\n{stderr}"
        );
        assert!(
            !stderr.contains("panicked") && !stderr.contains(&hold_line()),
            "an unarmed launch under HERMIT_RACE_PROBE={setting} held or panicked\n{stderr}"
        );
    }
}

/// An armed setup point takes nothing but a complete, valid hold for itself:
/// a bare point, a trailing `=`, an inverted range, a wait, an unknown point
/// beside its hold, and garbage each fail the launch, naming the point and
/// why, instead of letting it run unheld.
#[test]
fn an_armed_setup_point_with_a_refused_setting_fails_by_name() {
    let fixture = fixture();
    let settings = [
        (
            POINT.to_owned(),
            format!("\"{POINT}\" is not point=LO[-HI][@P]"),
        ),
        (
            format!("{POINT}="),
            "is not a number of milliseconds".to_owned(),
        ),
        (format!("{POINT}=9-1"), "the range is empty".to_owned()),
        (
            format!("{POINT}=5"),
            "is not a hold (hold or hold-each)".to_owned(),
        ),
        (
            format!("{POINT}=hold,no_such_point=hold"),
            "unknown point \"no_such_point\"".to_owned(),
        ),
        (
            format!("{POINT}=garbage"),
            "is not a number of milliseconds".to_owned(),
        ),
    ];
    for (index, (setting, reason)) in settings.iter().enumerate() {
        let held = fixture.launch_with_probe(
            &format!("armed-refused-{index}"),
            "",
            "true",
            Some(setting),
            true,
        );
        let (status, _, stderr) = held.wait();
        assert!(
            !status.success(),
            "the launch under {setting} ran\n{stderr}"
        );
        let refusal =
            format!("race_probe: {POINT} is armed, and HERMIT_RACE_PROBE is refused there: ");
        assert!(
            stderr
                .lines()
                .any(|line| line.contains(&refusal) && line.contains(reason.as_str())),
            "the launch under {setting} did not name its refusal ({reason})\n{stderr}"
        );
        assert!(
            !stderr.contains(&hold_line()),
            "the launch under {setting} held\n{stderr}"
        );
    }
}

/// A case whose fixture cannot be built fails, naming why, instead of passing
/// having run nothing: here the nested hermit does not run.
#[test]
#[should_panic(
    expected = "setup_retry could not build its fixture, so this case fails instead of passing having run nothing: nested hermit probe failed"
)]
fn an_unbuildable_fixture_fails_the_case_by_name() {
    fixture_with(Path::new("/bin/false"));
}
