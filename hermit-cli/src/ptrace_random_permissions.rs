// Included only in real_random's isolated tests. Linux capability ABI v3 and
// CAP_SYS_PTRACE=19 come from linux/capability.h; 21 is CAP_SYS_ADMIN.
const SYS_PTRACE_CAP: u32 = 19;
const SYS_PTRACE_BIT: u64 = 1 << SYS_PTRACE_CAP;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

// Called ONLY in the owned fork child's pre_exec hook. No allocator, lock,
// stdio formatting, namespace change, or change to the outer test process.
fn restrict_ptrace_copy() -> std::io::Result<()> {
    fn checked(result: libc::c_long) -> std::io::Result<()> {
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    let mut header = CapHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut caps = [CapData::default(); 2];
    checked(unsafe { libc::syscall(libc::SYS_capget, &mut header, caps.as_mut_ptr()) })?;
    checked(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } as _)?;
    checked(unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_LOWER,
            SYS_PTRACE_CAP,
            0,
            0,
        )
    } as _)?;
    // Bounding-set removal requires CAP_SETPCAP=8. Unprivileged callers may
    // retain this bit in Bnd: NNP and absent P/I/A still prevent exec gains.
    let bound = unsafe { libc::prctl(libc::PR_CAPBSET_READ, SYS_PTRACE_CAP, 0, 0, 0) };
    if bound < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if bound == 1 && caps[0].effective & (1 << 8) != 0 {
        checked(unsafe { libc::prctl(libc::PR_CAPBSET_DROP, SYS_PTRACE_CAP, 0, 0, 0) } as _)?;
        if unsafe { libc::prctl(libc::PR_CAPBSET_READ, SYS_PTRACE_CAP, 0, 0, 0) } != 0 {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
    }
    let bit = !(1 << SYS_PTRACE_CAP);
    caps[0].effective &= bit;
    caps[0].permitted &= bit;
    caps[0].inheritable &= bit;
    checked(unsafe { libc::syscall(libc::SYS_capset, &header, caps.as_ptr()) })?;
    let mut observed = [CapData::default(); 2];
    checked(unsafe { libc::syscall(libc::SYS_capget, &mut header, observed.as_mut_ptr()) })?;
    if observed != caps
        || unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1
        || unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_IS_SET,
                SYS_PTRACE_CAP,
                0,
                0,
            )
        } != 0
    {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Credentials {
    uid: String,
    gid: String,
    caps: [u64; 5], // effective, permitted, inheritable, ambient, bounding
    no_new_privs: u32,
    user_ns: (u64, u64),
    tracer: i64,
}

fn credentials(proc_path: &str, role: &str) -> Credentials {
    use std::os::unix::fs::MetadataExt;
    let text = std::fs::read_to_string(format!("{proc_path}/status")).unwrap();
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap()
            .trim()
            .to_owned()
    };
    let ns = std::fs::metadata(format!("{proc_path}/ns/user")).unwrap();
    let observed = Credentials {
        uid: field("Uid:"),
        gid: field("Gid:"),
        caps: ["CapEff:", "CapPrm:", "CapInh:", "CapAmb:", "CapBnd:"]
            .map(|key| u64::from_str_radix(&field(key), 16).unwrap()),
        no_new_privs: field("NoNewPrivs:").parse().unwrap(),
        user_ns: (ns.dev(), ns.ino()),
        tracer: field("TracerPid:").parse().unwrap(),
    };
    eprintln!("REAL_RANDOM_CREDENTIALS role={role} path={proc_path} {observed:?}");
    observed
}

fn restricted_thread_credentials(role: &str) -> (i64, Credentials) {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    let current = credentials(&format!("/proc/self/task/{tid}"), role);
    assert!(
        current.caps[..4]
            .iter()
            .all(|bits| bits & SYS_PTRACE_BIT == 0)
    );
    assert_eq!(current.no_new_privs, 1);
    // The pre_exec initializer precedes libtest and runtime thread creation.
    // Check the actual remaining harness/worker threads, not just the leader.
    for task in std::fs::read_dir("/proc/self/task").unwrap() {
        let task = task.unwrap();
        let worker = credentials(task.path().to_str().unwrap(), "isolated-worker");
        assert!(
            worker.caps[..4]
                .iter()
                .all(|bits| bits & SYS_PTRACE_BIT == 0)
        );
        assert_eq!(worker.no_new_privs, 1);
        assert_eq!(worker.uid, current.uid);
        assert_eq!(worker.gid, current.gid);
        assert_eq!(worker.user_ns, current.user_ns);
    }
    (tid, current)
}

fn assert_tracee_credentials(pid: i32, tid: i64, tracer: &Credentials) {
    let guest = credentials(&format!("/proc/{pid}"), "tracee-before-gate");
    assert_eq!(guest.tracer, tid);
    assert_eq!(guest.uid, tracer.uid);
    assert_eq!(guest.gid, tracer.gid);
    assert_eq!(guest.user_ns, tracer.user_ns);
    assert!(
        guest.caps[..4]
            .iter()
            .all(|bits| bits & SYS_PTRACE_BIT == 0)
    );
    assert_eq!(guest.no_new_privs, 1);
}
