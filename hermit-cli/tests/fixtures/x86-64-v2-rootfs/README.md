# The x86-64-v2 rootfs fixture

Input for `hermit-cli/tests/cpu_profile_v2_rootfs.rs`: the minimum
CentOS Stream 9 userland that lets a real glibc dynamic loader
enforce the x86-64-v2 level Hermit presents to its guests. The
fixture bytes are build inputs and are never committed; this README
is the provenance record and the regeneration recipe.

## Contents

The directory named by `HERMIT_TEST_V2_ROOTFS` holds exactly:

| Path | Source | Measured size (C9, 2026-11-15) |
| --- | --- | --- |
| `lib64/ld-linux-x86-64.so.2` | CentOS Stream 9 `glibc` package | 940 K |
| `lib64/libc.so.6` | CentOS Stream 9 `glibc` package | 2.6 M |

Nothing else. The test adds the guests it compiles
(`fixtures/isa/cpuid_v2_probe.c`, `fixtures/isa/needs_v3_lib.c`,
`fixtures/isa/isa_needed.c`), empty `proc`/`dev`/`tmp`
mountpoints, and assembles a one-layer OCI directory (~3.5 MB
rootfs, ~1.5 MB stored). CentOS 9's baseline *is* x86-64-v2, which
is the point: a baseline-v1 userland (e.g. Debian) cannot
distinguish "v2 works" from "baseline works". Static busybox
cannot substitute: it never runs ld.so's CPU-feature detection.

Before trusting a regenerated fixture, check it the way the test
does: the loader must report `x86.cpu_features.isa_1=0x3` under
`hermit run --cpu-profile=x86-64-v2` (and `0x7` under
`--cpu-profile=x86-64-v3`). Note CentOS 9's glibc carries no
`GNU_PROPERTY_X86_ISA_1_NEEDED` (measured 2026-11-15: only the
IBT/SHSTK feature property), so the loader diagnostics — not an
ELF-note refusal — are the enforcement oracle.

## fbsource

`fb-only/v2_rootfs:glibc-fixture` produces this directory by
extracting the two files from that package's CentOS 9 antlir2
layer (D131946467, the buck-e2e v2 retry's rootfs; this test's
diff is stacked on it). Reuse was evaluated against the plan's
four criteria and holds: the layer exports `rootfs.tar` as a
build artifact, its glibc is CentOS 9's v2 build (byte-identical
to a C9 host's, measured 2026-11-15), the layer is
buck-cached and shared with the retry, and the guests are
compiled at test time with explicit `-march` flags. The retry is
a stopgap with a deletion date; the extraction target is written
to outlive it. Regeneration is a package bump in that layer; the
two-file layout above is the contract.

## OSS

Extract the two files from the pinned minimal image (pin the
digest when first run; record it here):

```bash
podman pull quay.io/centos/centos:stream9-minimal
cid=$(podman create quay.io/centos/centos:stream9-minimal)
mkdir -p v2-rootfs/lib64
podman cp "$cid:/lib64/ld-linux-x86-64.so.2" v2-rootfs/lib64/
podman cp "$cid:/lib64/libc.so.6" v2-rootfs/lib64/
podman rm "$cid"
HERMIT_TEST_V2_ROOTFS=$PWD/v2-rootfs HERMIT_TEST_REQUIRE_V2_ROOTFS=1 \
  cargo test -p hermit-run --test cpu_profile_v2_rootfs
```

On a CentOS Stream 9 host the system files are already this
fixture. On any other host, do not substitute the host's own
`/lib64` files: the host distribution's baseline is exactly what
the fixture exists to pin down.
