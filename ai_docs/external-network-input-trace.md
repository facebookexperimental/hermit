# External network input trace: a sound first slice

Status: reviewed design plus an inert code foundation based on Hermit
`7b8fc74154b116338ccb3274eefa6b3d2d318974`.

The implemented foundation is deliberately narrower than an operational
recorder or replayer:

- `detcore-model::network_trace` defines the framed, versioned
  `NetworkTraceV1` schema keyed by `OpenFileId`;
- strict validation admits exactly one outbound IPv4/IPv6 TCP client channel
  whose `OpenFileId` comes from the socket allocation domain and was allocated
  before competing guest threads;
- the reader rejects truncation, trailing data, oversized payloads, unknown
  versions, epochs outside `GlobalTime`'s unsigned nanosecond domain, unknown
  channels, noncanonical offsets/releases, and events outside that envelope;
- `Config::network_trace` defaults to `Off`, is not a CLI flag, and is not read
  by any syscall or scheduler path; and
- `network_perturb_seed` is explicit and has no fallback to `seed` or
  `sched_seed`.

Still future work: socket classification/provenance, the Detcore-to-subtool
operation seam, recording at the deterministic commit boundary, replayed TCP
state, scheduler-owned eligibility, poll readiness, CLI plumbing, and the
end-to-end no-network replay test. No current host-ready completion has been
made reproducible by the model alone.

## Decision

Do not add a scheduler log of the `DetTid`s that happen to be ready in
`Scheduler::step2c_process_io_blockers`, and do not describe the existing
record/replay stream as a schedule-independent network recording. Neither is a
sound slice of the requested feature.

The smallest sound end-to-end slice is one externally connected TCP byte stream,
owned by a stable Detcore open-file-description identity, with:

1. an external-input trace distinct from the schedule trace and the existing
   whole-syscall replay streams;
2. inbound bytes, peer half-close, and terminal socket errors released at
   recorded virtual-time eligibility points;
3. outbound bytes checked as a stream, independently of syscall chunking;
4. level-triggered `poll` readiness derived from the replayed channel state; and
5. replay under an explicitly chosen scheduler seed.

The initial slice must fail closed for `accept`/server sockets, datagrams,
ancillary data, `MSG_PEEK`, `MSG_WAITALL`, urgent data, and epoll. Those features
need additional state machines; silently using the live kernel for any of them
would invalidate the guarantee.

The model foundation is safe to implement before that runtime seam because its
channel identity is already `OpenFileId` and no code emits an artifact keyed by
raw fd or thread timing. The existing Recorder interface still cannot identify
the Detcore open file on which a syscall operates, so enabling record or replay
must wait until that prerequisite API and the first end-to-end slice below are
implemented together.

## Existing behavior and the exact gap

`record_or_replay_config` enables `recordreplay_modes`, and Detcore routes an
external potentially blocking syscall through
`record_or_replay_blocking`. The thread:

1. submits `BlockingExternalIO(ExternalOpId { tid, syscall_count })`;
2. leaves the run queue and performs the real blocking operation;
3. asks the Recorder or Replayer subtool to service that syscall;
4. submits `BlockedExternalContinue` after the operation returns; and
5. waits for `step2c_process_io_blockers` to re-admit it.

The recorder already captures syscall-local results for `poll`, `ppoll`,
`epoll_wait`, `recvfrom`, and `recvmsg`, plus return values for `connect` and the
send family. The replayer returns those recorded values and copyouts. These are
per-thread whole-syscall streams. They also contain filesystem, clock, random,
and process-related events and require the replayed thread to issue the same
subscribed syscall sequence.

This does not provide the requested boundary:

- `step2c_process_io_blockers` reads whichever continuation Ivars host threads
  have filled at that instant. Sorting the resulting vector gives a stable order
  for one snapshot, but host timing still decides membership in the snapshot.
- The same `BlockingExternalIO` class includes network sockets and other host
  blocking operations. It carries no stable socket identity or operation kind.
- `Recorder::handle_syscall_event` sees its inner `RecorderThreadState`, not
  Detcore's `FileMetadata`, so it cannot key an event by `OpenFileId`.
- Existing `PollEvent` records raw fd numbers and a raw readiness result. It
  cannot derive readiness after a changed schedule consumes a different amount
  of a TCP stream.
- Replay `epoll_wait` sometimes re-enters the fresh kernel epoll object for a
  side effect. That is incompatible with a network-only replay until the
  watched channel state and epoll trigger state are modeled.
- The replay CLI has no supported scheduler-seed override; its Detcore config is
  reconstructed by `record_or_replay_config`.

## Input boundary

The external trace records only observations attributable to a peer outside the
Hermit container. The initial supported channel is:

- `AF_INET` or `AF_INET6`;
- `SOCK_STREAM`;
- created by the guest and successfully connected to a non-loopback address;
- created before competing guest threads can make socket-allocation order
  schedule-dependent;
- one open file description, including all `dup` and `fork` aliases;
- no descriptor passing or process migration across `exec` beyond the already
  modeled descriptor lifetime.

Unix sockets, socket pairs, pipes, eventfds, pidfds, and loopback TCP peers are
container-internal and remain under deterministic scheduling rather than this
trace. Netlink sockets are kernel interfaces, not external peers, and retain
their existing sanitizers.

The identity is `OpenFileId`, not a raw fd and not `ExternalOpId`:

- raw fd numbers are slots and can be closed/reused;
- `ExternalOpId` identifies one syscall attempt and changes when execution takes
  a different per-thread path;
- `OpenFileId` identifies the Linux open file description, survives `dup` and
  fork aliases, and already has a socket-specific deterministic allocation
  domain.

The socket's domain, type, protocol, local address, and peer address are recorded
as immutable channel metadata. The recorder must classify the peer after the
kernel-authoritative `connect` result. A failed connect is an operation result,
not an external channel.

## Versioned schema

The trace is a separate file with its own magic and version. It is not a new
variant in the existing per-thread `SyscallEvent` stream.

```text
NetworkTraceFramePrefix {
  magic: "HERMIT-NET-TRACE",
  version_le: 1,
  payload_length_le: u64,
}

NetworkTraceV1 {
  epoch: RFC3339 timestamp,
  channels: [NetworkChannel],
  inputs: [NetworkInputEvent],
  outputs: [NetworkOutput],
}

NetworkChannel {
  id: OpenFileId,
  transport: Tcp,
  role: OutboundClient,
  local_address: bytes,
  peer_address: bytes,
  created_before_competing_threads: bool,
}

NetworkInputEvent {
  ordinal: u64,
  channel: OpenFileId,
  release: {
    not_before_global_time: LogicalTime,
    after_transmitted_offset: u64,
  },
  kind: InboundBytes { stream_offset, bytes }
      | PeerWriteClosed { stream_offset }
      | SocketError { stream_offset, errno },
}

NetworkOutput {
  channel: OpenFileId,
  stream_offset: u64,
  bytes: bytes,
}
```

The payload uses bincode's standard configuration. Both writer and reader run
semantic validation; the reader additionally requires exact payload
consumption and EOF after the frame. The 64 MiB payload limit is checked before
allocation. Epoch conversion is checked before constructing `GlobalTime` and
uses its exact microsecond-truncated, unsigned-nanosecond representation;
pre-1970 timestamps and positive timestamps beyond that range are typed trace
validation errors rather than panics.

`ordinal` is a total order of externally observed input events. It records an
input when several peers become observable together; it is not a scheduler
turn. `not_before_global_time` is the absolute `LogicalTime` returned by
`GlobalTime::as_nanos`, including the epoch. Replay must initialize its
`GlobalTime` with the trace's epoch and compare those two values directly; it
must not subtract the epoch on only one side. `after_transmitted_offset`
captures request/response causality without naming a scheduler turn: a response
cannot be released before replay has validated the request bytes that preceded
it. Equal-time events are released in ordinal order.

Outbound traffic is compared as a byte stream at monotonically increasing
offsets. This deliberately permits replay to split one recorded `send` across
several sends, or coalesce several sends, while refusing the first mismatching
byte. A syscall-result trace would unnecessarily freeze call boundaries and
therefore the schedule-dependent implementation detail the feature is intended
to vary.

Inbound traffic is buffered by channel after its eligibility point. A normal
`recv` consumes at most the requested count from the available prefix. EOF is
visible only after all prior bytes are consumed. A terminal error becomes
visible at its recorded position. `SocketError` does not represent retryable
`EAGAIN`/`EWOULDBLOCK` or interrupted `EINTR` operations: none terminates the
peer's byte stream. Validation, writing and reading all reject those errno
values in terminal records. They require separate operation/readiness state in
future runtime integration; this model does not enable that integration.
The first slice rejects flags whose semantics
need extra state, including `MSG_PEEK`, `MSG_WAITALL`, and out-of-band data.

## Deterministic eligibility and scheduler integration

Recording converts a host-timed completion into explicit input exactly once:

1. Detcore publishes a typed `NetworkOperation` before the thread leaves the
   run queue. It contains `ExternalOpId`, `OpenFileId`, direction, and operation
   constraints captured before the syscall.
2. After the kernel operation completes, its result is returned to Detcore with
   the network event data.
3. Under the scheduler mutex, at the next `step2` drain, Detcore snapshots
   `committed_time`, assigns the global input ordinal, and appends the event.
4. The continuation becomes runnable only after the append succeeds. A trace
   write failure is terminal and cannot be exposed as a guest errno.

Replay never asks the host socket whether it is ready. It loads the next input
event and makes it eligible when both release conditions hold:
`global_time.as_nanos() >= not_before_global_time` and the channel has validated
at least `after_transmitted_offset` outbound bytes. Both time operands are
absolute logical nanoseconds including the same recorded epoch. If no ordinary guest work can
advance time, the scheduler advances to the next finite network eligibility
point whose transmit watermark is satisfied, using the same monotonic mechanism
as timed waiters. It never rewinds, freezes, rounds, or substitutes host wall
time.

Arrival before an operation is posted is retained in the channel buffer. An
operation posted before arrival remains blocked. Thus a changed schedule can
move a `recv` around the recorded arrival without changing the external input.
Trace exhaustion, an unknown channel, or an operation outside the supported
envelope is a typed replay failure, not a fallback to the live network.

The scheduler seed and network trace are independent inputs. Replaying with a
different seed changes only run-queue choices. It does not rewrite event
ordinals or eligibility times. Network delay, fragmentation, injected `EAGAIN`,
and reset perturbations use a separate `network_perturb_seed`; they must never
consume or derive from `sched_seed`. TCP byte order is immutable: the
perturbation layer may not reorder, corrupt, or silently drop recorded bytes.

## `poll` semantics in the first slice

The first slice supports `poll`/`ppoll` only when every nonnegative descriptor
in the set is either the one supported external TCP channel or an ignored
negative fd. Readiness is computed from modeled state:

- `POLLIN` when inbound bytes are buffered, peer write-close is visible, or a
  readable terminal error is pending;
- `POLLOUT` after successful connection while the local write half is open and
  no terminal error prevents writing;
- `POLLHUP` only once the peer close has become eligible;
- `POLLERR` from the recorded pending error;
- unrequested error/hangup bits are still reported as Linux specifies;
- finite timeout returns zero only when virtual time reaches its deadline before
  the next matching network event.

The original guest fd array and requested masks remain syscall inputs. Replay
writes only `revents`. Bad pointers, invalid `nfds`, and timeout copyout behavior
retain the existing Linux-faithful handling.

`epoll` is explicitly refused in v1. Correct support requires a modeled interest
list keyed by open-file identity plus registration identity, including
level-triggered versus edge-triggered delivery, `EPOLLONESHOT`, `EPOLLEXCLUSIVE`,
`EPOLL_CTL_MOD` rearming, close/fd-reuse behavior, and the fact that duplicate fds
can register one open file more than once. Replaying a recorded `epoll_wait`
array or reinjecting the fresh kernel call is not equivalent when the schedule
changes.

## Required code seam

Replace the marker-only `RecordOrReplay` trait with an explicit external-network
contract whose types live in `detcore-model` (or another dependency shared by
Detcore and `hermit-cli`). At minimum:

```text
begin_network_operation(op_id, channel_id, operation)
complete_network_operation(op_id, observed_result, virtual_time)
poll_network_channels(interests, virtual_deadline)
```

Recorder and Replayer implement that contract. `NoopTool` rejects its use.
Detcore resolves the raw fd to `DetFd`, validates `FdType::Socket`, extracts
`OpenFileId`, and owns scheduler blocking. This keeps descriptor identity and
the deterministic commit point in the layer that actually knows them; the inner
tool owns serialization and replay bytes.

The operation must retain its `DetFd`/open-file-description reference across the
await, just as existing captured-descriptor operations do, so close plus fd reuse
cannot redirect completion to a new socket. Close releases the channel only when
the final alias disappears.

## Minimal acceptance test

An end-to-end test uses an external controller TCP server and a guest with one
client connection:

1. Record the server sending `abc`, later `def`, then half-closing.
2. Record the guest polling and reading with buffers that do not match packet or
   send boundaries.
3. Stop the server and remove network access.
4. Replay the same network trace under at least two distinct scheduler seeds.
5. Require identical guest bytes, EOF/error behavior, and full strict DETLOG
   parity except for the explicitly varied schedule decisions.
6. Perturb only the trace's eligibility delay and fragmentation knobs, then
   require the corresponding allowed alternate read/poll behavior.

Negative tests must prove that replay performs no external connect/receive,
rejects a mismatched outbound byte, rejects a second external channel, rejects
unsupported socket flags and epoll, and does not consume an event before its
virtual eligibility point. Unit tests must bracket readiness before and after
operation publication and before and after scheduler selection.

## Implementation sequence

1. Add stable socket metadata (domain/type/protocol and external/loopback
   classification) to the shared open-file description. Preserve it across
   `dup`, fork, and exec; test close/fd reuse.
2. Add the typed Detcore-to-subtool network operation contract and versioned
   sidecar reader/writer. Fail closed when configured replay data is missing,
   malformed, truncated, or contains an unknown version/channel/event.
3. Implement connect, stream send validation, stream receive buffering, EOF,
   and errors for one outbound TCP channel.
4. Add virtual-time eligibility to the scheduler and the empty-run-queue path.
   Network events are separate timed inputs, not `BlockedExternalContinue`
   snapshots and not schedule events.
5. Implement the restricted level-triggered poll model and the end-to-end test.
6. Expose replay scheduler-seed override without changing the network trace.
7. Only then widen to multiple channels, server/accept, datagrams, ancillary
   data, and a fully modeled epoll interest/trigger state machine.

## Why the implemented model enables no runtime code path

The obvious tiny patches are unsound:

- logging `ready` in `step2c` records a schedule-relative thread snapshot, not
  network input, and cannot survive a new schedule;
- keying recorded data by raw fd aliases a later socket after close/reuse;
- returning recorded `poll` arrays preserves old syscall boundaries instead of
  deriving readiness from channel state;
- replaying the live kernel epoll object reintroduces the external input the
  trace is meant to replace;
- adding only a scheduler-seed flag suggests the existing whole-syscall replay
  can explore schedules safely, although its per-thread event streams and live
  side effects can desynchronize.

Implementing any one of those would create a green-looking but false
determinism claim. The typed boundary above is the minimum prerequisite for a
real vertical slice.
