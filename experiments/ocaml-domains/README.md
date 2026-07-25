# OCaml multicore domain scheduling

This experiment exercises scheduling nondeterminism in OCaml 5 multicore
domains. The program is `domain_completion_order.ml`.

Four workers created with `Domain.spawn` block on a condition-variable start
barrier. After the parent broadcasts the start condition, every domain performs
the same amount of deterministic CPU-bound work. An atomic counter records the
order in which they complete, and joined worker results form a deterministic
checksum.

The completion order is guest-visible scheduling state. Native Linux runs are
expected to produce more than one order, while Hermit strict-mode runs are
expected to produce exactly one.

## Toolchain

The measured toolchain is:

- OCaml 5.3.0 from the `5.3.0` opam switch;
- the `ocamlopt` native compiler;
- GCC with POSIX threads, targeting `x86_64-pc-linux-gnu`;
- program `domain_completion_order.ml`.

The CentOS Stream 9 `ocaml` RPM installed on the test host is version 4.11.1
and does not contain the multicore `Domain` module. The runner therefore
selects an explicit OCaml 5+ opam switch and rejects older compilers.

## Dual assertion

`run.sh` compiles the probe and enforces both sides of the determinism claim:

1. Forty native executions must contain at least two distinct stdout hashes.
2. Five `hermit run --strict` executions must have exactly one stdout hash.

The script exits nonzero if either assertion fails or any execution times out.
Override the run counts and workload with `NATIVE_RUNS`, `STRICT_RUNS`,
`WORKERS`, and `ITERATIONS`.

## Result

The default workload produced:

| Mode | Runs | Unique outputs | Result |
| --- | ---: | ---: | --- |
| Native Linux | 40 | 19 | Nondeterminism observed |
| Hermit `--strict` | 5 | 1 | Deterministic |

The repeated strict output was:

```text
order=2,1,3,0 checksum=716457125964051396
```

`NONDET_SOURCE` is OCaml domain scheduling: the worker computation and
checksum are fixed, while native completion order changes.

## Run

From the repository root:

```bash
cargo build -p hermit
./experiments/ocaml-domains/run.sh
```

Set `OUTPUT_ROOT` to a new path under the repository to retain per-run stdout
and stderr. Otherwise the runner uses an ignored temporary directory under
`target/ocaml-domains/` and removes it on exit.
