# Deciding whether an open pull request is already landed

Read this before sweeping the open-pull-request queue. Across six sweeps of
three repositories on 2026-08-24/25, several reached a WRONG VERDICT FIRST and
were corrected only by a later check — including two that were about to be
published as closes. The checks below are therefore ordered, and none of them is
sufficient alone.

**The one outcome that loses something is a wrong "already landed".** Closing a
pull request whose work is not on main destroys it. Everything here is arranged
to make that error expensive to commit and cheap to catch. When the checks
disagree, the safe answer is "needs a human read", not "close it".

## The rule underneath every check: most wrong verdicts are UNCONTROLLED INPUTS, not bad reasoning

Every check in this document exists because somebody was wrong first. Reading
them back, the striking thing is what KIND of wrong they were. Very few were
faulty reasoning about correct information. Most were **correct reasoning about
an input nobody had verified** — a command whose output did not mean what its
shape suggested, a snapshot that had gone stale, a message taken at face value.

That distinction decides the countermeasure, and getting it backwards is why
these keep recurring. Faulty reasoning is answered by thinking harder. An
uncontrolled input is not: the agent was already being careful, and care applied
to a false premise produces a *more confident* wrong answer, not a safer one.
**An uncontrolled input can only be answered mechanically — by a check that
fails loudly, not by a rule someone has to remember.**

Worked from one night's errors, deliberately including the coordinator's,
because the failure is structural rather than a property of any one agent:

| what went wrong | kind | what care would have done |
|---|---|---|
| `git rev-parse` ECHOES an unresolvable argument to stdout, so a missing file read as "different blob" and a triage reported `absent=0` on every row | uncontrolled input | nothing — the output looked exactly like a sha |
| `gh-merge-verified 2>&1 \| tail -3` hid the tool's real exit path, and a **P0 was filed against a tool that was correct** | uncontrolled input | nothing — the visible lines were true |
| `hermit-install/build.rs` returns silently unless `PROFILE == "release"`, so a debug build stages no SaBRe and the backend reports "not found in the Hermit installation" | uncontrolled input | nothing — the error names a real, wrong cause |
| a LiteInst hypothesis about a `thread_local` conversion, refuted by the same agent's own earlier review of that conversion | faulty reasoning | this one, yes |
| a queue snapshot broadcast as live state; two of six "newly free" heads were already CLOSED | uncontrolled input | nothing — the list was accurate when taken |
| a duplicate assumed to be out of routing because it was closed | uncontrolled input | nothing — closed rows really are usually done |
| a fleet-wide instruction issued from a message that was never verified | uncontrolled input | nothing — the message was plausible |

Six of seven. The countermeasure in every one of those rows is a command, not a
resolution: read the exit status directly rather than through a pipe; re-query
instead of quoting a snapshot; check `--verify --quiet`; name the profile a
build script requires. That is what the numbered checks below ARE — each one is
an input somebody trusted, converted into something you run.

So when a check here surprises you, the useful reaction is not "I should have
been more careful." It is **"what did I read that did not mean what I thought,
and what command would have said so."** Then add that command here.

### The same shape in the tools: `rebase --quit` versus `merge --abort`

The clearest live instance, because both halves of it are uncontrolled inputs.

**Wrong path.** In a linked worktree, `.git` is a file that points at the
worktree-specific Git directory; it is not a directory to search directly.
The authoritative state path is whatever Git reports with `git rev-parse
--git-dir`, typically `$GIT_COMMON_DIR/worktrees/<name>/rebase-merge` or
`.../rebase-apply`. Ask Git rather than guessing:

```bash
GD=$(git rev-parse --git-dir)          # authoritative worktree-local git dir
ls -d "$GD"/rebase-merge "$GD"/rebase-apply 2>/dev/null
ls -d "$(git rev-parse --git-common-dir)"/worktrees/*/rebase-* 2>/dev/null
```

**Wrong verb.** Having found orphaned state, `--quit` and `--abort` are not
interchangeable:

| command | effect on the local checkout |
|---|---|
| `git rebase --quit` | stops the rebase without resetting HEAD, the index, or the working tree; with `--autostash`, Git saves the temporary autostash in the stash list |
| `git rebase --abort` | aborts the rebase and resets HEAD to the original pre-rebase tip |
| `git merge --abort` | attempts to reconstruct the pre-merge state, may fail when uncommitted changes predated the merge, and cannot clear rebase state |

For orphaned rebase state after a landing, `rebase --abort` changes only the
local checkout: it can move local HEAD back to the pre-rebase tip even though
the remote landing remains intact. Prefer `git rebase --quit` when the current
HEAD, index, and working tree are the state to preserve; never recursively
delete the state directory, which can leave Git metadata inconsistent. Snapshot
and verify the local state, and inspect any autostash explicitly:

```bash
before_head=$(git rev-parse HEAD)
before_status=$(git status --short)
git rebase --quit
test "$(git rev-parse HEAD)" = "$before_head"
test "$(git status --short)" = "$before_status"
git stash list
# If --autostash was used, identify and preserve the saved autostash entry.
```

A tailed rebase hides its own failure exactly as a tailed push hides a
rejection, for the identical reason (`$?` after a pipeline is the LAST
command's status). Read the status of both directly.

## Why absence is so often reported wrongly: landing rewrites the SHA

`rrnewton/hermit` has **merge commits disabled** — `allow_merge_commit=false`,
squash and rebase only. Confirm with:

```console
gh api repos/rrnewton/hermit --jq '.allow_merge_commit'
```

So **every landing rewrites the commit SHA.** The bytes that reach `main` are not
the bytes that were on the branch. A later reader comparing by SHA, by ancestry,
or by tree finds no match and concludes the work is absent — when in fact it
landed under a different identity.

This is a property of how the project lands, not a mistake by whoever wrote the
branch. It explains a whole population of "genuinely lost" rows that were not
lost at all, and it is why the checks below anchor on content and behaviour
rather than on identity.

## Before you claim: search the pull request, not only the task graph

Every check below decides what a change IS. This one decides whether it is
YOURS to decide, and getting it wrong costs two agents instead of one.

**Search for ownership signals on the pull request number itself, recent-first,
REGARDLESS OF TASK STATUS.** Filtering the task graph for a live task is not
enough, and it is wrong often enough to be dangerous:

```console
# 1. the obvious search — necessary, and NOT sufficient
sqlite3 "$TG_DB_PATH" "SELECT local_id, status, owner FROM tasks
   WHERE (title LIKE '%<N>%' OR description LIKE '%<N>%') AND status != 'CLOSED'"

# 2. the search that actually fires
sqlite3 "$TG_DB_PATH" "SELECT created_at, task_id, content FROM task_notes
   WHERE content LIKE '%#<N>%' OR content LIKE '%pull/<N>%' OR content LIKE '%PR <N>%'
   ORDER BY created_at DESC LIMIT 5"
```

⚠️ **Match a pull-request REFERENCE, not a bare number.** `LIKE '%<N>%'` also
matches the digits inside a 40-hex SHA: searching for `2332` hits
`de3cac715bb85d799d1e659ab9a1622332e7c0c9`, which mentions no pull request at
all. Measured on the same corpus, the bare predicate returned 32 hits where the
reference predicate returned 26, and 36 where it returned 26 — a fifth to a
quarter of them spurious. A claim check that cries wolf gets skipped, which
returns you to the task filter that was insufficient in the first place.

⚠️ **Both searches above read the TASK GRAPH. Run the pull-request side too,
and order it by `updated_at`.**

```console
# 3. the pull-request side — comments and reviews, ordered by updated_at
ci-hub/bin/gh-api "/repos/rrnewton/hermit/issues/<N>/comments?per_page=100" \
  | jq -r 'sort_by(.updated_at) | reverse
           | .[] | "\(.updated_at)  \(.user.login)  \(.body[0:120])"'
```

**`created_at` is the wrong key and it fails in the dangerous direction.** An
edited comment keeps its original creation time, so ordering by `created_at`
ranks LAST exactly the comments someone cared enough to keep current — which are
the strongest evidence a row is owned. Measured on hermit#2407: two adversarial
reviews reading `created_at 09:43:02Z / 09:44:11Z`, which with a holding task
untouched since 08:56 made the row look abandoned and reclaimable. Both carried
`updated_at 12:08:4xZ` — the reviewer had edited them in place to re-cite
`exact head 323a87d1da5f against base e51f3303369b`, a base that landed at
12:06:43Z. Thirty minutes stale, not three hours.

⚠️ **The check that does not depend on trusting any timestamp:** if a signal
references a commit, head or artifact that DID NOT EXIST when the signal was
allegedly written, the signal was edited — re-read it as fresh, and discard any
staleness conclusion drawn from its timestamp. That is what exposed #2407, and
it generalises to every staleness argument: "nothing has happened here for three
hours" is a claim about a proxy, "nothing here refers to anything that exists"
is a claim about the content.

⚠️ **Neither side is sufficient alone, and they are blind in OPPOSITE
directions.** Check 14 below measures one direction: of 14 candidate heads
TaskGraph reported free, **12 carried a live ownership signal visible only on
the pull request**. This section measures the other: the protocol says claim on
the task BEFORE touching the head, so a correctly-behaved claimant is INVISIBLE
on the pull request during exactly the window when claiming matters — measured,
a head whose task had been claimed 53 seconds earlier scanned clean PR-side. So
the task graph misses the claimant who has already moved to the head, and the
pull request misses the one who correctly has not touched it yet. Run both every
time; a clean result from one side is not evidence.

Measured on 2026-08-25: **four consecutive claims where search 2 caught what
search 1 missed.** Three of them — hermit#2547, hermit#2546 and hermit#2460 —
were reported FREE by the task filter and all three were already held. The
filter was not wrong by its own rule; no live task referenced those numbers. The
ownership lived in a NOTE, and twice in a note on a task that was **already
closed**.

hermit#2547 is the sharp one: it was mid-collision between two agents, and the
holder's note opened "STOP BEFORE IMPLEMENTING". A third claimant would have
made it three. That single note was the only place the collision was visible.

Two corollaries, both paid for:

- **A closed task is not a released claim.** Work is routinely finished,
  recorded and closed while the pull request stays open on purpose. A closed
  task plus an open pull request means READ THE LAST NOTE, not "free".
- **A claim posted on a neighbouring task is invisible** to the next agent's
  search, which is how the duplicates happened. The row gets its OWN task.

### The claim is ADVISORY. Re-reading the owner field is the protocol

`tg claim` **overwrites the owner field unconditionally, prints success to the
second claimer, and warns neither party.** So a claim succeeding tells you
nothing whatever about whether you were first, and the only operation that
detects a loss is reading the owner field back. These are numbered steps, not
advice:

1. **Search** — the three searches above, task graph and pull request.
2. **Claim** on the row's own drain task, and post the claim note.
3. **RE-READ THE OWNER FIELD.** If it names someone else you lost the row:
   yield, and hand over what you found. Do not claim back.
4. **RE-READ IT AGAIN BEFORE THE IRREVERSIBLE STEP** — the close, the land, the
   force-push. Checking only at step 3 catches the race you lost in the first
   minute and misses the one you lost in the fortieth. Check 14 below is the
   full treatment and its evidence; this step exists so the protocol is complete
   where you read it.

⚠️ **A published claim note does not prevent an overwrite.** Measured three
times inside eight minutes on 2026-08-25, three different pairs of agents —
53s, 32s and 11s after the prior claim, two of the three posting no note at all.
In the 11-second case the canonical claim note had been published to the task
before the overwrite; it was correct, visible, and changed nothing. A protocol
that depends on the other party READING a note cannot survive a claimer that
only WRITES an owner field: the note is a good record and a bad lock. Treat
losing a row as routine, and yield with a handoff note rather than racing — a
re-claim war over a thirty-second margin costs more than any row is worth.

The durable fix is a first-writer-wins claim: refuse the overwrite when a task
already has a different live owner, and require an explicit `--steal` carrying a
reason that lands as a note. Until that exists, step 3 stands in for it.

If both searches are clean and you still find the row held, say so — a claim
that had nowhere to be recorded is a defect in the recording path, not a
collision you caused.

## Before you OPEN a row: deconflict by FILE and SYMBOL, not only by number

⚠️ **This is a SECOND and separate rule, and it is kept separate deliberately.**
The rule above asks *"does someone already hold this ROW?"* and searches by pull
request number. It is measured four for four on exactly that question. This one
asks a different question — *"is this DEFECT already being fixed?"* — and
folding it under the same heading would make that four-for-four describe
something it never tested. Separate predicate, separate evidence.

**The number search cannot catch this shape, at any quality.** When two agents
independently discover the same defect and open SEPARATE rows, there is no prior
claim and no shared number for an ownership search to find. Nothing is wrong
with the search; the duplicate is simply invisible to it.

Before you open a row, ask what the CHANGE touches:

```console
# 1. who has touched this file recently on main
git log --oneline -5 origin/main -- <the file you are about to change>

# 2. which OPEN pull requests already touch it  -- the one that matters
for pr in $(ci-hub/bin/gh-api '/repos/rrnewton/hermit/pulls?state=open&per_page=100' \
              | jq -r '.[].number'); do
    ci-hub/bin/gh-api "/repos/rrnewton/hermit/pulls/$pr/files?per_page=100" \
      | jq -e --arg p '<path>' 'any(.[]; .filename==$p)' >/dev/null && echo "$pr"
done

# 3. for a DEFECT rather than a file, the symbol
git grep -n '<the function or constant you are fixing>' origin/main
```

**Step 2 is the one nobody runs, and the stated reason — an API call per open
pull request — does not survive measurement.** Enumerating files for 174 pull
requests (38 open plus 136 recently merged) took 135 seconds, 0.8s each. The
open queue alone is about 30 seconds. That is cheaper than one duplicated
one-line fix.

There is also a cheap approximation: search open pull-request TITLES and BODIES
for the full path, and enumerate files only for the hits. On the four rows in
the instances below, the full path appeared in title-plus-body every time, so on
this corpus it loses nothing — this fleet's bodies name the paths they touch.
Search the full path rather than the basename; `files.rs` matches half the tree.

### The instances

- **hermit#2520 and hermit#2521 — the same one-line fix, opened three seconds
  apart, and BOTH LANDED.** Each is `+1/-1` to
  `detcore/src/syscalls/files.rs`, restoring the same import order to unbreak
  `lint.rustfmt` on main. Created `08:32:24Z` and `08:32:27Z` by two different
  agents on two different branches (`fix-rustfmt-container-output-import`,
  `fix-rustfmt-files-import-order`), merged 08:32:44 and 08:32:51. Neither
  number appears in the other; no ownership search could have connected them.
  A `git log` on that path would have.
- **hermit#2537 / #2533 / #2539 — three agents on adjacent pipe2 capacity-pin
  work inside about an hour**, all touching `detcore/src/syscalls/files.rs`.
  #2539's author DID deconflict — against #2537, while missing #2533, which was
  the actual duplicate of what remained after they reduced scope. The
  number-level signal surfaced one of the three; a file-level signal surfaces
  all three.
- **This very section is the third instance.** hermit#2548 added the claim rule
  above to this file and merged at `12:46:51Z`. The corrections now folded into
  it were committed independently at `12:43:52Z`, three minutes earlier, by an
  agent who never saw #2548 — it was OPEN and touching this exact path at the
  time, and the file-level check would have surfaced it. What was written
  instead was a second, parallel section on the same subject, which had to be
  discarded and re-applied in place.

### The SYMBOL half is weaker evidence than the FILE half, and is marked as such

The file check has three confirmed instances above. **The symbol check (step 3) has none, and it is
included on argument rather than on measurement.** Saying so is the point: a rule that presents its
measured and its reasoned halves at equal strength invites the reader to discount both.

The argument for keeping it is that the file check has a known blind spot, and check 13 below
already establishes the mechanism: **a name is not a mechanism, and code moves.** If the work you
are about to do has been done under a different function name, or in a different file after a move,
two focused rows can share a defect and share no path — and step 2 sees nothing. Step 3 is the same
predicate applied to the thing that survives a move.

⚠️ **I looked for an instance of that shape tonight and did not find one — recorded so the search is
not repeated blind.** Over the same 174-row corpus I compared every pair of focused rows with
DISJOINT file sets, created within twelve hours, sharing two or more distinctive title tokens. It
returned 86 pairs and every inspected one was a false positive, because the disclosure prefix
(`[hermit2, <agent>, unresolved, devbig014, role=impl]`) supplies shared tokens to every row this
fleet opens. **A title-token search cannot work on this corpus until the disclosure prefix is
stripped first**, and even then titles are a poor proxy for a symbol. The measurement that would
settle it is a symbol-level one: extract identifiers from each row's diff hunks and intersect those,
rather than intersecting titles. Until someone runs that, step 3 is a cheap precaution with a sound
mechanism behind it, not a measured rule.

### Co-touching is not duplication

Do not let this rule cry wolf. On the same corpus, 189 paths are touched by two
or more pull requests, and filtering to focused rows (six files or fewer),
non-generated paths, and a twelve-hour window still leaves 90 candidate pairs.
Those are CANDIDATES; a duplicate is a subset you confirm by reading.

hermit#2541 and hermit#2542 are the negative control: same file
(`hermit-cli/src/bin/hermit/run.rs`), one minute apart, two agents — and not a
duplicate at all. #2541's body names #2542 and says what each half does. That is
what a deconflicted pair looks like, and it is what this check is FOR: it
surfaces the neighbours so you can do exactly that.

Generated artifacts are the other false-positive source and should be excluded
outright: `ci/compat-envelope/cells.json` alone is touched by 40 pull requests,
along with `SCORECARD.md`, `ci/expected-e2e-plan.json` and the manifests. Those
are regenerated by nearly every coverage change and carry no signal about who is
fixing what.

## Before you PARALLELISE a failure set: triage it, because a node count is not a defect count

Two agents on one cause, dispatched from opposite nodes, is the most expensive
duplicate this queue produces — neither can see the other, because neither is
looking at the same artifact. Triage costs one reading pass; the duplicate costs
two investigations and is discovered late, if at all.

**Open every node's actual failure text before dispatching anyone.** Not the node
name — the assertion. Measured on a full validate at 2026-08-25T14:22Z, 62 nodes
executed, five reported as blocking failures:

| node | what the name suggests | what it actually was |
| --- | --- | --- |
| `test.regular_crates` | a crate test | `setpgid` escaping the DBT copied child |
| `test.cli` | a CLI test | `prlimit64` refusing a virtualized self-pid |
| `test.liteinst_strict` | a LiteInst strictness failure | `os.getpid()` returned 2, expected 3 |
| `test.sabre_examples` | a SaBRe example | run-to-run divergence on `rand.py` |
| `scorecard.compatibility` | a scorecard failure | **nothing failed** — "218 missing, 0 non-passing" |

⚠️ **Three of the five names misdescribe the failure**, and one names a failure
that did not happen. The `liteinst_strict` case is the sharpest: the test is
called `..._python_entropy`, its entropy assertions all pass, and the failing
line is the virtual pid.

**The five resolved into four dispositions, only one of which was startable:**

- **2 collapse into ONE cause** — `setpgid` and `prlimit64`-by-virtual-pid are two
  faces of the same unbuilt process-identity model, already covered by an open
  OWNER DECISION. Not fixable by whoever investigates them.
- **1 adjacent but not the same** — the pid VALUE is wrong by one, where the other
  two have no model at all. Same surface, different shape, different fix.
- **1 genuinely separate** — SaBRe, unowned and unexplained.
- **1 needs no investigation at all** — its own message says nothing failed.

So **three of five investigations should never start**: two would have put agents
on an owner-blocked question from opposite nodes, and one on a node reporting
success. **Three investigations not started beats three duplicates caught later**,
because the duplicate is only visible after both agents have paid.

**The discriminating questions, in order.** Do two nodes name the same subsystem
AND the same failure shape? Then one cause until shown otherwise. Does a node's
own text say nothing failed? Then it is downstream — re-run it last. Is the fix a
policy question someone else owns? Then it is not an investigation, it is a
blocked row, and dispatching it burns an agent who cannot finish.

⚠️ **And read whether a node RAN.** A dependency-skipped node is not a failure.
Counting `test.strict_compat` — "skipped (dependency failed, never ran)" — as a
sixth failure sends someone to investigate a node that never executed.

## Before you BELIEVE a refusal test: an exit code is a claim about the environment

**An assertion on an exit code alone is an assertion about the environment as
much as about the code**, because every failure path in a shell tool shares the
same small set of codes. A test that requires only "it exited 2" passes when the
tool refused for the reason you meant, and equally when it could not run at all.

Measured 2026-08-25 on `ci/run-node-args-test.sh`. Five checks asserted that
`ci/run-node.sh` refuses a malformed invocation with exit 2. But exit 2 is also
what that script returns for an unknown lane, an unwritable perf directory, and
`dagrun not found`. Removing `ci/dag/portable.json` — standing in for a box where
the runner is simply unavailable — produced:

    exit-code-only:   ok / ok / ok / ok / ok          <- all five vacuous
    reason-asserted:  ok / FAIL / ok / FAIL / FAIL

The three that flipped were exiting 2 on "unknown lane" and being counted as
proof that the CI refusal and the multi-node refusal worked. **A refusal test
that passes when the thing it tests is absent cannot fail for the reason it
names**, which makes it a member of the same family as a guard no node runs: a
mechanism producing a value that reads as information and carries none.

**The fix is one string per check.** Assert the refusal's own message alongside
its code:

    if [[ $output != *"$reason"* ]]; then
        fail "$what: exited 2 but for the wrong reason — no '$reason' in the output.
      This is what an environment failure (missing dagrun, bad lane) looks like."
    fi

That cost is what makes this adoptable rather than aspirational. Three
consequences worth stating:

- **Say which checks legitimately still pass.** Two of the five above were
  argument-parsing refusals that run before the lane file is read, so they were
  correctly unaffected. Naming them is the difference between a fixed test and a
  test that merely got noisier.
- **The same lens applies to diff assertions.** "Some steps changed" passes for
  either of two edits going wrong. Check each by name — "56 undeclared step(s)
  stamped 7200s; 0 declared step(s) left alone" — so a mutation that stamps the
  wrong set fails instead of matching a vague predicate.
- **Prove it by mutation.** Reword the refusal the test pins and confirm that
  check, by name, goes red; restore it and confirm it goes green. A guard that
  has never been observed failing is a guard whose failure path is unmeasured.

## The checks, in order

### 1. Content, anchored at the pull request's OWN merge base

A symbol counts as **already landed** only if it is:

- **ABSENT** at the pull request's own merge base, **and**
- **PRESENT** on current `main`.

A symbol that already existed at the merge base proves nothing in either
direction and must be excluded as indeterminate, not scored.

```console
BASE=$(git merge-base origin/main pr1234)
git diff "$BASE"...pr1234 -- '*.rs' | grep -E '^\+' \
  | grep -oE '\b(fn|struct|enum|const|static|trait)\s+[A-Za-z_][A-Za-z0-9_]*' \
  | awk '{print $2}' | sort -u
# then for each symbol: present at $BASE?  present at origin/main?
```

Anchoring matters and is not ceremony. Measured on real rows: one pull request
added 7 symbols of which only **1** was decisive; another added 24 with 11
pre-existing; another added 76 with 22 pre-existing. Scoring those without the
anchor produces confident wrong verdicts.

`git cherry origin/main <branch>` is a useful patch-id signal and is **never
proof of absence** — a squash-landed change has a different patch id.

### 2. Mechanism, because content re-lands under different bytes

Content is frequently re-implemented rather than merged. Take the pull request's
distinctive behaviour — an error string, a required rule, a constant, a defining
symbol — and ask whether **current main does that thing**, under any spelling.

Two measured examples:

- A pull request introducing a `ci = false` reason requirement scored "absent" by
  content. Main enforced the identical rule, in two directions, under a
  different function name, and refused a probe with the exact message.
- A pull request adding `DETERMINISTIC_PIPE_CAPACITY` scored "absent". Main pins
  the same thing as `DETERMINISTIC_PIPE_CAPACITY_BYTES` via `F_SETPIPE_SZ`.

**Grep for the mechanism, then PROVE it by exercising the behaviour.** A grep can
match a comment describing a defect rather than the code implementing the fix.

### 3. Residual, because a superseded mechanism can still carry unique content

This is the check that is skipped most often, and skipping it is how a wrong
close happens. A pull request whose mechanism has landed may still hold content
that has not.

The cheapest version takes seconds: for every file present on **both** sides,
compare sizes.

```console
for f in $(git diff --name-only "$BASE"...pr1234); do
  printf '%s pr=%s main=%s\n' "$f" \
    "$(git show pr1234:$f 2>/dev/null | wc -l)" \
    "$(git show origin/main:$f 2>/dev/null | wc -l)"
done
```

Measured: a pull request whose pipe-capacity mechanism had landed, whose test
files both existed on main, and whose target manifest had been deleted in a
format migration — every signal saying "close it" — turned out to carry **173
non-comment lines** of a distinct test scenario absent from main. The file sizes
were 669 vs 462 and 300 vs 167. Ten seconds of checking reversed a conclusion
that was about to be published as a close.

Same file name and same test-function names do **not** imply same content.

### 4. The dependency-pin hazard, in one direction only

`hermit` **pins** `reverie` and `agent-utils`. A mechanism can therefore arrive in
hermit's *build* through a pin bump while being absent from hermit's *tree*.
Such a change is landed-elsewhere, not absent.

This runs hermit-ward only. For reverie's or agent-utils' own pull requests the
authority is that repository's own `main`, and the hazard does not apply.

### 5. Effect, because a correct verdict about the past can be the wrong action

**Checks 1 to 4 all ask what already happened. This one asks what landing would
DO.** It is the only forward-looking check, and it catches cases the other four
cannot: the content is genuinely absent, the mechanism is genuinely absent, so
every earlier check says "pending, land it" — and landing would make current
behaviour WORSE while reading as progress.

Ask: **has main improved this code since the branch left it?**

```console
BASE=$(git merge-base origin/main pr1234)
for f in $(git diff --name-only "$BASE"...pr1234); do
  printf '%s  main-since-base %s  pr-since-base %s\n' "$f" \
    "$(git diff --numstat "$BASE" origin/main -- "$f" | awk '{print "+"$1"/-"$2}')" \
    "$(git diff --numstat "$BASE" pr1234    -- "$f" | awk '{print "+"$1"/-"$2}')"
done
git log --oneline "$BASE"..origin/main -- <the-directory-it-touches>
```

Two signals, both measured:

- **Main moved further than the branch in the same files.** One draft changed
  `detcore-dbt/src/lib.rs` by +248/-138 while main had moved it +373/-51. The
  branch DELETES 138 lines that main has since built on. Porting it is a
  regression wearing the shape of a feature.
- **A commit on main already does the branch's stated job.** The same draft was
  titled "route DBT verification through canonical logs"; `a811f33684`
  "detcore-dbt: route canonical logs through protected evidence" had landed 28
  hours earlier and more completely — 9 mentions of `canonical` in main's
  detcore-dbt against the draft's 2.

This check has already reversed a published verdict. A draft classified
"genuinely pending" from the symbol rule alone was closed by its reviewer
because porting it would have REGRESSED current accounting. The symbol verdict
was right about the past fact and wrong as an action.

Two closed drafts make the failure mode concrete:

- [Hermit #2308](https://github.com/rrnewton/hermit/pull/2308) had exact patch
  content and symbols absent from `main`, but its keep-going mechanism had been
  implemented more strongly. The mechanism check therefore found supersession,
  not a genuinely pending change. The effect check rejected the remaining
  cumulative `by_tag` closure because it would have replaced newer
  latest-attempt and unreported-node accounting.
- [Hermit #2410](https://github.com/rrnewton/hermit/pull/2410) deleted five
  unchanged demo files and merged cleanly. While the draft sat, commit
  `be5cc1a79f` added a workflow that executes `demos/05-qemu-busybox.sh` and
  watches that file, `demos/boot_qemu.sh`, and the two files under
  `demos/qemu-busybox/**`. Landing the deletion would therefore have left a
  tracked workflow pointing at missing runtime inputs. The unchanged blobs were
  not dead weight; they had become load-bearing without changing themselves.

#### Deletions invert the marker test

For an additions-only change, content present on `main` can support an
already-landed verdict. For a deletions-only change, that same observation
proves the literal deletion did not land; it does not prove the deletion remains
safe or valid to land now.

Before accepting a deletion, scan current callers of every removed path or
interface, including files added after the pull request's base:

```console
git fetch origin main
git grep -n -F '<removed path or interface>' origin/main
git log --oneline "$BASE"..origin/main -- <its callers and containing directory>
```

A clean merge says Git can perform the deletion. It does not say current code
can run afterward.

#### A stated prerequisite can become false while the draft waits

Treat every phrase such as "migrated first", "landed separately", "now lives
at", or "no callers remain" as a live precondition, not historical evidence.
Verify it against the current remote tree before landing:

```console
git -C <repository> fetch origin main
git -C <repository> cat-file -e origin/main:<required-path>
git -C <repository> show origin/main:<required-path>
```

Two drafts have already failed this check:

- [Hermit #2410](https://github.com/rrnewton/hermit/pull/2410) named four
  replacement paths in `dev-hermit` and said they had migrated first. All four
  were absent from current `dev-hermit/main`. That independently invalidated
  the deletion even before the new Hermit workflow caller was considered.
- [Hermit #2162](https://github.com/rrnewton/hermit/pull/2162) named coordinated
  Reverie and LiteInst heads as part of a five-repository warnings program.
  After fetching both repositories, neither named SHA was an ancestor of its
  current `main`. The mechanisms had also diverged: LiteInst carried the
  eight-root warning policy under different history, while Reverie carried none
  of those source-level warning attributes. The stale coordination claim could
  no longer authorize landing the Hermit snapshot; its remaining Python policy
  was parked for an owner decision instead.

A prerequisite verified when the draft was written can disappear, be renamed,
land under different history, or never land at all. Re-read it at the landing
tip, and compare the mechanism when ancestry differs.

### 6. A symbol absent from BOTH sides means the check cannot speak

⚠️ **A marker absent at the merge base AND absent on main does not mean unlanded.
It means the check could not decide.** Classifying those as absent manufactures
confident wrong verdicts at scale, because check 2 above is exactly the case that
produces them: the mechanism landed under a different symbol name, so no
identifier is shared and both counts read zero.

This is not hypothetical, and the abstract rule will not stop anyone. The example:

    hermit#2422 added   detcore/src/consts.rs:45         pub const DET_PIPE_CAPACITY: i32 = 8192;
    main already had    detcore/src/syscalls/files.rs:78 const DETERMINISTIC_PIPE_CAPACITY_BYTES: i32 = 8 * 1024;

Same value. Same pipe-creation handler. Same `F_SETPIPE_SZ` on the read end. Same
stated rationale, down to the same cause — Linux sizes a new pipe from the host's
per-user `pipe-user-pages-soft` accounting, so a parallel validate crosses the
threshold using only its own concurrent guests. **Both symbols count zero at the
merge base and zero on main.** A pure symbol rule reads that as "genuinely
unlanded, land candidate". It was already landed, and landing the pull request
would have added a second constant and a second pin site for one property.
hermit#2331 was the same shape against main's `sabre_backend_evidence_line`.

So route both-sides-absent to **indeterminate**, never to *genuinely pending*, and
resolve it by hand with a search for the **value and the rationale rather than the
identifier**: grep main for the constant's value, the syscall it configures, and a
distinctive phrase from its comment.

Measured cost of getting this wrong: in the 2026-08-25 sweep of hermit's 46 open
`[SALVAGE]` drafts, **21 of 46 — the largest bucket — landed in this state.** Had
they been auto-classified as absent, 21 verdicts would have been wrong in the
direction that wastes a receipt and re-lands existing work.

### 7. Backend presence, before you blame a red on the pull request

**A red you see while assessing may belong to your box, not to the branch.**
Backend absence is not reported uniformly, and one form MANUFACTURES failures:

- **Missing DynamoRIO makes the DBT tests FAIL.** On a box without the backend
  this produces roughly **20 reds that have nothing to do with any pull
  request**.
- **Missing SaBRe makes its tests SKIP.** Same condition, opposite reporting.

So before attributing a DBT red to the branch under assessment, establish
whether the backend is actually present:

```console
ls target/install_pkg/rsrcs/            # dynamorio, sabre, e9patch, liteinst runtimes
./target/debug/hermit run --backend=dbt -- /bin/true
```

**Distinguish two different causes that look identical.** A build without
`--features third-party-backends` reports `backend 'dbt' is unavailable: DBT
support was not included in this build` — that is a MISSING FEATURE FLAG in your
binary, not a missing backend on the host, and not a defect in the branch.
Neither is the pull request's fault, and neither should be recorded against it.

⚠️ **AND IT IS NOT A REASON TO ABSTAIN — IT IS ONE FLAG.** This paragraph
previously stopped at "nobody's fault", and that half-truth cost a night of
unnecessary abstentions: several agents, and the coordinator, told each other to
decline DBT results because a default build reports the backend unavailable.

`third-party-backends = ["dbt", "sabre", "e9patch"]` is declared in
`hermit-cli/Cargo.toml`, and the workspace manifest documents the invocation:

```console
cargo build -p hermit --features third-party-backends
```

**DBT and SaBRe are verifiable on these boxes.** "Not included in this build"
describes a build default you can change, not a capability the host lacks. Build
with the flag and measure, rather than parking the verdict — and state which
binary and which commit produced the number, because a DBT measurement from a
default build is not a measurement at all.

The genuinely unavailable thing here is narrower and unrelated: `api.github.com`
for `gh` **writes**. Reads go through `ci-hub/bin/gh-api`, and landings go by
pushing directly.

This is the only check here about the ENVIRONMENT you are assessing in rather
than about the branch or about main.

#### A PRESENT backend does not make a red the branch's either

Establishing that the backend exists closes only half of this. The reds can still
belong to the box, to the binary, or to a standing defect — and then they look
exactly like reds the branch caused, because the backend ran.

**Run the control: the same cases, the same binary, on unmodified `main`.** Only
the difference between the two runs is attributable to the branch.

```console
python3 tests/backend-parity/run_matrix.py --backend dbt --verify --hermit <dbt-capable-hermit>
git stash && python3 tests/backend-parity/run_matrix.py --backend dbt --verify --hermit <same-binary>
```

Measured on hermit#2342: DynamoRIO was present and the binary genuinely ran under
it, and the ported change produced **25 FAIL**. The control on unmodified `main`
produced **the same 25 FAIL**. The change was responsible for none of them.
Without the control the sweep would have published "this pull request turns 25
DBT cases red" — a wrong verdict reached with the backend present, which check 7
as written above would not have caught.

Note also that a DBT-capable build may exist elsewhere on the box even when the
one in your worktree reports `DBT support was not included in this build`. Probe
the binaries you have before concluding a backend is unmeasurable; state which
binary and which commit produced any number you report.

#### The feature-flag message is NOT DBT-specific, and it is nobody's fault

The same wording appears for every gated backend, so do not read it as the
manufactured-reds case. Measured 2026-08-25 on one box, two builds of the same
repository:

```
hermit/target/debug/hermit   --backend sabre  ->  WARN hermit::sabre: :: Backend:
                                                  sabre static rewriting + ptrace runtime
hermit/target/release/hermit --backend sabre  ->  Error: backend `sabre` is
                                                  unavailable: SaBRe support was
                                                  not included in this build
```

**Same host, same source, opposite answers** — the difference is the feature flag
the binary was built with, nothing else.

This is the *inverse* of the trap above and needs saying because the next reader
will meet the message and reach for the manufactured-reds explanation. Four wrong
verdicts in one night came from an absent backend being read as failure; this is
an absence with a **benign cause**, correctly attributable to **nobody** — not the
branch, not `main`, not the host.

Two consequences worth acting on:

- **`unavailable: … not included in this build` is a statement about your binary.**
  It is not evidence about the pull request and must never be recorded against it.
- **Probe every hermit binary on the box before declaring a backend unmeasurable.**
  The build that lacks the backend is often not the only one present.

### 8. Look for a policy that declares the thing intentional, before calling it a defect

⚠️ **A line read in isolation cannot tell you whether it is an oversight or a
stated decision.** Check whether something nearby declares it on purpose. If it
does, the pull request is not a fix — it is a policy change, and it carries the
obligations of one.

The case, and it cost a wrong recommendation to three agents before it was
caught:

`tests/backend-parity/run_matrix.py` invokes the matrix with
`("--verify", "--verify-allow", "both")` and no `--verify-strict`. Read alone,
that is plainly a defect: bare `--verify` is the lossy Stripped comparator and
cannot establish L2, so the whole backend-parity matrix appears to be comparing
under a comparator that cannot support the claim the matrix exists to make.
hermit#2342 adds the flag in one line, and it looks like the cheapest possible
win.

Twenty lines above, `DEFAULT_VERIFY_POLICY` says:

```python
DEFAULT_VERIFY_POLICY = VerifyPolicy.checked(
    hermit_flags=("--verify", "--verify-allow", "both"),
    expected_non_kvm_tier="stripped",
    comparison_claim=(
        "Stripped DETLOG comparison "
        "(numbers/addresses/paths normalized; NOT bitwise)"
    ),
)
```

The limitation is **declared**, in those words, including *NOT bitwise*. And
`VerifyPolicy.checked` refuses the one-line patch outright:

```python
if requests_canonical != expects_bitwise:
    raise ValueError("--verify-strict and the bitwise evidence tier must move together")
```

Applying hermit#2342's single line to main and importing the module produces
exactly that `ValueError`. The real change is three coupled edits — the flag, the
tier from `stripped` to `bitwise`, and the claim text — which flips
`assurance_label()` from *below L2* to **L2** for the entire matrix, and per
`AGENTS.md` an L2 claim must be established with `bitwise_parity: true` rather
than asserted.

**The generalisation.** A declaration next to the code is evidence about intent
that the code alone does not carry. Before filing a line as a defect, grep its
neighbourhood for a policy object, a named constant, a stated tier, or a comment
that owns the choice. Where one exists, the honest verdict is *policy change,
needs measurement*, not *one-line fix* — and the difference is the whole scope of
the work.

A well-built guard like the one above is worth noticing for its own sake: it
makes the flag and the claim it licenses move together, so no one can quietly
upgrade a comparator without upgrading the assurance claim. That is the opposite
of a gate that passes without looking.

### 9. Read the MERGE BASE, not only the diff — a draft can revert what it never touched

⚠️ **The other eight checks read the diff. This one reads the base, and it is the
only one that catches a draft reverting work its diff never mentions.**

What a merge writes is not the branch's diff. For a file both sides changed and
git cannot auto-merge, the resolution decides which side survives — and if the
branch's base predates a fix that landed in that file, taking the branch's side
silently reverts it. **Nothing in the diff shows this.** The reviewer reads a
change about one subject and lands a revert of another.

The case:

hermit#2426 is a compat-envelope draft. Asked what it does with a first-verify
run that produced no output, the answer is *nothing* — the only first-run
language anywhere in its diff is an unrelated fixture string,
`collect2: error: ld returned 1 exit status`. By diff, it is irrelevant to that
subject.

But its merge base is `770b95c505`, before the typed `no_result` /
`guest_exit_code` / `guest_signal` reporting landed. Counting those symbols:

| file | main | the draft |
| --- | --- | --- |
| `scripts/validate.rs` | **65** | 7 |
| `scripts/lib/validate_runtime.rs` | **2** | **0** |

and **both files are in that draft's conflict set.** A take-the-branch resolution
drops 58 matching lines from one and all of them from the other, undoing the
change that stopped a zero-byte first run being silent — a result reached only
after eight candidates were ruled out.

**The precise hazard, because "old base" alone is not it.** A rebase or
cherry-pick replays only the branch's changes, so it cannot revert content the
branch never touched. The danger is specifically **a conflicted file resolved
toward the branch**. So the test is not "is the base old" but:

> Does this draft **conflict** in a file that has gained something since its base?

**What to do.** Rebase rather than merge, so conflicts surface hunk by hunk
instead of as one take-a-side. Then assert the survivors mechanically, against
the merged tree rather than the intent:

```console
git grep -c -E 'guest_exit_code|no_result' -- scripts/validate.rs scripts/lib/validate_runtime.rs
# must not fall below main's counts
```

**Why this is acute right now.** Every draft in the queue was written before
tonight, and the typed no-result reason, the capture fix, the KVM comparator fix,
the RNG determinism fix and the format gate all landed in the last hours. Any of
them can be reverted by a draft whose diff looks unrelated. Pick the symbols that
matter for the files in the conflict set, and count them before and after.

### 10. File-identity alone establishes NEITHER "absent" NOR "no residual"

Counting files where `main` matches the pull request's head answers one narrow
question and is routinely mistaken for two others. Use the **three-way** split:

| bucket | test | means |
|---|---|---|
| **arrived** | `main` blob == PR blob | that file's content is on `main` |
| **absent** | `main` blob == MERGE-BASE blob | `main` never moved; definitely not landed |
| **moved on** | matches neither | undecidable from blobs; `main` went somewhere else |

**`absent == 0` is the close test, not `arrived == all`.** A branch far behind
`main` will show few arrivals and many moved-on files while being entirely
superseded, because `main` built past it.

Measured on real rows:

- `#2420` and `#2398`, both closed as already-landed: **arrived == all,
  absent 0, moved-on 0.** Every changed file byte-identical, corroborated by
  matching per-file `numstat` from the same base.
- `#2436`: **arrived 1 of 24, absent 0, moved-on 23.** A file-identity read of
  "1 of 24 identical" was published as evidence it was PARTIAL and needed
  residual extraction. That was wrong. `absent 0` plus 41 of 54 markers landed
  and **zero** both-absent made it `supersede-and-regress`, and `main` had moved
  two to nine times further in every substantive source file.

And "no residual" is a separate question that file-identity cannot answer at
all. On `#2405` a close was published with *"residual: none"* after comparing
the headline constant and its call site; the descriptor-cleanup path in the
**error branch** was absent from `main` and was missed. Diff the error paths,
and check distinctive string literals as well as symbols — on `#2436` that meant
210 literals grepped against `main`, of which 0 were absent.

### 11. What the change DEPENDS ON, not only what it changes

Checks 1 to 3 ask what a change contains and whether `main` already has it. This
one asks the opposite question about the residual you decided to extract: **does
the piece you are lifting out still work once separated from the piece you left
behind?**

A salvage branch is usually assessed file by file, and extraction is proposed the
same way — take the good test, drop the superseded code. That is exactly where
this fails, because **a test and the change that makes it pass are one unit even
when they sit in different files.**

Measured on [hermit#2339](https://github.com/rrnewton/hermit/pull/2339). Its
residual contained a genuinely valuable test strengthening: `main` already asserts
*exactly one* `scheduler-empty` and *exactly one* `fallback-completed`, and the
branch adds the same for `SCHEDULER_FIZZLE` and `BACKEND_EVIDENCE` plus a four-way
ordering assertion. Clean, self-contained, 13 lines, an obvious extract.

**It is not extractable alone.** The exactly-once-fizzle assertion is true only
because of the branch's *other* change — relocating the empty-system diagnostic
out of `step2_process_blocked` to the scheduler loop's single terminal exit. That
code change is superseded (`main` resolved the same double-emission by keeping the
`step2` site instead) and should NOT be ported. Lift the test out on its own and
you assert a property whose cause you deliberately left behind.

The failure mode is quiet: the extracted piece reviews well, merges cleanly, and
reds the suite on a claim nobody connected to the omitted half.

So before extracting a residual, ask:

- **Does this assert or rely on behaviour the SAME BRANCH introduced elsewhere?**
  Grep the branch's other files for the symbol, log string, or constant the piece
  keys on. A test naming `SCHEDULER_FIZZLE` wants whatever emits it.
- **Is the thing it depends on part of what you are dropping?** If yes, the
  residual is not two items, it is one item with a decision attached.
- **Can you establish the dependency holds on `main` as-is?** If you cannot
  measure it, say so rather than extracting hopefully. On #2339 that question was
  left open honestly: `/bin/true` under SaBRe never reaches the fizzle path, so
  settling it needs the test's own example guest.

This is check 8's cousin. Check 8 stops you calling an intentional line a defect;
this stops you lifting a correct line away from the thing that makes it correct.

Record the coupling in the disposition. "Extract the test" is not a disposition;
"extract the test **once the emission-site question is settled**" is.

### 12. A comparison over nothing is not agreement

Checks 1 to 11 ask whether a change is present. This one asks whether the
EVIDENCE a change relies on can fail at all.

**A zero-record comparison reads as agreement having observed nothing.** Two runs
that each produced no records compare equal. Nothing in the verdict distinguishes
"these agreed" from "there was nothing to disagree about", and the second is not
a result.

This is the shape behind most of what tonight's sweeps actually found, across
parts of the system that share no code:

- **Heap DETLOG under DBT.** The kernel labels `[heap]` only for
  `[mm->start_brk, mm->brk)`. Under DBT the guest's heap is an unlabelled
  anonymous mapping, so the heap comparison had no records to compare and
  reported agreement. Nothing was wrong and nothing had been checked.
- **`no_result` as a verdict.** `--verify-json` pre-writes
  `verdict: "no_result"` at invocation. A run that dies before comparison leaves
  exactly that, and it reads as an outcome rather than as "never reached
  comparison".
- **Backend-parity fixtures that returned 0 unconditionally.** A fixture counted
  its checks, printed the tally, and exited 0 whatever the tally reached. Under
  `--verify` both runs lower the number identically, the comparison matches, and
  the cell stays green having verified nothing.
- **A test that skipped silently.** An integration test returned early when its
  backend artifacts were absent, reporting `ok` in 0.00s having executed nothing.

**The check.** For any claim a pull request makes about evidence, ask what the
evidence looks like when the mechanism is ABSENT rather than merely broken. If
absent and correct produce the same output, the evidence cannot support the
claim.

Three ways to make it able to fail, in decreasing order of strength:

1. **Count what was compared, and refuse zero.** A comparison that comes back
   empty should say so rather than pass.
2. **Assert the mechanism is reachable.** A positive control — one record you
   know must be present — turns a silent zero into a failure.
3. **Say what was skipped, out loud.** If a check cannot run, it must print why.
   A skip that prints nothing is indistinguishable from a pass.

⚠️ AND THIS APPLIES TO THE SWEEP'S OWN CHECKS. A symbol grep that matches nothing
on both sides (check 6) is the same defect wearing sweep clothing: it reports no
difference because it looked at nothing. Check 6 exists because that happened.

### 13. A symbol on the branch and absent from `main` is not yet a residual — it may be a RENAME

Check 2 warns that a symbol *present* on `main` need not mean the mechanism
landed. This is the **other direction**, and it produces the more expensive
mistake, because it invents work rather than skipping it: a symbol that is
absent at the merge base **and** absent from `main` reads exactly like the one
thing a superseded branch still uniquely carries. That is the signature of a
genuine loss, so it is acted on — and a rename forges it perfectly.

**A function name is not a mechanism. Hash the body.**

Measured on `#2178` (35 files, 14 real conflicts, 13 days stale). Of its 11
distinctive identifiers in `hermit-cli/tests/flock_exclusion.rs`, six were absent
at its own merge base and present on `main` — properly landed under check 1 —
four were common words carrying no signal, and exactly one,
`replay_reissues_every_flock_against_the_kernel`, was absent on **both** sides.
On the symbol rule alone that is a residual, and the disposition would have been
"extract the surviving test".

It was not a residual. `main` carries the same test as
`replay_reissues_every_flock_for_a_materialized_file`. The eighteen-line
assertion body — the `requested` / `injected` counters, the `requested > 0`
anti-vacuity guard, and the failure message verbatim — is **byte-identical**,
`sha1 4c03071d9b639807e36556fdcb6e0245ec0408e9` on both sides. Only the name
differs. Landing the "residual" would have committed a duplicate test.

The cheap settling move, before believing any absent-on-main symbol:

```bash
# not the name -- a distinctive line from the body, and the payload's hash
git grep -F 'replay re-issued {injected} of {requested} flock calls' "$MAIN" -- .
git show "$PR:$file" | sed -n "$a,$b p" | sha1sum   # vs the candidate on main
```

A message string survives a rename; a symbol does not. This is why check 10 ends
by telling you to grep **distinctive string literals as well as symbols** — the
literal is the rename-proof handle, and here it was the only thing that spoke.

Note what did *not* settle it. `main`'s file was 951 lines against the branch's
491, and its versions of all four `add/add` fixtures were larger — up to 3.7x on
`tests/c/flock_exclusion.c`. **Size is not content**; it is the same
argument-from-bulk that check 10 already rejects for blob counts. `main` was also
strictly stronger in a way no name or size check reveals: a second call site
asserts `injected == requested + 1`, a tighter invariant the branch never had.

Final verdict on `#2178`: **fully superseded, zero residual** — closed, not
rebased. The 14 conflicts were never worth resolving.

### 14. Re-read the owner field at BOTH ends — a claim can be overwritten after you take it

The landed rule says re-read the owner field **after** claiming, because `tg claim`
overwrites unconditionally and tells neither party. That is necessary and it is not
sufficient: nothing about the overwrite is limited to the moment you claim. A claim
you verified on acquisition can be taken from you at any point while you work, and
the first you would learn of it is a second agent's verdict landing on the same
head.

**So check at acquisition AND immediately before you act.** The second read costs
one query and it is the one that protects the irreversible step — the close, the
land, the force-push. A stale `owner` is harmless while you are only reading; it is
expensive exactly when you stop reading and start writing.

    tg claim <task>
    # re-read #1 -- did my claim actually take?
    sqlite3 ... "SELECT owner FROM tasks WHERE local_id='<task>'"
    ... do the work ...
    # re-read #2 -- do I still hold it, now that I am about to close/land?
    sqlite3 ... "SELECT owner FROM tasks WHERE local_id='<task>'"

**And the ownership signal is not in TaskGraph alone.** Measured across two sweep
rounds on 14 candidate heads that TaskGraph reported free: **12 of 14 carried a
live ownership signal visible only on the pull request** — an author still pushing
after review (`#2478`), a landing-order-coupled pair one agent held both halves of
(`#2549`/`#2550`), `Claimed this head to take it end to end` in a comment
(`#2304`), two published verdicts (`#2551`), a terminal disposition (`#2368`,
`#2425`), a reviewer's `TAKING` note (`#2178`). A task-only view was wrong **most
of the time, not occasionally**.

The cheap form of the PR-side check, sorted newest first because a claim from
fourteen days ago and one from four minutes ago are not the same fact:

    gh pr view <n> --json assignees,labels,reviews,comments \
      --jq '.comments | sort_by(.createdAt) | reverse | .[0:3]'

Sort the candidate queue on `updatedAt`, never `createdAt`: a head created a week
ago and touched four minutes ago is the one most likely to have someone on it, and
creation order hides exactly that.

⚠️ **A closed duplicate task does not remove the head from routing.** Measured on
`#2327`: an agent closed a duplicate task as a duplicate, the routing layer handed
it out again anyway, and two agents ended up on the same head simultaneously — one
of them landing the content before the stand-down reached it. In that agent's own
words, *"closing a duplicate task does not stop the routing layer from handing it
out again. A closed task is not the same as a removed one, and I assumed it was."*
Neither agent was careless. **Do not treat an assignment reaching you as evidence
that the head is free** — claim-check the pull request itself.


### 15. On a head that has sat, diff the MERGE BASE — the main diff is the change PLUS everything main did meanwhile

Check 9 is about what a *merge writes*. This one is about what a *reviewer reads*,
and it fires earlier: before any merge is attempted, on the diff you open to
decide what the change even is.

`git diff origin/main <head>` on a stale head is not the change. It is the change
**minus** every commit main has gained since the base — which renders as
**deletions the author never wrote**. Two failure modes, opposite directions:

- the reviewer sees a wall of deletions and rejects a small correct change; or
- the reviewer accepts that reading, merges, and **reverts work that landed while
  the head sat**.

**Measured on four heads in one afternoon, 2026-08-25.** For each, the same head
read two ways:

| PR | merge-base diff (the change) | `origin/main` diff (change + staleness) | what the main-diff reading would have reverted |
| --- | --- | --- | --- |
| #2552 | manifests + derived artifacts | + `portable.json −9`, `portable-shards.json −1` | `check.backend_parity_suites`, landed **20 minutes earlier** |
| #2556 | **1 file, +118/−15** | 8 files, 252 deletions | the chaos ratchet landed **6 minutes earlier**, and 50 lines of THIS document |
| #2216 | **3 files, +1051/−10** | 30 files, 1,109 deletions | `c-programs.yaml −106`, `determinism-stress-c.yaml −54` — a ratchet landed the **same day** |
| #2549 | **1 file, +61/−4** | 65 files, **3,420** deletions | `overflow-gid-resolves.sh −60` — a regression cell landed the **same day** |

In every row the two readings tell opposite stories, and in every row the
merge-base diff is the honest one. Nothing in the tooling flags the difference:
both are valid `git diff` invocations that exit 0.

**The rule.**

```console
BASE=$(git merge-base origin/main "$HEAD")
git diff --stat "$BASE" "$HEAD"      # THE CHANGE — review this
git diff --stat origin/main "$HEAD"  # change + staleness — a MERGE-RISK signal only
```

Read the first to judge the change. Read the second only to answer a different
question: *how much has main moved under this head, and does anything it would
displace matter?* A large gap between them is not a finding about the author —
it is a rebase requirement.

> [!WARNING]
> **Take `$HEAD` from the branch ref, not `refs/pull/N/head`.** On #2216 the pull
> ref resolved 387 commits behind the branch ref, so a review taken from it
> diffed the wrong head against the wrong base and showed 418 files with 173,238
> deletions — none of them the author's. `git ls-remote origin refs/heads/<branch>`
> and the API agreed; only the pull ref was stale. Wrong head and wrong base
> compound.

This sits beside check 14 for a reason. That rule says **the task view is not
ownership**; this one says **the main diff is not the change**. Both are cases
where the view you reach for first is the wrong one, and both fail silently.


### 16. Split the population by whether the rule APPLIES, before counting violations

**Counting a population without splitting it by whether the rule applies is the
same error as counting green cells without asking whether they were measured.**
Both produce a large, alarming, technically-true number about a set that was
never obliged to satisfy the thing you counted.

**Measured 2026-08-25.** Asking whether the required adversarial-review set is
enforced, over the 30 most recently merged pull requests:

| reading | count | what it looked like |
| --- | ---: | --- |
| no `APPROVED-AT` line anywhere | 27 of 30 | "90% of landings bypass review" |
| …of those, **not triggered** at all | 28 of 30 | the protocol working exactly as designed |
| genuinely triggered | **2** | the only rows the rule speaks to |

`post-facto-human-review` applies to a specific trigger set. Twenty-eight of
those merges were never required to carry an adversarial review, so their
absence of one is compliance, not evasion. The real denominator was **2**, and
the alarming 27 was an artefact of counting a rule's violations across rows the
rule does not govern.

**And the second half of the same mistake: measure the property, not the token
that usually spells it.** That scan keyed on the literal string `APPROVED-AT:`.
hermit#2370 does not use it — it carries a full adversarial review ending
`**approve** — bound to exact head 19ab6f8b4287…`, which *is* the head that
merged. It was properly reviewed and the scan called it unreviewed. One of the
two remaining rows was a false positive, leaving exactly one real instance.

```console
# WRONG — counts the token, over every row
gh api ... | grep -c 'APPROVED-AT:'

# RIGHT — restrict to rows the rule governs, then match the property
#   1. filter to triggered PRs (the post-facto-human-review label)
#   2. accept ANY verdict bound to the exact head, however spelled
#   3. compare the bound sha against the sha that MERGED, not against the branch
```

**Second face of the same rule, found independently the same night, in a
different tool: a linter that keyed on the name of a local variable.** The
parent's `scripts/lint-rust-error-string-proxies.py` forbids branching on an
error's `Display` text. It rightly exempts the ordinary Rust idiom
`x.map_err(|e| e.to_string())?` inside a condition, where the `?` propagates and
nothing branches on the rendered string. But it decided that exemption with
`_is_error_name`, which accepts `err`, `error`, and anything ending `_err` or
`_error` — and rejects `e`, the commonest name in Rust for a bound error. So the
exemption keyed on **the spelling of a closure binding**. Five byte-equivalent
snippets, differing only in that name:

| binding | verdict |
| --- | --- |
| `\|e\|`, `\|ex\|`, `\|problem\|` | **reported** |
| `\|err\|`, `\|error\|` | clean |

A pure rename flipped the verdict. This produced all three findings the gate
held against `ci/manifest-plan/src/runner.rs` — two `if let Some(status) =
child.try_wait().map_err(|e| e.to_string())?` and one
`if entry.file_type().map_err(|e| e.to_string())?.is_dir()`. None compares an
error string; the branches are on `Some`/`None` and on a `bool`. Because the
parent's scan covers submodule sources, **a clean hermit reddened the parent's
lint, and the obvious repair — renaming correct variables here — would have put
the fix in the wrong repository.** The real fix was one function in the parent:
exempt when the identifier *bound* by the closure is the identifier being
rendered, whatever it is called.

Two details worth carrying:

- **It survived because both of the rule's own exemption tests spelled the
  binding `|error|`.** The suite only ever exercised the name the rule happened
  to accept, so the gap was invisible to the thing built to find it.
- **Do not fix a token-keyed check by widening the token set.** Accepting "there
  is a `map_err` somewhere" would have gone green immediately and blinded the
  gate inside every `map_err` closure. Key on the property: the bound error is
  the thing being rendered, so `map_err(|e| other.to_string())` still reports.

The two instances differ in a way that matters. hermit-004's kept a token in
**data** (`APPROVED-AT:` in a pull-request body); this one kept a token in
**code**, inside the checker itself. Same rule, and the second shows that
being a program does not protect a check from it.

This is the same shape as check 13 (an absent symbol may be a rename) and
check 12 (a comparison over nothing is not agreement). In all four the
mechanism was present and the *predicate* was wrong — a name, an empty set, a
spelling, a variable. A check whose predicate is a string will keep exiting 0
while measuring something nobody asked about.

### 17. A rejection set enumerated as VALUES silently narrows every time the value advances

An assertion of the form "reject these specific inputs" is a test that stops
testing. Each time the thing it guards moves, the listed values drift further
from the boundary that matters, and the test keeps passing while checking less.
It cannot fail for the reason its name gives, and nothing announces the moment
it went quiet.

**Derive the rejection set from the compatibility rule instead of listing it.**

Measured on `hermit-cli/src/metadata.rs`, the record/replay version gate — which
exists precisely to stop a build replaying a stream whose schema it does not
understand. Two instances, which is what makes it a pattern rather than a bug:

| where | assertion | what it actually says at `RECORD_VERSION = 0x10e` |
|---|---|---|
| `main` | rejects `0x10a`, `0x10c`, `0x105`, `0x110` | four fixed points, none near the boundary |
| `#2176` | rejects `0x109` only | true for **every** version except `0x109` |

`#2176`'s is the clearer illustration: `!compatible_with(0x109)` passes at
`0x10a`, `0x10e`, `0x10f`, `0x999`. The moment the constant leaves `0x109` the
test runs, passes, and asserts nothing about the schema advance it is named for.

The comparison rule is exact equality, so the property to assert is *every other
version is refused*. Re-derive the cases from the constant and the window travels
with it:

```rust
let current = RECORD_VERSION.0;
for delta in 1..=16u32 {
    assert!(!RECORD_VERSION.compatible_with(&RecordVersion(current - delta)));
    assert!(!RECORD_VERSION.compatible_with(&RecordVersion(current + delta)));
}
```

⚠️ **But check what the list was doing BY ACCIDENT before you delete it.**
`!compatible_with(0x10a)` also fails if someone *sets* `RECORD_VERSION` to
`0x10a`. A derived window cannot catch that — it re-derives from whatever the
constant currently says. Replacing the list without replacing that second job
trades a stale check for a weaker one, which is check-list-shaped goalpost
moving. Restore it explicitly, and prefer a **compile-time** assertion where both
sides are constants, because it cannot be skipped, filtered or left unrun:

```rust
const HIGHEST_SHIPPED_RECORD_VERSION: u32 = 0x10e;
const _: () = assert!(RECORD_VERSION.0 >= HIGHEST_SHIPPED_RECORD_VERSION, "...");
```

⚠️ **And the floor is itself an enumerated value sitting beside a moving one —
this check prescribes a weaker form of the disease it diagnoses.** Measured by
agent(hermit-007) against `#2549`, which implements exactly the guard above:

| step | build errors |
| --- | ---: |
| advance `RECORD_VERSION` to `0x112`, leave the floor at `0x10e` | **0** — silently accepted |
| then regress `0x112` to `0x10f` | **0** — the backward move is **NOT** caught |

After one missed update the floor guards below the last value someone remembered
to write down rather than below the current one, and nothing announces that it
went quiet — which is this check's own sentence, turned on its own remedy. The
floor is still strictly better than nothing: `#2549` does break the build on a
straight `0x10e -> 0x10b` regression. But prefer the DERIVED form. The highest
shipped version is recoverable from history as the maximum `RECORD_VERSION` ever
on `main`, so a CI assertion that the literal equals that maximum makes the floor
travel with the constant instead of with someone's memory.

The two guards are complementary, and each was demonstrated able to fail at
exactly what the other misses: regressing the constant `0x10e -> 0x10a` breaks
the **build** while the derived window passes; relaxing `compatible_with` to
tolerate one version of drift fails the **window** while the floor passes. If a
replacement guard cannot be shown to fail where the old one did, it is weaker,
whatever else it improves.

The hazard is not hypothetical. A long-stale branch that bumped `0x109 -> 0x10a`
against a base predating `main`'s advance to `0x10e` regresses the constant the
moment its conflict is resolved by taking the branch side — **and the same hunk
deletes the enumerated assertion that would have caught it.** The regression and
the loss of its detector arrive together, neither visible in the other's diff.

Generalises past version gates: any allowlist, denylist, skip list, or
`backends_disabled` map enumerated as values has this shape.

#### The merge rule this implies: constant FORWARD, set as a UNION

The two halves above are one finding seen from two directions, and the merge rule
falls out of it. When a branch and `main` have both edited a **monotonic
constant** and the **set that pins it**:

> Merge the constant **FORWARD** — to the maximum of the two, plus one if either
> side's schema changed. Merge the pinning set as a **UNION** — every value
> either side rejects, plus the other side's current value.

Taking one side of *both* is the failure, and it is worse than either alone
because **it loses a regression and its detector together**. The branch's
constant is lower, so "theirs" regresses it; the branch's rejection set predates
`main`'s recent values, so the same resolution deletes precisely the assertion
that would have gone red. Neither half is visible in the other's diff hunk, and
the merge is clean — no tool flags it.

Concretely, for the pair above: `0x10f`, not `0x10a`; and a rejection set that
still refuses `0x10a` and `0x10c` from `main` **and** now refuses `0x10e`.
Resolving that test by taking either side wholesale is wrong in both directions.

⚠️ **The same shape applies to a rationale comment.** A conflict can resolve
cleanly while deleting the paragraph that records *why* a value is what it is —
including an owner ruling written into the code after the branch forked. That is
not a merge error any tool reports; the result compiles, passes, and has quietly
lost the reason. When resolving a conflict in a file whose comments carry
decisions, diff the comment block separately from the code and carry it forward
deliberately.

### 18. A diagnostic makes CLAIMS — moving where it fires can falsify them silently

A log line is not decoration; in this project it is compared evidence. Its text
asserts two things beyond the words: **where it came from**, and **what was true
at the moment it fired**. Moving the emission site can falsify either without
touching a character of the message, and nothing fails.

Both halves, from one hunk in `#2304`:

```rust
// text unchanged, now emitted from `sched_loop_inner`:
info!("scheduler (step2_process_blocked): zero threads left anywhere, fizzling.");
```

1. **Wrong origin.** The message names `step2_process_blocked` and no longer
   comes from it. Anyone grepping the string to find the emitter — which is the
   normal way to trace a line back — lands in the wrong function. The string was
   accurate when written and became a lie by being moved.
2. **Narrowed predicate.** At the old site the condition was
   `futex_empty && timed_empty && blockers_empty`. The new site additionally
   requires `pending_run_queue_admissions.is_empty()` and
   `pending_run_queue_removals.is_empty()`. **That is not a pure move.** The line
   now means something stricter than it did, under the same words, so a reader
   comparing two runs across the change is comparing two different propositions.

There is a third effect worth separating, because it is the one that looks like
an improvement: the old site could fire MORE THAN ONCE per run; the new one fires
at most once, and the accompanying test asserts exactly one. **Collapsing a count
to a constant makes the evidence stream stable without making the system
deterministic.** Whether that is a fix or a loss depends on whether the count was
carrying information — here it was measured to be, so it became an owner
question rather than a review verdict.

**What to check when a diagnostic moves:**

- Does the message still name its actual emitter? Grep the string; land where you
  expect.
- Is the guarding condition the same, or has it gained or lost a conjunct?
- Can it still fire the same NUMBER of times? A per-occurrence line and a
  once-per-run line are different instruments even with identical text.

This is the same family as checks 12 and 14, and tonight it was the fourth
instance: a comparator over an empty set reporting agreement, two version gates
whose enumerated rejection sets had stopped covering the boundary, and this. Each
is **a check that runs, passes, and asserts less than its name says.** The
version-gate case suggests the general remedy where it is available — a guarantee
placed in the BUILD cannot be forgotten or enumerated wrong, whereas the same
guarantee in a test can be both. A diagnostic cannot be moved into the build, so
here the remedy is the checklist above rather than a stronger mechanism.

## `mergeable` is THREE-VALUED, and the third value is not an answer

`gh pr view --json mergeable` returns `MERGEABLE`, `CONFLICTING`, or **`UNKNOWN`**
(the REST field is `null`). GitHub computes mergeability *lazily*: the query does
not read a stored verdict so much as ask for one to be produced, so the first
read after a push is expected to be `UNKNOWN` and says nothing about the head.

**Both ways of collapsing the third value are wrong.** Treat `UNKNOWN` as
"cannot merge" and you block a head that is fine. Treat it as "can merge" and you
land on no information at all.

**Measured 2026-08-25, two heads polled at the same moment:**

| PR | observed |
| --- | --- |
| hermit#2587 | `UNKNOWN/UNKNOWN` at t+0s, `MERGEABLE/CLEAN` by t+20s |
| hermit#2588 | `UNKNOWN/UNKNOWN` at t+0, +20, +40, +60, +85, +110, +135s |

⚠️ **CORRECTION — the claim this section originally drew from hermit#2588 was
wrong, and the mistake is worth more than the claim was.** It said #2588 "merged
while `mergeable` still read `UNKNOWN`, which it still reads on the merged pull
request", and concluded the field can persist as `UNKNOWN` *through* a successful
merge. It cannot be concluded from that:

```console
gh pr list --state merged --limit 10   # then read .mergeable on each
  -> 10 of 10 read UNKNOWN
gh pr list --state open   --limit 8
  ->  8 of 8 read MERGEABLE
```

**GitHub stops computing mergeability when a pull request closes**, so *every*
merged PR reads `UNKNOWN`. The post-merge reading is therefore a constant and
says nothing about what the field held before the merge — and #2588's lander had
in fact polled `MERGEABLE CLEAN` immediately before merging it. The author read
an accurately-reported observation as support for an inference it could not
support, which is [check 12](#the-checks-in-order)'s shape committed inside a
section about verifying claims. The falsifying test was one `gh pr list` away and
was not run until a reviewer ran it.

**What is true, and is the reason to care:** the field is **not populated on a
merged pull request**, so it cannot serve as after-the-fact evidence of what was
true at merge time. If you need that, capture it while the pull request is open
or reconstruct it locally — the answer no longer exists in the API afterwards.

⚠️ **It also goes backwards on an OPEN pull request.** hermit#2587 read
`MERGEABLE/CLEAN` and then `UNKNOWN/UNKNOWN` minutes later while still open
(`state=OPEN mergedAt=null`, read immediately before — so this is not the
closed-PR effect above). `main` had advanced to `1417d7ce` in between; that is
the plausible cause but it was not isolated, so treat the observation as measured
and the explanation as inference. Either way a good reading is not durable: where
main churns roughly one commit every two and a half minutes, a `MERGEABLE`
fetched a minute ago describes a base that may no longer exist.

A lander therefore cannot wait for `MERGEABLE` and cannot read `UNKNOWN` as
`CONFLICTING`. Answer it locally instead.

Do not poll it unboundedly and do not infer from it. When it matters, **answer
the question locally instead** — this needs no API and works from a
proxy-blocked box:

```bash
git fetch -q --force <remote> \
    "refs/heads/<branch>:refs/tmp/mergecheck/head" \
    "refs/heads/main:refs/tmp/mergecheck/base" \
  || { echo "FETCH FAILED — not a merge answer" >&2; exit 2; }
git merge-tree --write-tree refs/tmp/mergecheck/base refs/tmp/mergecheck/head >/dev/null
case $? in
  0) echo MERGES-CLEAN; exit 0 ;;
  1) echo CONFLICTS;    exit 1 ;;
  *) echo "merge-tree ERROR — not a merge answer" >&2; exit 2 ;;
esac
```

That computes the same thing GitHub is computing, from objects you already have,
with an exit status you can read directly. `mergeable` is then a convenience to
be believed when it says `MERGEABLE` or `CONFLICTING`, and ignored when it does
not.

⚠️ **THREE DETAILS OF THAT RECIPE ARE LOAD-BEARING, AND THE OBVIOUS SHORTER FORM
LIES IN BOTH DIRECTIONS.** This section landed first with
`git fetch -q <remote> refs/heads/<branch>:h refs/heads/main:m` followed by
`git merge-tree --write-tree m h >/dev/null && echo MERGES-CLEAN || echo CONFLICTS`.
Measured on git 2.53.0, that form:

- **printed `CONFLICTS` for a ref that does not exist.** `merge-tree --write-tree`
  exits **1 for a real conflict and 1 for a bad ref alike**, so a typo'd or
  deleted branch is indistinguishable from a genuine conflict. Hence the `case`
  on the exit code rather than `&&`/`||`, and hence checking the fetch first —
  once both refs are known to exist and be fresh, `1` really does mean conflict.
- **printed `MERGES-CLEAN` about a base it never fetched.** `:h` and `:m` are
  unqualified destinations, so they resolve to *local branches*. With a
  pre-existing local `m`, the update is not fast-forward, **fetch refuses it,
  `-q` hides the message, the unchecked exit status is discarded, and
  `merge-tree` then answers about the ref the user already had** — measured
  output `fetch exit=1`, `m` still at the user's own commit, recipe prints
  `MERGES-CLEAN`. Hence `--force`, the namespaced `refs/tmp/mergecheck/*`, and
  the `||` on the fetch.
- **left two stray branches** named `h` and `m` in the reader's repository.

⚠️ **A FOURTH detail, added after the three above landed: the verdict has to be
in the EXIT STATUS, not only on stdout.** As first corrected, the `case` echoed
its verdict and then fell out of `esac`, so the block exited **0 for
`CONFLICTS`** as well as for `MERGES-CLEAN`. Extracted verbatim from the
committed document and run:

```console
feat (genuinely clean)          stdout=MERGES-CLEAN   exit status=0
conflicting (genuine conflict)  stdout=CONFLICTS      exit status=0
```

Which is a false green one layer up, at the caller — the same recipe wrapped in
the obvious `if`:

```console
if <recipe>; then echo LANDABLE; else echo NOT LANDABLE; fi
  feat        -> LANDABLE
  conflicting -> LANDABLE      <-- and it genuinely conflicts
```

Hence the explicit `exit 0` / `exit 1` / `exit 2`. **Three outcomes, and all
three have to be spellable by a caller that reads only the status**, or
*could-not-determine* gets read as one of the other two — which is the whole
argument of this section applied to its own remedy. This one survived the round
of review that fixed the other three, because every check run on it read the
printed word rather than the status.

⚠️ **And the reason checking the fetch is not optional once the refs are
namespaced.** `refs/tmp/mergecheck/*` does not appear in `git branch`, and it
persists between runs, so an unchecked fetch leaves a stale ref *about a
different branch* to answer from. Measured on the intermediate form — forced
refspecs, namespaced refs, no fetch check:

```console
step 1: check 'feat'      (genuinely clean)     ->  MERGES-CLEAN
        git branch now shows only:  * main
step 2: check 'conflicting', typed 'conflcting' ->  MERGES-CLEAN
step 3: check 'conflicting', spelled correctly  ->  CONFLICTS
```

Steps 2 and 3 ask about the same branch and disagree. `MERGES-CLEAN` about a head
you did not ask about is the same class as `MERGES-CLEAN` about a base you never
fetched. **It is a trade rather than a strict improvement, and the axis matters:**
the namespaced typo fails **loudly but invisibly-in-evidence** (`rc=128`, a
one-line `fatal: couldn't find remote ref` on stderr, and a stale ref that
`git branch` will not show you), while the old non-fast-forward fails **silently
but visibly** (`rc=1`, *zero* bytes of stderr, and a stale ref you can see with
`git branch`).
Both measured. Forcing the refspec without checking the fetch does not fix the
recipe; it moves the evidence somewhere a reader is less likely to look. Only
checking the fetch fixes it.

All three outcomes of the corrected form, measured in the same clone that broke
the original — **with the exit status, because the printed word is exactly the
channel this section says not to trust:**

    conflicting     -> CONFLICTS                         rc=1
    feat (clean)    -> MERGES-CLEAN                      rc=0
    no-such-branch  -> FETCH FAILED — not a merge answer  rc=2

⚠️ **The general rule, which is this document's own and was violated by its own
recipe: an exit code is a claim about the environment as much as about the code.**
A block offered as the reliable answer to a question the API answers unreliably
has to be at least as trustworthy as what it replaces, and `MERGES-CLEAN` about a
base you never fetched is worse than the `UNKNOWN` it was written to replace.

The local check does not have the durability problem either. Run at the same
moment GitHub reported `UNKNOWN` for hermit#2587, against the same two refs, it
returned MERGES-CLEAN — definitively, in one command.

Related: `mergeStateStatus` carries the same third value (`UNKNOWN`) and the same
caveat. And note the separate trap in the other direction — a head does not need
rebasing to be mergeable; see the two-commits-behind case below.
## Run the gate that covers the file's TYPE, not the one covering your change

⚠️ **Deliberately not a numbered check**, for the reason given below in the
two-lane section: numbered additions collide by construction.

**A new file joins a gate's population by existing.** So the gate you must run is
the one that covers the file's TYPE — not the one that covers your change's
subject. Adding a shell script to a Rust change puts you in the shellcheck
population whether or not you were thinking about shell.

The failure is quiet in a specific way: the gate you *did* run passes honestly,
because the thing it covers really is fine. Nothing lies. The population simply
grew somewhere you were not looking.

**Three instances in one night, all mine, which is why this is written down
rather than assumed obvious:**

| landed as | what I ran | what I owed | result |
| --- | --- | --- | --- |
| `b120fe5d76` (hermit#2573) | `rust-script --force … --help` | `scripts/check-script-sigpipe.sh` | two portable gates red on main |
| `962fad5e` (dev-hermit) | `rustfmt` on the changed files | the push gate's own file list | ten unformatted files landed |
| `e706d05d` (dev-hermit) | `shellcheck` on the four files that SOURCE the new one | `shellcheck` on the new file | `shellcheck` red on main, SC2148 |

Each has a different-looking cause and the same shape:

1. **`--help` proves compilation, not cleanliness.** A rust-script compiles when
   invoked, so `--help` really does prove it builds — which is why it feels
   sufficient. It runs no clippy. `check-script-sigpipe.sh` says so in its own
   failure text: *"No cargo gate compiles these, so cargo build/test/clippy/fmt
   will all still pass."*
2. **I formatted the files and the push gate agreed with me.** It had refused the
   push once, naming a file; after formatting it passed — and the commit it passed
   still contained ten unformatted files. Verified afterwards in a scratch worktree
   at that commit. A gate that passes what it just refused is worth its own
   investigation, but the lesson here is narrower: *its* agreement was not
   *evidence* I had run the right check.
3. **I checked the consumers, not the artefact.** The new file was
   `lint-exclusions.sh`, sourced by four enumerators. I shellchecked all four and
   not the file itself, which had no shebang and no `shell` directive.

**The check, and it is one question:**

> For every file this change ADDS — not modifies — which gate's population does it
> now belong to, and did I run that gate?

`git status --short` marks additions with `??` before staging and `A` after. That
list is the input to the question. Modified files are usually already covered;
**it is the added ones that move a boundary.**

This is the same family as the coverage-boundary checks elsewhere in this
document, seen from the author's side rather than the gate's: those ask whether a
gate's population is missing files it should hold, this asks whether YOUR file
just joined a population you never ran. A gate can be correctly defined and still
never see your work, because you never invoked it.

## Landing a two-lane head: the ordering

⚠️ **Deliberately not a numbered check.** Numbered additions collide by
construction — one was renumbered three times in a single night — and four heads
need this by name rather than by number.

An `APPROVED-AT` line names a sha. A push changes the sha. So a two-lane head can
be pushed out from under its own approvals faster than the second lane arrives:
measured on hermit#2478, **28 `APPROVED-AT: codex` lines in seven hours, none
naming the current head**, the most recent killed by a **pure rebase** whose
changed lines were byte-identical (8 files, 932 lines, each head against its own
merge base).

**That is an ordering failure, not an impossibility.** Two facts decide it, and
both are counter-intuitive enough to be worth measuring rather than assuming:

- **A head does not need rebasing to be mergeable.** hermit#2549 sat *two commits
  behind* `main` at `mergeable_state: clean`. Being behind is not an obstacle;
  only a **conflict** is.
- **`main` advancing does not invalidate an approval. Only a push to the branch
  does.** Conflating those two is what makes this look unsolvable.

### The ordering

1. **Rebase first, while the head is still unapproved.** There is nothing to lose
   at that point, and it is the only moment a rebase is free.
2. **Do not pursue a validate receipt.** It is the sole forcing function for a
   *later* rebase, and it is unsatisfiable under contention anyway — measured
   `wall=914s` queued, then `REFUSE` on freshness, because the target is fixed at
   submission while freshness is re-checked after an unbounded wait.
3. **Collect both lanes at that one sha, with no push to the branch between
   them.** Labels and comments do not move the sha; confirm that after any label
   edit rather than assuming it.
4. **Merge with `gh pr merge --rebase`.** It rebases *server-side* and never
   pushes the branch, so the bindings survive the merge itself.

### Verify before you merge, and record which signal you relied on

Check all three and require them to **agree**; do not treat any one as
authoritative while the head-versus-content ruling is open.

| signal | where | fails |
| --- | --- | --- |
| binding | newest `APPROVED-AT: <lane> <40-hex>` per lane, equal to the head | **closed** — a push makes it `SUPERSEDED` by construction |
| label | `passed-review-<lane>` | **open** — carries no sha, cannot expire; nothing strips it on push today |
| independence | the disclosure tag inside each binding comment | nothing checks it at all |

⚠️ **The independence check is yours to run by hand.** No gate performs it, and
the GitHub author is a shared machine identity, so only the tag in the comment
body distinguishes agents. **Pushing a head — even a purely mechanical rebase —
makes you an author of it for this purpose.** A reviewer landing a head they
reviewed is normal; an author binding their own lane is not.

### Evidence that this works

- **Existence:** hermit#2236 held both lanes at one sha (`dc8738bebc80`).
- **Single-lane, executed:** #2563, #2564, #2553, #2550 — rebase first, no
  receipt, poll to clean, merge directly.
- **Two-lane, executed end to end:** #2549 and #2566. On #2549 one agent supplied
  the claude lane at `45a96114aa8c` and flagged on the pull request that one
  codex line at the *same* sha with no push between would complete it; a second
  agent supplied exactly that an hour later. On #2566 the lanes arrived in the
  other order. Neither needed a rebase, a receipt, or a policy change.

## After you land: a merged flag says a merge happened, not that YOUR commit was in it

⚠️ **A commit pushed to a branch that is already in the lander's hands is a
SILENT loss.** The push succeeds. The pull request merges. The API reports
`merged: true`. The commit is simply not in the outcome, and nothing anywhere
reports that it was dropped.

Measured 2026-08-25 on hermit#2577. Three commits were pushed to
`ci/run-node-targeted-args`. The lander had already snapshotted and rebased the
branch to `220946c4ab18` — its FIRST commit only — and merged that. The two later
commits never reached `main`, while the pull request read as delivered.

**The detection is to grep `main` for the change's OWN text**, not to read the
merged flag:

    git show origin/main:<file> | grep -F "<a distinctive string from the commit>"

Run it once per commit, not once per pull request. On #2577 that produced:

    "Node-command arguments must follow a literal"   run-node.sh: YES   <- landed
    "exited 2 but for the wrong reason"              *** ABSENT ***
    "lane CPU budget onto"                           *** ABSENT ***

⚠️ **Choose the probe string carefully, or the check lies in the other
direction.** A fourth probe on the same sweep, `"The cost is one string per
check"`, reported ABSENT for text that had landed — the document says *"The fix
is one string per check"*. Pick a string you can see in `git show HEAD:<file>`
right now, and treat a single ABSENT as a prompt to re-probe before it is a
finding. This check has the same failure mode as everything else it protects
against.

**Two symptoms show the race before the merge, and both are easy to explain
away:**

- `git ls-remote origin refs/heads/<branch>` and the API's `head.sha`
  **disagree** — on #2577, branch ref `2bd6a473f3` against API head
  `220946c4ab18`. Do not write this off as GitHub lag; it was stable across
  re-reads twenty seconds apart. This is a second reason the standing rule is to
  take the head from the branch ref AND confirm it against the API, requiring the
  two to agree: the disagreement is itself the signal.
- the API head's parent is not the base you pushed from.

**Recovery is cheap if you catch it.** Cherry-pick the orphaned commits onto
current `main` in a new pull request; they apply clean, because the lander's
rebase carried the same base. Say in the description that this is not new work
and show the grep, so a reviewer is not re-reviewing a change they believe they
already approved.

**And the task-hygiene corollary.** `implemented` plus a pull-request link goes
stale the moment that pull request merges without your commit. The tags stay
technically accurate — `implemented` was true, `landed` was never claimed — and
the note still misleads, because a reader takes "PR #NNNN" next to a merged pull
request as delivery. Re-verify against `main` before treating a merged pull
request as delivery, and correct the note in place when it turns out not to be.

## A lenient reader turns a producer defect into an absence

⚠️ **Absence is indistinguishable from nothing-to-report.** A reader that skips
what it cannot parse, instead of failing, converts every upstream defect into a
clean, quiet, zero. The producer reports success, the reader reports success, and
the evidence simply never appears — so the pipeline looks *idle* rather than
*broken*, and idle is the one state nobody investigates.

**Worked example, measured 2026-08-25 between two halves that landed hours apart
the same night.** `ci/compat-envelope/pressure-test.rs` emits series rows;
`scorecard.rs project-observations` reads them. One row was emitted in exactly
the format the producer writes, and then:

```console
$ scorecard.rs project-observations --series-root <dir> --refreshed-at <stamp>
compatibility scorecard: projected 0 cell(s) from 0 series row(s) under <dir>
  note: the series is EMPTY, so every observation here remains PRE-SERIES evidence
  skipped <dir>/series/hermit2/devbig014/2026-08.jsonl:1: unknown field `schema`,
    expected one of `cell`, `first_divergent_scheduler_turn`, ...
$ echo $?
0
```

Two incompatibilities, either of which alone is fatal:

- **Shape.** The producer emits an enveloped row —
  `{schema, event_id, …, series:{cell, tree, run_index, outcome, coordinates}}`.
  The reader's `SeriesRow` is **flat** — `{cell, first_divergent_*}` — with
  `deny_unknown_fields`. No enveloped row can deserialize.
- **Key.** Even with the shape fixed, the producer's `series_cell()` builds
  `test/mode/backend` while the reader looks up `display_id()` =
  `lane/category/test/mode@backend`. Those are never equal — and `@` is not in
  the linter's `_CELL_RE`, so a row in the *reader's* key format could not pass
  the write boundary at all.

**What each layer said about it:** the producer succeeded. The reader exited **0**.
The summary said *"the series is EMPTY"* — it was not; it held one row that could
not be read. And the trailing advice, *"expected until plan step 4 lands a
producer"*, was stale: step 4 had landed minutes earlier. Only one line was true,
the `skipped` note, and it is a note rather than a failure.

⚠️ **AND THE OBVIOUS TEST PASSES.** "Emit a row and confirm the projection block
changes" is the natural check, and it **succeeds here** — the block is rewritten
with `rows_read: 0, pre_series_corpus: true`. A projection *over nothing* is still
a projection. The check that discriminates is narrower:

```console
# NOT sufficient — the block is written even when every row was skipped
#   did cells.json change?            -> yes, and it means nothing
# SUFFICIENT — assert on the destination, and on the skip count
#   did the TARGET CELL gain an observation?   -> the actual question
#   rows_read > 0, and skipped == 0            -> a skip is a failure, not a note
```

Here the target cell stayed `measurement: never-measured` with zero
observations, and the population-wide count stayed at 2 — while every surface
reported success.

**The rule.** When a reader may skip, a zero from it is never evidence of
absence. Either make the skip fail loudly, or assert on the *destination* rather
than on the reader's own summary — and count the skips, because a skip is the
defect wearing the costume of an empty input. This is the same family as check 12
(a comparison over nothing is not agreement): in both, the mechanism ran, the
exit code was clean, and the thing being asked about was never examined.

### 19. A guard whose cases all share one SHAPE tests the shape, not the property

A test file can be thorough, well named, and green, and still be blind to the
only input that matters — because every case in it is the same shape as every
other. The guard then measures that shape, not the property it claims, and
nothing in the file says so. Coverage counts do not reveal it: eight cases of one
shape look like eight cases.

**Measured on [hermit#2592](https://github.com/rrnewton/hermit/pull/2592).**
`ci/run-node-args-test.sh` had five reasoned refusals, a usage check and a
single-node edit — a careful file. **Every case passed a SINGLE node tag.** But
`ci-portable.yml` invokes the script with a comma-joined multi-node selection:
`preflight_nodes` is 11 tags and 251 bytes. That went into a scratch filename,
`"<lane>." + sel + ".effective.json"` cost 275 bytes against a `NAME_MAX` of 255,
and the write died with `OSError: [Errno 36] File name too long`. The whole
preflight job reddened while every single-node case stayed green.

The property is *any valid selection writes its scratch DAG*. The shape tested
was *one tag*. Those are not the same claim, and the file could not tell you
which one it was making.

**The remedy is a case of each shape the property must span, taken from the real
producer rather than written down.** #2592's fix reads the selection out of
`ci/portable-shards.json` instead of hardcoding 251 bytes, so the case cannot
drift from what CI actually passes. A hardcoded long string would have been a
third shape invented by the test author; the shard file is the shape production
uses.

The later one-DAG cutover removed this scratch-DAG command-replacement path
entirely. `ci/run-node.sh` now selects exact committed nodes only, refuses every
trailing replacement argument, and invokes `scripts/validate.rs`; its long-input
control still reads the real shard selection, but no longer creates or mutates a
second DAG.

⚠️ **The sibling failure is a guard whose cases all assert the same DIRECTION.**
A file where every case says "this must be refused" passes just as well against
an implementation that refuses *everything*. On
[hermit#2551](https://github.com/rrnewton/hermit/pull/2551) the tier-evidence
suite asserted refusal in eighteen cases; it needed one case asserting that a
genuine measured record still certifies, or a predicate that rejected all input
would have been green. Same for a diagnostic that names a conjunct: four cases
require the name and two require SILENCE, because without the second pair the
check passes by printing on every row.

So ask two questions of any guard before trusting it:

- **What shape is every case?** If they are all one shape, name the shapes the
  property must hold across and add the missing ones. Prefer reading the real
  input from its producer over composing one.
- **What direction is every case?** If they all assert refusal, add the case that
  must be accepted; if they all assert acceptance, add the one that must be
  refused. A guard that cannot be shown to pass *and* fail is measuring nothing.

This is check 12's cousin. Check 12 catches a comparison over an empty set —
agreement asserted about nothing. This catches a comparison over a set that is
non-empty but uniform: many observations of one case, reported as many cases.

### 20. Counting a marker's TEXT counts the arguments about it, not the marker

⚠️ **A grep for a marker matches every sentence that mentions it** — the request
asking someone to post one, the review quoting one, the correction explaining why
one did not bind. Those are discussion *about* the marker. The marker is a
binding. A count that cannot tell them apart reports the loudest thread as the
strongest evidence, and threads are loudest exactly where the situation is
contested.

**Measured 2026-08-25, twice, in unrelated contexts.**

On [hermit#2176](https://github.com/rrnewton/hermit/pull/2176) a lander checked
whether the codex lane had approved the current head:

```console
$ grep -ic "APPROVED-AT: codex $HEAD" bodies.txt
3
```

Three matches, and the head was one lane from landing. All three were prose
*requesting* the lane — `**Codex lane: please emit, alone on its own line,
`APPROVED-AT: codex b3ca4e15…`**` — plus a checklist item repeating it. Exact
whole-line matches: **0**. Worse, the same pull request carried a codex
*refusal*, so acting on the count would have solicited an approval from a
reviewer who had already said no.

The sibling instance is the merge-base symbol rule (check 3): a symbol counted on
`main` is not evidence of landing if it was already present at the merge base.
Same defect — a token counted in a context that does not mean what the count is
being used to mean.

**The check is one word: whole-line.** A binding is a complete line in a fixed
grammar, and a mention is not. Two greps show you the gap:

```console
grep -ci 'APPROVED-AT'                                bodies.txt   # mentions
grep -cE '^APPROVED-AT: (claude|codex) [0-9a-f]{40}$' bodies.txt   # NOT the bindings
```

⚠️ **When those two numbers differ, the gap is the finding, not noise.** It is
where people are arguing about a verdict that does not exist yet. On #2176 the
gap was 3 versus 0.

⚠️ **BUT NEITHER NUMBER IS THE BINDING COUNT, AND THE SECOND ONE IS LABELLED
ABOVE AS WHAT IT IS NOT.** This is the correction the check earned on itself.
The first draft of this section presented that anchored pattern as `# bindings`.
Three independent reviewers blocked on it, and they were right: **a pattern
stricter than the parser reports false absence** — the exact direction the third
bullet below warns about, committed by the recipe printed next to the warning.

`ci-hub/health/approval_binding.py` normalises before it matches. It strips
block-level prefixes (`#{1,6} `, `> `, and any of `-`, `+`, `*` as a list marker)
and wrapping emphasis or backticks **to a fixpoint**, then applies
`^APPROVED-AT:\s*(claude|codex)\s+[0-9a-f]{40}$` **case-insensitively**. Measured
against `strip_decoration` + `parse_marker` at `origin/main`, with a plain line
and a prose mention as controls in both directions — and replayed through the
route that actually decides, `marker_lines` → `_undecorate_block` →
`parse_marker`, because a table derived from a function pair nothing calls would
describe nothing; **10 of 10 rows agree between the two routes**:

| line, all naming the same head        | the grep | the parser |
| ------------------------------------- | -------- | ---------- |
| `APPROVED-AT: claude <sha>` (control)  | match    | **binds**  |
| `**APPROVED-AT: claude <sha>**`        | —        | **binds**  |
| `` `APPROVED-AT: claude <sha>` ``      | —        | **binds**  |
| `## APPROVED-AT: claude <sha>`         | —        | **binds**  |
| `- APPROVED-AT: claude <sha>`          | —        | **binds**  |
| `> APPROVED-AT: claude <sha>`          | —        | **binds**  |
| `APPROVED-AT:  claude <sha>` (2 spaces)| —        | **binds**  |
| `APPROVED-AT: CLAUDE <sha>`            | —        | **binds**  |
| `CODEX-LANE APPROVED-AT: codex <sha>`  | —        | —          |
| prose quoting the line (control)       | —        | —          |

**Seven real bindings missed, by four distinct mechanisms** — decoration, block
prefix, inner whitespace, and case. A reader following the old recipe on
[hermit#2608](https://github.com/rrnewton/hermit/pull/2608) got **0** on a head
that carries a valid exact-head claude approval, and would have concluded no lane
had approved.

**So the fix is not a better regex — it is not restating the grammar at all:**

```console
ci-hub/health/approval_binding.py --repo rrnewton/hermit --pr <n>   # bindings
```

⚠️ **A second copy of a grammar is check 14, and this document was the second
copy.** The parser's own header says a lander had duplicated this parsing, that
there were two parsers for one grammar, and that *"they had already drifted"*.
Writing the pattern into a checklist made a third. Policy may differ between a
board and a lander; the grammar may not. Keep the greps for the loose-versus-
strict *contrast* that exposes the mention/binding gap, and get the binding count
from the parser.

Four consequences worth keeping:

- **A quotation is not a retraction and not a re-assertion.** Deciding which
  requires reading, so a counter must not guess; it reports the whole-line count
  and the gap, and a human reads the gap.
- **This applies to absence too.** "Zero refusals" from a loose grep is as
  unsound as "three approvals" — a refusal written in a variant the pattern does
  not match is counted as absent, which reads as permission. Absence claims need
  the *authoritative* instrument, not merely a stricter one: strictness beyond
  the parser manufactures absence rather than proving it.
- **A tightening needs a control that must still match.** The old recipe's
  defect is invisible without one — every row above except the first and the
  last two is a silent miss, seven of them, and only the plain control
  distinguishes "this pattern is precise" from "this pattern matches nothing".
  The last two rows are not misses: the grep and the parser *agree* there, and
  the `CODEX-LANE` row is the case **neither** catches, so a bullet that swept
  it in would tell a reader the parser handles a variant it does not.
- **Say which instrument produced the number.** "2 of 185" means nothing without
  "whole-line match on raw comment text, not through the parser". A count is a
  measurement and carries its method or it carries nothing.

### 21. A new refusal needs a NEW exit code, or an explicit argument for sharing one

⚠️ **Borrowing a nearby exit code is locally plausible and globally wrong.** The
condition you are adding resembles one already on the list, the code is right
there, and reusing it costs nothing to write. What it costs is the caller's
ability to act: two conditions behind one code cannot be reacted to differently,
and if they could be reacted to identically they would not be two conditions.

**An exit code passed on from an INNER call carries the inner meaning, not the
outer one.** This is the specific trap. A guard that cannot resolve its input
says "could not determine" — truthfully, about itself. The caller reads that code
as the *tool's* could-not-determine, which is about something else entirely. Same
code, two meanings, one boundary.

**Measured 2026-08-25, three instances in one night, two of them added by the
same agent in the act of fixing the first.**

| condition | shared code | should be |
| --- | --- | --- |
| stale-TOOL refusal (the lander is not current) | 6 | 7 |
| stale-LABEL refusal (approval bound to another sha) | 6 | 6 |
| admission cannot resolve comments | 4 | 8 |
| merge outcome cannot be determined | 4 | 4 |

The second pair is the costly one, because the reactions are **opposite**:

- after an admission failure the merge was **never attempted**, so a plain re-run
  is safe;
- after a merge-outcome failure the merge **was** attempted and may have
  succeeded, so re-running risks a second landing.

A caller that treats them alike either re-merges something already on `main`, or
refuses to retry something that never ran.

⚠️ **And a shared code hides a dead code path.** While admission-cannot-resolve
and merge-cannot-determine both exited 4, three race cases in
`gh-merge-verified-test` passed on an exit 4 produced by the *admission guard* —
the race they claimed to exercise never ran. The assertion checking the CODE
passed; the two assertions checking the REASON failed. The code was right and
meant nothing. That is check 20's error inside a test suite.

**The rule, in enforceable form.** When you add a refusal:

- give it its own code, **or** write down why sharing is correct — and "they are
  both failures" is not a reason, it is a restatement;
- say what a caller should DO with it, not only what it means. A code without an
  action is a label;
- state which side of the retry boundary it falls on. *Can I re-run this safely?*
  is the only question most callers actually ask.

**The tell that you are about to do this:** you are adding a `die(N, ...)` and
you picked `N` by looking at the line above. Reaching for a neighbouring code is
the moment to check whether the neighbour means what you mean.

## A guard keyed on a PROXY rejects the safest cases first

⚠️ **When you enforce a rule, write down the PROPERTY you want, then check whether
you are testing it or testing something correlated with it.** A proxy is chosen
because it holds for the cases you had in mind. The cases you did *not* have in
mind are the ones that satisfy the property by a different route — and those are
usually the ones that satisfy it *more strongly*, because they got there
deliberately rather than by convention. So a proxy-keyed guard does not fail
randomly: **it rejects the safest inputs first.**

That is the opposite of how a guard is expected to fail, which is why it reads as
the guard working rather than the guard being wrong.

**Worked example, measured 2026-08-25, and it deadlocked the fleet.**
`ci-hub/bin/tg-note-verified` grew a rule against cross-agent note collisions:
`/tmp` is shared by every agent and the staging names are predictable
(`/tmp/note.txt`, `/tmp/ask030.txt`), so two agents can pick the same path and one
silently overwrites the other. The rule required a `--file` path under `/tmp` to
carry the calling agent's id.

- **The property:** *no other agent can have written this file.*
- **The proxy:** *the agent id appears in the filename.*

The proxy is sound for hand-written staging paths — agent ids are unique. But
`ci-hub/bin/close-task` stages its closure note through
`tempfile.NamedTemporaryFile(suffix=".note")`, which lands at
`/tmp/tmpXXXXXXXX.note`: a random name, created `O_EXCL`. That satisfies the
property **more strongly than namespacing does** — no other writer can have
produced or collided with it at all — and it contains no agent id, so the guard
refused it:

```console
$ ./ci-hub/bin/close-task <task> --disposition "..."
tg-note-verified: REFUSED -- unnamespaced staging path in a shared directory.
```

Every closure in the fleet died at the note write. **No task could be closed by
any agent**, and the landed-but-not-closed population started growing again by a
new route. Both guards were individually correct; the defect was in the
composition, and specifically in the proxy.

The repair is not to weaken the rule but to let a caller assert the property
directly: `TG_NOTE_PRIVATE_TEMP=1` is a claim only the process that created the
file can honestly make. A hand-written `/tmp/note.txt` still has no such creator
and is still refused.

**What to ask before shipping a guard.** For each rule, name the property and the
test, then find one input that has the property *without* passing the test. If you
cannot construct one, the test may be the property. If you can, that input is your
first false refusal and it is probably a tool, not a person — tools reach the safe
state deliberately and by an unusual route.

## After guarding a shared path, run the OTHER tools that use it

⚠️ **A guard is tested against the case that motivated it. Its blast radius is
every OTHER caller of the same path, and those are exactly the callers nobody
re-runs.** The motivating case is the one input guaranteed to be exercised; a
sibling tool that happens to use the same directory, filename convention, or
subcommand is guaranteed *not* to be, because it has nothing to do with the bug
you were fixing.

Measured 2026-08-25: two guards went into the task-note path within one hour —
one refusing unnamespaced `/tmp` staging paths, one requiring closures to go
through the verifier. Each was tested against its own motivating case and neither
was run against the other's caller. Together they deadlocked every closure in the
fleet, as described in the section above. The check that would have caught it was
one command: run `close-task` after installing the staging guard.

**The rule.** Before installing enforcement on a shared resource — a directory, a
CLI subcommand, a PATH shim, a file-naming convention — enumerate the other
programs that touch it and run one end-to-end invocation of each. `grep` the tree
for the path or subcommand you just constrained; the callers are the list.

This is the operational form of the proxy rule above. The proxy rule says your
guard will reject the safest inputs; this says the safest inputs belong to your
neighbours, so go and run your neighbours.

⚠️ **And install enforcement where it can be withdrawn in one command.** Both
guards live in a PATH shim installed as a *copy* at `/home/newton/orc-bin/tg`,
removable with `rm`. A guard that cannot be pulled quickly turns a false refusal
into an outage of unbounded length, and the deadlock above lasted only as long as
it took to diagnose because pulling it was always available.

## An instrument that only looks for good news reports a clean board

⚠️ **A checker built to recognise the success condition usually has no channel
for the failure condition, so "no problems found" is manufactured rather than
observed.** The positive marker gets a grammar, a parser and a counter; the
negative marker gets whatever falls through. Then absence of a finding is read as
absence of the fault, and the one state nobody investigates is the state the
instrument cannot represent.

This is not the same as a checker being wrong. Each instance below is *correct*
on everything it looks at. The defect is the shape of the question — and the third
instance is this document's own lane-status board, which merged a blocked head.

### Instance 1 — the malformed-binding detector shares the grammar's anchor

Before dev-hermit commit `8d03485cf8ce2eba307cc13248b9353a23091988`,
`ci-hub/health/approval_binding.py` carried a second regex beside the approval
grammar. Both were anchored at line start:

```python
approve = re.compile(r"^APPROVED-AT:\s*(claude|codex)\s+([0-9a-f]{40})$", re.I)
suspect = re.compile(r"^(?:APPROV|CHANGES-REQUESTED|REQUEST\s+CHANGES|…)[^\n]*\b[0-9a-f]{40}\b", re.I)
```

An early draft published a four-row result without recording either the source
commit or the command that produced it. That table is withdrawn: a parser result
without the parser bytes is not reproducible evidence. The underlying heading case
is independently established by the fix commit and its tests: the same leading
decoration defeated both the strict parser and the check intended to report malformed
marker lines. It was one observed input, not the whole input set.

⚠️ **THE TABLE MUST DISTINGUISH ASSERTIONS FROM MENTIONS.** Measured
2026-08-27 against dev-hermit
`a76221a934d500691b59ca0bcfe6ff85ec57b57e`, by driving `lane_shas` from
`ci-hub/health/approval_binding.py`, one comment per probe, role tag present,
`author_association=MEMBER`. The expected intent was assigned before running the
parser: an assertion row uses the marker line as the comment's review statement;
a mention row quotes, hides or discusses the marker rather than issuing it.

| line form | expected intent | parser sees assertion? | binds? | flagged malformed? |
|---|---|---|---|---|
| `APPROVED-AT: claude <sha>` — the control | assertion | **yes** | **yes** | – |
| `## APPROVED-AT: claude <sha>` | assertion | **yes** | **yes** | – |
| `+ APPROVED-AT: claude <sha>` | assertion | **yes** | **yes** | – |
| `* APPROVED-AT: claude <sha>` | assertion | **yes** | **yes** | – |
| `1. APPROVED-AT: claude <sha>` | assertion | no | no | **no** |
| `1) APPROVED-AT: claude <sha>` | assertion | no | no | **no** |
| `> > APPROVED-AT: claude <sha>` | mention | no | no | **no** |
| `<!-- APPROVED-AT: claude <sha> -->` | mention | no | no | **no** |
| `<p>APPROVED-AT: claude <sha></p>` | assertion | no | no | **no** |
| `- [x] APPROVED-AT: claude <sha>` | assertion | no | no | **no** |
| `• APPROVED-AT: claude <sha>` | assertion | no | no | **no** |
| `<U+200B>APPROVED-AT: claude <sha>` | assertion | no | no | **no** |
| `Posting APPROVED-AT: claude <sha>` | mention | no | no | **no** |

Of the ten hand-labelled assertions, four bind and **six are silently missed**.
The three mention rows are correctly ignored and are controls, not missed
verdicts. The numbered-list rows alone demonstrate the remaining failure: a
reviewer can use a Markdown list item as the complete review statement while the
parser neither binds it nor reports it malformed. This result does not treat every
line containing marker text as an assertion.

**The block-level-prefix case was fixed, and the fix validated the reasoning.**
`8d03485c` strips block-level prefixes before matching. Direct execution before
the change at dev-hermit `9c735c1364cb02832058c3bcd441474c99a73acb` and
after it at `8d03485cf8ce2eba307cc13248b9353a23091988` produced:

| line form | before `8d03485c` | after `8d03485c` | control |
|---|---|---|---|
| `## APPROVED-AT: claude <sha>` | did not bind, not flagged | **binds** | valid payload is accepted |
| `## APPROVED-AT: reviewer <sha>` | did not bind, not flagged | does not bind, **flagged malformed** | invalid lane is rejected |
| `## Posting APPROVED-AT: claude <sha>` | did not bind, not flagged | does not bind, not flagged | prose mention remains a mention |
| `## CHANGES-REQUESTED-AT: claude <sha>` after an approval | **approval stood** | clears the approval; lane is **UNDETERMINED** | the refusal affects admission |

That after-state changed admission, not distinct reporting. At `8d03485c`,
`lane_shas()` had no refusals out-parameter: both a heading refusal after an
approval and a lane with no review returned an empty list, and
`classify_approval()` reported both as `UNDETERMINED`. Dev-hermit commit
`7820d2be4b5378d16bef450fd0c1a3bc84bd356b` later added the separate `REFUSED`
state and the refusal output needed to distinguish those cases.

The refusal direction is the one that mattered and it was not in the original finding:
a *block* wearing markdown could leave an earlier approval standing. This was a real
failure mode, but it was **not** the mechanism in
https://github.com/rrnewton/hermit/pull/2591. That pull request's refusal was plain,
exact-head and parsed; the landing path failed to invoke the refusal gate.

⚠️ **The supportable claim is narrower: normalization leaves the marker
payload grammar strict.** A valid lane and full SHA still have to match after the
block-level prefix is stripped. The positive control above proves that a valid
heading-form payload is newly accepted; the invalid-lane control proves that
normalization does not bypass the payload grammar; the prose control proves that
one surrounding-word case remains a mention; and the refusal control proves that
the same widened context no longer lets an earlier approval stand. The accepted
syntactic context did widen, so this change did admit new lines and does require
positive and negative controls rather than a claim that no corpus re-audit is
needed.

The positive and negative paths must be measured separately. The current
`lane_shas` measurement above shows that a leading word still neither binds an
approval nor reaches the malformed counter. A refusal gate may deliberately accept
more spellings in order to fail closed, but that result does not establish what the
approval parser accepts.

⚠️ **`scripts/core-review-protocol-lint.sh` IS NOW A SECOND CONSUMER OF THIS
GRAMMAR.** The historical version measured in this section read only supplied
labels and required PR-body sections. The current script also reads the exact
pull-request head and complete issue-comment history, parses `APPROVED-AT`,
refusals and withdrawals, and rejects a standing reviewer-owned refusal. Its
self-test must therefore cover both directions, and changes must still be
compared with dev-hermit's pinned `approval_binding.py` authority.

⚠️ **PIN THE PATH AND COMMIT BEFORE QUOTING THIS BEHAVIOR.** The parser changed
several times while this section was reviewed. The current table above names both:
dev-hermit `ci-hub/health/approval_binding.py` at
`a76221a934d500691b59ca0bcfe6ff85ec57b57e`. A statement that names only
`approval_binding.py` cannot be reproduced after the file moves again.

### Instance 2 — a detector gated behind the label could not see missing bindings

Before dev-hermit commit `7820d2be4b5378d16bef450fd0c1a3bc84bd356b`,
the default fleet scan called `lane_shas()` only for pull requests already carrying
a tracked `passed-review-*` label. The scan therefore examined heads already
credited with a lane and omitted heads whose binding failed badly enough never to
earn the label.

One historical measurement reported:

```console
$ approval_binding.py --repo rrnewton/hermit --json | jq .malformed_source_lines
0

$ # independent sweep of every PR touched that day, counting lines where an
$ # APPROVED-AT keyword shares a line with a 40-hex sha but the grammar rejects it
[withdrawn -- see below]
```

⚠️ **THE HEADLINE COUNT THAT STOOD HERE — "18 unparseable attempted bindings across
14 pull requests" — IS WITHDRAWN BECAUSE IT DOES NOT REPRODUCE.** It was published
with no query, no cutoff and no exclusion rule, so nobody could check it. Three
independent attempts have now produced three answers:

| who | population and grammar | result |
|---|---|---|
| the original claim | unstated | 18 lines / 14 PRs |
| `agent(codex-rev-2602)` | repository issue comments on 2026-08-25, cut at this pull request's creation time 18:23:15Z | broad keyword **25 / 17**; exact `APPROVED-AT:` token **14 / 8** |
| `agent(hermit-139)`, 2026-08-27 | `gh api repos/rrnewton/hermit/issues/comments?since=2026-08-25T00:00:00Z --paginate`, 75,584 comment rows, line must contain a 40-hex sha and fail `^APPROVED-AT:[ \t]*(claude\|codex)[ \t]+[0-9a-f]{40}$` | broad **14 / 13**; exact token **4 / 3** |

The three disagree because the population and the grammar are unstated, and mine
differs from the codex lane's partly because I did not cut the corpus at the same
instant. **That is the point, and it belongs in this document rather than being
quietly repaired: a number published without its query is not evidence, and this
one was in a document about instruments that report confidently wrong figures.**

The historical mechanism does not depend on the withdrawn count: a binding that
failed badly enough never to earn a label was exactly the binding the scan could
not see.

**Fixed in `7820d2be4b5378d16bef450fd0c1a3bc84bd356b`.** The current fleet scan also
calls `lane_shas()` for unlabelled Claude and Codex lanes and retains a row when it
finds either an approval or a refusal. Targeted `--pr` mode does the same. The
population is no longer selected only by the success label it is checking.

### Instance 3 — a sweep that extracts only the success marker cannot see a block

⚠️ **This one caused a bad merge, and the instrument was this document's own
lane-status board.**

The board was built by extracting `APPROVED-AT:` lines and comparing them to the
head. It never extracted `CHANGES-REQUESTED-AT:`. Both markers share a grammar, a
comment stream and a chronology; only one was read. So a head carrying a bound,
current, correctly-formatted **block** rendered on the board exactly like a head
that had simply not been reviewed yet — and a head carrying an approval *followed*
by a block rendered as **ready to land**.

**https://github.com/rrnewton/hermit/pull/2591, measured.** A reviewer posted a
plain `CHANGES-REQUESTED-AT: claude <head>` at the exact current head. The marker
parsed; the landing path did not invoke the refusal gate. The lane-status still
showed the earlier approval, a lander read "carries a valid non-author lane", and
the head merged over the block at 18:09:01Z. The content that landed was worse than
no change: three defects the block existed to stop, including a recipe that fails
on its second run.

**https://github.com/rrnewton/hermit/pull/2176, the same blindness caught before it
cost anything.** Re-running the sweep with both markers showed `claude=BLOCKED` at
the current head. The approval-only board had it as "awaiting a codex lane" — which
would have routed the only codex-family agent on the fleet onto a blocked head.

Current `approval_binding.py` returns approvals and outstanding refusals separately,
so a consumer can distinguish `REFUSED` from an unreviewed lane. The defect was in a
downstream sweep that reimplemented only the success half of the authority's
grammar.

**Fix:** extract both verdict markers and preserve issuer-owned refusal handling. A
later approval by a different reviewer does not discharge another reviewer's
refusal; the refusing reviewer must approve or withdraw it. A lane then has three
states, not two, and a board that cannot distinguish them must say so on its face
rather than render blocked and unreviewed identically.

### The fix shape, and it is already in this codebase twice

All three instances share one repair: **give the negative outcome a first-class value
and a counter of its own, sourced independently of the positive path.** This
project has built that twice already, and they are the models to copy.

- **`detcore/src/logdiff.rs` and `hermit-cli/src/bin/hermit/logdiff.rs`.** Detcore's
  `matched_with_evidence()` requires nonzero compared-message counts; its tests
  `nothing_written_yet_is_a_no_result_not_a_match` and
  `empty_selection_is_a_no_result_not_a_match` pin the negative direction. The CLI
  gives the resulting states names: an empty selection is `no_comparable_messages`,
  an input it cannot compare is `refused`, and `no_result` is the report written
  before comparison starts so a crash cannot leave a false match.
- **dev-hermit `ci-hub/health/approval_binding.py` — `landing_observability()`.** At
  dev-hermit `a76221a934d500691b59ca0bcfe6ff85ec57b57e` it refuses an empty window —
  *"zero commits measured is not zero unobserved landings, it is no measurement"*
  — and emits the literal string `"NOT MEASURED"` when coverage cannot be
  established. This is a separate repository and the commit is part of the claim.

### The check to apply

For any guard, gate, counter or comparator, ask:

1. **What does it output when it cannot evaluate?** If that is indistinguishable
   from a pass, it only looks for good news.
2. **Does its negative channel share the positive's parsing, gating, or source of
   truth?** If so it inherits the blind spot, and the two will fail together on
   exactly the inputs that matter. Independence of the negative channel is the
   property, not merely its existence.
3. **Is the population it scans defined by the thing it is measuring?** A detector
   that only inspects already-credited artifacts cannot find the uncredited ones.

⚠️ **And report the denominator.** "Zero malformed" and "zero out of a population
I could not enumerate" are different claims, and only one of them is evidence.

## Verdict vocabulary

| verdict | meaning | action |
|---|---|---|
| **genuinely pending** | absent by content, mechanism absent, no residual question | land it |
| **already landed** | mechanism present on main AND no residual content | close, citing the landing commit |
| **partial** | mechanism landed, unique content remains | extract the residual; do NOT close |
| **indeterminate** | rule cannot decide — no extractable symbols, or every symbol pre-existed | human read; do NOT guess |
| **blocked on a cross-repo dependency** | content and mechanism both absent, but the capability its tests assert has not landed in `reverie` or `agent-utils`, or hermit's pin does not include it | LEAVE OPEN and NAME THE SUCCESSOR; landing it creates a red |
| **blocked on a defect** | genuinely pending and wanted, and part of its claim is VERIFIED, but landing would make a gate ASSERT something a defect elsewhere makes measurably false | LEAVE OPEN and NAME THE DEFECT with its smallest reproduction; record which part of the claim is verified and which is not |
| **supersede-and-regress** | landing would weaken newer invariants, remove a current dependency, or rely on a prerequisite that is now false | do NOT port; extract any residual, then close citing the newer mechanism or dependency |

Docs-only and config-only pull requests add no symbols and land in
**indeterminate** by construction. That is a correct verdict, not a failure of
the check, and those rows should be collected for one batched decision rather
than guessed at individually.

## Four dispositions, not two

A sweep that can only close or land will record the other outcomes as nothing,
and **four supersession chains dead-ended that way in one night** — each one a
pull request whose real status was known by somebody and written down nowhere.

| disposition | when | what to record |
|---|---|---|
| **close** | already landed, or abandoned and unsafe to port | the superseding commit, or why porting is unsafe |
| **land** | genuinely pending and landing improves main | the usual receipt |
| **correctly parked, with a NAMED SUCCESSOR** | real work, correctly blocked — a cross-repo dependency, an unlanded capability, a decision it waits on | **WHO or WHAT unblocks it**, by name |
| **blocked on a NAMED DEFECT** | real work, correctly blocked by something BROKEN rather than something pending | **the defect, reduced to its smallest reproduction**, plus which part of the claim is already verified |

The third is not a failure to decide. It is a decision, and it is only useful if
the successor is named: "waiting on reverie#430 to land and hermit's pin to
advance" is a disposition; "still open" is not. A parked pull request with no
named successor is indistinguishable from a forgotten one, which is exactly how
the four chains were lost.

### The fourth: blocked on a defect, which is not the same as parked

Parking waits for something **pending**. This waits for something **broken**, and
the difference decides who can act. A parked row has a successor to watch. A
defect-blocked row has nobody watching anything until the defect is written down,
so the reproduction IS the disposition — without it the row is indistinguishable
from "the author gave up".

It is also not **supersede-and-regress**. There the port is wrong and should be
abandoned. Here the port is RIGHT and wanted; it simply cannot be asserted yet,
because a gate would state as true something that is measurably false.

The distinguishing question: **would landing make a check CLAIM something?** A
change that only alters behaviour can be judged on behaviour. A change that
alters what a gate ASSERTS must be judged against whether the assertion holds on
every backend or configuration it will now speak for.

Measured, on [hermit#2342](https://github.com/rrnewton/hermit/pull/2342), which
flips the backend-parity matrix from a `stripped` DETLOG comparison to canonical
`--verify-strict` / `bitwise`:

- **Verified for ptrace:** 28/28 cases reached bitwise, exit 0. The claim is true
  where it could be measured.
- **False for DBT, and not because of the pull request:** all 25 DBT cases failed
  — *identically, with and without the change*. The two DBT logs were
  byte-identical once large integers were normalised; the only difference was the
  thread id. DBT emits the **host TID** where ptrace emits a virtualized
  `DetPid` (`dettid 1163782` vs `1163891` across two runs of `/bin/true`, against
  `dettid 3` twice under ptrace), so a DBT DETLOG can never match across runs
  under *either* policy.
- **Why that blocks landing:** `expected_non_kvm_tier` is one value for all
  non-KVM backends, so landing puts "requested policy is L2" in the runner's own
  banner for a backend measured unable to deliver it.

Recording that as "still open" would have lost it. Recording it as
`dbt-pid-virtualization`, reduced to a single-threaded `/bin/true`, makes it
actionable by someone who never read the pull request.

**Name the defect, not the symptom.** "DBT parity fails" is a symptom that
invites a rebase. "DBT emits the host TID instead of a virtualized DetPid" is a
defect somebody can fix.

## What sweeps have actually returned

**Measured, not assumed: six sweeps across `hermit`, `reverie` and `dev-hermit`
found ZERO clean already-landed closes.**

A reader arriving at a queue of 88 open pull requests will reasonably assume it
is full of duplicates. It is not, and this table exists so nobody spends another
night establishing that:

| repository | outcome |
|---|---|
| `dev-hermit` | reached zero by **merging**, not closing |
| `reverie` | all 23 open classified; **zero** already-landed, 16 genuinely pending, 2 indeterminate |
| `hermit` | sweeps found **one** closable (`#2178`); several land candidates |

`#2178` is the first genuinely closable hermit row any sweep has returned, and it
took check 13 to see it: every cheaper signal said PARTIAL. Treat "zero closable"
as the strong prior it has earned, not as a rule — but note that the one
exception was found by hashing a body, not by counting files or names.

**Two near-misses, both caught by the residual check (check 3), both PARTIAL
rather than closable.** One had its mechanism landed, both test files present on
main, and its target manifest deleted in a format migration — every signal
saying close — and still carried 173 non-comment lines of a distinct test
scenario. The other was superseded by a commit 28 hours older and would have
regressed main, yet still held two standalone C guests absent from main.

The queue is **unlanded work, not duplicates**, and the shortage is **rebases,
not candidates**. Plan sweeps on that basis: budget for rebasing, extracting
residuals and naming successors, not for closing.

## Four fields on a pull request whose plain reading is wrong

⚠️ **The GitHub pull-request API has at least four fields a reviewer naturally
reads as an answer, and each of them answers a different question than the one
being asked.** They are collected here because they were found one at a time,
scattered across four sections and five pull requests in a single evening, and a
reviewer who learns them one at a time learns each one by being burned by it.

| field | reads as | actually means |
| --- | --- | --- |
| `mergeable` | can this land | *computed at some past moment*, `UNKNOWN` until asked, and never recomputed once the pull request closes |
| `merged` / the merged flag | my commit is on `main` | **a** merge happened on this pull request; says nothing about which commits were in it |
| `merge_commit_sha` | it merged, here is the commit | **populated on pull requests that were never merged** |
| `head.sha` | the head I am reviewing | the head has not *moved*; says nothing about whether it is still *open* |

The first two have their own sections above --
[`mergeable` is THREE-VALUED](#mergeable-is-three-valued-and-the-third-value-is-not-an-answer)
and the merged-flag section. The last two are recorded here.

### `merge_commit_sha` is populated on a pull request that never merged

Measured 2026-08-25 on hermit#2604, closed as obsolete rather than merged:

```console
state             closed
merged            False
merged_at         None
merged_by         None
merge_commit_sha  aa565ed86abf922b396749ee334c2f546738dd4e   <-- populated
```

And the content is verifiably absent from `main`: grepping `origin/main` for a
distinctive string from the change returns **0**.

So a populated `merge_commit_sha` is not evidence of a merge, and a sha that
looks like a landing is not a landing. GitHub computes a *candidate* merge commit
to answer the mergeability question, and the field keeps that value whatever
happens to the pull request afterwards. **Check `merged`, and then check `main`.**

### `head.sha` agreeing does not mean the head is still open

The standing rule for supplying a review lane is to resolve the head from the
branch ref **and** the API and require the two to agree, because a head that
moves under a reviewer invalidates the binding. That rule is correct and it is
not sufficient: **a sha stays valid on a closed pull request**, so agreement
proves the head has not *moved* and says nothing about whether it is still
*alive*.

Measured 2026-08-25, and the author of this section is the one who got it wrong:

```
18:48:58Z  author posts "Closing: obsolete"
18:48:59Z  pull request CLOSED, merged=false
18:50:13Z  reviewer posts APPROVED-AT + passed-review-codex   <-- 74s too late
```

The reviewer re-read the head immediately before posting, exactly as the protocol
requires. The API returned `state: closed` **in the same response the sha was
read from**. The sha was checked; the state was not.

**The rule is therefore: re-read the head AND the state.** One extra field, in a
request already being made. An approving lane on a closed pull request is not
harmless -- it is an approval a scanner will count, on a head nobody can land,
and it has to be retracted by hand with a `CHANGES-REQUESTED-AT` at the same sha.

### The shape they share

Each of these fields reports something true. `mergeable` really was `UNKNOWN`
when asked; the merge really did happen; the candidate merge commit really was
computed; the head really has not moved. **The defect is never that the field
lies -- it is that the question a reviewer is asking is one step further on than
the question the field answers.** So the check is the same in all four cases:
name the question you actually need answered, and confirm the field answers
*that* one, rather than one adjacent to it.

## A hold keyed on "the related task is still open" can never clear

⚠️ **If the board withholds a head because a task is open, and that task is the
one the head closes, the hold is self-sustaining: the change that would close the
task is the change the hold is preventing.** It never times out and it never
resolves, because nothing else can close the task either.

**Measured 2026-08-25.** hermit#2594 was held on `hoist_the_backend_name` being
open. That task's stated fix is the two-line hoist in
`hermit-cli/src/backend_stats.rs` that hermit#2594 *is*. The head landed at
18:17:32Z as `9fd184ad6e` and the task closed against that commit at 18:46Z — the
hold's own precondition was satisfied only by defeating the hold.

**The test to apply before adding any task-state hold:** ask what closes the task
if the hold holds. If the answer is "the thing being held", the hold is circular
and must not be added.

Two properly-shaped alternatives, neither circular:

- Key the hold on the task being **unowned or unclaimed** — nobody is driving it,
  which a landing does not fix.
- Key it on the head **not naming a task at all**, which is a hazard landing
  genuinely does not resolve.

**The same MISREADING is recorded elsewhere, though not the same failure.**
`ci-hub/landing/task_pr_join.py` required `_task_is_open()` and so "excluded
exactly the population this join exists to find, so population C silently emptied
as the lifecycle took effect". Both mistakes share one premise: **an open task is
evidence that work is OUTSTANDING, and a head is how outstanding work stops being
outstanding.** Reading the first as a reason to withhold the second inverts what
the signal means.

⚠️ **But the two are NOT the same shape, and an earlier version of this section
claimed they were — failing its own test.** A reviewer applied the test above to
the precedent, and it does not have the property:

| | board hold | `task_pr_join.py` |
| --- | --- | --- |
| what is withheld | **the head itself** | a row in a *report* |
| what closes the task | the withheld head | the head, which nothing withheld |
| loop? | **yes — deadlock** | **no** — the task can still be implemented, landed and closed |

The join emptied a population; it never prevented anything from landing. So it is
a precedent for the misreading and **not** for the circularity, and citing it as
"the same shape" overstated the case in exactly the direction that makes a
section persuasive rather than true.

⚠️ **A stated reason is not a checkable reason, and this hold proves the
distinction.** An earlier version of this section also claimed the hold was
invisible — that "nothing in the board's own output said 'held, because a related
task is open'". **That was false, and a reviewer falsified it from the board's own
rows:**

```
#2594  needs both  -> HELD by hoist_the_backend_name
```

The precondition was printed, by name, in the row. What the row did not say — and
could not — is that the precondition was **unsatisfiable**, because seeing that
requires knowing the task's CONTENT: that the fix it names is the head being held.
A reader has to hold both facts at once, and the board shows one of them.

So the lesson is narrower than "make holds visible", and more useful: **a hold's
reason being printed is not evidence the reason is reachable.** Every circular
hold will look perfectly well-documented in the row that states it.

⚠️ **And the legibility is what killed it, within seven minutes.** The row is how
this surfaced: a reviewer declined the head *citing that row* at 18:00:58Z, and
`agent(hermit-triage)` challenged the hold *by quoting it* at 18:06:30Z. The
earlier version credited "a human read the board and asked why the head had not
moved" — also false; the record shows agents, prompted by the row. Corrected
because the true story is the stronger one: the printed reason did its job, and
printing the reason still was not enough.

## 22. Run the review-protocol lint by hand; no pull-request event runs it

`scripts/core-review-protocol-lint.sh` is called by the `core-review-protocol` job
in `.github/workflows/merge-gate.yml`. That workflow's only trigger is
`workflow_dispatch`, so **no automatically triggered pull-request job runs the
lint against a real pull request**. The Makefile schedules
`scripts/core-review-protocol-lint-test.sh`, which tests fixtures rather than a
live pull request.

The distinction still matters, but the script now checks the complete input it
needs: labels, body, pull-request number, current head, issue comments, and a
Boolean saying whether changed paths or labels identify a KVM change. For a
`post-facto-human-review` pull request it requires each review lane's trusted
approval to bind the supplied exact head and refuses outstanding reviewer-owned
change requests. An exit 0 means that supplied snapshot passed those checks, or
that the post-facto label is absent; it is not evidence that the snapshot fetch
itself succeeded unless every guarded command below succeeded.

**The earlier label/body-only lint was still useful.** Measured from
`2026-08-27T07:40:16Z` through `2026-08-27T07:42:08Z`, using the checker and
workflow at fetched `main` SHA
`63b778646101bb7228a1fc1674c937274b75aab4`. The exact GitHub search window was
`2026-08-25T00:00:00Z..2026-08-25T23:59:59Z`:

```console
with-proxy gh api --method GET search/issues \
  -f q='repo:rrnewton/hermit is:pr is:merged merged:2026-08-25T00:00:00Z..2026-08-25T23:59:59Z' \
  --jq '{total_count}'
with-proxy gh api --method GET search/issues \
  -f q='repo:rrnewton/hermit is:pr is:merged merged:2026-08-25T00:00:00Z..2026-08-25T23:59:59Z label:post-facto-human-review' \
  --jq '{total_count, numbers:[.items[].number]}'
```

The raw results were:

```json
{"total_count":220}
{"numbers":[2568,2534,2517,2486,2434,2407,2373,2370,2312,2236,2172,2150],"total_count":12}
```

Fetching each of those twelve pull requests' live labels, body, and complete
paginated changed-file list, then supplying `PR_IS_KVM`, produced:

| historical label/body lint outcome | count | pull requests |
| --- | --- | --- |
| blocked | **9 of 12** | #2568 #2517 #2486 #2434 #2407 #2312 #2236 #2172 #2150 |
| passed | 3 | #2534 #2373 #2370 |

Run the current lint before landing a labelled pull request. Every fetch is
checked, including the file list used to derive `PR_IS_KVM`; a failed fetch is an
error, never a non-KVM answer:

```console
set -euo pipefail
repo=rrnewton/hermit
pr=<n>

if ! pr_json=$(with-proxy gh api "repos/$repo/pulls/$pr"); then
  echo "pull-request fetch FAILED -- this is not a pass" >&2
  exit 2
fi
if ! labels=$(jq -r '.labels[].name' <<<"$pr_json"); then
  echo "label decode FAILED -- this is not a pass" >&2
  exit 2
fi
if ! body=$(jq -r '.body // ""' <<<"$pr_json"); then
  echo "body decode FAILED -- this is not a pass" >&2
  exit 2
fi
if ! head_sha=$(jq -r '.head.sha' <<<"$pr_json"); then
  echo "head decode FAILED -- this is not a pass" >&2
  exit 2
fi
pr_comments_file=$(mktemp)
trap 'rm -f "$pr_comments_file"' EXIT
if ! with-proxy gh api "repos/$repo/issues/$pr/comments?per_page=100" \
    --paginate --slurp | jq -ce 'add // []' >"$pr_comments_file"; then
  echo "comment fetch FAILED -- this is not a pass" >&2
  exit 2
fi
if ! files=$(with-proxy gh api --paginate "repos/$repo/pulls/$pr/files" \
    --jq '.[].filename'); then
  echo "changed-file fetch FAILED -- cannot decide whether this is KVM" >&2
  exit 2
fi

if grep -qiE 'kvm' <<<"$files" || grep -Fixq kvm <<<"$labels"; then
  is_kvm=true
else
  is_kvm=false
fi

lint_status=0
PR_NUMBER="$pr" PR_LABELS="$labels" PR_BODY="$body" PR_IS_KVM="$is_kvm" \
PR_HEAD_SHA="$head_sha" PR_COMMENTS_FILE="$pr_comments_file" \
  bash scripts/core-review-protocol-lint.sh || lint_status=$?

case "$lint_status" in
  0) ;;
  1) exit 1 ;;
  2) exit 2 ;;
  126)
    echo "review-protocol lint FAILED to execute (exit 126) -- this is not a pass" >&2
    exit 126
    ;;
  127)
    echo "review-protocol lint was not found (exit 127) -- this is not a pass" >&2
    exit 127
    ;;
  *)
    echo "review-protocol lint returned undocumented exit $lint_status -- this is not a pass" >&2
    exit "$lint_status"
    ;;
esac
```

The lint itself emits only three statuses: 0 means the supplied snapshot passed
the label, body, exact-head approval, and standing-refusal checks or the lint did
not apply; 1 means it found a protocol violation; 2 means it could not decide
because of a usage or internal error. Those three are not the shell's complete
status range. Status 126 means the command was found but could not be executed;
127 means it was not found. Statuses 3..125 and 128..255 are also outside the
lint's contract (128 plus a signal number commonly reports signal termination).
Every status other than 0 is a failure; an undocumented status is never a pass.

⚠️ **CAPTURE EACH FETCH AND CHECK IT.** A failing command substitution can still
leave a variable set and empty. The linter deliberately treats an explicitly
empty label set as the affirmative statement "this pull request has no labels",
so an unchecked fetch can turn a network failure into a not-applicable exit 0.
The guarded assignments above keep missing evidence distinct from an empty value.

⚠️ **CURRENT ENFORCEMENT IS NARROWER THAN "NO PROTECTION".** The same live
measurement returned all of these facts:

- `GET /repos/rrnewton/hermit/branches/main` reports `protected: true`, while its
  legacy `protection` object has `enabled: false` and no required status checks.
- The active `main history protection` ruleset prevents deletion and
  non-fast-forward updates and requires linear history.
- The active `main check gating (admin-bypassable)` ruleset has `rules: []`, so
  no status check currently requires this lint.
- Manual `workflow_dispatch` runs of `merge-gate.yml` exist; for example,
  https://github.com/rrnewton/hermit/actions/runs/32553733143 was created on
  `2026-08-22T05:12:22Z`.

Thus a merge is not unconstrained, and a required manually dispatched check
would be satisfiable. What is absent is an automatically triggered pull-request
run and a required status check for this lint. Skipping the hand-run lint is not
reported by the current repository settings.

⚠️ **DO NOT CHANGE THAT POLICY UNILATERALLY.** Adding a `pull_request` trigger
would contradict the standing directive that Actions run automatically only
for the integration branch. That portable run is supplemental evidence and
does not gate. Requiring the existing manually dispatched check instead would
make every pull request wait for a deliberate dispatch. Either policy changes
repository-wide landing behavior and therefore needs an owner decision; the
present partial lint remains a hand-run check until then.
