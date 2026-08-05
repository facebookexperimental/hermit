---
name: ux-tester
description: "Tire-kick a tool, command, UI, report, demo, or workflow as a first-time user and judge output quality, not only exit status. Use whenever validating that something works, especially help text, CLI output, generated artifacts, or user-facing behavior."
---

# UX Tester

Act like an intelligent first-time user. Actually use the thing through its
public entrypoint, read what it produces, and decide both whether it functions
and whether the experience makes sense.

## Run It For Real

1. Start from the documented entrypoint and a realistic clean state.
2. Perform the primary user action. Also try the obvious discovery path such as
   `-h`, `--help`, an empty state, or the first screen.
3. Capture the exit status and the user-visible stdout, stderr, files, or UI.
4. Read the complete result. Do not treat output as an opaque string whose only
   requirement is that the process exited zero.

Use first-time-user knowledge, not implementation trivia. If the workflow only
works after guessing an undocumented flag, internal path, or hidden setup step,
that is a UX problem even when the underlying code works.

## Read Critically

Flag anything a reasonable user would find:

- broken, incomplete, contradictory, or misleading;
- random, nonsensical, irrelevant, or unexpectedly verbose;
- ugly, confusing, poorly ordered, clipped, or misformatted;
- contaminated with debug logs, source comments, audit markers, stack traces,
  internal paths, hostnames, implementation jargon, or other leaked internals;
- inconsistent with the command, label, documentation, examples, or surrounding
  product conventions.

Check spelling, alignment, headings, units, defaults, examples, error recovery,
and whether the next action is clear. A technically accurate result can still
be unusable.

## Report Two Verdicts

Always report these separately:

```text
Works? YES | NO | PARTIAL - <what actually ran and whether it completed>
UX sensible? YES | NO | PARTIAL - <whether a first-time user gets a clear,
                                   coherent, appropriately formatted result>
```

Then list the exact command or action, observed output, and every problem. Keep
observed facts separate from hypotheses about the cause.

## File Every Problem

Create one TaskGraph task per independent problem; do not bury defects only in
chat or combine unrelated symptoms into one task. Search first and reuse an
existing task for the same defect. Use a symptom-focused title and include
reproduction steps, expected behavior, actual behavior, user impact, evidence,
and a concrete acceptance check. Choose honest impact, effort, priority, tags,
and parent/project relationships, for example:

```bash
tg add "Help output leaks internal source comments" \
  --impact 50 --effort 0.5 --tags ux,bug \
  --blocks <owning-task-or-goal> \
  --description "Repro: ... Expected: ... Actual: ... Acceptance: ..."
```

Report the created task IDs beside the corresponding findings. If no problem is
found, say so explicitly; do not invent work to satisfy the checklist.

## Canonical Catch

`validate.sh -h` once exited successfully but printed every comment from the
script, including `TODO-HUMAN-REVIEW` and `AUTONOMOUS-BOT-IMPLEMENTED` markers.
A pass/fail test saw exit 0. A UX test read the output and correctly reported:

```text
Works? YES - the help path runs and exits zero.
UX sensible? NO - the output is a source-comment dump with leaked internals,
                  not a concise help page.
```

That distinction is the purpose of this skill.
