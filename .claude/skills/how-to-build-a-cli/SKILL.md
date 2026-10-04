---
name: how-to-build-a-cli
description: "Build or review command-line interfaces whose help, configuration, exit status, state claims, errors, and tests meet Hermit's owner requirements."
---

# How to Build a CLI

Apply the six requirements below when building or reviewing a CLI in this
project. These are the only normative requirements in this skill.

1. **Every command surface provides both help forms.** The top-level command,
   every subcommand, and every sibling binary must accept both `--help` and
   `-h`. Review this by invoking both forms on every surface, not only on the
   top-level command. Source: owner ruling, 2026-09-03.

2. **Flags are the primary interface.** Put user choices behind command-line
   flags. An environment variable may supplement a flag only where plumbing a
   flag through is genuinely hard, but it must not be the only discoverable
   interface and its behavior must be documented in `--help`. Review the help
   against every supported environment supplement. Source: owner ruling,
   2026-09-03.

3. **Exit codes 126 and 127 are invocation failures.** Never report either code
   as success. Review the paths that launch another command and verify that an
   unexecutable command (`126`) and a missing command (`127`) remain failures in
   the CLI's status and output. Source: owner ruling, 2026-09-03.

4. **State-dependent claims are truthful.** A message about current or changed
   state must match the state the CLI actually established. Review each claim
   against the observed state, including the case where the attempted change
   fails. Source: owner ruling, 2026-09-03.

5. **Errors redirect the user to the working path.** An error must name the
   failed condition and give an actionable replacement or remedy. A refusal
   that only says what is wrong is incomplete. Review the exact error text by
   triggering each important failure: it must tell a well-intentioned user what
   to run, provide, or change next. Source: owner ruling, 2026-09-03.

6. **Test the happy path and the redirecting error path.** Exercise a normal
   successful invocation and the failures that should redirect the user.
   Verify both what the command reports and whether it succeeds or fails; a
   review of only the happy path is incomplete. Source: owner ruling,
   2026-09-03.
