# Authentic pre-facility Chosen recording

`chosen-before-reserved-input.json` is the complete, unedited output of Hermit
commit `49487f73effaff0c94c0568ef4bdafde9cc5df7e`, linked to Reverie
`0c35d21fca4485fac3194071b13b6e34f6573f1b` before reserved-input provenance existed.
It was captured on x86_64 Linux on 2026-10-30. Its 96 events describe `/bin/true`
with Chosen base 5000, strict host-ID refusal, and JSON format 3. It has no
`reserved_input` member. It is a refusal fixture, never a new-head replay positive.

- Recording SHA-256: `7a82e4fd61918aa71ff1b21b0165f05363642caaaf8d55ce64f02d22bb057462` (12,467 bytes).
- Producing ELF SHA-256: `6cd1d12a797e75b6379e7c91d0c4490dbff0977410259e5d5b4d15249235d5e4`.
- Guest ELF SHA-256: `b9b6ec37ac996b4152caca5a87e5ae23abb649bbc82dc6e1ca1d423d7bf4c171`.

The capture ran under the existing safehermit adapter in an isolated user,
PID, mount and network namespace with an 8 GiB memory bound and a 180-second
outer deadline. The retained runtime receipt confirms exit 0, the exact ELF
hash before and after, IN_PLACE safehermit operation and no output truncation.
Its Hermit arguments were:

```
run --no-namespace --base-env=minimal --pid-base=5000
--max-timeslice=disabled --epoch=2026-01-01T00:00:00Z --summary
--record-preemptions-to=<output.json> -- /bin/true
```

The full argv, sanitized environment, original output and runtime receipt are
retained in the parent repository's
`ignored/pr-drain/reports/fix-4864-guestids-2026-10-30/legacy-chosen-fixture-49487-capture.json`.
That old-head capture provides provenance for these bytes, not evidence that
the new admission implementation passed a test.
