# Cargo-Native Integration Test Runner

The `hermit integration` command provides a Cargo-native integration test runner that replaces the internal Buck-based test matrix. It discovers guest programs from `tests/` and `flaky-tests/`, executes them with various Hermit modes, and provides stable filtering, timeout, logging, and failure-reporting behavior.

## Quick Start

```bash
# Build the project first
cargo build --workspace

# Run local validation (quick sanity check)
cargo test -p hermit --test integration_runner -- local_validation -- --nocapture

# Or via the CLI
./target/debug/hermit integration --local-validation

# Run the full integration matrix
./target/debug/hermit integration

# Run with filters
./target/debug/hermit integration --filter echo
./target/debug/hermit integration --category basic
./target/debug/hermit integration --mode default --mode strict

# Generate coverage manifest
./target/debug/hermit integration --generate-manifest > coverage-manifest.tsv

# Dry run to see what would be executed
./target/debug/hermit integration --dry-run
```

## Command Line Options

| Option | Short | Description |
|--------|-------|-------------|
| `--filter` | `-f` | Filter tests by name pattern |
| `--category` | `-c` | Filter tests by category (basic, determinism, threading, ipc, memory, flaky, stress, standalone, shell) |
| `--mode` | `-m` | Filter tests by Hermit mode (default, strict, chaos, virtual-time, virtual-random, record, replay, verify) |
| `--parallel` | `-j` | Maximum parallel test executions (default: 4) |
| `--timeout` | | Override timeout for all tests in seconds |
| `--silently-pass-hardware-tests` | | Silently pass hardware-dependent tests instead of skipping |
| `--output-format` | `-o` | Output format: human, json, junit (default: human) |
| `--output-file` | | Write output to file instead of stdout |
| `--generate-manifest` | | Generate coverage manifest and exit |
| `--dry-run` | | Show what would be executed without running |
| `--local-validation` | | Run local validation only (quick sanity check) |

## Test Categories

Tests are automatically categorized based on their source directory and name patterns:

- **basic** - Simple commands (echo, ls, cat)
- **determinism** - Time, clock, randomness tests
- **threading** - Futex, thread, scheduling tests
- **ipc** - Network, pipe, IPC tests
- **memory** - Mmap, heap, stack tests
- **flaky** - Intentionally racy tests from flaky-tests/
- **stress** - Stress tests from tests/stress/
- **standalone** - Shell script tests from tests/standalone/
- **shell** - Shell tests from tests/shell/

## Hermit Modes

Each test can be run with different Hermit modes:

- **default** - Basic determinism (`--no-virtualize-cpuid --preemption-timeout=disabled`)
- **strict** - Full determinism (all hardening enabled)
- **chaos** - Chaos mode scheduling (randomized thread scheduling)
- **virtual-time** - Virtualized time (deterministic time progression)
- **virtual-random** - Virtualized randomness (deterministic RDRAND/RDSEED)
- **record** - Record execution trace
- **replay** - Replay from recorded trace
- **verify** - Verify mode (`--verify`)

## Hardware Requirements

Some tests require specific hardware capabilities:

| Requirement | Tests | Detection |
|-------------|-------|-----------|
| PMU | mem_race, futex_wait_parent, scheduling_fairness | `/proc/sys/kernel/perf_event_paranoid <= 1` |
| CPUID Interception | RDRAND/RDSEED tests | KVM available |
| CPU Features (RDRAND/RDSEED) | rdrand_basic, rdseed_basic | `/proc/cpuinfo` flags |
| rr | rr_suite, record_replay_matrix | `rr` in PATH |
| KVM | QEMU L2 boot | `/dev/kvm` accessible |
| DynamoRIO | DBI backend | `DYNAMORIO_HOME`, `HERMIT_DRRUN` env vars |

Tests requiring unavailable hardware are **skipped with a clear reason**, never silently passed (unless `--silently-pass-hardware-tests` is used).

## Output Formats

### Human (default)
```
Hermit Integration Test Matrix Report
=====================================

Summary: 45 total, 38 passed, 2 failed, 3 skipped, 2 hw_skipped, 0 xfail, 0 xpass

category       program                mode         result   time  detail
--------------------------------------------------------------------------
basic          echo                   default      PASS     100ms exit=Some(0), output=match
basic          ls                     default      PASS     120ms exit=Some(0), output=match
determinism    nanosleep              strict       PASS     200ms exit=Some(0), output=match
threading      futex_and_print        chaos        PASS     150ms exit=Some(0), output=match
...

FAILURES:

nginx (default):
stdout:
stderr:
nginx: configuration file /tmp/.../nginx.conf test failed

HARDWARE SKIPPED:

  mem_race (default): PMU access not available [Pmu]
  futex_wait_parent (strict): PMU access not available [Pmu]
```

### JSON
```json
{
  "total": 45,
  "passed": 38,
  "failed": 2,
  "skipped": 3,
  "hardware_skipped": 2,
  "expected_fail": 0,
  "unexpected_pass": 0,
  "total_time_seconds": 12.5,
  "categories": [
    {
      "category": "basic",
      "tests": [
        {
          "name": "echo",
          "mode": "Default",
          "status": "Pass",
          "duration_ms": 100,
          "detail": "exit=Some(0), output=match",
          "diagnostic": null,
          "hardware_requirement": "None"
        }
      ]
    }
  ]
}
```

### JUnit XML
```xml
<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="hermit-integration-matrix" tests="45" failures="2" time="12.5">
  <testcase classname="hermit.integration.basic" name="echo::Default" time="0.1">
  </testcase>
  <testcase classname="hermit.integration.expected-fail" name="nginx::Default" time="0.05">
    <failure message="exit code 1">stdout:
stderr:
nginx: configuration file test failed</failure>
  </testcase>
</testsuite>
```

## Coverage Manifest

The `--generate-manifest` option produces a TSV mapping internal Buck scenarios to Cargo-native port status:

```tsv
# Hermit Integration Test Coverage Manifest
# Maps internal Buck scenarios to Cargo-native port status

buck_scenario	cargo_test	status	hardware_req	notes
hermit/tests/echo	echo::default	ported	None	
hermit/tests/echo	echo::strict	ported	None	
hermit/tests/echo	echo::chaos	ported	None	
hermit/tests/echo	echo::virtual_time	ported	None	
hermit/flaky-tests/hello_race	hello_race::default	ported	None	
hermit/flaky-tests/hello_race	hello_race::chaos	ported	None	
...
rr/full_suite	rr_suite	excluded	rr	Requires rr recordings
qemu/l2_boot	qemu_l2	excluded	kvm	Requires QEMU + kernel
```

Status values:
- `ported` - Test is ported and runs in Cargo
- `pending_hardware` - Test is ported but requires unavailable hardware
- `excluded` - Intentionally excluded (requires Meta-internal infrastructure)

## CI Integration

Add to your CI workflow:

```yaml
- name: Run Integration Matrix
  run: |
    cargo build --workspace
    cargo test -p hermit --test integration_runner -- --output-format junit --output-file junit.xml
```

## Local Development Workflow

1. **Before committing**: Run local validation
   ```bash
   ./target/debug/hermit integration --local-validation
   ```

2. **During development**: Run specific tests
   ```bash
   ./target/debug/hermit integration --filter futex --mode strict
   ```

3. **Before PR**: Run full matrix (if hardware available)
   ```bash
   ./target/debug/hermit integration --output-format junit --output-file results.xml
   ```

4. **Check coverage**: Generate manifest
   ```bash
   ./target/debug/hermit integration --generate-manifest > manifest.tsv
   ```

## Implementation Details

The integration runner consists of several modules in `hermit-cli/tests/runners/`:

- **matrix_discovery.rs** - Discovers guest programs and generates test matrix
- **matrix_executor.rs** - Executes tests with timeout and process management
- **matrix_reporting.rs** - Generates human-readable, JSON, and JUnit reports
- **hardware_detection.rs** - Detects hardware capabilities and classifies tests
- **cargo_integration_runner.rs** - Main orchestration and CLI entry point

## Extending the Test Matrix

To add new guest programs:

1. Add Rust binaries to `tests/Cargo.toml` or `flaky-tests/Cargo.toml`
2. Add shell scripts to `tests/standalone/` or `tests/shell/`
3. The runner will automatically discover them

To add new test categories or modes, modify the `categorize_program` and `applicable_modes` functions in `matrix_discovery.rs`.