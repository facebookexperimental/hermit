# Initial randomness for the in-guest runtime

LiteInst and in-guest-trap use the same initial-image mechanism. Detcore
prepares one deterministic 16-byte draw. A libc-free interpreter bootstrap
writes it to the real kernel `AT_RANDOM` target before the original dynamic
loader starts. The launch owner authenticates the child and image, reads back
all sixteen bytes, and only then permits transfer. The ordinary Tool constructor
redeems the completed Detcore state once, so subsequent `getrandom` calls
continue the same stream. This does not enable native exec or monitor the
original loader's syscalls.

The bootstrap reserves `[0x520000000000, 0x520000080000)`. Before transferring
to the original loader, it clears its writable nonce, configuration, map
snapshots, saved registers and stack. The sole retained writable word is the
original loader entry address, derivable from `AT_BASE` and the loader's ELF
header. It restores the original registers, flags and stack pointer without
writing the guest's original stack, apart from the intended `AT_RANDOM` bytes.
The integer-only bootstrap is compiled using general registers only, leaving
the original floating-point and SIMD state untouched.

After that transfer, the bootstrap and redemption code never access, wipe,
reprotect, reclaim or reuse the retired storage. In particular, a `DT_NEEDED`
library constructor may replace a page with `MAP_FIXED`; later redemption must
preserve that guest mapping. Repeated redemption in the same loaded runtime
image is a named refusal. A real kernel exec replaces the address space normally.

## Named visibility gaps

**Retained initial-image bootstrap range:** the 512 KiB reservation remains
visible in the process's maps. This is not a claim that all those bytes remain
runtime-owned: a guest may replace them. Ownership reporting must eventually
use the shared backend ownership-facts interface and prove current ownership;
a historical address interval alone must never hide a guest replacement.
No second maps renderer or unchecked private-range declaration is introduced.

**Runtime-layout-dependent patch arenas:** addresses observed by a guest can
change when the runtime ELF changes, because patch-arena placement depends on
that ELF's layout. The fix is to place patch arenas independently of the runtime
DSO. Same-build strict comparisons remain exact; cross-version INFO differences
must be retained and physically attributed, never normalized away.
