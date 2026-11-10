# Running Hermit in Podman or Docker

Hermit can run inside an OCI container on x86-64 Linux. Keep the image, guest
files, arguments, environment, seeds, and virtual-time epoch fixed, and use
`--network=none` on the container runtime for an offline audit. Network
isolation alone does not freeze filesystem inputs or establish determinism.

The measurements below used rootless Podman 5.8.5 and an Ubuntu 24.04.3 image.
Docker commands are analogous, untested examples; there is no measured Docker
minimum.

## Prepare an offline image

Use a locally cached x86-64 Ubuntu or Fedora base whose glibc ABI can load your
Hermit build. Check `ldd image-install/hermit` and the optional backend shared
libraries, and supply missing non-libc runtime libraries, such as libunwind and
LZMA, from packages compatible with that base. Copying a newer host libc into an
older base is not a substitute for an ABI-compatible installation.

Assemble this build context before going offline:

```text
Containerfile
image-install/hermit          # executable, not a dangling staging symlink
image-install/rsrcs/          # matching optional backend resources, if used
image-libs/                  # required extra runtime libraries, dereferenced
probe                        # your executable guest
```

The 0.5.0 release provides ptrace and KVM. DBT, LiteInst, and SaBRe require a
[feature-enabled source build](../README.md#execution-backends). For a source
installation, `cp -aL target/install_pkg/ image-install/` preserves the layout
while dereferencing its executable symlink. A CLI built with an optional
feature can still refuse a run when its matching runtime assets are absent.
DBT needs the optimized DynamoRIO client, its Detcore runtime, and DynamoRIO's
launcher and libraries. LiteInst needs `libdetcore_liteinst.so` built from the
same source tree. SaBRe needs its loader and tool libraries. For a core release,
put the release executable in `image-install/hermit`. e9patch preprocessing also
needs `e9tool` and its companion assets; `HERMIT_E9TOOL` can name its executable.

Check that the DBT client's dynamic dependencies resolve inside the image,
including libraries loaded by DynamoRIO. The measured validate installation
needed its build-directory RPATH changed to paths relative to the bundled
client before copying it:

```bash
patchelf --set-rpath \
  '$ORIGIN:$ORIGIN/dynamorio/ext/lib64/release:$ORIGIN/dynamorio/lib64/release' \
  image-install/rsrcs/libreverie_dbt_client.so
```

This assumes `libdetcore_dbt.so` is beside the client and the DynamoRIO tree is
under `rsrcs/dynamorio/`. Inspect the staged installation rather than retaining
absolute paths into its build machine.

For example, use this `Containerfile` with an already cached `ubuntu:24.04`
base, an executable `probe`, and the directories above:

```dockerfile
FROM ubuntu:24.04
COPY image-install/ /opt/hermit/
COPY image-libs/ /opt/hermit/lib/
COPY --chmod=0755 probe /audit/probe
ENV PATH="/opt/hermit:${PATH}"
ENV LD_LIBRARY_PATH="/opt/hermit/lib"
ENV TZ=UTC LC_ALL=C
WORKDIR /audit
```

There is no entrypoint, so the run commands below explicitly name `hermit`.
Build using only cached images and local files:

```bash
podman build --pull=never --network=none -t hermit-audit:local .
```

Record the resulting image ID and base digest in the audit. Docker's analogous
build command is
`docker build --pull=false --network=none -t hermit-audit:local .` with an
already loaded base. Run-time `--pull=never` prevents an image download.

## Host prerequisites and preflight

Read the host policy before launching the container; none of these commands
changes it:

```bash
sysctl kernel.perf_event_paranoid kernel.yama.ptrace_scope user.max_user_namespaces
podman run --rm --pull=never --network=none \
  --security-opt seccomp=unconfined hermit-audit:local hermit host-capabilities
```

A kernel without Yama may have no `kernel.yama.ptrace_scope` key. The relevant
requirements are:

- Rootless Podman needs usable user namespaces and UID/GID mappings. Hermit's
  default mode creates nested user, PID, mount, UTS, and network namespaces;
  namespace quotas and host LSM policy must permit them. A positive
  `user.max_user_namespaces` value alone is not sufficient.
- Ptrace needs same-UID parent-child tracing, permitted by seccomp and the host
  ptrace/LSM policy. `CAP_SYS_PTRACE` is normally unnecessary for tracing a child;
  adding it cannot remove an inherited seccomp filter or override every host
  restriction. Yama modes 0 and 1 normally allow the parent-child relationship;
  inspect the actual refusal on stricter hosts.
- Precise ptrace preemption needs `perf_event_open` and a usable, correctly
  counting PMU. `kernel.perf_event_paranoid` controls unprivileged access; its
  sufficient value depends on the kernel, event, and capabilities. Read the
  evidence beside `exact-branch-counter`: `PRESENT` can also mean that perf is
  unavailable and no counter is armed.
- KVM needs hardware virtualization, a loaded KVM module, and read-write access
  to `/dev/kvm` for the user launching rootless Podman. Pass the device with
  `--device /dev/kvm`. Passing a device does not grant missing host permissions.
- The outer policy must allow the syscalls used by the selected backend and
  guest. Namespace setup uses `unshare`, namespace-capable `clone`, mounts,
  `sethostname`, and `setdomainname`; process setup uses
  `personality(ADDR_NO_RANDOMIZE)`. Guest-memory access uses
  `process_vm_readv` and `process_vm_writev`. Ptrace needs `ptrace`; CPU
  instruction interception needs usable CPUID faulting with
  `arch_prctl(ARCH_GET_CPUID/ARCH_SET_CPUID)` on paths that use it. Ptrace and
  LiteInst each need to install their own filters through `seccomp` and `prctl`.

`host-capabilities --json` emits machine-readable evidence. The preflight
reports CPUID faulting, KVM device access, and branch-counter qualification; it
does not prove that namespace setup, every required syscall, a backend's
assets, or your program will work. For KVM, add `--device /dev/kvm` to the
preflight too; otherwise an absent KVM device inside that container is expected.
Run a small representative guest next. No extra `--cap-add` was needed for the
successful measurements below.

## Measured container settings

Hermit refuses to start any guest under an inherited seccomp filter, including
a runtime's default profile and a custom allowlist. The filter is a guest input
Hermit does not record. `--security-opt seccomp=unconfined` must be passed when
creating the container; a process cannot remove an already inherited filter.
This flag does not disable host LSM policy or grant devices or capabilities.

For ptrace and KVM, Hermit 0.5.0 at source commit
`7b16e1f62194609c48e8863064ab68fd1163fb25` ran a shell probe containing `date`,
16 bytes from `/dev/urandom` hashed with SHA-256, and a four-thread C program.
Each cell ran twice with `--epoch=2020-01-01T00:00:00Z` and outer
`--network=none`; KVM always included `--device /dev/kvm`.

| Additional Podman settings | Hermit namespaces | Hermit `--no-namespace` |
| --- | --- | --- |
| Default | Exit 122: inherited seccomp filter | Exit 122: inherited seccomp filter |
| `--cap-add=SYS_PTRACE` | Exit 122: inherited seccomp filter | Exit 122: inherited seccomp filter |
| Custom seccomp profile below | Exit 122: inherited seccomp filter | Exit 122: inherited seccomp filter |
| `--security-opt seccomp=unconfined` | Exit 125: `/sys` mount `EPERM` | Both exit 0; identical stdout |
| `--privileged` | Both exit 0; identical stdout | Both exit 0; identical stdout |

These are repeat-output results, without `--strict` or `--verify`. They do not
establish full-observation parity or a guarantee for another guest.

### Ptrace and KVM with strict namespace setup

The following narrower alternative to `--privileged` also ran the combined
probe twice under `--strict`, with identical raw stdout and exit 0 for ptrace
and KVM. It is the smallest successful namespaced configuration among the
settings tested; other kernels and OCI defaults may differ.

```bash
podman run --rm --pull=never --network=none \
  --security-opt seccomp=unconfined \
  --security-opt 'unmask=/proc/*:/sys/*' \
  --mount type=bind,src=/sys,dst=/sys,rw \
  hermit-audit:local hermit --backend=ptrace run \
  --strict --epoch=2020-01-01T00:00:00Z -- /audit/probe

podman run --rm --pull=never --network=none --device /dev/kvm \
  --security-opt seccomp=unconfined \
  --security-opt 'unmask=/proc/*:/sys/*' \
  --mount type=bind,src=/sys,dst=/sys,rw \
  hermit-audit:local hermit --backend=kvm run \
  --strict --epoch=2020-01-01T00:00:00Z -- /audit/probe
```

Hermit mounts fresh sysfs for its nested network namespace. The default OCI
system-mount restrictions prevented `mount(NULL, "/sys", "sysfs", 0)` even
with seccomp unconfined. Both the writable `/sys` bind and the unmask setting
were needed: either alone failed, as did unmasking only `/proc/*` with the
bind. Unmasking `/proc/*:/sys/*` removes more outer path protection and the bind
exposes host sysfs subject to the launching user's kernel permissions. Use a
trusted image and guest. Podman's `unmask` option is runtime-specific; a strict
Docker configuration needs a separate measurement of its system-mount policy.

### Minimal repeat-output diagnostic

The smallest working setting in the original five-setting matrix was seccomp
unconfined with Hermit's namespace setup disabled:

```bash
podman run --rm --pull=never --network=none \
  --security-opt seccomp=unconfined hermit-audit:local \
  hermit --backend=ptrace run --no-namespace \
  --epoch=2020-01-01T00:00:00Z -- /audit/probe
```

For KVM, add `--device /dev/kvm` before the image and select `--backend=kvm`.
An analogous, untested Docker ptrace command is:

```bash
docker run --rm --pull=never --network=none \
  --security-opt seccomp=unconfined hermit-audit:local \
  hermit --backend=ptrace run --no-namespace \
  --epoch=2020-01-01T00:00:00Z -- /audit/probe
```

The same KVM device/backend substitutions apply to Docker. `--no-namespace`
reuses the outer container's process, filesystem, `/tmp`, and network state.
It is intended for trusted guests and reduces the isolation Hermit provides.
Hermit rejects `--strict --no-namespace`: even when the outer container uses
`--network=none`, Hermit's own run mode shares its surrounding network
namespace. Matching outputs in this mode are a diagnostic.

### Optional backends: scope of the measured guests

The optional backend measurements used a validate-profile, feature-enabled
build from source `749465a236c1edf702c63e6f11d24c6dff4158da`, reporting version
0.4.1, rather than the 0.5.0 core release. Every default, `SYS_PTRACE`, and
custom-profile cell refused the inherited seccomp filter (exit 122).

| Backend or selector | Combined shell/threaded probe with usable permissions | Single-process dynamic C smoke |
| --- | --- | --- |
| DBT | Exit 101 after time/random output; no error diagnostic or thread results | Both exit 0; identical raw stdout |
| LiteInst | Exit 122: `execveat: the in-guest backend cannot replace the process image: execve and execveat are not forwarded` | Both exit 0; identical raw stdout |
| `in-guest-trap` | Exit 122: `the in-guest backend cannot replace the process image` | No separate smoke measured |
| SaBRe | Exit 125: `SaBRe executable was not found` | No working configuration measured; runtime asset absent |
| e9patch preprocessing with ptrace | Exit 125: `e9tool was not found in PATH` with namespaces; `requires mount namespaces to overlay the rewritten ELF at its original guest path` with `--no-namespace` | No working configuration measured; tool asset absent |

The smoke used `clock_gettime`, `getrandom` for 16 bytes, and a 500,000-iteration
branch loop. DBT and LiteInst each completed it twice with seccomp unconfined
and `--no-namespace`, and with `--privileged` in both namespace modes. Raw stdout
was compared without normalization or log suppression. Their runtime stderr
contains timestamps; no byte-identical stderr or strict/verify result was
established for these optional backends.

For that supported single-process scope, the minimal measured commands are:

```bash
podman run --rm --pull=never --network=none \
  --security-opt seccomp=unconfined hermit-audit:local \
  hermit --backend=dbt run --no-namespace \
  --epoch=2020-01-01T00:00:00Z -- /audit/probe

podman run --rm --pull=never --network=none \
  --security-opt seccomp=unconfined hermit-audit:local \
  hermit --backend=liteinst run --no-namespace --max-timeslice=disabled \
  --epoch=2020-01-01T00:00:00Z -- /audit/probe
```

Replacing `podman` with `docker` gives untested Docker examples for this same
scope. LiteInst requires `--max-timeslice=disabled` and refuses `--verify`.
Static and privilege-bearing programs, thread creation, and `exec` are
unsupported by that in-guest path. Disabling the timeslice removes precise
preemption; it does not establish support for threaded guests. `in-guest-trap`
shares this runtime and these restrictions. e9patch is preprocessing followed
by ptrace, needs its matching tools, and requires Hermit's mount namespaces;
it refuses `--no-namespace`. All measured LiteInst and `in-guest-trap` cells
used `--max-timeslice=disabled`. Neither `in-guest-trap` nor e9patch has a
measured working command in this experiment.

## A custom seccomp profile

[seccomp-hermit.json](seccomp-hermit.json) extends the installed Podman default
profile used for this experiment. It is an example for investigating syscall
policy. Runtime defaults and argument filters vary; it is not derived from
Docker's default. The baseline already permits `clone`, `clone3`, and `unshare`
without argument restrictions. The additions permit per-task `perf_event_open`
with CPU `-1` and close-on-exec flags, `personality(ADDR_NO_RANDOMIZE)`, and
Hermit's hostname/domainname setup. They leave host kernel checks in place;
seccomp cannot inspect the event configuration pointed to by the
`perf_event_open` argument.

Using any such profile still requires the explicit global option
`--unsafe-ignore-host-seccomp` before `run`. Hermit warns that the unrecorded
filter can change guest behavior and that the run may not reproduce where the
filter differs. Keep this relaxation visible in an audit record, together with
the exact profile bytes. It does not establish an equivalent deterministic
configuration to the unconfined commands above. The unsafe diagnostic example
below was not part of the measured matrix.

```bash
podman run --rm --pull=never --network=none \
  --security-opt seccomp=docs/seccomp-hermit.json hermit-audit:local \
  hermit --unsafe-ignore-host-seccomp --backend=ptrace run \
  --no-namespace --epoch=2020-01-01T00:00:00Z -- /audit/probe
```

## Recognize failures

| Error text or distinguishing excerpt | Meaning and next check |
| --- | --- |
| `hermit inherited a seccomp filter` | Hermit's admission policy refused an unrecorded filter. Restart the container with `seccomp=unconfined`, or explicitly accept the custom-filter diagnostic above. |
| `unshare(CLONE_NEWUSER \| CLONE_NEWPID) was denied` | Nested user/PID namespaces were denied by seccomp, a namespace limit, or host policy. |
| `child PTRACE_TRACEME probe was denied` | Parent-child tracing was denied; inspect the outer syscall profile and Yama/LSM policy. |
| `seccomp(SECCOMP_SET_MODE_FILTER) was denied` | Hermit could not install its own tracee filter; allow `seccomp` and `prctl(PR_SET_NO_NEW_PRIVS)`. |
| `Operation not permitted` or `Permission denied` during namespace mount/setup | Find the failing syscall and path. An OCI masked or read-only system mount can block a nested mount even with seccomp unconfined; see the `/sys` case above. |
| `this build was compiled without` the requested backend | Rebuild with its optional feature; added container privileges cannot add code to the binary. |
| `the DynamoRIO runtime was not found` | The DBT installation is incomplete or cannot be located. Copy the matching staged resources beside Hermit. |
| `SaBRe executable was not found` | Supply the matching loader/tool installation; `HERMIT_INSTALL_DIR` or `HERMIT_SABRE_BINARY` can locate it. No privilege flag fixes an absent asset. |
| `e9tool was not found in PATH` | Package e9patch's tool/assets or set `HERMIT_E9TOOL`; its ptrace execution still needs namespace setup. |
| `requires mount namespaces to overlay the rewritten ELF at its original guest path` | e9patch preprocessing cannot use Hermit's `--no-namespace` diagnostic. |
| `/dev/kvm` missing or not readable/writable | Pass `--device /dev/kvm` and inspect the launching user's host device access and KVM support. |
| `it cannot deliver Detcore's preemption timer yet` | LiteInst requires `--max-timeslice=disabled`; this is a backend limit. |
| `--strict` rejects `--no-namespace` | Use the supported namespaced path for a strict run. Outer `--network=none` does not change this run-mode restriction. |

For a supported backend and guest, stronger verification uses a fixed epoch
and `run --strict --verify --verify-strict --verify-json FILE`. Read the report:
full-observation parity requires `bitwise_parity: true` with nonzero compared
INFO-message counts. Plain `--verify` uses a lossy diagnostic comparator, and
matching stdout alone proves only matching stdout for the commands tested.
