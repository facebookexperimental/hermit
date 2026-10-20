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
