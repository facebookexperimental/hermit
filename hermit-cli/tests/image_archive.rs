/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for the local `--image` inputs
//! (`--image=oci-dir:...` and `--image=docker-archive:...`): both formats
//! materialize and run a no-libc guest with identical trees,
//! malformed inputs are refused by name with exit 125 before any guest
//! starts, and the pre-existing podman-store path keeps working (driven by
//! a scripted fake `podman`), while prefixed runs never invoke podman.
//!
//! Fixtures are built in-test from the same layer tars — no network, no
//! real podman. The guest is a no-libc binary (`cc -nostdlib
//! -nodefaultlibs -static`, raw syscalls only), because the image rootfs
//! contains nothing but the fixture and the pinned validation root has
//! no static libc to link a `cc -static` guest against.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

const GUEST_MTIME: i64 = 1_650_000_000;
const APPDIR_MTIME: i64 = 1_600_000_123;

static GUEST: OnceLock<PathBuf> = OnceLock::new();

/// The no-libc guest: prints a marker, the image-declared env, and its
/// working directory so tests can prove config application. It uses raw
/// x86_64 syscalls (write, getcwd, exit) from its own `_start` and links
/// with `-nostdlib -nodefaultlibs`, so building it needs no libc at all —
/// the pinned validation root ships no static libc for `cc -static`.
/// Its stdout is byte-for-byte the old libc guest's: the marker line,
/// `TEST_ENV=`/`STORE_ENV=` (or `(unset)`), and `CWD=` when getcwd
/// succeeds.
fn guest_binary() -> &'static Path {
    GUEST.get_or_init(|| {
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("image-archive-guest")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&build_root).expect("create guest build dir");
        let source = build_root.join("guest.c");
        std::fs::write(
            &source,
            r#"
/* No-libc guest: raw x86_64 syscalls only (write=1, getcwd=79, exit=60). */
typedef unsigned long u64;
static inline long sys3(long n, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret)
                     : "a"(n), "D"(a), "S"(b), "d"(c)
                     : "rcx", "r11", "memory");
    return ret;
}
static u64 slen(const char *s) {
    u64 n = 0;
    while (s[n] != '\0') n++;
    return n;
}
static void write_str(const char *s) {
    u64 left = slen(s);
    const char *p = s;
    while (left > 0) {
        long w = sys3(1, 1, (long)p, (long)left);
        if (w <= 0) return;
        p += w;
        left -= (u64)w;
    }
}
static const char *find_env(char **envp, const char *name) {
    u64 nlen = slen(name);
    for (u64 i = 0; envp[i] != 0; i++) {
        const char *e = envp[i];
        u64 j = 0;
        while (j < nlen && e[j] == name[j]) j++;
        if (j == nlen && e[j] == '=') return e + nlen + 1;
    }
    return "(unset)";
}
static char cwd_buf[4096];
static char io_buf[4096];
/* Mount-test operations, one per guest argument after argv[0]:
 * "read:<path>" prints FILE <path>=<contents> (or =ERR<errno>),
 * "write:<path>:<text>" writes and prints WROTE <path> (or ERR<errno>).
 * With no such arguments the guest's output is exactly as before. */
static void write_errno(long r) {
    long e = -r;
    char digits[24];
    int n = 0;
    if (e == 0) digits[n++] = '0';
    while (e > 0) { digits[n++] = (char)('0' + (e % 10)); e /= 10; }
    write_str("ERR");
    while (n > 0) { char c[2] = {digits[--n], '\0'}; write_str(c); }
}
static int has_prefix(const char *s, const char *prefix) {
    while (*prefix != '\0') {
        if (*s++ != *prefix++) return 0;
    }
    return 1;
}
static void guest_ops(char **argv, long argc) {
    for (long i = 1; i < argc; i++) {
        const char *arg = argv[i];
        if (has_prefix(arg, "read:")) {
            const char *path = arg + 5;
            long fd = sys3(2, (long)path, 0, 0);
            write_str("FILE ");
            write_str(path);
            write_str("=");
            if (fd < 0) {
                write_errno(fd);
            } else {
                long n;
                while ((n = sys3(0, fd, (long)io_buf, (long)sizeof(io_buf) - 1)) > 0) {
                    io_buf[n] = '\0';
                    write_str(io_buf);
                }
                if (n < 0) write_errno(n);
                sys3(3, fd, 0, 0);
            }
            write_str("\n");
        } else if (has_prefix(arg, "write:")) {
            const char *rest = arg + 6;
            const char *colon = rest;
            while (*colon != '\0' && *colon != ':') colon++;
            if (*colon != ':') continue;
            /* The path is rest[..colon-rest]; copy it out NUL-terminated. */
            char path[512];
            u64 plen = (u64)(colon - rest);
            if (plen >= sizeof(path)) continue;
            for (u64 j = 0; j < plen; j++) path[j] = rest[j];
            path[plen] = '\0';
            long fd = sys3(2, (long)path, 0x241, 0644);
            write_str("WROTE ");
            write_str(path);
            write_str("=");
            if (fd < 0) {
                write_errno(fd);
            } else {
                const char *text = colon + 1;
                long w = sys3(1, fd, (long)text, (long)slen(text));
                if (w < 0) write_errno(w); else write_str(text);
                sys3(3, fd, 0, 0);
            }
            write_str("\n");
        }
    }
}
void guest_main(long *stack) {
    long argc = stack[0];
    char **argv = (char **)(stack + 1);
    char **envp = argv + argc + 1;
    write_str("GUEST_MARKER=hello-from-image\n");
    guest_ops(argv, argc);
    write_str("TEST_ENV=");
    write_str(find_env(envp, "TEST_ENV"));
    write_str("\n");
    write_str("STORE_ENV=");
    write_str(find_env(envp, "STORE_ENV"));
    write_str("\n");
    long r = sys3(79, (long)cwd_buf, (long)sizeof(cwd_buf), 0);
    if (r > 0) {
        cwd_buf[sizeof(cwd_buf) - 1] = '\0';
        write_str("CWD=");
        write_str(cwd_buf);
        write_str("\n");
    }
}
__asm__(
    ".globl _start\n"
    "_start:\n"
    " mov %rsp, %rdi\n"
    " and $-16, %rsp\n"
    " call guest_main\n"
    " mov $60, %rax\n"
    " xor %edi, %edi\n"
    " syscall\n");
"#,
        )
        .expect("write guest source");
        let guest = build_root.join("guest");
        let output = Command::new("cc")
            .args([
                "-nostdlib",
                "-nodefaultlibs",
                "-ffreestanding",
                "-fno-builtin",
                "-static",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-o",
            ])
            .arg(&guest)
            .arg(&source)
            .output()
            .expect("failed to run cc for the image guest");
        assert!(
            output.status.success(),
            "image guest compilation failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        guest
    })
}

// ---------------------------------------------------------------------------
// Layer fixture construction (shared verbatim by both image formats)
// ---------------------------------------------------------------------------

struct Entry {
    path: String,
    kind: tar::EntryType,
    mode: u32,
    mtime: u64,
    uid: u64,
    gid: u64,
    data: Vec<u8>,
    link: Option<String>,
}

fn entry(path: &str, kind: tar::EntryType, mode: u32, data: &[u8]) -> Entry {
    Entry {
        path: path.to_string(),
        kind,
        mode,
        mtime: 1_700_000_000,
        uid: 0,
        gid: 0,
        data: data.to_vec(),
        link: None,
    }
}

fn dir_entry(path: &str, mode: u32, mtime: u64) -> Entry {
    let mut e = entry(path, tar::EntryType::Directory, mode, b"");
    e.mtime = mtime;
    e
}

fn build_tar(entries: &[Entry]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for e in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(e.kind);
        header.set_mode(e.mode);
        header.set_mtime(e.mtime);
        header.set_uid(e.uid);
        header.set_gid(e.gid);
        header.set_size(e.data.len() as u64);
        if let Some(link) = &e.link {
            header.set_link_name(link).unwrap();
        }
        if e.kind == tar::EntryType::Char {
            header.set_device_major(5).unwrap();
            header.set_device_minor(1).unwrap();
        }
        builder
            .append_data(&mut header, &e.path, e.data.as_slice())
            .unwrap();
    }
    builder.into_inner().unwrap()
}

/// The two fixture layers (plain tar bytes). Layer 1 carries a directory
/// tree with a guest binary, a hardlink, a symlink, a FIFO, a `/dev`
/// device entry to skip, and files later layers delete; layer 2 edits one
/// file, whiteouts one file, and makes one directory opaque.
fn fixture_layers(
    extra_layer1: &[Entry],
    layer1_patch: Option<(&str, &str)>,
) -> (Vec<u8>, Vec<u8>) {
    let guest = std::fs::read(guest_binary()).unwrap();
    let mut guest_entry = entry("bin/guest", tar::EntryType::Regular, 0o755, &guest);
    guest_entry.mtime = GUEST_MTIME as u64;
    guest_entry.uid = 1234;
    guest_entry.gid = 4321;
    let mut hardlink = entry("bin/guest-hard", tar::EntryType::Link, 0o755, b"");
    hardlink.link = Some("bin/guest".to_string());
    let mut symlink = entry("link.txt", tar::EntryType::Symlink, 0o777, b"");
    symlink.link = Some("etc/marker.txt".to_string());
    let mut layer1 = vec![
        dir_entry("bin", 0o755, 1_600_000_000),
        guest_entry,
        hardlink,
        dir_entry("etc", 0o755, 1_600_000_000),
        entry(
            "etc/marker.txt",
            tar::EntryType::Regular,
            0o640,
            b"hello-layer1\n",
        ),
        entry(
            "etc/to-delete.txt",
            tar::EntryType::Regular,
            0o644,
            b"gone\n",
        ),
        dir_entry("opt", 0o755, 1_600_000_000),
        dir_entry("opt/opaque", 0o755, 1_600_000_000),
        entry(
            "opt/opaque/old.txt",
            tar::EntryType::Regular,
            0o644,
            b"old\n",
        ),
        dir_entry("opt/appdir", 0o750, APPDIR_MTIME as u64),
        entry(
            "opt/appdir/data.txt",
            tar::EntryType::Regular,
            0o600,
            b"data\n",
        ),
        symlink,
        dir_entry("dev", 0o755, 1_600_000_000),
        entry("dev/console", tar::EntryType::Char, 0o600, b""),
        dir_entry("run", 0o755, 1_600_000_000),
        entry("run/myfifo", tar::EntryType::Fifo, 0o644, b""),
    ];
    for extra in extra_layer1 {
        layer1.push(Entry {
            path: extra.path.clone(),
            kind: extra.kind,
            mode: extra.mode,
            mtime: extra.mtime,
            uid: extra.uid,
            gid: extra.gid,
            data: extra.data.clone(),
            link: extra.link.clone(),
        });
    }
    let mut layer1 = build_tar(&layer1);
    if let Some((placeholder, target)) = layer1_patch {
        patch_entry_name(&mut layer1, placeholder, target);
    }
    let layer2 = build_tar(&[
        entry(
            "etc/marker.txt",
            tar::EntryType::Regular,
            0o640,
            b"hello-layer2\n",
        ),
        entry("etc/.wh.to-delete.txt", tar::EntryType::Regular, 0o644, b""),
        entry(
            "opt/opaque/.wh..wh..opq",
            tar::EntryType::Regular,
            0o644,
            b"",
        ),
        entry(
            "opt/opaque/new.txt",
            tar::EntryType::Regular,
            0o644,
            b"new\n",
        ),
    ]);
    (layer1, layer2)
}

/// Rewrite a built tar's placeholder entry name (same length) and fix the
/// header checksum: the tar writer refuses `..`/absolute names, so hostile
/// reader inputs are patched into a valid archive.
fn patch_entry_name(tar: &mut [u8], placeholder: &str, target: &str) {
    assert_eq!(placeholder.len(), target.len());
    let at = tar
        .windows(placeholder.len())
        .position(|w| w == placeholder.as_bytes())
        .expect("placeholder name in built tar");
    tar[at..at + target.len()].copy_from_slice(target.as_bytes());
    let block = at / 512 * 512;
    for byte in &mut tar[block + 148..block + 156] {
        *byte = b' ';
    }
    let sum: u32 = tar[block..block + 512].iter().map(|b| *b as u32).sum();
    let encoded = format!("{sum:06o}\0 ");
    tar[block + 148..block + 156].copy_from_slice(encoded.as_bytes());
}

fn sha256_hex(data: &[u8]) -> String {
    detcore::Digest::new(data).to_string()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn config_bytes(diff_ids: &[String]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "created": "2024-01-01T00:00:00Z",
        "config": {
            "Env": ["TEST_ENV=from-image", "PATH=/bin:/usr/bin"],
            "WorkingDir": "/opt/appdir",
            "Cmd": ["/bin/guest"]
        },
        "rootfs": {
            "type": "layers",
            "diff_ids": diff_ids.iter().map(|d| format!("sha256:{d}")).collect::<Vec<_>>()
        }
    }))
    .unwrap()
}

/// One layer as stored: blob bytes plus its OCI media type.
struct StoredLayer {
    blob: Vec<u8>,
    media_type: &'static str,
}

const MT_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
const MT_TAR_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const MT_TAR_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

fn blob_path(root: &Path, hex: &str) -> PathBuf {
    root.join("blobs").join("sha256").join(hex)
}

fn write_blob(root: &Path, bytes: &[u8]) -> String {
    let hex = sha256_hex(bytes);
    let path = blob_path(root, &hex);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    hex
}

/// Write a complete OCI layout directory; returns the manifest digest hex.
fn write_oci_dir(root: &Path, config: &[u8], layers: &[StoredLayer]) -> String {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    let config_hex = write_blob(root, config);
    let layer_descriptors: Vec<_> = layers
        .iter()
        .map(|layer| {
            let hex = write_blob(root, &layer.blob);
            serde_json::json!({
                "mediaType": layer.media_type,
                "digest": format!("sha256:{hex}"),
                "size": layer.blob.len(),
            })
        })
        .collect();
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_hex}"),
            "size": config.len(),
        },
        "layers": layer_descriptors,
    }))
    .unwrap();
    let manifest_hex = write_blob(root, &manifest);
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_hex}"),
            "size": manifest.len(),
            "platform": {"architecture": "amd64", "os": "linux"},
        }],
    }))
    .unwrap();
    std::fs::write(root.join("index.json"), index).unwrap();
    manifest_hex
}

/// The valid OCI fixture: layer 1 plain, layer 2 gzip (gzip coverage), both
/// verified against diff_ids of the plain tars.
fn write_valid_oci_dir(root: &Path) {
    let (layer1, layer2) = fixture_layers(&[], None);
    let config = config_bytes(&[sha256_hex(&layer1), sha256_hex(&layer2)]);
    write_oci_dir(
        root,
        &config,
        &[
            StoredLayer {
                blob: layer1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: gzip(&layer2),
                media_type: MT_TAR_GZIP,
            },
        ],
    );
}

/// Write a docker-archive outer tar from raw entries.
fn write_outer_tar(path: &Path, entries: &[Entry]) {
    std::fs::write(path, build_tar(entries)).unwrap();
}

fn outer_file(path: &str, data: &[u8]) -> Entry {
    entry(path, tar::EntryType::Regular, 0o644, data)
}

/// The valid docker-archive fixture with gzip-compressed layer tars
/// (`diff_id`s still name the plain tars).
fn write_valid_docker_archive_gzip(path: &Path) {
    let (layer1, layer2) = fixture_layers(&[], None);
    let config = config_bytes(&[sha256_hex(&layer1), sha256_hex(&layer2)]);
    let manifest = serde_json::to_vec(&serde_json::json!([{
        "Config": "config.json",
        "RepoTags": ["hermit-test:latest"],
        "Layers": ["layer1/layer.tar", "layer2/layer.tar"],
    }]))
    .unwrap();
    write_outer_tar(
        path,
        &[
            outer_file("config.json", &config),
            outer_file("layer1/layer.tar", &gzip(&layer1)),
            outer_file("layer2/layer.tar", &gzip(&layer2)),
            outer_file("manifest.json", &manifest),
        ],
    );
}

/// The valid docker-archive fixture over the same layers (plain layer tars,
/// the canonical `docker save` shape).
fn write_valid_docker_archive(path: &Path) {
    let (layer1, layer2) = fixture_layers(&[], None);
    let config = config_bytes(&[sha256_hex(&layer1), sha256_hex(&layer2)]);
    let manifest = serde_json::to_vec(&serde_json::json!([{
        "Config": "config.json",
        "RepoTags": ["hermit-test:latest"],
        "Layers": ["layer1/layer.tar", "layer2/layer.tar"],
    }]))
    .unwrap();
    write_outer_tar(
        path,
        &[
            outer_file("config.json", &config),
            outer_file("layer1/layer.tar", &layer1),
            outer_file("layer2/layer.tar", &layer2),
            outer_file("manifest.json", &manifest),
        ],
    );
}

// ---------------------------------------------------------------------------
// Running Hermit and inspecting the materialized cache
// ---------------------------------------------------------------------------

fn run_hermit(cache: &Path, args: &[String]) -> Output {
    let mut command = Command::new(hermit_test::hermit_binary());
    command
        .args(args)
        .env("XDG_CACHE_HOME", cache)
        .stdin(std::process::Stdio::null());
    hermit_test::hermit_output("hermit image run", &mut command)
}

fn run_image(cache: &Path, image: &str) -> Output {
    run_hermit(
        cache,
        &[
            "run".to_string(),
            format!("--image={image}"),
            "--strict".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ],
    )
}

/// Run the guest with `hermit`'s working directory set, so `./`- and
/// `../`-prefixed local image paths resolve as a user's shell would.
fn run_image_in_dir(cache: &Path, cwd: &Path, image: &str) -> Output {
    let mut command = Command::new(hermit_test::hermit_binary());
    command
        .args([
            "run".to_string(),
            format!("--image={image}"),
            "--strict".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ])
        .current_dir(cwd)
        .env("XDG_CACHE_HOME", cache)
        .stdin(std::process::Stdio::null());
    hermit_test::hermit_output("hermit image run (relative)", &mut command)
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The single materialized rootfs under a per-test XDG cache.
fn find_rootfs(cache: &Path) -> PathBuf {
    let base = cache.join("hermit").join("oci-rootfs");
    let mut rootfs_dirs: Vec<PathBuf> = std::fs::read_dir(&base)
        .unwrap_or_else(|e| panic!("cache base {} unreadable: {e}", base.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path().join("rootfs")))
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(
        rootfs_dirs.len(),
        1,
        "expected exactly one materialized rootfs under {}, found {rootfs_dirs:?}",
        base.display()
    );
    rootfs_dirs.pop().unwrap()
}

fn assert_guest_ran(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "guest run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_of(output),
        stderr_of(output)
    );
    let stdout = stdout_of(output);
    assert!(stdout.contains("GUEST_MARKER=hello-from-image"), "{stdout}");
    assert!(stdout.contains("TEST_ENV=from-image"), "{stdout}");
    assert!(stdout.contains("CWD=/opt/appdir"), "{stdout}");
}

/// The golden-tree expectations both formats must materialize.
fn assert_golden_tree(rootfs: &Path) {
    let read = |rel: &str| std::fs::read(rootfs.join(rel)).unwrap();
    assert_eq!(read("etc/marker.txt"), b"hello-layer2\n");
    assert_eq!(
        std::fs::metadata(rootfs.join("etc/marker.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o640
    );
    assert!(
        !rootfs.join("etc/to-delete.txt").exists(),
        "whiteout must delete"
    );
    assert!(
        !rootfs.join("opt/opaque/old.txt").exists(),
        "opaque must clear"
    );
    assert_eq!(read("opt/opaque/new.txt"), b"new\n");

    let guest_meta = std::fs::metadata(rootfs.join("bin/guest")).unwrap();
    assert_eq!(read("bin/guest"), std::fs::read(guest_binary()).unwrap());
    assert_eq!(guest_meta.permissions().mode() & 0o7777, 0o755);
    assert_eq!(guest_meta.mtime(), GUEST_MTIME);
    // SAFETY: getuid always succeeds.
    let uid = unsafe { libc::getuid() };
    assert_eq!(
        guest_meta.uid(),
        uid,
        "layer uid/gid flatten to the invoker"
    );

    let hard_meta = std::fs::metadata(rootfs.join("bin/guest-hard")).unwrap();
    assert_eq!(
        hard_meta.ino(),
        guest_meta.ino(),
        "hardlink shares the inode"
    );
    assert_eq!(
        std::fs::read_link(rootfs.join("link.txt")).unwrap(),
        PathBuf::from("etc/marker.txt")
    );

    let appdir_meta = std::fs::metadata(rootfs.join("opt/appdir")).unwrap();
    assert_eq!(appdir_meta.permissions().mode() & 0o7777, 0o750);
    assert_eq!(appdir_meta.mtime(), APPDIR_MTIME, "deferred dir mtime");
    assert_eq!(read("opt/appdir/data.txt"), b"data\n");

    let fifo_meta = std::fs::symlink_metadata(rootfs.join("run/myfifo")).unwrap();
    assert_eq!(
        fifo_meta.mode() & libc::S_IFMT,
        libc::S_IFIFO,
        "FIFO preserved"
    );

    // The device entry under dev/ is skipped; finish provides placeholders
    // for its own bind targets only, so dev/console is deterministically
    // ABSENT (Hermit never binds the host console into an image guest).
    match std::fs::symlink_metadata(rootfs.join("dev/console")) {
        Ok(meta) => panic!(
            "dev/console must be absent (skipped device), found mode {:o}",
            meta.mode()
        ),
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
    }

    // finish_materialized_rootfs products, shared with the store path.
    assert_eq!(read("etc/hostname"), b"hermetic-container.local\n");
    assert!(rootfs.join("proc").is_dir() && rootfs.join("sys").is_dir());
    assert!(rootfs.join("dev/null").is_file() && !rootfs.join("dev/null").is_symlink());
    let env = String::from_utf8(read(".hermit-oci-env")).unwrap();
    assert!(env.contains("TEST_ENV=from-image"), "{env}");
    assert_eq!(read(".hermit-oci-workdir"), b"/opt/appdir\n");
    assert_eq!(read(".hermit-oci-cmd"), b"/bin/guest\n");

    // No whiteout markers survive anywhere in the tree.
    fn walk(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            assert!(
                !entry.file_name().to_string_lossy().starts_with(".wh."),
                "whiteout marker survived at {}",
                entry.path().display()
            );
            if entry.path().is_dir() && !entry.path().is_symlink() {
                walk(&entry.path());
            }
        }
    }
    walk(rootfs);
}

/// A comparable snapshot of a rootfs tree for OCI/docker parity.
fn tree_snapshot(rootfs: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let mode = meta.mode();
            let desc = if meta.file_type().is_symlink() {
                format!("symlink->{}", std::fs::read_link(&path).unwrap().display())
            } else if meta.is_dir() {
                format!("dir mode={:o}", mode & 0o7777)
            } else if mode & libc::S_IFMT == libc::S_IFIFO {
                format!("fifo mode={:o}", mode & 0o7777)
            } else {
                // No mtime here: finish-created files carry the host
                // wall-time of their run. Layer mtimes are asserted
                // exactly in assert_golden_tree for both formats.
                format!(
                    "file mode={:o} sha256={}",
                    mode & 0o7777,
                    sha256_hex(&std::fs::read(&path).unwrap())
                )
            };
            out.insert(rel, desc);
            if meta.is_dir() && !meta.file_type().is_symlink() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(rootfs, rootfs, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn oci_dir_materializes_runs_and_reuses_cache() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("cache");
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    let image = format!("oci-dir:{}", image_dir.display());

    assert_guest_ran(&run_image(&cache, &image));
    let rootfs = find_rootfs(&cache);
    assert_golden_tree(&rootfs);

    // A second run reuses the cached tree: a file the layers would
    // rewrite on re-materialization keeps the observer's edit instead.
    std::fs::write(rootfs.join("etc/marker.txt"), b"observer-edit\n").unwrap();
    assert_guest_ran(&run_image(&cache, &image));
    assert_eq!(
        std::fs::read(rootfs.join("etc/marker.txt")).unwrap(),
        b"observer-edit\n",
        "second run must reuse the cached tree, not re-apply layers"
    );
    assert_eq!(find_rootfs(&cache), rootfs, "same cache dir reused");
}

/// `--mount` and `--bind` compose with `--image`: a bind mount, a tmpfs,
/// and a `--bind` work directory land inside the rootfs at their guest
/// targets, a `readonly` bind refuses guest writes, and mounts Hermit
/// cannot install in a rootfs are refused by name before the guest runs.
#[test]
fn oci_dir_composes_with_mount_and_bind() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("cache");
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    let image = format!("oci-dir:{}", image_dir.display());

    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("input.txt"), b"from-bind-mount\n").unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("work.txt"), b"from-bind-flag\n").unwrap();

    // /data is not in the fixture image (created as a mountpoint),
    // /opt/appdir is (an existing directory target), /scratch is a
    // fresh tmpfs, and /work comes through --bind.
    let output = run_hermit(
        &cache,
        &[
            "run".to_string(),
            format!("--image={image}"),
            "--strict".to_string(),
            format!("--mount=type=bind,source={},target=/data,readonly", data.display()),
            "--mount=type=tmpfs,target=/scratch".to_string(),
            format!("--bind={}:/work", work.display()),
            "--".to_string(),
            "/bin/guest".to_string(),
            "read:/data/input.txt".to_string(),
            "write:/data/input.txt:nope".to_string(),
            "write:/scratch/scratch.txt:from-tmpfs".to_string(),
            "read:/scratch/scratch.txt".to_string(),
            "read:/work/work.txt".to_string(),
            "write:/work/out.txt:guest-output".to_string(),
            "read:/proc/self/mountinfo".to_string(),
        ],
    );
    assert_guest_ran(&output);
    let stdout = stdout_of(&output);
    assert!(stdout.contains("FILE /data/input.txt=from-bind-mount"), "{stdout}");
    assert!(
        stdout.contains("WROTE /data/input.txt=ERR30"),
        "a readonly bind refuses the guest write with EROFS: {stdout}"
    );
    assert!(stdout.contains("WROTE /scratch/scratch.txt=from-tmpfs"), "{stdout}");
    assert!(stdout.contains("FILE /scratch/scratch.txt=from-tmpfs"), "{stdout}");
    assert!(stdout.contains("FILE /work/work.txt=from-bind-flag"), "{stdout}");
    assert_eq!(
        std::fs::read(work.join("out.txt")).unwrap(),
        b"guest-output",
        "a writable --bind into the rootfs writes through to the host"
    );
    assert_eq!(
        std::fs::read(data.join("input.txt")).unwrap(),
        b"from-bind-mount\n",
        "the readonly bind's host source is unchanged"
    );
    // The guest's mount table shows each bind at its guest target as
    // its root, never the host source subpath (measured before the
    // rewrite: the /data row's root was the host tempdir subpath, and
    // the tmpfs row carried the host uid/gid).
    let mountinfo_row = |mountpoint: &str| {
        stdout
            .lines()
            .find(|line| line.split(' ').nth(4) == Some(mountpoint))
            .unwrap_or_else(|| panic!("no mountinfo row at {mountpoint}:\n{stdout}"))
            .to_string()
    };
    let data_row = mountinfo_row("/data");
    assert_eq!(
        data_row.split(' ').nth(3),
        Some("/data"),
        "the bind row's root is its guest target: {data_row}"
    );
    assert!(
        !stdout.contains(&data.display().to_string()),
        "the host bind source path leaked into the guest mount table:\n{stdout}"
    );
    let scratch_row = mountinfo_row("/scratch");
    assert!(
        !scratch_row.contains("uid=") && !scratch_row.contains("gid="),
        "the tmpfs row carries host ids in its superblock options: {scratch_row}"
    );

    // Determinism: the read-only composition under --verify --verify-strict
    // runs the guest twice and compares the runs; a writable bind would
    // let run one change run two's inputs, so this uses the readonly bind
    // and the (always empty at start) tmpfs only.
    let verified = run_hermit(
        &cache,
        &[
            "run".to_string(),
            format!("--image={image}"),
            "--strict".to_string(),
            "--verify".to_string(),
            "--verify-strict".to_string(),
            format!("--mount=type=bind,source={},target=/data,readonly", data.display()),
            "--mount=type=tmpfs,target=/scratch".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
            "read:/data/input.txt".to_string(),
            "write:/scratch/scratch.txt:from-tmpfs".to_string(),
            "read:/scratch/scratch.txt".to_string(),
        ],
    );
    assert_eq!(
        verified.status.code(),
        Some(0),
        "verified image+mount run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&verified),
        stderr_of(&verified)
    );
    assert!(
        stdout_of(&verified).contains("FILE /data/input.txt=from-bind-mount"),
        "{}",
        stdout_of(&verified)
    );

    // Targets under /tmp, both spellings: prepared in the private /tmp
    // staging the guest's /tmp is bound from, not in the cached rootfs
    // (where the staging bind would hide the mountpoint).
    let under_tmp = run_hermit(
        &cache,
        &[
            "run".to_string(),
            format!("--image={image}"),
            "--strict".to_string(),
            format!("--mount=type=bind,source={},target=/tmp/data,readonly", data.display()),
            format!("--bind={}:/tmp/work", work.display()),
            "--".to_string(),
            "/bin/guest".to_string(),
            "read:/tmp/data/input.txt".to_string(),
            "read:/tmp/work/work.txt".to_string(),
            "write:/tmp/work/tmp-out.txt:from-tmp-bind".to_string(),
        ],
    );
    assert_guest_ran(&under_tmp);
    let stdout = stdout_of(&under_tmp);
    assert!(stdout.contains("FILE /tmp/data/input.txt=from-bind-mount"), "{stdout}");
    assert!(stdout.contains("FILE /tmp/work/work.txt=from-bind-flag"), "{stdout}");
    assert_eq!(
        std::fs::read(work.join("tmp-out.txt")).unwrap(),
        b"from-tmp-bind",
        "a --bind below /tmp in an image writes through to the host"
    );

    // Refusals, by name, before any guest runs.
    for (mount, named) in [
        ("--mount=type=volume,target=/data", "type=volume is not supported"),
        ("--mount=type=tmpfs,target=/proc", "inside /proc"),
        ("--mount=type=tmpfs,target=relative", "must be an absolute path"),
        ("--mount=target=/data", "bind mount with no source"),
        ("--bind=/srv:/proc", "inside /proc"),
        ("--bind=/srv:/", "would replace the whole OCI rootfs"),
    ] {
        let output = run_hermit(
            &cache,
            &[
                "run".to_string(),
                format!("--image={image}"),
                mount.to_string(),
                "--".to_string(),
                "/bin/guest".to_string(),
            ],
        );
        assert_ne!(output.status.code(), Some(0), "{mount} must refuse");
        let stderr = stderr_of(&output);
        assert!(stderr.contains(named), "{mount}: {stderr}");
        assert!(
            !stdout_of(&output).contains("GUEST_MARKER"),
            "{mount}: the guest must not run"
        );
    }
}

#[test]
fn docker_archive_materializes_runs_with_oci_tree_parity() {
    let tmp = tempfile::TempDir::new().unwrap();
    // OCI side.
    let oci_cache = tmp.path().join("cache-oci");
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    assert_guest_ran(&run_image(
        &oci_cache,
        &format!("oci-dir:{}", image_dir.display()),
    ));
    let oci_rootfs = find_rootfs(&oci_cache);
    assert_golden_tree(&oci_rootfs);

    // Docker-archive side over the same layers.
    let docker_cache = tmp.path().join("cache-docker");
    let archive = tmp.path().join("image.tar");
    write_valid_docker_archive(&archive);
    assert_guest_ran(&run_image(
        &docker_cache,
        &format!("docker-archive:{}", archive.display()),
    ));
    let docker_rootfs = find_rootfs(&docker_cache);
    assert_golden_tree(&docker_rootfs);

    assert_eq!(
        tree_snapshot(&oci_rootfs),
        tree_snapshot(&docker_rootfs),
        "both formats must materialize identical trees"
    );
}

#[test]
fn relative_prefixed_paths_run_oci_and_gzip_docker_archive() {
    let tmp = tempfile::TempDir::new().unwrap();
    // `./` against the OCI layout (whose layer 2 is already gzip).
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    let output = run_image_in_dir(
        &tmp.path().join("cache-rel-oci"),
        tmp.path(),
        "oci-dir:./oci-image",
    );
    assert_guest_ran(&output);
    assert_golden_tree(&find_rootfs(&tmp.path().join("cache-rel-oci")));
    // `../` against a docker-archive whose layer tars are gzip.
    let archive = tmp.path().join("image-gzip.tar");
    write_valid_docker_archive_gzip(&archive);
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let output = run_image_in_dir(
        &tmp.path().join("cache-rel-docker"),
        &work,
        "docker-archive:../image-gzip.tar",
    );
    assert_guest_ran(&output);
    assert_golden_tree(&find_rootfs(&tmp.path().join("cache-rel-docker")));
}

/// H1 end to end: a root-level `.wh...` whiteout targets `..`; the run is
/// refused by name and nothing outside the (never-published) rootfs is
/// removed.
#[test]
fn whiteout_targeting_parent_is_refused_end_to_end() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sentinel = tmp.path().join("host-sentinel.txt");
    std::fs::write(&sentinel, b"host").unwrap();
    let (layer1, layer2) = fixture_layers(
        &[entry(".wh...", tar::EntryType::Regular, 0o644, b"")],
        None,
    );
    let dir = tmp.path().join("whiteout-escape");
    let config = config_bytes(&[sha256_hex(&layer1), sha256_hex(&layer2)]);
    write_oci_dir(
        &dir,
        &config,
        &[
            StoredLayer {
                blob: layer1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: layer2,
                media_type: MT_TAR,
            },
        ],
    );
    let output = run_image(
        &tmp.path().join("cache-whiteout"),
        &format!("oci-dir:{}", dir.display()),
    );
    assert_refused_by_name(&output, "whiteout escape", &["refusing whiteout"]);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"host");
}

/// H3 end to end: a top layer that replaces `etc` with a symlink to a
/// host directory is refused during finishing, and no hostname lands in
/// the host directory.
#[test]
fn symlinked_etc_is_refused_end_to_end_without_touching_host() {
    let tmp = tempfile::TempDir::new().unwrap();
    let outside = tmp.path().join("outside-etc");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), b"host").unwrap();
    let (layer1, _) = fixture_layers(&[], None);
    let mut etc_link = entry("etc", tar::EntryType::Symlink, 0o777, b"");
    etc_link.link = Some(outside.to_string_lossy().into_owned());
    let layer2 = build_tar(&[etc_link]);
    let dir = tmp.path().join("symlink-etc");
    let config = config_bytes(&[sha256_hex(&layer1), sha256_hex(&layer2)]);
    write_oci_dir(
        &dir,
        &config,
        &[
            StoredLayer {
                blob: layer1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: layer2,
                media_type: MT_TAR,
            },
        ],
    );
    let output = run_image(
        &tmp.path().join("cache-symlink-etc"),
        &format!("oci-dir:{}", dir.display()),
    );
    assert_refused_by_name(&output, "symlinked etc", &["symlink"]);
    assert!(!outside.join("hostname").exists());
    assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"host");
}

/// M2: a partial tree left at the final cache location (marker gone, a
/// layer directory locked read-only) does not block the next run: the
/// retry publishes a fresh tree atomically.
#[test]
fn local_materialization_recovers_from_partial_tree() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = tmp.path().join("cache");
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    let image = format!("oci-dir:{}", image_dir.display());
    assert_guest_ran(&run_image(&cache, &image));
    let rootfs = find_rootfs(&cache);
    let cache_entry = rootfs.parent().unwrap().to_path_buf();
    // Simulate an interrupted/legacy partial tree at the final location.
    std::fs::remove_file(cache_entry.join(".hermit-oci-ready")).unwrap();
    std::fs::remove_file(rootfs.join("etc/marker.txt")).unwrap();
    std::fs::set_permissions(rootfs.join("etc"), std::fs::Permissions::from_mode(0o555)).unwrap();
    assert_guest_ran(&run_image(&cache, &image));
    let rootfs = find_rootfs(&cache);
    assert_golden_tree(&rootfs);
}

fn assert_refused_by_name(output: &Output, case: &str, needles: &[&str]) {
    assert_eq!(
        output.status.code(),
        Some(hermit::HERMIT_INTERNAL_FAILURE_EXIT),
        "{case}: expected exit 125, got {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout_of(output),
        stderr_of(output)
    );
    let stderr = stderr_of(output);
    for needle in needles {
        assert!(
            stderr.contains(needle),
            "{case}: stderr must name {needle:?}\nstderr:\n{stderr}"
        );
    }
    assert!(
        !stdout_of(output).contains("GUEST_MARKER"),
        "{case}: the guest must not have run\nstdout:\n{}",
        stdout_of(output)
    );
}

#[test]
fn malformed_local_images_are_refused_by_name() {
    struct Case {
        name: &'static str,
        image: String,
        needles: &'static [&'static str],
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let mut cases: Vec<Case> = Vec::new();
    let mut add = |name: &'static str, image: String, needles: &'static [&'static str]| {
        cases.push(Case {
            name,
            image,
            needles,
        });
    };

    // Missing oci-dir path.
    add(
        "missing oci-dir",
        format!("oci-dir:{}/nope", tmp.path().display()),
        &["does not exist", "oci-dir"],
    );
    // Missing docker-archive path.
    add(
        "missing docker-archive",
        format!("docker-archive:{}/nope.tar", tmp.path().display()),
        &["does not exist", "podman save"],
    );
    // No index.json.
    let no_index = tmp.path().join("no-index");
    write_valid_oci_dir(&no_index);
    std::fs::remove_file(no_index.join("index.json")).unwrap();
    add(
        "no index.json",
        format!("oci-dir:{}", no_index.display()),
        &["no index.json"],
    );
    // Zero linux/amd64 manifests.
    let zero_amd64 = tmp.path().join("zero-amd64");
    write_valid_oci_dir(&zero_amd64);
    let index = std::fs::read_to_string(zero_amd64.join("index.json")).unwrap();
    std::fs::write(
        zero_amd64.join("index.json"),
        index.replace("\"amd64\"", "\"arm64\""),
    )
    .unwrap();
    add(
        "zero amd64",
        format!("oci-dir:{}", zero_amd64.display()),
        &["no linux/amd64 manifest"],
    );
    // Two linux/amd64 manifests.
    let two_amd64 = tmp.path().join("two-amd64");
    write_valid_oci_dir(&two_amd64);
    let mut index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(two_amd64.join("index.json")).unwrap()).unwrap();
    let descriptor = index["manifests"][0].clone();
    index["manifests"].as_array_mut().unwrap().push(descriptor);
    std::fs::write(
        two_amd64.join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    add(
        "two amd64",
        format!("oci-dir:{}", two_amd64.display()),
        &["2 linux/amd64 manifests", "ambiguous"],
    );
    // Missing layer blob.
    let missing_blob = tmp.path().join("missing-blob");
    let (layer1, _) = fixture_layers(&[], None);
    write_valid_oci_dir(&missing_blob);
    std::fs::remove_file(blob_path(&missing_blob, &sha256_hex(&layer1))).unwrap();
    add(
        "missing blob",
        format!("oci-dir:{}", missing_blob.display()),
        &["missing blob"],
    );
    // Corrupted manifest blob (digest mismatch).
    let bad_manifest = tmp.path().join("bad-manifest");
    let (l1, l2) = fixture_layers(&[], None);
    let config = config_bytes(&[sha256_hex(&l1), sha256_hex(&l2)]);
    let manifest_hex = write_oci_dir(
        &bad_manifest,
        &config,
        &[
            StoredLayer {
                blob: l1.clone(),
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: l2.clone(),
                media_type: MT_TAR,
            },
        ],
    );
    std::fs::write(blob_path(&bad_manifest, &manifest_hex), b"{}").unwrap();
    add(
        "manifest digest mismatch",
        format!("oci-dir:{}", bad_manifest.display()),
        &["manifest digest mismatch"],
    );
    // diff_id mismatch.
    let bad_diff = tmp.path().join("bad-diff");
    let config = config_bytes(&[sha256_hex(b"bogus"), sha256_hex(&l2)]);
    write_oci_dir(
        &bad_diff,
        &config,
        &[
            StoredLayer {
                blob: l1.clone(),
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: l2.clone(),
                media_type: MT_TAR,
            },
        ],
    );
    add(
        "diff_id mismatch",
        format!("oci-dir:{}", bad_diff.display()),
        &["diff_id mismatch"],
    );
    // zstd layer.
    let zstd_dir = tmp.path().join("zstd");
    let config = config_bytes(&[sha256_hex(&l1), sha256_hex(&l2)]);
    write_oci_dir(
        &zstd_dir,
        &config,
        &[
            StoredLayer {
                blob: l1.clone(),
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: vec![0x28, 0xb5, 0x2f, 0xfd, 0, 0],
                media_type: MT_TAR_ZSTD,
            },
        ],
    );
    add(
        "zstd layer",
        format!("oci-dir:{}", zstd_dir.display()),
        &["zstd", "not supported"],
    );
    // Device entry outside dev/.
    let (dl1, dl2) = fixture_layers(&[entry("usr/tty9", tar::EntryType::Char, 0o600, b"")], None);
    let device_dir = tmp.path().join("device");
    let config = config_bytes(&[sha256_hex(&dl1), sha256_hex(&dl2)]);
    write_oci_dir(
        &device_dir,
        &config,
        &[
            StoredLayer {
                blob: dl1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: dl2,
                media_type: MT_TAR,
            },
        ],
    );
    add(
        "device outside dev",
        format!("oci-dir:{}", device_dir.display()),
        &["device entry", "outside /dev"],
    );
    // ../ traversal and absolute path (patched into a valid tar).
    const TRAVERSAL_NEEDLES: &[&str] = &["path traversal"];
    const ABSOLUTE_NEEDLES: &[&str] = &["absolute path"];
    for (name, placeholder, target, needles) in [
        ("traversal", "zz/evil", "../evil", TRAVERSAL_NEEDLES),
        ("absolute", "zabs/evil", "/abs/evil", ABSOLUTE_NEEDLES),
    ] {
        let (pl1, pl2) = fixture_layers(
            &[entry(placeholder, tar::EntryType::Regular, 0o644, b"x")],
            Some((placeholder, target)),
        );
        let dir = tmp.path().join(name);
        let config = config_bytes(&[sha256_hex(&pl1), sha256_hex(&pl2)]);
        write_oci_dir(
            &dir,
            &config,
            &[
                StoredLayer {
                    blob: pl1,
                    media_type: MT_TAR,
                },
                StoredLayer {
                    blob: pl2,
                    media_type: MT_TAR,
                },
            ],
        );
        // Leak the needles slice of string literals is fine: they are 'static.
        add(
            match name {
                "traversal" => "path traversal",
                _ => "absolute path",
            },
            format!("oci-dir:{}", dir.display()),
            needles,
        );
    }
    // Escaping hardlink.
    let mut escape = entry("escape", tar::EntryType::Link, 0o644, b"");
    escape.link = Some("/etc/passwd".to_string());
    let (hl1, hl2) = fixture_layers(&[escape], None);
    let hardlink_dir = tmp.path().join("hardlink");
    let config = config_bytes(&[sha256_hex(&hl1), sha256_hex(&hl2)]);
    write_oci_dir(
        &hardlink_dir,
        &config,
        &[
            StoredLayer {
                blob: hl1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: hl2,
                media_type: MT_TAR,
            },
        ],
    );
    add(
        "escaping hardlink",
        format!("oci-dir:{}", hardlink_dir.display()),
        &["escaping hardlink"],
    );
    // Corrupt gzip layer.
    let corrupt_dir = tmp.path().join("corrupt-gzip");
    let (cl1, cl2) = fixture_layers(&[], None);
    let mut corrupt = gzip(&cl2);
    let mid = corrupt.len() / 2;
    corrupt[mid] ^= 0xff;
    corrupt[mid + 1] ^= 0xff;
    corrupt.truncate(corrupt.len() - 4);
    let config = config_bytes(&[sha256_hex(&cl1), sha256_hex(&cl2)]);
    write_oci_dir(
        &corrupt_dir,
        &config,
        &[
            StoredLayer {
                blob: cl1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: corrupt,
                media_type: MT_TAR_GZIP,
            },
        ],
    );
    add(
        "corrupt gzip",
        format!("oci-dir:{}", corrupt_dir.display()),
        &["corrupt gzip"],
    );
    // docker-archive without manifest.json.
    let no_manifest = tmp.path().join("no-manifest.tar");
    let (nl1, _) = fixture_layers(&[], None);
    write_outer_tar(&no_manifest, &[outer_file("layer1/layer.tar", &nl1)]);
    add(
        "no manifest.json",
        format!("docker-archive:{}", no_manifest.display()),
        &["no manifest.json"],
    );
    // docker-archive with two images.
    let two_images = tmp.path().join("two-images.tar");
    let config = config_bytes(&[sha256_hex(&l1), sha256_hex(&l2)]);
    let one = serde_json::json!({
        "Config": "config.json",
        "RepoTags": ["a:latest"],
        "Layers": ["layer1/layer.tar", "layer2/layer.tar"],
    });
    let manifest = serde_json::to_vec(&serde_json::json!([one.clone(), one])).unwrap();
    write_outer_tar(
        &two_images,
        &[
            outer_file("config.json", &config),
            outer_file("layer1/layer.tar", &l1),
            outer_file("layer2/layer.tar", &l2),
            outer_file("manifest.json", &manifest),
        ],
    );
    add(
        "two images",
        format!("docker-archive:{}", two_images.display()),
        &["contains 2 images"],
    );
    // Truncated docker-archive.
    let truncated = tmp.path().join("truncated.tar");
    write_valid_docker_archive(&truncated);
    let bytes = std::fs::read(&truncated).unwrap();
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
    add(
        "truncated archive",
        format!("docker-archive:{}", truncated.display()),
        &["truncated or corrupt"],
    );
    // A plain rootfs tar is not a docker-archive.
    let plain = tmp.path().join("plain-rootfs.tar");
    let (rl1, _) = fixture_layers(&[], None);
    std::fs::write(&plain, &rl1).unwrap();
    add(
        "plain rootfs tar",
        format!("docker-archive:{}", plain.display()),
        &["no manifest.json", "plain rootfs tar"],
    );
    // Bare-relative prefixed path.
    add(
        "bare relative",
        "oci-dir:some-image".to_string(),
        &["bare relative path", "./some-image"],
    );

    for (i, case) in cases.iter().enumerate() {
        let cache = tmp.path().join(format!("cache-{i}"));
        let output = run_image(&cache, &case.image);
        assert_refused_by_name(&output, case.name, case.needles);
    }
}

/// A scripted `podman` that records argv and serves one store image, so the
/// store path is regression-tested without a real podman or network.
struct FakePodman {
    bin_dir: PathBuf,
    log: PathBuf,
    graph_root: PathBuf,
    layer: PathBuf,
}

fn write_fake_podman(tmp: &Path) -> FakePodman {
    let bin_dir = tmp.join("fakebin");
    let layer = tmp.join("store-layer");
    std::fs::create_dir_all(bin_dir.clone()).unwrap();
    std::fs::create_dir_all(layer.join("bin")).unwrap();
    let guest = layer.join("bin/guest");
    std::fs::copy(guest_binary(), &guest).unwrap();
    std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = bin_dir.join("podman");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_PODMAN_LOG"
case "$1" in
  info)
    printf '%s\n' "$FAKE_PODMAN_GRAPHROOT"
    ;;
  pull)
    printf '%s\n' "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ;;
  unshare)
    shift
    exec "$@"
    ;;
  image)
    case "$*" in
      *Digest*)
        printf 'sha256:%s\n' "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        ;;
      *".Config"*)
        printf '%s' "$FAKE_PODMAN_CONFIG"
        ;;
      *GraphDriver*)
        printf '{"Name":"overlay","Data":{"UpperDir":"%s"}}' "$FAKE_PODMAN_LAYER"
        ;;
      *)
        echo "unexpected podman args: $*" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    echo "unexpected podman args: $*" >&2
    exit 1
    ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    FakePodman {
        bin_dir,
        log: tmp.join("podman.log"),
        graph_root: tmp.join("graphroot"),
        layer,
    }
}

impl FakePodman {
    fn command(&self, cache: &Path, args: &[String]) -> Output {
        let fake_path = format!(
            "{}:{}",
            self.bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(hermit_test::hermit_binary());
        command
            .args(args)
            .env("XDG_CACHE_HOME", cache)
            .env("PATH", fake_path)
            .env("FAKE_PODMAN_LOG", &self.log)
            .env("FAKE_PODMAN_GRAPHROOT", &self.graph_root)
            .env("FAKE_PODMAN_LAYER", &self.layer)
            .env(
                "FAKE_PODMAN_CONFIG",
                r#"{"Env":["STORE_ENV=from-store","PATH=/bin"],"WorkingDir":"","Cmd":["/bin/guest"],"Entrypoint":null}"#,
            )
            .stdin(std::process::Stdio::null());
        hermit_test::hermit_output("hermit fake-podman run", &mut command)
    }

    fn recorded(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[test]
fn store_reference_still_materializes_via_podman_store() {
    let tmp = tempfile::TempDir::new().unwrap();
    let fake = write_fake_podman(tmp.path());
    let output = fake.command(
        &tmp.path().join("cache"),
        &[
            "run".to_string(),
            "--image=store-test:latest".to_string(),
            "--strict".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "store run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("GUEST_MARKER=hello-from-image"), "{stdout}");
    assert!(stdout.contains("STORE_ENV=from-store"), "{stdout}");
    let recorded = fake.recorded();
    for needle in [
        "info --format",
        "pull --policy=never store-test:latest",
        "image inspect",
        "unshare cp -a",
    ] {
        assert!(
            recorded.contains(needle),
            "fake podman must record {needle:?}\nrecorded:\n{recorded}"
        );
    }
}

#[test]
fn prefixed_runs_never_invoke_podman() {
    let tmp = tempfile::TempDir::new().unwrap();
    let fake = write_fake_podman(tmp.path());
    let image_dir = tmp.path().join("oci-image");
    write_valid_oci_dir(&image_dir);
    let output = fake.command(
        &tmp.path().join("cache"),
        &[
            "run".to_string(),
            format!("--image=oci-dir:{}", image_dir.display()),
            "--strict".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ],
    );
    assert_guest_ran(&output);
    assert!(
        !fake.log.exists(),
        "prefixed runs must never invoke podman; recorded:\n{}",
        fake.recorded()
    );
}

#[test]
fn oci_run_refuses_prefixed_images_with_hermit_run_remedy() {
    let tmp = tempfile::TempDir::new().unwrap();
    let fake = write_fake_podman(tmp.path());
    let output = fake.command(
        &tmp.path().join("cache"),
        &[
            "oci".to_string(),
            "run".to_string(),
            "oci-dir:/definitely/not/used".to_string(),
            "--strict".to_string(),
            "--".to_string(),
            "/bin/guest".to_string(),
        ],
    );
    assert_refused_by_name(
        &output,
        "oci run prefixed",
        &[
            "hermit oci run",
            "hermit run --image=oci-dir:/definitely/not/used",
        ],
    );
    assert!(
        !fake.log.exists(),
        "the prefixed refusal must precede any podman access; recorded:\n{}",
        fake.recorded()
    );
}

#[test]
fn run_help_documents_local_image_forms_without_buildah() {
    let output = Command::new(hermit_test::hermit_binary())
        .args(["run", "--help"])
        .output()
        .expect("run hermit for help");
    assert_eq!(output.status.code(), Some(0));
    let help = stdout_of(&output);
    assert!(help.contains("oci-dir:"), "{help}");
    assert!(help.contains("docker-archive:"), "{help}");
    assert!(help.contains("flattened to the invoking user"), "{help}");
    let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("no manifest blob digest"), "{help}");
    assert!(flat.contains("config hash and layer diff_ids"), "{help}");
    assert!(
        flat.contains("unspecified os/architecture is accepted"),
        "{help}"
    );
    assert!(flat.contains("subsecond mtimes"), "{help}");
    assert!(
        !help.contains("buildah"),
        "help must not claim buildah materialization:\n{help}"
    );
    assert!(
        !help.contains("PROTOTYPE"),
        "image mode is supported, not a prototype:\n{help}"
    );
    assert!(
        flat.contains("compose with it"),
        "help must state that --mount/--bind compose with --image:\n{help}"
    );
}
