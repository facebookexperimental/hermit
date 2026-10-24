/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Refusing to start a guest under an inherited seccomp filter
//! (<https://github.com/rrnewton/hermit/issues/3942>).
//!
//! A seccomp filter installed by whatever launched hermit (a container
//! runtime's default profile, a service manager's system-call restriction) is
//! inherited by hermit and by every guest, and cannot be removed. It can make
//! system calls fail in ways the guest observes, and hermit records nothing
//! about it, so the same command can behave differently inside and outside the
//! container. Hermit therefore refuses to start a guest when
//! `/proc/self/status` reports such a filter, unless the invocation passes
//! `--unsafe-ignore-host-seccomp`. That override is recorded: a warning on
//! stderr and an INFO record in the log and the run evidence.
//!
//! Declaring an acknowledged host filter in the run config is tracked in the
//! same issue and is not implemented.

use std::sync::OnceLock;

use anyhow::Context;
use hermit::Error;

/// The seccomp state hermit inherited, as `/proc/self/status` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostSeccomp {
    /// The `Seccomp:` field: 1 for strict mode, 2 for filter mode.
    pub(crate) mode: u32,
    /// The `Seccomp_filters:` field, which Linux reports since 5.9.
    pub(crate) filters: Option<u32>,
}

impl HostSeccomp {
    /// The inherited seccomp state described by the text of a
    /// `/proc/<pid>/status` file, or `None` when nothing is inherited
    /// (`Seccomp: 0`, or no `Seccomp:` field on a kernel without seccomp).
    pub(crate) fn from_status(status: &str) -> Result<Option<HostSeccomp>, Error> {
        let field = |name: &str| -> Result<Option<u32>, Error> {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
                .map(|value| {
                    value.trim().parse::<u32>().with_context(|| {
                        format!("the {name} field of /proc/self/status is not a number: {value:?}")
                    })
                })
                .transpose()
        };
        let mode = field("Seccomp")?;
        let filters = field("Seccomp_filters")?;
        Ok(match mode {
            None | Some(0) => None,
            Some(mode) => Some(HostSeccomp { mode, filters }),
        })
    }

    fn read() -> Result<Option<HostSeccomp>, Error> {
        let status = std::fs::read_to_string("/proc/self/status")
            .context("cannot read /proc/self/status to check for an inherited seccomp filter")?;
        Self::from_status(&status)
    }

    /// `Seccomp: 2 (filter mode), Seccomp_filters: 1`.
    pub(crate) fn describe(&self) -> String {
        let mode = match self.mode {
            1 => " (strict mode)",
            2 => " (filter mode)",
            _ => "",
        };
        let filters = match self.filters {
            Some(filters) => format!(", Seccomp_filters: {filters}"),
            None => String::new(),
        };
        format!("Seccomp: {}{mode}{filters}", self.mode)
    }
}

/// The inherited filter this invocation started a guest under, through
/// `--unsafe-ignore-host-seccomp`. Read when tracing starts, so that every
/// log the run writes records it.
static UNVERIFIED: OnceLock<HostSeccomp> = OnceLock::new();

/// The inherited filter a guest of this invocation runs under, if
/// `--unsafe-ignore-host-seccomp` admitted one.
pub(crate) fn unverified() -> Option<HostSeccomp> {
    UNVERIFIED.get().copied()
}

/// Checks hermit's own seccomp state before a guest is started: refuses an
/// inherited filter as a policy refusal, or, with `unsafe_ignore`, warns and
/// records it.
pub(crate) fn admit(unsafe_ignore: bool) -> Result<(), Error> {
    if let Some(host) = decide(HostSeccomp::read()?, unsafe_ignore)? {
        // Best effort, like hermit's other warnings: a closed or full stderr
        // must not turn an admitted run into a failure. The INFO record
        // written when tracing starts is the durable trace.
        crate::tracing::write_stderr_diagnostic(&format!(
            "WARNING: --unsafe-ignore-host-seccomp: hermit is starting the guest under a seccomp \
             filter it inherited ({}). Hermit neither records nor enforces that filter, so this \
             run may not reproduce wherever the filter differs, for example on a host without \
             it.\n",
            host.describe()
        ));
        let _ = UNVERIFIED.set(host);
    }
    Ok(())
}

/// The admission decision for an inherited seccomp state: `Ok(None)` when
/// nothing is inherited, `Ok(Some(..))` when `unsafe_ignore` admits it, and a
/// policy refusal otherwise.
fn decide(host: Option<HostSeccomp>, unsafe_ignore: bool) -> Result<Option<HostSeccomp>, Error> {
    match host {
        None => Ok(None),
        Some(host) if unsafe_ignore => Ok(Some(host)),
        Some(host) => Err(Error::new(crate::container::PolicyRefusal).context(format!(
            "hermit inherited a seccomp filter ({}) from whatever launched it, for example a \
             container runtime's default profile, and refuses to start the guest. The filter is \
             an input hermit does not record: it can make system calls fail in ways the guest \
             observes, so the run would not reproduce wherever the filter differs. Run hermit \
             where no filter is inherited; a filter cannot be removed from inside a running \
             container, so for Docker create the container with \
             `--security-opt seccomp=unconfined`. Or accept a run that may not reproduce by \
             passing --unsafe-ignore-host-seccomp (in a run config: \
             `global: {{unsafe-ignore-host-seccomp: true}}`).",
            host.describe()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "Name:\thermit\nNoNewPrivs:\t1\nSeccomp:\t2\nSeccomp_filters:\t3\n";

    #[test]
    fn the_status_fields_are_read() {
        assert_eq!(
            HostSeccomp::from_status(STATUS).unwrap(),
            Some(HostSeccomp {
                mode: 2,
                filters: Some(3)
            })
        );
        assert_eq!(
            HostSeccomp::from_status("Seccomp:\t0\nSeccomp_filters:\t0\n").unwrap(),
            None
        );
        // A kernel without seccomp reports neither field.
        assert_eq!(HostSeccomp::from_status("Name:\thermit\n").unwrap(), None);
        // Before Linux 5.9 there is no filter count.
        assert_eq!(
            HostSeccomp::from_status("Seccomp:\t1\n").unwrap(),
            Some(HostSeccomp {
                mode: 1,
                filters: None
            })
        );
        let error = HostSeccomp::from_status("Seccomp:\tfilter\n").unwrap_err();
        assert!(format!("{error:#}").contains("not a number"), "{error:#}");
    }

    #[test]
    fn an_inherited_filter_is_a_policy_refusal_that_names_the_remedies() {
        let host = HostSeccomp::from_status(STATUS).unwrap();
        let error = decide(host, false).unwrap_err();
        assert!(
            error.root_cause().is::<crate::container::PolicyRefusal>(),
            "{error:#}"
        );
        let message = format!("{error:#}");
        for expected in [
            "Seccomp: 2 (filter mode), Seccomp_filters: 3",
            "an input hermit does not record",
            "--security-opt seccomp=unconfined",
            "--unsafe-ignore-host-seccomp",
            "global: {unsafe-ignore-host-seccomp: true}",
        ] {
            assert!(message.contains(expected), "{expected:?} in {message}");
        }
    }

    #[test]
    fn the_unsafe_override_admits_and_reports_the_filter() {
        let host = HostSeccomp::from_status(STATUS).unwrap();
        assert_eq!(decide(host, true).unwrap(), host);
        assert_eq!(decide(None, false).unwrap(), None);
        assert_eq!(decide(None, true).unwrap(), None);
    }
}
