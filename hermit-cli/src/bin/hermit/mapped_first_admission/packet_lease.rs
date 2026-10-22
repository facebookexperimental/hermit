//! One pre-inherited packet OFD, filled from the real watched admission only.
//! This module has no launcher, Config/nonce constructor, wire codec or Ready.
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use reverie_inguest::launcher::exec_stop::InitialExecStop;
use reverie_inguest::launcher::exec_stop::physical::PreparedMapping;

use super::MappedFirstAdmission;

const MAX_PACKET: usize = detcore_model::liteinst_initial_image::MAX_PACKET_LEN;
const SEALS: i32 = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}
fn need(ok: bool, message: &str) -> io::Result<()> {
    if ok { Ok(()) } else { Err(invalid(message)) }
}
fn seals(file: &File) -> io::Result<i32> {
    // SAFETY: query only, using this retained owned descriptor.
    let value = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}
fn offset(file: &File) -> io::Result<i64> {
    // SAFETY: SEEK_CUR with offset zero observes this same retained OFD.
    let value = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) };
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}
fn empty(file: &File) -> io::Result<()> {
    need(
        file.metadata()?.len() == 0 && seals(file)? == 0 && offset(file)? == 0,
        "mapped packet lease is not pristine empty unsealed OFD",
    )
}
fn verify_file(file: &File, bytes: &[u8]) -> io::Result<()> {
    need(
        !bytes.is_empty() && bytes.len() <= MAX_PACKET,
        "mapped packet size outside scalar packet bound",
    )?;
    need(
        file.metadata()?.len() == bytes.len() as u64 && seals(file)? == SEALS && offset(file)? == 0,
        "mapped packet sealed length/seals/offset differ",
    )?;
    let mut actual = vec![0; bytes.len()];
    file.read_exact_at(&mut actual, 0)?;
    need(actual == bytes, "mapped packet retained OFD bytes differ")?;
    let mut trailing = [0];
    need(
        file.read_at(&mut trailing, bytes.len() as u64)? == 0,
        "mapped packet retained OFD has trailing bytes",
    )?;
    need(
        file.metadata()?.len() == bytes.len() as u64 && seals(file)? == SEALS && offset(file)? == 0,
        "mapped packet changed across positional verification",
    )
}
fn finish_file(file: &File, bytes: &[u8]) -> io::Result<()> {
    empty(file)?;
    need(
        !bytes.is_empty() && bytes.len() <= MAX_PACKET,
        "mapped packet size outside scalar packet bound",
    )?;
    // Positional IO does not advance the inherited shared OFD's offset. No
    // pathname, reopened descriptor, dup-created replacement or mmap is used.
    file.write_all_at(bytes, 0)?;
    file.set_len(bytes.len() as u64)?;
    file.sync_all()?;
    // SAFETY: add immutable content/size/seal closure to the retained memfd.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, SEALS) } < 0 {
        return Err(io::Error::last_os_error());
    }
    verify_file(file, bytes)
}

// One attempt and permanent failure are distinct predicates. These atomics do
// not serialize file/admission operations or turn verification into a commit.
// Physical caller exclusivity remains required. No method clears poisoned.
#[derive(Default)]
struct PacketState {
    attempted: AtomicBool,
    poisoned: AtomicBool,
}
impl PacketState {
    fn healthy(&self) -> io::Result<()> {
        need(
            !self.poisoned.load(Ordering::Acquire),
            "mapped packet lease is permanently poisoned",
        )
    }
    fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }
    fn begin_fill(&self) -> io::Result<()> {
        self.healthy()?;
        if self.attempted.swap(true, Ordering::AcqRel) {
            self.poison();
            return Err(invalid("mapped packet lease fill was already attempted"));
        }
        self.healthy()
    }
    // The same production predicate brackets filling AND sealed verification.
    // Any ordinary Result error records permanent failure, including a failure
    // observed only at the final barrier after the operation itself succeeded.
    fn checked<T>(&self, operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let result = (|| {
            self.healthy()?;
            let value = operation()?;
            self.healthy()?;
            Ok(value)
        })();
        if result.is_err() {
            self.poison();
        }
        result
    }
}

/// Create before the actual launch; borrow this exact descriptor into PreparedExec.
/// No mutable File/OwnedFd export or replacement/import constructor exists.
/// The launch owner must retain this lease and its returned BorrowedFd through
/// terminal cleanup, without unsafe close/dup2/offset mutation or concurrent IO.
pub(super) struct PendingPacketLease {
    file: File,
    state: PacketState,
}
impl PendingPacketLease {
    pub(super) fn new() -> io::Result<Self> {
        // Reuses the existing verify-input producer's memfd creation mechanism.
        let fd =
            crate::verify_inputs::new_unsealed_memfd(c"hermit-mapped-initial").map_err(invalid)?;
        let lease = Self {
            file: File::from(fd),
            state: PacketState::default(),
        };
        empty(&lease.file)?;
        need(
            lease.file.as_raw_fd() > 2,
            "mapped packet descriptor must not be stdio",
        )?;
        Ok(lease)
    }
    pub(super) fn inherited_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// The actual retained launch request must have borrowed this exact FD before
    /// exec. The new R bridge validates that request and the held birth together.
    /// Every failed attempt poisons this lease and the admission; the caller
    /// retains and cancels the real stopped child. No retry or guest fallback.
    pub(super) fn fill<'a>(
        &'a self,
        stopped: &InitialExecStop<'_>,
        admission: &Arc<MappedFirstAdmission>,
    ) -> io::Result<SealedPacketLease<'a>> {
        let result = self.state.checked(|| {
            self.state.begin_fill()?;
            empty(&self.file)?;
            let birth = stopped
                .duplicate_birth_pidfd_for_inherited(self.file.as_fd())
                .map_err(invalid)?;
            // The same existing watched-peer predicate compares actual pidfd
            // birth identity; a caller PID or equal inode/content is insufficient.
            super::require_watched_peer(&admission.watch, &birth, &admission.failed)?;
            let bytes = admission.initial_packet()?;
            finish_file(&self.file, &bytes)?;
            let after = stopped
                .duplicate_birth_pidfd_for_inherited(self.file.as_fd())
                .map_err(invalid)?;
            super::require_watched_peer(&admission.watch, &after, &admission.failed)?;
            need(
                admission.initial_packet()? == bytes,
                "actual admission packet changed during sealing",
            )?;
            Ok(SealedPacketLease {
                pending: self,
                admission: admission.clone(),
                bytes,
            })
        });
        if let Err(error) = &result {
            admission.fail(error);
        }
        result
    }
}

/// Retains the same sealed inherited OFD plus the actual admission and exact
/// packet bytes. This is packet evidence, never child cleanup or entry authority.
pub(super) struct SealedPacketLease<'a> {
    pending: &'a PendingPacketLease,
    admission: Arc<MappedFirstAdmission>,
    bytes: Vec<u8>,
}
impl SealedPacketLease<'_> {
    pub(super) fn fd(&self) -> BorrowedFd<'_> {
        self.pending.file.as_fd()
    }
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub(super) fn verify(&self, stopped: &InitialExecStop<'_>) -> io::Result<()> {
        self.verify_with_birth(|| {
            stopped
                .duplicate_birth_pidfd_for_inherited(self.fd())
                .map_err(invalid)
        })
    }

    /// Recheck the same inherited packet after mapping, while the original
    /// physical owner retains its held stop. This grants no entry or Ready.
    pub(super) fn verify_prepared(&self, prepared: &PreparedMapping<'_>) -> io::Result<()> {
        self.verify_with_birth(|| {
            prepared
                .duplicate_birth_pidfd_for_inherited(self.fd())
                .map_err(invalid)
        })
    }

    // Only the two concrete adapters above supply this private operation.
    // Poison is checked before the original owner is queried, and every Result
    // failure still poisons both the packet lease and its actual admission.
    fn verify_with_birth(&self, birth: impl FnOnce() -> io::Result<OwnedFd>) -> io::Result<()> {
        let result = self.pending.state.checked(|| {
            let birth = birth()?;
            super::require_watched_peer(&self.admission.watch, &birth, &self.admission.failed)?;
            verify_file(&self.pending.file, &self.bytes)?;
            need(
                self.admission.initial_packet()? == self.bytes,
                "mapped packet no longer matches actual admission",
            )
        });
        if let Err(error) = &result {
            self.admission.fail(error);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // These are real memfd storage controls only, NOT a watched-child positive.
    #[test]
    fn packet_storage_keeps_same_ofd_zero_offset_exact_bytes_and_seals() {
        let lease = PendingPacketLease::new().unwrap();
        let raw = lease.file.as_raw_fd();
        let alias = lease.file.try_clone().unwrap(); // a real duplicate shares OFD
        let bytes = b"storage-control-not-admission";
        finish_file(&lease.file, bytes).unwrap();
        assert_eq!(lease.file.as_raw_fd(), raw);
        assert_eq!(offset(&alias).unwrap(), 0);
        verify_file(&alias, bytes).unwrap();
        assert_eq!(
            alias.write_at(b"X", 0).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        assert!(verify_file(&lease.file, b"different").is_err());
        assert!(finish_file(&lease.file, bytes).is_err());
    }
    #[test]
    fn packet_storage_refuses_nonpristine_offset_and_size_bounds() {
        let lease = PendingPacketLease::new().unwrap();
        // SAFETY: this data-only test owns the OFD and changes its offset.
        assert_eq!(
            unsafe { libc::lseek(lease.file.as_raw_fd(), 1, libc::SEEK_SET) },
            1
        );
        assert!(finish_file(&lease.file, b"x").is_err());
        assert_eq!(lease.file.metadata().unwrap().len(), 0);
        let other = PendingPacketLease::new().unwrap();
        assert!(finish_file(&other.file, b"").is_err());
        assert!(finish_file(&other.file, &vec![0; MAX_PACKET + 1]).is_err());
        assert_eq!(other.file.metadata().unwrap().len(), 0);
        assert_eq!(seals(&other.file).unwrap(), 0);
    }

    #[test]
    fn packet_predicate_repeated_fill_failure_invalidates_prior_storage_success() {
        let lease = PendingPacketLease::new().unwrap();
        let bytes = b"state-control-not-admission";
        lease
            .state
            .checked(|| {
                lease.state.begin_fill()?;
                finish_file(&lease.file, bytes)
            })
            .unwrap();
        lease
            .state
            .checked(|| verify_file(&lease.file, bytes))
            .unwrap();
        // These two observers are data only, not fabricated MappedFirstAdmission
        // instances. Production fill/verify notify their actual admission on Err.
        let mut observer_b_failed = false;
        let later = lease.state.checked(|| lease.state.begin_fill());
        if later.is_err() {
            observer_b_failed = true;
        }
        assert!(observer_b_failed);
        assert!(lease.state.attempted.load(Ordering::Acquire));
        assert!(lease.state.poisoned.load(Ordering::Acquire));
        // The file itself is unchanged and still satisfies storage checks.
        verify_file(&lease.file, bytes).unwrap();
        let mut observer_a_failed = false;
        let old = lease.state.checked(|| verify_file(&lease.file, bytes));
        if old.is_err() {
            observer_a_failed = true;
        }
        assert!(
            observer_a_failed,
            "earlier success must not remain a valid lease"
        );
        assert!(
            old.unwrap_err()
                .to_string()
                .contains("permanently poisoned")
        );
    }
    #[test]
    fn packet_predicate_verification_failure_cannot_be_repaired_into_success() {
        let lease = PendingPacketLease::new().unwrap();
        let bytes = b"offset-state-control";
        lease
            .state
            .checked(|| {
                lease.state.begin_fill()?;
                finish_file(&lease.file, bytes)
            })
            .unwrap();
        // SAFETY: this storage-only control owns the memfd and its shared offset.
        assert_eq!(
            unsafe { libc::lseek(lease.file.as_raw_fd(), 1, libc::SEEK_SET) },
            1
        );
        assert!(
            lease
                .state
                .checked(|| verify_file(&lease.file, bytes))
                .is_err()
        );
        // SAFETY: restore the exact offset; permanent poison must still refuse.
        assert_eq!(
            unsafe { libc::lseek(lease.file.as_raw_fd(), 0, libc::SEEK_SET) },
            0
        );
        verify_file(&lease.file, bytes).unwrap();
        assert!(
            lease
                .state
                .checked(|| verify_file(&lease.file, bytes))
                .is_err()
        );
    }
    #[test]
    fn packet_predicate_final_barrier_observes_concurrent_recorded_failure() {
        use std::sync::Barrier;
        let state = Arc::new(PacketState::default());
        state.checked(|| state.begin_fill()).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let poisoned = Arc::new(Barrier::new(2));
        let other = state.clone();
        let a = entered.clone();
        let b = poisoned.clone();
        let verifier = std::thread::spawn(move || {
            other.checked(|| {
                // A verification operation is in flight after its initial barrier.
                a.wait();
                b.wait();
                Ok(())
            })
        });
        entered.wait();
        assert!(state.checked(|| state.begin_fill()).is_err());
        poisoned.wait();
        assert!(
            verifier
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("permanently poisoned")
        );
        assert!(state.checked(|| Ok(())).is_err());
    }
}
