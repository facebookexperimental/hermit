# Per-syscall Detcore pages: schema `syscall-pages/v2` (normative)

One facts-only page per x86-64 syscall in Detcore's own census lives at
`docs/syscalls/<name>.md`. The census is not a number in this file: the
tool derives it from `detcore/src/syscall_classification.rs` in the tree
being checked (the reviewed name lists plus the compile-time
`EXPECTED_SYSNO_COUNT` and last-id assertions), so a pinned-table bump
changes the required page set and `check --quick` fails until pages are
regenerated. A page records code-derived facts only: Detcore's class,
dispatch, deterministic-refusal and subscription state for that
syscall, one generated strategy sentence, source citations, a
discussion link, and an append-only status log. Checked analysis prose
is not in these pages yet; it returns in batches, each paragraph
checked against Detcore with a `file:line` citation that resolves in
the tree the page ships in.

`scripts/syscall-pages.rs lint` and `check` enforce every rule below
that has an id.

Scope rules:

- Pages track Detcore strategy only. Which backends can run a syscall
  is orthogonal and is not recorded here; there are no per-backend
  fields.
- Test coverage is a separate concept. It is linked, never merged:
  generated indexes link the
  [test-coverage measurement](https://github.com/facebookexperimental/hermit/blob/main/docs/SYSCALL_TEST_COVERAGE.md)
  (not `SYSCALL_COVERAGE.md`, which audits trap dispositions),
  and no page, JSON row, or table column carries a coverage count.
- Facts on a page were exported or derived at the revision in its
  `code_facts.as_of`. Every Status log carries an entry at that
  revision, and all pages in one generation share it. The full facts
  export (`facts`, full `check`) needs a git checkout with the
  `reverie/` submodule and proves exact syscall numbers and
  refused/subscription values there. The in-tree gate (`check
  --quick`, the Makefile target) proves census membership, classes,
  dispatch labels, citations, public text, and generated-output
  freshness from Detcore source alone; it runs in an fbsource tree
  as-is and cannot prove exact numbers or refusal/subscription values.

## Page layout

YAML front matter, then exactly three level-2 headings in order (P02):

| heading | content |
|---|---|
| `## Strategy` | The generated strategy sentence (a pure function of `code_facts`, P35) and its source citations |
| `## Discussion` | The one discussion link for this syscall (P34) |
| `## Status` | Dated, append-only log (P12, P13) |

## Front matter

```yaml
---
schema: syscall-pages/v2
syscall: read
nr_x86_64: 0
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=...
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: <40-hex source revision>
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
---
```

Exactly these keys. An unknown, missing, or duplicated key fails (P01,
P05). `syscall` equals the file name and a Detcore census name (P06,
census check). `related` holds full public GitHub URLs only (P08).
`code_facts` is tool-owned: only `generate` and `facts --apply` write
it. `dispatch` is `typed`, `sysno-guard`, `pred:is_<fn>`,
`passthrough`, `unsupported`, or `fallback` (P09); `fallback` for a
determinized syscall fails `check`. The dispatch and census parsers
report `parser-failed` on any Detcore shape they do not understand,
never a pass (P27).

The strategy shown in `syscalls.json`, the index, and the site derives
only from code facts: `determinized`, `determinized-refused`,
`passthrough`, or `unsupported`.

## Status entries

```text
- 2026-11-15 · rev 0123456789ab · <text>
```

One bullet per entry (P12), at least one entry, dates never decrease
(P13). There is no author or agent field. Entries are append-only.
Coverage is not recorded in these pages at all; see the linked audit
above. Future prose batches may reintroduce recorded test evidence in
Status under the Done section, with its own tooling.

## Done, and the owner's rule

The owner's 2026-10-02 rule for the predecessor issue system, quoted
verbatim from `syscall-tracking/v1` §6 so its text is preserved
exactly as written:

> Closed means `done`: every flag and option listed under
> `### Flags and options` is covered by a named test, recorded in Status as
> `T<n> covered by <path>`, or, for an error only the host's configuration
> can produce, by a `host-config` U item that cites where Hermit forwards the
> error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item
> does not count until Hermit hides that error or reports it as an
> infrastructure error; each such test exists at the cited hermit SHA and
> runs on ptrace in CI; the code realizes `target_policy`; and
> `ptrace_verification` is `verified`. If a problem is later found with this
> syscall, the issue reopens: `tracking_status` returns to `todo` or
> `in-progress` with a Status entry saying why.

Page-era amendment (coordinator ruling, 2026-11-15): the
`ptrace_verification` field and the ptrace-in-CI machine check were
removed when per-backend status was dropped from these pages. That is
a coordinator ruling, not a restatement of the rule above, and no
generated page text may present amended wording under the owner's
label. Facts-only v2 pages carry no done state. When checked prose
returns, its done criteria must either meet the rule above as written
or record a newer owner ruling in this section.

Also not ported from the predecessor: its C08 rule resolving a
recorded coverage token to a strict-container test. Until that check
exists, no page may claim machine-verified test coverage.

## Discussion

Every page carries one discussion link (P34). `existing` links a
facebookexperimental/hermit issue; `new` is a prefilled new-issue form
titled `Syscall discussion: <name>` and linking the page, filing
nothing until a human submits it. Tooling never creates issues.

## Public text (P30, P31)

These pages, `syscalls.json`, the generated index, and the generated
site are public output and are scanned by `check`. Refused: host
names and home, scratch, `ai_docs/`, `ignored/`, and private-workspace
paths or phrasing (the forms `scripts/check-private-workspace-refs.sh`
defines); internal domains; internal diff, task, and tracker ids
(`D…`, `P…`, `S…`, `T…`, `dh-…`, `tbh-…` and tracker words); agent,
lane, author, and model names. The dropped v1 key
`ptrace_verification` may appear only inside Status entries (P31).
Every refused class has a test in the tool that fails without its
check.

## Generated files and commands

`docs/syscalls/syscalls.json`, `docs/syscalls/README.md`, and
`docs/site/syscalls/` are generated and must not be hand-edited.
`check` compares all of them byte for byte (including orphan rendered
files) when the rendered tree exists, and scans them under P30.

| command | effect |
|---|---|
| `lint` | schema, sections, strategy sentence, Status, discussion, public text |
| `generate` | (re)generate facts-only pages from this tree's Detcore sources, optionally overlaying an exporter TSV |
| `facts [--apply] [--date D]` | print or apply exporter facts (git checkout with `reverie/` only) |
| `aggregate [--check]` | regenerate or verify `syscalls.json` and the index |
| `render [--check]` | regenerate or verify the static site, deleting orphans |
| `check [--quick]` | lint + census + dispatch + citations + freshness; full mode adds exporter fact equality |

## Rule index

| id | rule |
|---|---|
| P01 | front matter parses as the fixed subset, no duplicate keys |
| P02 | the three level-2 headings, once, in order |
| P03 | Strategy non-empty |
| P05 | exactly the schema keys; `schema` is `syscall-pages/v2` |
| P06 | `syscall` matches the file name |
| P07 | `nr_x86_64` is non-negative (range is the census check's) |
| P08 | `related` holds public GitHub URLs |
| P09 | code-fact values inside their enums; `as_of` is 40-hex |
| P12 | Status entry format; at least one entry |
| P13 | Status dates non-decreasing |
| P27 | census, dispatch, citations, and (full mode) exporter facts agree with this tree |
| P30 | no internal or host-specific text in pages or generated outputs |
| P31 | `ptrace_verification` only inside Status entries |
| P34 | discussion kind, URL shape, and section link |
| P35 | Strategy contains the exact code-derived strategy sentence |
