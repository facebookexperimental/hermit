//! Bounded ordinary-loader handoff into a normally constructed Detcore root.
//!
//! The loader owns the callback; the existing supervisor authenticates its
//! requests. Detcore owns the payload schema, config and image binding. Neither
//! installing this transport nor decoding its bytes grants lifecycle authority
//! or proves a typed RPC, thread admission or scheduler readiness.

use std::io;
use std::io::Read;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use reverie::Pid;
use reverie::Tool;

pub(crate) type TakeStateFn = unsafe extern "C" fn(*mut libc::c_void, usize) -> libc::c_long;
type RootState = <detcore::Detcore as Tool>::ThreadState;

const MAX_STATE_BYTES: usize = 4096;
const _: () = assert!(MAX_STATE_BYTES == detcore::random::MAX_INITIAL_STATE_BYTES);

// Both fields are allocation/TLS-free before the guarded Rust entry. The
// pointer is an exact loader-owned function, never an environment address.
struct LoaderCallback {
    function: AtomicUsize,
    taken: AtomicU8,
}

impl LoaderCallback {
    const fn new() -> Self {
        Self {
            function: AtomicUsize::new(0),
            taken: AtomicU8::new(0),
        }
    }

    /// # Safety
    /// The loaded ordinary loader owns this lifetime-stable function. It must
    /// write at most the supplied capacity and return its count or -errno.
    unsafe fn install(&self, callback: TakeStateFn) -> Result<(), i32> {
        self.function
            .compare_exchange(0, callback as usize, Ordering::Release, Ordering::Relaxed)
            .map(|_| ())
            .map_err(|_| libc::EPROTO)
    }

    fn take(&self, destination: &mut [u8]) -> io::Result<Option<usize>> {
        if destination.is_empty() || destination.len() > MAX_STATE_BYTES {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let function = self.function.load(Ordering::Acquire);
        if function == 0 {
            return Ok(None);
        }
        // A refusal is terminal for this engine installation. Never retry the
        // loader transfer or fall back to a newly seeded root after an error.
        self.taken
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| io::Error::from_raw_os_error(libc::EPROTO))?;
        // SAFETY: install accepted only the loader's lifetime-stable callback.
        let callback: TakeStateFn = unsafe { core::mem::transmute(function) };
        // SAFETY: the destination is writable for this exact bounded capacity.
        let result = unsafe { callback(destination.as_mut_ptr().cast(), destination.len()) };
        if (-4095..0).contains(&result) {
            return Err(io::Error::from_raw_os_error((-result) as i32));
        }
        if result <= 0 || result as usize > destination.len() {
            return Err(io::Error::from_raw_os_error(libc::EPROTO));
        }
        Ok(Some(result as usize))
    }
}

static CALLBACK: LoaderCallback = LoaderCallback::new();

/// Set only by the real loader's symbol negotiation, before engine installation.
/// This stores a transport callback, not a private-phase permit or Tool state.
///
/// # Safety
/// The function must satisfy [`TakeStateFn`]'s loader lifetime/write contract.
pub(crate) unsafe fn install(callback: TakeStateFn) -> Result<(), i32> {
    unsafe { CALLBACK.install(callback) }
}

pub(crate) fn initialize_root(
    config: &detcore::Config,
    pid: Pid,
    state: &mut RootState,
) -> io::Result<()> {
    let image = own_initial_image(pid.as_raw())?;
    let mut bytes = [0; MAX_STATE_BYTES];
    let count = CALLBACK
        .take(&mut bytes)?
        .ok_or_else(|| io::Error::other("required initial random handoff was not negotiated"))?;
    apply_handoff(&bytes[..count], config, image, state)
}

fn apply_handoff(
    bytes: &[u8],
    config: &detcore::Config,
    image: detcore::random::InitialImage,
    state: &mut RootState,
) -> io::Result<()> {
    match detcore::random::decode_loader_state(bytes, config, image).map_err(io::Error::other)? {
        detcore::random::LoaderState::InitialRandom { .. } => {
            state
                .apply_initial_random_state(bytes, config, image)
                .map_err(io::Error::other)?;
        }
        detcore::random::LoaderState::ObservedExecContinuation
        | detcore::random::LoaderState::InitialStaticLegacy => {
            // The held loader/supervisor proved the real legacy image. Keep
            // every normal constructor field; post-exec still runs unchanged.
        }
    }
    Ok(())
}

// These two image readers retain the legacy Detcore adapter's exact bounds
// and PID/start-time/AT_RANDOM checks. No wall clock or host seed is imported.
fn initial_image_generation(pid: i32, bytes: &[u8]) -> io::Result<u64> {
    // The kernel's comm field is raw bytes, including possible ") " bytes.
    // Decode only the PID and start-time fields, outside the final delimiter.
    let end = bytes
        .windows(2)
        .rposition(|pair| pair == b") ")
        .ok_or_else(|| io::Error::other("invalid process stat"))?;
    let prefix = &bytes[..end];
    let owner_end = prefix
        .windows(2)
        .position(|pair| pair == b" (")
        .ok_or_else(|| io::Error::other("invalid stat owner"))?;
    let owner = std::str::from_utf8(&prefix[..owner_end]).map_err(io::Error::other)?;
    if owner.parse::<i32>().map_err(io::Error::other)? != pid {
        return Err(io::Error::other("initial random handoff owner mismatch"));
    }
    let start = bytes[end + 2..]
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
        .nth(19)
        .ok_or_else(|| io::Error::other("missing process generation"))?;
    std::str::from_utf8(start)
        .map_err(io::Error::other)?
        .parse()
        .map_err(io::Error::other)
}

fn own_initial_image(pid: i32) -> io::Result<detcore::random::InitialImage> {
    fn bounded(path: &str) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(4097)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            return Err(io::Error::other("initial image identity exceeds bound"));
        }
        Ok(bytes)
    }
    let before = bounded("/proc/self/stat")?;
    let start_time_ticks = initial_image_generation(pid, &before)?;
    let auxv = bounded("/proc/self/auxv")?;
    if auxv.len() % 16 != 0 {
        return Err(io::Error::other("incomplete initial auxv"));
    }
    let mut at_random = None;
    let mut terminated = false;
    for row in auxv.as_chunks::<16>().0 {
        let key = u64::from_ne_bytes(row[..8].try_into().unwrap());
        let value = u64::from_ne_bytes(row[8..].try_into().unwrap());
        if key == libc::AT_NULL {
            terminated = value == 0;
            break;
        }
        if key == libc::AT_RANDOM && at_random.replace(value as usize).is_some() {
            return Err(io::Error::other("duplicate initial AT_RANDOM"));
        }
    }
    if !terminated {
        return Err(io::Error::other("unterminated initial auxv"));
    }
    Ok(detcore::random::InitialImage {
        pid,
        start_time_ticks,
        at_random: at_random
            .filter(|p| *p != 0)
            .ok_or_else(|| io::Error::other("missing initial AT_RANDOM"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn refused(_: *mut libc::c_void, _: usize) -> libc::c_long {
        -(libc::ESTALE as libc::c_long)
    }
    unsafe extern "C" fn empty(_: *mut libc::c_void, _: usize) -> libc::c_long {
        0
    }
    unsafe extern "C" fn too_long(_: *mut libc::c_void, capacity: usize) -> libc::c_long {
        capacity as libc::c_long + 1
    }
    unsafe extern "C" fn valid(buffer: *mut libc::c_void, capacity: usize) -> libc::c_long {
        if capacity < 2 {
            return -(libc::ENOSPC as libc::c_long);
        }
        unsafe { core::ptr::copy_nonoverlapping([0x12, 0x34].as_ptr(), buffer.cast::<u8>(), 2) };
        2
    }

    #[test]
    fn transport_refusals_never_become_an_empty_or_retryable_state() {
        for (callback, expected) in [
            (refused as TakeStateFn, libc::ESTALE),
            (empty as TakeStateFn, libc::EPROTO),
            (too_long as TakeStateFn, libc::EPROTO),
        ] {
            let slot = LoaderCallback::new();
            let mut bytes = [0xa5; 32];
            unsafe { slot.install(callback) }.unwrap();
            assert_eq!(
                slot.take(&mut bytes).unwrap_err().raw_os_error(),
                Some(expected)
            );
            assert_eq!(bytes, [0xa5; 32]);
            assert_eq!(
                slot.take(&mut bytes).unwrap_err().raw_os_error(),
                Some(libc::EPROTO)
            );
        }
        let slot = LoaderCallback::new();
        assert_eq!(slot.take(&mut [0; 1]).unwrap(), None);
        assert_eq!(
            slot.take(&mut []).unwrap_err().raw_os_error(),
            Some(libc::EINVAL)
        );
        assert_eq!(
            slot.take(&mut [0; MAX_STATE_BYTES + 1])
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EINVAL)
        );
    }

    #[test]
    fn transport_is_exact_bounded_and_cannot_be_replaced_or_taken_twice() {
        let slot = LoaderCallback::new();
        unsafe { slot.install(valid) }.unwrap();
        assert_eq!(unsafe { slot.install(empty) }, Err(libc::EPROTO));
        let mut bytes = [0xa5; 4];
        assert_eq!(slot.take(&mut bytes).unwrap(), Some(2));
        assert_eq!(bytes, [0x12, 0x34, 0xa5, 0xa5]);
        assert_eq!(
            slot.take(&mut bytes).unwrap_err().raw_os_error(),
            Some(libc::EPROTO)
        );
    }

    fn root(config: &detcore::Config, pid: i32) -> RootState {
        let pid = Pid::from_raw(pid);
        let tool = <detcore::Detcore as Tool>::new(pid, config);
        tool.init_thread_state(pid, None)
    }
    fn normal_fields(state: &RootState) -> String {
        format!(
            "{:?}",
            (
                state.dettid,
                state.detpid,
                state.physical_tid,
                state.mm_id,
                &state.chaos_prng,
                &state.thread_logical_time,
                state.committed_clock_value,
            )
        )
    }
    fn image() -> detcore::random::InitialImage {
        detcore::random::InitialImage {
            pid: 31,
            start_time_ticks: 17,
            at_random: 0x1000,
        }
    }

    #[test]
    fn authenticated_random_handoff_changes_only_its_declared_fields_once() {
        let config = detcore::Config::default();
        let image = image();
        let mut state = root(&config, image.pid);
        let fields = normal_fields(&state);
        let prng = detcore::random::root_prng(97);
        let bytes =
            detcore::random::encode_initial_state(&config, image, &prng, [0x5a; 16], &[]).unwrap();
        apply_handoff(&bytes, &config, image, &mut state).unwrap();
        assert_eq!(format!("{:?}", state.prng), format!("{prng:?}"));
        assert_eq!(normal_fields(&state), fields);
        assert!(apply_handoff(&bytes, &config, image, &mut state).is_err());
        assert_eq!(normal_fields(&state), fields);
    }

    #[test]
    fn authenticated_continuations_preserve_normal_state() {
        let config = detcore::Config::default();
        for continuation in [
            detcore::random::LoaderState::ObservedExecContinuation,
            detcore::random::LoaderState::InitialStaticLegacy,
        ] {
            let image = image();
            let mut state = root(&config, image.pid);
            let fields = normal_fields(&state);
            let rng = format!("{:?}", state.prng);
            let bytes = detcore::random::encode_continuation(&config, image, continuation).unwrap();
            apply_handoff(&bytes, &config, image, &mut state).unwrap();
            assert_eq!(normal_fields(&state), fields);
            assert_eq!(format!("{:?}", state.prng), rng);
        }
    }

    #[test]
    fn wrong_config_image_and_noncanonical_handoffs_refuse_without_reseeding() {
        let config = detcore::Config::default();
        let image = image();
        let prng = detcore::random::root_prng(97);
        let bytes =
            detcore::random::encode_initial_state(&config, image, &prng, [0x5a; 16], &[]).unwrap();
        let mut state = root(&config, image.pid);
        let fields = normal_fields(&state);
        let rng = format!("{:?}", state.prng);
        let mut other_config = config.clone();
        other_config.seed ^= 1;
        assert!(apply_handoff(&bytes, &other_config, image, &mut state).is_err());
        assert!(
            apply_handoff(
                &bytes,
                &config,
                detcore::random::InitialImage {
                    start_time_ticks: 18,
                    ..image
                },
                &mut state
            )
            .is_err()
        );
        assert!(
            apply_handoff(
                &bytes,
                &config,
                detcore::random::InitialImage { pid: 37, ..image },
                &mut state
            )
            .is_err()
        );
        assert!(
            apply_handoff(
                &bytes,
                &config,
                detcore::random::InitialImage {
                    at_random: 0x2000,
                    ..image
                },
                &mut state
            )
            .is_err()
        );
        let mut noncanonical = bytes.clone();
        noncanonical.push(b' ');
        assert!(apply_handoff(&noncanonical, &config, image, &mut state).is_err());
        assert!(apply_handoff(&[], &config, image, &mut state).is_err());
        assert_eq!(normal_fields(&state), fields);
        assert_eq!(format!("{:?}", state.prng), rng);
    }

    #[test]
    fn initial_generation_accepts_raw_worker_comm_and_rejects_invalid_identity() {
        struct RestoreComm {
            tid: i32,
            name: [u8; 16],
        }
        impl Drop for RestoreComm {
            fn drop(&mut self) {
                assert_eq!(unsafe { libc::syscall(libc::SYS_gettid) } as i32, self.tid);
                assert_eq!(
                    unsafe { libc::prctl(libc::PR_SET_NAME, self.name.as_ptr()) },
                    0
                );
            }
        }
        // /proc/self/stat names the leader. Use this owned libtest worker's
        // actual stat to exercise the same production parser without renaming
        // the leader or claiming that an injected SaBRe guest was executed.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let path = format!("/proc/{tid}/stat");
        let expected = initial_image_generation(tid, &std::fs::read(&path).unwrap()).unwrap();
        let mut restore = RestoreComm { tid, name: [0; 16] };
        assert_eq!(
            unsafe { libc::prctl(libc::PR_GET_NAME, restore.name.as_mut_ptr()) },
            0
        );
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_NAME, c"raw) \xff) task".as_ptr()) },
            0
        );
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() <= 4096);
        assert!(std::str::from_utf8(&bytes).is_err());
        assert!(bytes.windows(12).any(|row| row == b"raw) \xff) task"));
        assert_eq!(initial_image_generation(tid, &bytes).unwrap(), expected);
        assert!(
            initial_image_generation(tid + 1, &bytes)
                .unwrap_err()
                .to_string()
                .contains("owner mismatch")
        );
        assert!(initial_image_generation(tid, b"missing delimiter").is_err());
        let end = bytes.windows(2).rposition(|row| row == b") ").unwrap();
        let mut fields: Vec<&[u8]> = bytes[end + 2..]
            .split(u8::is_ascii_whitespace)
            .filter(|field| !field.is_empty())
            .collect();
        let mut malformed = bytes[..end + 2].to_vec();
        fields[19] = b"not-a-number";
        malformed.extend(fields.join(&b' '));
        assert!(initial_image_generation(tid, &malformed).is_err());
        assert!(initial_image_generation(tid, &bytes[..end + 2]).is_err());
        let owner_end = bytes.windows(2).position(|row| row == b" (").unwrap();
        let mut malformed_owner = b"not-a-pid".to_vec();
        malformed_owner.extend_from_slice(&bytes[owner_end..]);
        assert!(initial_image_generation(tid, &malformed_owner).is_err());
        drop(restore);
    }

    #[test]
    fn raw_stat_parser_binds_owner_generation_and_reads_real_initial_identity() {
        let mut stat = b"31 (raw) \xff) task) S".to_vec();
        for _ in 0..18 {
            stat.extend_from_slice(b" 0");
        }
        stat.extend_from_slice(b" 17 0\n");
        assert_eq!(initial_image_generation(31, &stat).unwrap(), 17);
        assert!(initial_image_generation(37, &stat).is_err());
        assert!(initial_image_generation(31, b"31 (x) S").is_err());
        assert!(initial_image_generation(31, b"missing delimiter").is_err());
        let pid = unsafe { libc::syscall(libc::SYS_getpid) } as i32;
        let image = own_initial_image(pid).unwrap();
        assert_eq!(image.pid, pid);
        assert_ne!(image.start_time_ticks, 0);
        assert_ne!(image.at_random, 0);
        assert!(own_initial_image(pid + 1).is_err());
    }

    #[test]
    fn root_initializer_requires_negotiated_loader_state() {
        let config = detcore::Config::default();
        let pid = unsafe { libc::syscall(libc::SYS_getpid) } as i32;
        let mut state = root(&config, pid);
        let fields = normal_fields(&state);
        let rng = format!("{:?}", state.prng);
        assert!(
            initialize_root(&config, Pid::from_raw(pid), &mut state)
                .unwrap_err()
                .to_string()
                .contains("not negotiated")
        );
        assert_eq!(normal_fields(&state), fields);
        assert_eq!(format!("{:?}", state.prng), rng);
        // A separate slot exercises the actual setter's duplicate refusal;
        // no test changes the process-wide production callback.
        let _: unsafe fn(TakeStateFn) -> Result<(), i32> = install;
    }
}
