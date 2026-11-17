# Per-syscall Detcore pages: schema `syscall-pages/v3` (normative)

One page per x86-64 syscall in Detcore's own census lives at
`docs/syscalls/<name>.md`. The census is not a number in this file: the
tool derives it from `detcore/src/syscall_classification.rs` in the tree
being checked (the reviewed name lists plus the compile-time
`EXPECTED_SYSNO_COUNT` and last-id assertions), so a pinned-table bump
changes the required page set and `check --quick` fails until pages are
regenerated. A page records code-derived facts: Detcore's class,
dispatch, deterministic-refusal and subscription state for that
syscall, one generated strategy sentence, source citations, a
discussion link, and an append-only status log. A page may additionally
carry checked analysis prose (the Determinism strategy section below),
each claim verified against Detcore at the revision in the page's
`prose.based_on`, with hash-anchored citations that the tool recomputes
in the tree the page ships in.

Schema v3 is additive over v2. A `syscall-pages/v2` page (no `prose`
block, no Determinism strategy section) stays valid indefinitely and is
never mass-rewritten; a page becomes v3 in the batch that adds its
checked prose. Everything v2 required of facts, generated outputs, and
public text still holds.

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

YAML front matter, then the level-2 headings in order (P02). A
facts-only page has exactly `## Strategy`, `## Discussion`, and
`## Status`. A page with checked prose reads summary-first, below;
its front-matter prose block and its prose sections are present or
absent together (P36):

| heading | content |
|---|---|
| `## In short` | Three or four plain sentences: what is nondeterministic natively, what Hermit does, and a generated known-gap count (P45); prose pages only |
| `## Strategy` | The generated strategy sentence (a pure function of `code_facts`, P35) and its source citations; facts-only pages only (prose pages carry it under Appendix) |
| `## Determinism strategy` | Checked prose parts 1-4 of the rubric below, with small citation markers (P36–P44); prose pages only |
| `## Known gaps` | Rubric part 5, as its own section; prose pages only |
| `## Discussion` | The one discussion link for this syscall (P34) |
| `## Appendix` | Exactly `### Code facts` (the generated strategy sentence and fact citations), `### Tests` (rubric part 6), `### Citations` (the footnote definitions); text before the first subsection, unknown subsections, and duplicates are refused (P02); prose pages only |
| `## Status` | Dated, append-only log (P12, P13) |

## Front matter

```yaml
---
schema: syscall-pages/v3
syscall: read
nr_x86_64: 0
related: []
discussion:
  kind: discussion/new
  url: https://github.com/facebookexperimental/hermit/discussions/new?category=syscalls&title=...
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: <40-hex source revision>
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
# prose: human/agent-authored, machine-checked (syscall-pages.rs check); v3 only
prose:
  status: checked
  reviewed_at: 2026-11-16
  based_on: <40-hex source revision>
  passthrough_risk: none
---
```

Exactly these keys (the `prose` block is the only optional one, and
`passthrough_risk` is optional inside it). An unknown, missing, or
duplicated key fails (P01, P05). `syscall` equals the file name and a
Detcore census name (P06, census check). `related` holds full public
GitHub URLs only (P08). `code_facts` is tool-owned: only `generate`
and `facts --apply` write it. `dispatch` is `typed`, `sysno-guard`,
`pred:is_<fn>`, `passthrough`, `unsupported`, or `fallback` (P09);
`fallback` for a determinized syscall fails `check`. The dispatch and
census parsers report `parser-failed` on any Detcore shape they do not
understand, never a pass (P27).

The two revision stamps age independently, by design. `code_facts.as_of`
(tool-owned) moves whenever facts regenerate; `prose.based_on` and
`prose.reviewed_at` move only when the prose claims are re-verified.
A page whose prose stamp lags its facts stamp is visibly stale prose,
which waves report; it is not a lint error. `prose.status` is
`checked` (the only value; unchecked prose does not ship, so absence
of the block is how "no checked prose" is represented).
`passthrough_risk` records the reviewed triage of a passthrough page
(`none`, `host-state`, `identity`, or `timing`) and is an error on any
other strategy (P36). There is no author or reviewer field anywhere
in front matter (P30).

The strategy shown in `syscalls.json`, the index, and the site derives
only from code facts: `determinized`, `determinized-refused`,
`passthrough`, or `unsupported`.

## Checked prose: the rubric

Checked prose is a high-level summary that complements the code,
never a transcription of it: name mechanisms, not registers, fields,
or lock orders. It has six parts in order, each opened by its exact
bold lead-in (P37). Parts 1-4 live in `## Determinism strategy`,
part 5 is the `## Known gaps` section, and part 6 is `### Tests` in
the Appendix; citations are small markers in the text whose
definitions live in `### Citations`.

1. **What is nondeterministic natively.** — the entropy, time,
   host-state, or scheduling source, and which outputs (return value,
   buffers, errno, blocking behaviour) it can perturb.
2. **What Hermit does.** — determinize (what virtual substitute or
   serialization makes the result a function of guest state and
   configuration), refuse (which errno, and whether unconditionally
   or only fail-closed), or pass through (which Hermit invariant
   makes that safe). The first sentence must name the strategy the
   code facts derive (P40).
3. **Interface fidelity.** — every documented flag or option is
   *honored*, *coalesced* (accepted, with another mode's result; say
   so), or *rejected* (say the errno); plus partial-completion and
   restart/interruption behaviour where the handler can fault or
   block.
4. **Record & replay.** — what replay does with this syscall
   (regenerate from named model state, replay a recorded value, or
   refuse identically), consistent with the subscription facts (P42).
5. **Known gaps.** — one bullet each, stable ids `G1`, `G2`, … (append,
   never renumber), each marked *by design*, *unimplemented*, or
   *unmeasured*. A refused syscall's gaps say what a guest can do
   instead, or that there is no fallback.
6. **Tests.** — named tests evidencing the parts above: a backticked
   `.rs` path (which must exist) followed by at least one backticked
   test name (which must be a function in that file, P43), each saying which rubric
   part it evidences. Non-Rust evidence ids (manifest cells) are
   named as text, not machine-checked. Recorded tests are evidence,
   not machine-verified coverage (see Done). P43 additionally
   requires relevance: the test's brace-matched extent (plus one
   level of same-file helpers it calls), scanned with comments and
   strings masked, must mention the syscall as a whole word
   (`Sysno::<name>`, `SYS_<name>`, `handle_<name>`, the CamelCase
   type, or `libc::<name>(`), or call a credited function — one the
   page's own cited ranges call that is actually defined in the
   cited file and is not a prelude/std shape. The brace matcher
   handles comments, nested block comments, and ordinary strings;
   it does not handle char literals containing braces or quotes, or
   raw strings containing quotes, and a body-less declaration has no
   extent. None of those occur in the tests cited today. The rule
   proves the test touches this syscall's code, not that its
   assertions check the page's claims; that last step stays a
   review check, stated plainly so the machine part is not mistaken
   for the whole proof.

Short forms: a passthrough page triaged `passthrough_risk: none`
needs only parts 2, 5, and 6 (any optional part present keeps the
order). Determinized-refused pages keep all six parts but part 2 is
expected to be two or three sentences; family text may be shared only
where the cited predicate and dispatch arm are literally the same
code.

### Citations (P38)

Every paragraph of the section, and every Known gaps bullet, carries
at least one footnote citation. A code citation footnote looks like:

```text
[^c1]: detcore/src/lib.rs:<line> sha256:<16 lowercase hex digits>
  https://github.com/facebookexperimental/hermit/blob/main/detcore/src/lib.rs#L<line>
```

One footnote may cite several ranges (`<path>:<a>-<b> sha256:<h>; …`).
The hash is the first 16 lowercase hex digits of the SHA-256 of the
cited lines' exact stored bytes (lines `a` through `b` inclusive,
each with its trailing newline as stored). Exact `check`
recomputes every hash against the tree being checked; the committed
(hard) mode instead compares citations with `citations.json` and
reads nothing under `detcore/`. A mismatch — code moved, tree
differs, lines stale — fails the mode whose inputs changed. Line
numbers alone are not citations: they drift between trees, hashes
do not. The optional GitHub link under a citation is a human-facing
copy of its range and must repeat that footnote's citation range
exactly (P46), in every mode including committed.
Definitional statements about Linux cite the `[^linux]` footnote (a
public link, no hash) instead; that link must pin a kernel tag (for
example `?h=v7.1.3`), never a moving branch (P44), so the Linux
behaviour being claimed stays checkable. Every footnote must be
referenced and every reference defined. Citation paths stay under
`detcore/` in this tree; absolute paths and `..` escapes are refused.

### Numbers (P39)

Checked prose contains no hand-typed numbers that could fossilize.
Outside footnote definitions, code spans, and link targets, a token
containing a digit is refused unless it is a date (`YYYY-MM-DD`), a
gap id (`G1`), the architecture name (`x86_64`/`x86-64`), or the
syscall's own number. Quantities a reader needs are already in the
generated strategy sentence and the facts table; counts inside prose
are spelled as claims under citation, not as bare digits.

### Cross-checks (P40–P42)

Prose is held to the page's own code facts mechanically, not only by
review:

- P40: part 2's first paragraph names the derived strategy
  (`determiniz…`, `refus…`, `pass…`/`forward…`, or `unsupported`).
- P41: an errno named as a positive claim in parts 2–3 must appear
  inside some line range this page cites. Errnos inside a negative
  claim ("there is no `EINTR` path") are exempt — they are claims of
  absence, collected by `report` for the reviewer instead.
- P42: the subscription phrases are held to the facts: claiming the
  syscall is "not subscribed" requires both subscription facts to be
  false; claiming it is "subscribed in both" modes requires both to
  be true.

## Status entries

```text
- 2026-11-15 · rev 0123456789ab · <text>
```

One bullet per entry (P12), at least one entry, dates never decrease
(P13). There is no author or agent field. Entries are append-only.
Coverage is not recorded in these pages at all; see the linked audit
above. Recorded test evidence lives in the rubric's Tests part (P43),
not in Status; a Status entry notes when prose was added or
re-verified, and at which revision.

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

Page-era amendment (recorded project decision, 2026-11-15): the
`ptrace_verification` field and the ptrace-in-CI machine check were
removed when per-backend status was dropped from these pages. That is
a later project decision, not a restatement of the rule above, and no
generated page text may present amended wording under the owner's
label. Facts-only v2 pages carry no done state. When checked prose
returns, its done criteria must either meet the rule above as written
or record a newer owner ruling in this section.

Page-era done (recorded project decision, 2026-11-16, recorded here
as the newer ruling the paragraph above requires; subject to owner
override): a syscall page is done when its checked prose is
rubric-complete (P37), every citation hash recomputes in the tree the
page ships in (P38), its Tests part records named tests (P43), every
claim — positive and negative — has been checked against the cited
code in review (the `report` command enumerates them), and every
Known gaps item cites the code that shows the gap. Facts-only pages
carry no done state. This is a project decision like the amendment
above; no generated page text may present it under the owner's
2026-10-02 label.

Also not ported from the predecessor: its C08 rule resolving a
recorded coverage token to a strict-container test. Until that check
exists, no page may claim machine-verified test coverage. The
page-era done criteria above therefore record test evidence; they do
not assert C08-style verified coverage.

## Discussion

Every page carries one discussion link (P34). Newly generated pages
use `discussion/new`: a prefilled GitHub Discussions form in the
`syscalls` category, titled `Syscall discussion: <name>`. Its sparse
placeholder links this page, names the facts revision embedded when
the link was generated, and asks what the reader wants to discuss; it
does not recapitulate the page. `discussion/existing` links a
specific facebookexperimental/hermit discussion. The earlier `new`
and `existing` spellings remain supported for prefilled and existing
GitHub issues, including adoption of a human-filed issue. Prefilled
forms post and file nothing until a human submits them; tooling never
creates discussions or issues. A `discussion/new` link is meaningful
only once GitHub Discussions and the `syscalls` category are enabled;
generation itself never contacts GitHub.

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
`docs/site/syscalls/` are generated and must not be hand-edited. So
is `docs/syscalls/citations.json`, the committed citation snapshot
written only by `anchor` (from live Detcore, after a facts refresh):
the hard gate checks pages against it and reads nothing under
`detcore/`. Exact checks (`check --quick` and full `check`) rebuild
that snapshot from live Detcore and require a byte-for-byte match,
so a hand edit (for example an added cited errno) or a malformed
file fails. The hard gate separately refuses snapshot entries for
missing or prose-less pages and any entry whose `based_on` differs
from its page's `prose.based_on`.
`check` compares all of them byte for byte (including orphan rendered
files) when the rendered tree exists, and scans them under P30. The
internal validation tree builds this tool unchanged and runs it as
two tests over a declared snapshot of these trees. The hard tier,
`check.syscall_pages`, runs `check --committed`: its inputs are the
pages, the generator, and the committed facts, so it fails on an
inconsistent page, a stale prose citation hash, or a stale generated
output — and an unrelated Detcore change cannot fail it. The soft
tier runs `check --quick`, the full derivation against live
Detcore, and reports every drift as a named warning in its log,
always exiting 0. Drift warnings are cleared by scheduled
facts-refresh diffs (monthly, and whenever syscall policy changes;
owner: the maintainers), and semantic drift
— a class, dispatch, refusal, or subscription change, as opposed to
a line move — additionally gets a filed note from the maintainers,
so nothing goes quietly stale.

`syscalls.json` is `syscalls.json/v3` once any page carries checked
prose: one row per page with the code facts, the derived strategy,
the discussion link, and the prose stamps (`prose_status`,
`prose_based_on`, `prose_reviewed_at`, `passthrough_risk`), plus
strategy and prose counts. A generation with no checked prose
anywhere emits the exact `syscalls.json/v2` document, and index/site
prose chrome likewise appears only once some page has prose — the
staging rule that lets a tool change land with generated outputs
untouched. Coverage numbers never appear in it (owner scope rule).

| command | effect |
|---|---|
| `lint` | schema, sections, strategy sentence, checked-prose rules (P36–P46), Status, discussion, public text |
| `generate` | (re)generate code facts from this tree's Detcore sources, optionally overlaying an exporter TSV; preserves any checked prose, discussion link, and Status log |
| `facts [--apply] [--date D]` | print or apply exporter facts (git checkout with `reverie/` only) |
| `aggregate [--check]` | regenerate or verify `syscalls.json` and the index |
| `render [--check]` | regenerate or verify the static site, deleting orphans |
| `check [--quick|--committed]` | lint + census + dispatch + citations (incl. prose citation hashes) + freshness; full mode adds exporter fact equality; `--committed` derives nothing from live Detcore and checks citations against `citations.json` (the hard CI tier) |
| `anchor` | rewrite `citations.json` from live Detcore after a facts refresh; refuses while lint has errors |
| `report` | per-page prose status for review: the full claim inventory, negative claims verbatim, passthrough triage, stamp lag; never gates |
| `wave --from A --to B` | worklist for an update wave over two revisions of a git checkout at B: facts-changed, handler-touched, and citation-stale syscalls with reasons; reports only (apply with `facts --apply` / `generate`) |

## Rule index

| id | rule |
|---|---|
| P01 | front matter parses as the fixed subset, no duplicate keys |
| P02 | the level-2 headings for the page's prose state, once, in order |
| P03 | Strategy non-empty |
| P05 | exactly the schema keys; `schema` is `syscall-pages/v2` or `syscall-pages/v3` |
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
| P36 | prose block (v3 only) and Determinism strategy section present together; stamps well-formed; `passthrough_risk` only on passthrough pages |
| P37 | rubric parts present, in order, non-empty (short form for `passthrough_risk: none`) |
| P38 | every prose paragraph cited; citation hashes recompute in this tree; references and definitions match |
| P39 | no hand-typed numbers in checked prose outside the allowed forms |
| P40 | "What Hermit does" names the code-fact-derived strategy |
| P41 | a positively-claimed errno appears inside a cited line range |
| P42 | subscription claims agree with the subscription facts |
| P43 | Tests part: cited `.rs` files exist, each is followed by at least one named test, and each name is a function in that file |
| P44 | the `[^linux]` footnote pins a kernel tag, never a moving branch |
| P45 | `## In short`: 3-4 plain sentences, no citations or hand-typed numbers, ending with the generated known-gap count sentence |
| P46 | a GitHub source link in a citation footnote repeats that footnote's own citation range exactly |
