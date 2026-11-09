# Reading and inspecting schedule histories

`hermit show --preemptions FILE` reads a preemption or slice-boundary JSON
history without starting a guest. It prints JSON containing the format, stored
event and boundary row counts, each thread's priority/RCB/chaos-epoch/callback
row counts and final priority, and the recorded clock epoch, pipe-wake latency
and signal-copy setting. Legacy `boundary_rows: null` means that format has no
boundary protocol; it is distinct from an empty boundary stream. Optional
settings keep `null` distinct from `0` and `false`.

The command prints only after complete deserialization, EOF and the existing
format/history validators pass. Unknown fields retain their existing format
policy. An invalid boundary envelope never falls back to legacy or manifest
parsing. Positional `hermit show RECORDING` keeps showing its manifest as
before and cannot be combined with `--preemptions`.

Regular seekable histories use buffered input and two complete passes: a
format probe and typed deserialization. SHA-256 fingerprints of all consumed
bytes, including trailing whitespace, must agree. Both passes use the same
descriptor. This detects inconsistent passes; it does not make the file an
immutable snapshot. Pipes/FIFOs retain the historical whole-byte compatibility
buffer because they cannot be rewound. Metadata-only readers still accept
their own legacy field independently; boundary metadata requires the full
boundary format to validate.

File output uses compact JSON, a terminal LF and an explicitly checked flush.
Borrowed boundary views avoid cloning the complete tape for serialization.
Validation and a serialization pass into a sink precede opening the destination
so a serialization refusal does not truncate an existing file. Descriptor
paths keep the same open behavior. A failed explicit recording write/flush
fails finalization; neither the consumed writer nor its output buffer retries
the failed write on drop. This is not a durable-storage/fsync guarantee.

Streaming I/O removes the extra raw/JSON-tree/output-string copies for regular
files, but the typed vectors, maps and existing validators still use memory
proportional to the recording. Inspection can still exceed external memory or
time bounds. Counts and structural validation establish no replay progress,
terminal consumption, observed callback execution, crash reproduction or
record/replay certificate.
