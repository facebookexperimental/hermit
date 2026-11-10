# Shared in-guest Detcore engine

This first-party package owns one compiled `ToolHost<Detcore, EngineRuntime>`,
allocator, Tool TLS, continuation storage and RPC client in one engine DSO.
Native shims link that artifact; they do not compile another Detcore or runtime.
The same concrete runtime delegates the actual mechanism services inside this
linked Rust image. Futures, references and ownership tokens do not cross its
synchronous C boundary.

The included native module uses `include/hermit-engine-native-v1.h` for scalar
callbacks from a C mechanism shim into this same engine. It preserves the common
Guest, Tool process owner, RPC and deferred callback lifetimes. Source inclusion
does not establish that its exports link or that a native loader runs them; the
actual producer, ELF and lifecycle checks remain required.

`include/hermit-engine-v1.h` declares the ordinary GNU Linux x86-64 boundary.
The preload entry and saved-frame loader entry establish the maintained fixed
bootstrap stack/backing before installation. Settings are bounded, unknown and
duplicate settings refuse, and failed/repeated installation poisons its latch.
The header version does not replace exact source/build/cohort qualification.
The maintained launcher must bind the engine and its shims as one artifact set.

The ordinary saved-frame loader also negotiates its lifetime-stable state
callback through the header. The setter stores only that pointer, without
allocation, TLS or Tool construction. During guarded installation the engine
takes at most 4096 bytes once, checks the existing canonical Detcore schema,
effective configuration and real PID/start-time/AT_RANDOM image identity, and
applies only the declared initial random fields. Refusal, missing negotiation,
replacement or duplicate transfer never falls back to a fresh seed. An
authenticated exec/static continuation leaves normal constructor state intact.
The shared installer prepares this root before publication, while the original
first-event thread-start and post-exec callbacks retain their original order.
Fork inheritance and the private lifecycle installers are separate unchanged
owners; this transport supplies no admission or readiness fact.

This package exports no initial-image, successor, native-exec or mapped-image
entry and grants no such authority through C arguments. Existing private phase
issuers, affine owners and their measured-source finalizers remain in their
maintained installation paths. They require separate integration and actual
qualification before those modes can use moved engine code.

The default build has no diagnostic observer. `dispatch-observer` installs the
existing actual Detcore entry observer and emits bounded getpid and brk identity
records; these are entry observations, not semantic completion or a verdict.
The observer never changes configuration, a syscall answer, scheduling policy,
a phase permit or the brk model. Ordinary probe results do not qualify full
record/replay, verification, private-image or native behavior.

`loader_state` is a focused integration test target for the exact transport and
state implementation under a normal test allocator. It does not load this DSO,
run a guest, or qualify its constructors/loader. The shared host's separate
ordinary-root controls exercise both actual dispatch paths and lifecycle order.

`native_policy` includes the exact production helper module under a normal test
allocator. Its eight transferred tests retain the maintained syscall, clone and
partial-random assertions. They do not load the engine or qualify native thread,
exec, signal, RPC or loader behavior.

The source-owned neutral artifact/header installation interface is documented
in [ARTIFACTS.md](ARTIFACTS.md). Its output records actual Cargo units and exact
source pins; installation alone supplies no loader or phase qualification.
