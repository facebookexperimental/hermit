# RPC transport ping-pong

This experiment measures the steady-state cost of two candidate transports for
Reverie's cross-process `GlobalTool` RPC:

- a blocking Unix-domain socket with the shared transport's four-byte
  big-endian length prefix and bincode 2;
- tarpc over a Unix-domain socket with tarpc's bincode codec.

The script uses Detcore's real private RPC enums without copying or exposing
them. `ActualRequest` and `ActualResponse` are aliases for the public
`GlobalTool` associated types on `detcore::GlobalState`. At runtime, the script
prints the resolved names, which include
`detcore::tool_global::GlobalRequest` and `GlobalResponse`. The fixture is a
`GlobalTimeLowerBound` request and response. Its bincode 2 payload sizes are 61
and 13 bytes, respectively.

## Run

Install [`rust-script`](https://rust-script.org/), then run from the repository
root:

```sh
./experiments/rpc-transport/ping_pong.rs
```

The first run downloads and compiles the declared dependencies. On a Meta host,
use the network wrapper for that run:

```sh
with-proxy rust-script experiments/rpc-transport/ping_pong.rs \
  --iterations 100000 --warmup 10000
```

Use `--help` for the two tunables. `rust-script` builds optimized binaries by
default; passing its `--debug` option is not representative for measurement.

## Method

Both cases use a connected UDS pair with the client and server on separate OS
threads. Connections are created before timing. Calls are sequential with one
request in flight, matching `GlobalRPC::send_rpc`. Each measured round trip
includes request serialization, length framing, two kernel socket crossings,
server deserialization and dispatch, response serialization, and client
deserialization. Warmup samples are discarded.

The raw path uses `bincode` 2 with `config::legacy()`, matching Hermit's current
bincode configuration. tarpc 0.37 uses `tokio-serde` 0.9, whose bincode codec is
bincode 1.3; tarpc also carries its context, request ID, client driver, channel,
and spawned dispatch task. The result therefore compares the two deployable
transport stacks, not only their framing functions.

This is a single-client latency/throughput benchmark. It does not measure
multi-client contention, scheduler work inside `GlobalState::receive_rpc`,
cross-process startup, or end-to-end guest syscall cost. Host scheduling and
power state can move the absolute values, so compare transports in the same
invocation and repeat trials.

## Results

Measured on 2026-07-25 at Hermit base commit
`a2132d1e94a90fa1d9d4e4c29492f7c336d080b6`:

- Linux `6.17.13-0_fbk0_crackerjackhost_0_g2b4321c50d79`, x86-64;
- AMD EPYC 9D85 158-Core Processor, 316 logical CPUs;
- three trials, each with 10,000 warmups and 100,000 measured RPCs.

Exact command:

```sh
for run in 1 2 3; do
  echo "=== trial $run ==="
  with-proxy rust-script experiments/rpc-transport/ping_pong.rs \
    --iterations 100000 --warmup 10000
done
```

The table reports the median result across the three trials. Parentheses show
the observed minimum and maximum.

| Transport | p50 latency (us) | p99 latency (us) | Throughput (RPS) |
| --- | ---: | ---: | ---: |
| raw UDS + bincode 2 | 10.176 (10.035-10.666) | 22.404 (18.718-22.514) | 88,362 (86,179-92,239) |
| tarpc + bincode | 19.449 (18.327-19.629) | 34.993 (32.809-35.473) | 47,752 (46,709-47,972) |

Using the median values, tarpc took 1.91 times the raw p50 latency and 1.56
times the raw p99 latency, while delivering 0.54 times the sequential
throughput. The median p50 difference was 9.273 microseconds per RPC. For an RPC
on every intercepted syscall, that difference is direct per-syscall transport
overhead before Detcore's own coordinator work.

These measurements support the dependency-light raw UDS + bincode transport
for the latency-sensitive Detcore RPC. tarpc remains a useful fallback if its
service generation, request context, cancellation, and channel management
become more valuable than the measured overhead.
