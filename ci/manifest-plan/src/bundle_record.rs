//! What a Buck e2e cell may take from its bundle instead of computing it again.
//!
//! A Buck validation runs every e2e cell as its own `test-harness run`
//! process over one immutable bundle (ci/buck-e2e/defs.bzl): the same Hermit
//! binary, harness binaries and source snapshot for each of the 2,248 cells of
//! ci/expected-e2e-plan.json. Two of the harness's per-cell costs depend on
//! the bundle alone:
//!
//! - the SHA-256 of the Hermit binary, taken before and after each cell (about
//!   0.04 CPU-s each for a 62 MB release Hermit);
//! - the canonical manifest planner's acceptance of the source snapshot, a
//!   child process of about 0.09 CPU-s whose output the harness discards.
//!
//! The bundle rule computes both once into `BUNDLE_RECORD.json`
//! (ci/buck-e2e/bundle-record), and cell.sh names that file in
//! [`BUNDLE_RECORD_ENV`]. A cell still checks everything it checked before:
//!
//! - The Hermit binary must have the digest the record names, or the cell is an
//!   infrastructure error. The digest comes from [`binary_digest`], which hashes
//!   a file once per process and again only when its identity changes (device,
//!   inode, size, modification and status-change times), and which may also
//!   take it from a per-host cache keyed by that identity
//!   ([`DIGEST_CACHE_DIR_ENV`]). Any write to or replacement of the binary
//!   changes its status-change time or inode, so a swapped binary is hashed
//!   afresh and refused, and a binary that changes during a cell still makes
//!   that cell an infrastructure error.
//! - The planner's verdict is taken from the record only when the record says
//!   it accepted this very source SHA with this very planner binary (by
//!   digest); in every other case the harness runs the planner itself, as
//!   before.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use sha2::Digest;
use sha2::Sha256;

/// The bundle record a cell reads (set by ci/buck-e2e/cell.sh).
pub const BUNDLE_RECORD_ENV: &str = "HERMIT_E2E_BUNDLE_RECORD";
/// A host-local directory of verified file digests (set by cell.sh for cells
/// that run on the host).
pub const DIGEST_CACHE_DIR_ENV: &str = "HERMIT_E2E_DIGEST_CACHE_DIR";
pub const BUNDLE_RECORD_SCHEMA: &str = "hermit-e2e-bundle-record/v1";

/// A file younger than this is hashed every time and never remembered. File
/// times advance in clock ticks, so a write within the tick of the identity
/// read could leave the identity unchanged (Git's "racy clean" index entries);
/// a bundle's binaries are minutes old by the time any cell runs.
const SETTLED: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BundleRecord {
    pub schema: String,
    /// The commit the bundle's `src/` was archived from (its SOURCE_SHA).
    pub source_sha: String,
    /// The SHA-256 of the bundle's `hermit/hermit`.
    pub hermit_sha256: String,
    /// The SHA-256 of the bundle's `bin/hermit-manifest-plan`.
    pub planner_sha256: String,
    /// What that planner said about `src/`: `accepted` or `rejected`.
    pub manifest_plan: String,
}

impl BundleRecord {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let record = serde_json::from_slice::<Self>(bytes).map_err(|error| error.to_string())?;
        if record.schema != BUNDLE_RECORD_SCHEMA {
            return Err(format!(
                "schema is {:?}, not {BUNDLE_RECORD_SCHEMA:?}",
                record.schema
            ));
        }
        let hex40 = |value: &str| value.len() == 40 && is_lower_hex(value);
        let hex64 = |value: &str| value.len() == 64 && is_lower_hex(value);
        if !hex40(&record.source_sha) {
            return Err(format!(
                "source_sha {:?} is not a full commit",
                record.source_sha
            ));
        }
        for (name, value) in [
            ("hermit_sha256", &record.hermit_sha256),
            ("planner_sha256", &record.planner_sha256),
        ] {
            if !hex64(value) {
                return Err(format!("{name} {value:?} is not a SHA-256"));
            }
        }
        if !matches!(record.manifest_plan.as_str(), "accepted" | "rejected") {
            return Err(format!(
                "manifest_plan is {:?}, not accepted or rejected",
                record.manifest_plan
            ));
        }
        Ok(record)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|error| {
            format!("cannot read the bundle record {}: {error}", path.display())
        })?;
        Self::parse(&bytes)
            .map_err(|error| format!("bundle record {} is invalid: {error}", path.display()))
    }
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The record [`BUNDLE_RECORD_ENV`] names, read once per process: `Ok(None)`
/// when the variable is unset, an error when it names an unreadable or invalid
/// record.
pub fn record_from_env() -> Result<Option<(PathBuf, BundleRecord)>, String> {
    static RECORD: OnceLock<Result<Option<(PathBuf, BundleRecord)>, String>> = OnceLock::new();
    RECORD
        .get_or_init(|| {
            let Some(path) = std::env::var_os(BUNDLE_RECORD_ENV).filter(|v| !v.is_empty()) else {
                return Ok(None);
            };
            let path = PathBuf::from(path);
            BundleRecord::load(&path).map(|record| Some((path, record)))
        })
        .clone()
}

/// The Hermit digest the bundle records, if a record is in force.
pub fn recorded_hermit_sha256() -> Result<Option<String>, String> {
    Ok(record_from_env()?.map(|(_, record)| record.hermit_sha256))
}

/// Whether the planner's acceptance of `root` may be taken from `record` at
/// `record_path` instead of running `planner`: only when the record sits
/// beside `root` in one bundle, names `source_sha`, says `accepted`, and
/// `planner` has the digest the record names. `Err` says why not, for the
/// caller to print before it runs the planner itself.
pub fn manifest_plan_accepted(
    record_path: &Path,
    record: &BundleRecord,
    root: &Path,
    planner: &Path,
    source_sha: &str,
) -> Result<(), String> {
    let bundle_of = |path: &Path| fs::canonicalize(path).ok()?.parent().map(Path::to_path_buf);
    if bundle_of(record_path).is_none() || bundle_of(record_path) != bundle_of(root) {
        return Err(format!(
            "the record {} is not in the bundle of {}",
            record_path.display(),
            root.display()
        ));
    }
    if record.source_sha != source_sha {
        return Err(format!(
            "the record is for source {}, not {source_sha}",
            record.source_sha
        ));
    }
    if record.manifest_plan != "accepted" {
        return Err(format!("the record says {}", record.manifest_plan));
    }
    match binary_digest(planner) {
        Some(digest) if digest == record.planner_sha256 => Ok(()),
        Some(digest) => Err(format!(
            "{} has sha256 {digest}, but the record names {}",
            planner.display(),
            record.planner_sha256
        )),
        None => Err(format!("cannot read {}", planner.display())),
    }
}

/// Everything about a file that a write or a replacement changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileIdentity {
    fn of(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        meta.is_file().then(|| Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        })
    }

    /// Whether both file times are at least [`SETTLED`] before `now`.
    fn settled(&self, now: SystemTime) -> bool {
        let Ok(now) = now.duration_since(UNIX_EPOCH) else {
            return false;
        };
        [self.mtime, self.ctime].iter().all(|&(secs, nsecs)| {
            u64::try_from(secs).is_ok_and(|secs| {
                let at = Duration::new(secs, u32::try_from(nsecs).unwrap_or(0));
                now.checked_sub(at).is_some_and(|age| age >= SETTLED)
            })
        })
    }

    fn cache_name(&self) -> String {
        let key = format!(
            "{}:{}:{}:{}.{}:{}.{}",
            self.dev, self.ino, self.size, self.mtime.0, self.mtime.1, self.ctime.0, self.ctime.1
        );
        format!("{:x}", Sha256::digest(key.as_bytes()))
    }
}

/// The SHA-256 of the file at `path`, or `None` when it cannot be read.
/// It streams the file rather than reading it whole: a debug Hermit is about
/// 450 MB.
pub fn file_digest(path: &Path) -> Option<String> {
    let mut digest = Sha256::new();
    std::io::copy(&mut fs::File::open(path).ok()?, &mut digest).ok()?;
    Some(format!("{:x}", digest.finalize()))
}

/// [`file_digest`] of `path`, hashed once per process for as long as the
/// file's identity is unchanged, and taken from [`DIGEST_CACHE_DIR_ENV`]'s
/// per-host cache when that names a directory.
pub fn binary_digest(path: &Path) -> Option<String> {
    let cache = std::env::var_os(DIGEST_CACHE_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    binary_digest_with(path, cache.as_deref())
}

type Memo = Mutex<HashMap<PathBuf, (FileIdentity, String)>>;

fn binary_digest_with(path: &Path, cache: Option<&Path>) -> Option<String> {
    static MEMO: OnceLock<Memo> = OnceLock::new();
    binary_digest_in(MEMO.get_or_init(Default::default), path, cache)
}

fn binary_digest_in(memo: &Memo, path: &Path, cache: Option<&Path>) -> Option<String> {
    let Some(before) = FileIdentity::of(path) else {
        return file_digest(path);
    };
    if let Some((identity, digest)) = memo.lock().ok()?.get(path) {
        if *identity == before {
            return Some(digest.clone());
        }
    }
    let settled = before.settled(SystemTime::now());
    let remember = |digest: &str| {
        if let Ok(mut memo) = memo.lock() {
            memo.insert(path.to_path_buf(), (before, digest.to_string()));
        }
    };
    if let (true, Some(cache)) = (settled, cache) {
        if let Ok(text) = fs::read_to_string(cache.join(before.cache_name())) {
            let digest = text.trim_end_matches('\n');
            if digest.len() == 64 && is_lower_hex(digest) {
                remember(digest);
                return Some(digest.to_string());
            }
        }
    }
    let digest = file_digest(path)?;
    if settled && FileIdentity::of(path) == Some(before) {
        remember(&digest);
        if let Some(cache) = cache {
            write_cache_entry(cache, &before.cache_name(), &digest);
        }
    }
    Some(digest)
}

/// Best effort: a cell that cannot write the cache hashes, as it would anyway.
fn write_cache_entry(cache: &Path, name: &str, digest: &str) {
    if fs::create_dir_all(cache).is_err() {
        return;
    }
    let partial = cache.join(format!(".{name}.{}", std::process::id()));
    if fs::write(&partial, format!("{digest}\n")).is_ok()
        && fs::rename(&partial, cache.join(name)).is_ok()
    {
        return;
    }
    let _ = fs::remove_file(&partial);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bundle-record-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A clock [`SETTLED`] ahead: ctime cannot be set back, so a test file is
    /// settled only against a later clock.
    fn later() -> SystemTime {
        SystemTime::now() + SETTLED + Duration::from_secs(1)
    }

    fn record_json(manifest_plan: &str) -> String {
        format!(
            r#"{{"schema":"{BUNDLE_RECORD_SCHEMA}","source_sha":"{}","hermit_sha256":"{}","planner_sha256":"{}","manifest_plan":"{manifest_plan}"}}"#,
            "a".repeat(40),
            "b".repeat(64),
            "c".repeat(64)
        )
    }

    #[test]
    fn a_record_is_parsed_only_in_its_exact_shape() {
        let record = BundleRecord::parse(record_json("accepted").as_bytes()).unwrap();
        assert_eq!(record.manifest_plan, "accepted");
        assert!(BundleRecord::parse(record_json("maybe").as_bytes()).is_err());
        let extra = record_json("accepted").replace('}', r#","more":1}"#);
        assert!(BundleRecord::parse(extra.as_bytes()).is_err());
        let short = record_json("accepted").replace(&"b".repeat(64), &"b".repeat(63));
        assert!(BundleRecord::parse(short.as_bytes()).is_err());
        let upper = record_json("accepted").replace(&"a".repeat(40), &"A".repeat(40));
        assert!(BundleRecord::parse(upper.as_bytes()).is_err());
        let old = record_json("accepted").replace("/v1", "/v0");
        assert!(BundleRecord::parse(old.as_bytes()).is_err());
    }

    #[test]
    fn a_digest_is_remembered_only_while_the_file_is_unchanged() {
        let dir = scratch("memo");
        let file = dir.join("hermit");
        fs::write(&file, b"one").unwrap();
        let memo = Memo::default();
        // A file written just now is never remembered.
        assert_eq!(
            binary_digest_in(&memo, &file, None),
            Some(format!("{:x}", Sha256::digest(b"one")))
        );
        assert!(memo.lock().unwrap().is_empty());
        // A remembered digest is returned while the identity matches...
        let identity = FileIdentity::of(&file).unwrap();
        memo.lock()
            .unwrap()
            .insert(file.clone(), (identity, "f".repeat(64)));
        assert_eq!(binary_digest_in(&memo, &file, None), Some("f".repeat(64)));
        // ...and not after a write, which hashes the new content. (A new size:
        // two writes within one clock tick may share a ctime on a kernel
        // without fine-grained timestamps. A remembered digest is only ever
        // of a settled file, whose next write is a tick later.)
        fs::write(&file, b"second").unwrap();
        assert_eq!(
            binary_digest_in(&memo, &file, None),
            Some(format!("{:x}", Sha256::digest(b"second")))
        );
        // A replacement (a new inode) is hashed too.
        memo.lock().unwrap().insert(
            file.clone(),
            (FileIdentity::of(&file).unwrap(), "f".repeat(64)),
        );
        let other = dir.join("other");
        fs::write(&other, b"three").unwrap();
        fs::rename(&other, &file).unwrap();
        assert_eq!(
            binary_digest_in(&memo, &file, None),
            Some(format!("{:x}", Sha256::digest(b"three")))
        );
        assert_eq!(binary_digest_in(&memo, &dir.join("missing"), None), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_cache_entry_is_keyed_by_identity_and_read_only_for_a_settled_file() {
        let dir = scratch("cache");
        let file = dir.join("hermit");
        fs::write(&file, b"one").unwrap();
        let identity = FileIdentity::of(&file).unwrap();
        assert!(!identity.settled(SystemTime::now()));
        assert!(identity.settled(later()));
        let cache = dir.join("cache");
        write_cache_entry(&cache, &identity.cache_name(), &"e".repeat(64));
        assert_eq!(
            fs::read_to_string(cache.join(identity.cache_name())).unwrap(),
            format!("{}\n", "e".repeat(64))
        );
        // The file is not settled, so the entry is ignored and the file hashed.
        assert_eq!(
            binary_digest_in(&Memo::default(), &file, Some(&cache)),
            Some(format!("{:x}", Sha256::digest(b"one")))
        );
        // A write changes the identity, and so the entry's name.
        fs::write(&file, b"second").unwrap();
        assert_ne!(
            FileIdentity::of(&file).unwrap().cache_name(),
            identity.cache_name()
        );
        // A cache that cannot be written is no failure.
        let blocked = dir.join("blocked");
        fs::write(&blocked, b"").unwrap();
        write_cache_entry(&blocked, "x", &"e".repeat(64));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_planner_verdict_is_taken_only_for_its_own_bundle_source_and_planner() {
        let dir = scratch("planner");
        let bundle = dir.join("bundle");
        fs::create_dir_all(bundle.join("src")).unwrap();
        fs::create_dir_all(bundle.join("bin")).unwrap();
        let planner = bundle.join("bin/hermit-manifest-plan");
        fs::write(&planner, b"planner").unwrap();
        let record_path = bundle.join("BUNDLE_RECORD.json");
        let mut record = BundleRecord::parse(record_json("accepted").as_bytes()).unwrap();
        record.planner_sha256 = format!("{:x}", Sha256::digest(b"planner"));
        fs::write(&record_path, b"{}").unwrap();
        let src = bundle.join("src");
        let sha = "a".repeat(40);
        assert_eq!(
            manifest_plan_accepted(&record_path, &record, &src, &planner, &sha),
            Ok(())
        );
        let other_sha = "d".repeat(40);
        assert!(manifest_plan_accepted(&record_path, &record, &src, &planner, &other_sha).is_err());
        let elsewhere = dir.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        assert!(manifest_plan_accepted(&record_path, &record, &elsewhere, &planner, &sha).is_err());
        let rejected = BundleRecord {
            manifest_plan: "rejected".into(),
            ..record.clone()
        };
        assert!(manifest_plan_accepted(&record_path, &rejected, &src, &planner, &sha).is_err());
        fs::write(&planner, b"another planner").unwrap();
        assert!(manifest_plan_accepted(&record_path, &record, &src, &planner, &sha).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
