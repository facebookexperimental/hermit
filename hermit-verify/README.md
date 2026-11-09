# hermit-verify

Verification harness for Hermit runs, records and schedules. Verification levels and retained evidence are described in the Hermit user guide.

This crate is part of [Hermit](https://hermetic-infra.org), an execution
engine for reproducible Linux testing. For installation and run/record/replay
examples, see the [hermit-run README](https://crates.io/crates/hermit-run).

- [API documentation](https://docs.rs/hermit-verify)
- [User guide](https://github.com/rrnewton/hermit/blob/main/docs/USER_GUIDE.md)
- [2022 introduction: Hermit deterministic Linux testing](https://developers.facebook.com/blog/post/2022/11/22/hermit-deterministic-linux-testing/)

Supported execution targets are x86_64 Linux. The CLI selects the ptrace and
KVM execution backends; libraries in this package do not select third-party
execution backends. See the CLI README for native build prerequisites, namespace
policy, kernel qualification and performance-counter requirements.

## License

BSD-3-Clause. See `LICENSE`.

Schedule interpolation preserves a complete input record when the requested
order is exactly that input's order. Both inputs must use the same timing
format. A genuinely mixed order is refused when its recorded time jumps or
callback checkpoints have no established mapping to the edited events, or
when the remaining recording metadata differs. Replay an unchanged endpoint,
or record compatible inputs before editing their order.

Analyze and bisect likewise require all source schedules to use one timing
format: legacy, format 2 and format 3 have different jump/switch-in charging
rules. They refuse mixed formats before writing a trial instead of selecting
an older format and silently changing its clock behavior. Record both inputs
with the same Hermit version and timing format. Same-format legacy schedules
without jumps and current-format schedules remain supported.
