# Agent-driven debugging with Hermit: literature review and vision

Status: research/vision doc, 2026-07-22. Not a commitment or a spec.

This document surveys the research and tooling landscape for LLM/agent-driven
interactive debugging, then sketches a vision for a Hermit MCP server that lets
an agent drive Hermit's deterministic record/replay, time-travel, and schedule
exploration to find and fix bugs — especially concurrency bugs — with far more
leverage than an agent driving an ordinary debugger.

---

## Part 1 — Literature review

### 1.1 LLMs driving debuggers

**ChatDBG** (Levin, van Kempen, Berger, Freund; UMass Amherst / Williams;
arXiv:2403.16354) is the anchor result. ChatDBG connects an LLM to standard
debuggers — LLDB, GDB, and Python's `pdb` — and, crucially, lets the model
**"take the wheel"**: rather than answering a single prompt, the LLM acts as an
autonomous agent that issues debugger commands (backtrace, frame navigation,
variable inspection, expression evaluation) via the model's function-calling
interface, observes the results, and iterates.

Reported effectiveness on their benchmark of buggy programs:

- Plain LLM assistance (no autonomy): ~57% of defects diagnosed/fixed.
- **+"Take the Wheel"** (agent issues its own debugger commands): ~67%.
- With one additional follow-up dialog step: **~85%**, and up to ~87%
  diagnose-and-fix in their strongest configuration.

The headline lesson is not the absolute numbers but the *shape*: giving the model
**agency to interrogate live program state** was worth more than a larger prompt.
The debugger is the tool; autonomy over it is the multiplier. ChatDBG also saw
substantial real-world adoption, suggesting the workflow is practically useful,
not just a benchmark artifact.

**Related LLM-debugging work.**

- **LDB — "Debug like a Human"** (arXiv:2402.16906) verifies a program's runtime
  execution step by step, segmenting the trace and checking intermediate state
  against the model's intent. It reinforces that *runtime* signal (not just
  source) sharpens LLM debugging.
- **Self-Debugging** (Chen et al., arXiv:2304.05128) shows models can improve
  generated code by explaining and re-running it — "rubber-duck" debugging
  without a human — but is limited to what the model can execute and observe.
- The **SWE-bench / SWE-agent** lineage established the agent-plus-repository
  loop (localize → edit → test) that most coding agents now use; it is strong at
  static navigation and test-driven iteration but typically treats execution as
  a black box (run tests, read output), not as an interactive, inspectable
  timeline.

**Gap the literature exposes.** These systems make the *debugger* agentic, but
they inherit the debugger's limitations: a live process that moves forward only,
where re-running to reach a failure can change timing (Heisenbugs), and where
concurrency bugs may not reproduce at all under the debugger. None of them give
the agent a bug that *holds still*.

### 1.2 MCP servers for debuggers (the plumbing already exists)

The Model Context Protocol has rapidly become the standard way to expose a tool
to an agent, and debuggers are already wired up:

- **LLDB ships an official, upstream MCP server** (`lldb.llvm.org/use/mcp.html`)
  — first-party evidence that "agent drives debugger over MCP" is now mainstream.
- Community servers cover the rest: `pansila/mcp_server_gdb`, `smadi0x86/GDB-MCP`,
  `dbgmcp` (GDB + LLDB + PDB), and `netcoredbg-mcp` for .NET.
- Products such as **debugg.ai** explicitly market "orchestrating gdb/lldb via
  MCP for **deterministic bug repro** and fixes," converging on the same insight
  this vision is built around: determinism is what makes agent debugging reliable.

Implication: Hermit does not need to invent the transport. The differentiated
value is *what* it exposes over MCP — a deterministic, navigable execution — not
that it speaks MCP.

### 1.3 Time-travel debugging

Time-travel (a.k.a. reverse) debugging records an execution once and then lets
you move **backward and forward** through it deterministically:

- **rr** (O'Callahan et al., "Engineering Record and Replay for Deployability,"
  USENIX ATC 2017) made record/replay cheap enough for everyday use on Linux and
  is the reference OSS implementation; it underpins reverse-execution in GDB.
- **Undo UDB** (commercial Linux TTD) and **Microsoft WinDbg TTD** bring the same
  model to enterprise and Windows respectively (see Wikipedia, "Time travel
  debugging").

The nascent-but-growing "agentic time-travel" thread (e.g. "Agentic Debugging
with Time Travel: The Architecture of Certainty"; debugg.ai's "Deterministic
Replay Will Save Your Code Debugging AI" and "Time-Travel CI ... Kill
Heisenbugs") argues the pairing is natural: an agent that can rewind never loses
the failure state and can test hypotheses against a fixed timeline. What is
missing across these tools is **deterministic control of concurrency** —
rr replays a single recorded schedule but does not systematically *explore* the
schedule space, and none expose schedule-level root-causing to an agent.

### 1.4 Concurrency debugging and schedule exploration

Concurrency is where determinism pays off most:

- **CHESS** (Musuvathi & Qadeer, OSDI 2008, "Finding and Reproducing Heisenbugs
  in Concurrent Programs") systematically enumerates thread interleavings and can
  **reproduce** a found schedule — reproduction being the hard part of concurrency
  bugs.
- **PCT** (Burckhardt, Kothari, Musuvathi, Nagarakatte; ASPLOS 2010, "A
  Randomized Scheduler with Probabilistic Guarantees of Finding Bugs") shows a
  randomized scheduler can find deep bugs with quantifiable probability — the
  intellectual ancestor of Hermit's **chaos mode**.

These give *search* and *reproduction*, but historically require a human to
interpret the resulting schedules. No mainstream tool hands an agent the ability
to (a) search schedules for a failure, (b) automatically localize the failure to
a specific scheduling decision, and (c) let the agent explore around that
decision.

### 1.5 Static + dynamic complementarity

Static analysis (linters, type systems, data-race detectors, symbolic
tools) is cheap and global but imprecise (false positives, no runtime values);
dynamic debugging is precise but local and needs a reproduction. The productive
pattern — used implicitly by ChatDBG and SWE-agent — is **static to form
hypotheses, dynamic to confirm them**. An agent is unusually well suited to run
this loop quickly, *if* the dynamic half is deterministic enough that a confirmed
hypothesis stays confirmed.

### 1.6 Synthesis

1. Agency over a debugger is the force multiplier (ChatDBG).
2. The MCP plumbing to expose a debugger to an agent is solved and upstream.
3. Time-travel removes "the bug moved while I looked away."
4. Schedule search + reproduction (CHESS/PCT) is the concurrency counterpart, but
   is not yet agent-facing or self-localizing.
5. The unfilled niche: **a deterministic runtime that exposes time-travel *and*
   schedule exploration *and* automatic schedule-level root-causing to an agent
   over MCP.** That is precisely what Hermit already implements internally.

---

## Part 2 — Vision: the Hermit MCP server

### 2.1 The core advantage — the bug holds still

Every limitation in §1.1 stems from one fact: ordinary debugging targets a
*moving* process. Hermit runs a guest under deterministic record/replay, so a
recorded failure replays **bit-for-bit** every time. For an agent this is
transformative:

- A hypothesis tested at replay step *N* gives the same answer on every replay.
- The agent can rewind, re-inspect, and branch its investigation without fear of
  perturbing the bug (no Heisenberg effect from breakpoints or logging).
- Concurrency bugs — the ones that "never reproduce" — become as reproducible as
  a crash in straight-line code, because the *schedule* is part of the recording.

> Determinism turns debugging from *observation of a live system* into *analysis
> of a fixed artifact*. Agents are much better at the latter.

### 2.2 What the MCP server exposes

A Hermit MCP server would surface Hermit's existing capabilities as agent tools.
Grounded in what is already in-tree (`record`/`replay`, `--chaos` with
`--record-preemptions`, and the critical-schedule search in
`hermit-cli/src/bin/hermit/schedule_search.rs`):

| MCP tool | Backed by | What the agent can do |
| --- | --- | --- |
| `record(cmd)` / `list_recordings` | `hermit record` | Capture a run (incl. its schedule) as a replayable artifact. |
| `replay(id)` / `run_to(event)` | `hermit replay` | Deterministically re-execute to any point. |
| `time_travel(step±)` | replay + reverse | Move forward/backward along the fixed timeline. |
| `inspect(state)` | detcore/reverie state | Read registers, memory, fds, logical time, thread states at the current point. |
| `explore_schedules(seed…)` | `--chaos` + preemption record/replay | Search alternate legal interleavings for a failure; each hit is itself replayable. |
| `bisect_schedule(pass, fail)` | `schedule_search` | Auto-localize a concurrency failure to the single scheduling decision that flips pass→fail. |
| `diff_runs(a, b)` | schedule/log diff | Compare two deterministic runs (state or schedule) to see exactly what diverged. |

The two capabilities without a mainstream analogue are `explore_schedules` and
`bisect_schedule`.

### 2.3 The killer feature — automatic schedule bisection

Hermit's `schedule_search` already finds a **failing schedule that is
edit-distance one from a passing schedule**: it aligns the passing and failing
event sequences (Needleman–Wunsch) and identifies the *critical adjacent event
pair* — the single swap that changes the outcome. Exposed to an agent, this is
automatic root-cause localization for concurrency:

> "This data race manifests exactly when thread B's write at `foo.rs:42` is
> ordered before thread A's read at `bar.rs:88`. Here are both schedules; here is
> the one event that differs."

Instead of the agent guessing at interleavings, Hermit hands it *the* decision
that matters, plus two replayable executions (one good, one bad) that differ by
that decision alone. The agent's job collapses from "explore an exponential
schedule space" to "explain one event swap and fix the missing synchronization."

### 2.4 The agent workflow

```
        ┌───────────────────────────────────────────────────────────┐
        │ 1. STATIC: read code / linters / types → form hypotheses    │
        │    ("these two threads touch `counter` without a lock")     │
        └───────────────┬───────────────────────────────────────────┘
                        v
        ┌───────────────────────────────────────────────────────────┐
        │ 2. REPRODUCE: hermit record; if flaky, explore_schedules    │
        │    until a failing (and REPLAYABLE) schedule is captured    │
        └───────────────┬───────────────────────────────────────────┘
                        v
        ┌───────────────────────────────────────────────────────────┐
        │ 3. LOCALIZE: bisect_schedule(pass, fail) → the one critical │
        │    event swap that flips the outcome                        │
        └───────────────┬───────────────────────────────────────────┘
                        v
        ┌───────────────────────────────────────────────────────────┐
        │ 4. INVESTIGATE: time_travel + inspect around that event —   │
        │    replay is deterministic, so every probe is repeatable    │
        └───────────────┬───────────────────────────────────────────┘
                        v
        ┌───────────────────────────────────────────────────────────┐
        │ 5. FIX & VERIFY: edit; re-record; confirm the failing seed  │
        │    and the bisected schedule now pass (regression = a saved │
        │    schedule, not a hope that timing stays lucky)            │
        └───────────────────────────────────────────────────────────┘
```

Steps 2–4 are where Hermit differs from a plain MCP-over-GDB agent: the bug is
reproducible on demand, the root cause is bisected automatically, and the
investigation timeline never shifts.

### 2.5 Why this beats printf debugging (especially for slow-to-recompile systems)

Printf/log debugging has three costs that dominate large systems work:

1. **Recompile-and-rerun latency.** Each new print requires a rebuild and a
   re-run — brutal for systems that take many minutes to build and where the bug
   is intermittent. With Hermit the failing execution is *already recorded*: the
   agent adds "observation" by inspecting the existing replay, with **zero
   rebuilds**. New questions cost a replay, not a build.
2. **Observer effect.** Adding logging perturbs timing and can hide concurrency
   bugs. Deterministic replay observes without perturbing.
3. **Reproduction roulette.** A printf only helps if the bug reappears. Hermit
   captures the exact schedule once; it reappears every time.

For an autonomous agent these costs compound: an agent that must rebuild to test
each hypothesis is throttled by build latency and flakiness, whereas an agent
querying a fixed recording can test dozens of hypotheses per minute against a
stable artifact.

### 2.6 Relationship to existing work

- vs. **ChatDBG**: same "take the wheel" agency, but over a *deterministic,
  reversible* execution with concurrency control — removing the moving-target and
  non-reproducibility limits that bound live-debugger agents.
- vs. **rr / UDB / WinDbg TTD**: Hermit adds deterministic *concurrency* control
  and schedule search/bisection on top of time-travel, and targets an
  agent (MCP) consumer rather than a human at a prompt.
- vs. **CHESS / PCT**: Hermit provides the search *and* auto-localization *and*
  an agent interface, closing the loop from "found a schedule" to "explained and
  fixed the bug."
- vs. **MCP-over-GDB servers**: same protocol, but the exposed substrate is a
  deterministic timeline with schedule tools, not a single forward-only process.

### 2.7 Open questions / risks

- **Runtime coverage.** Deterministic replay must cover the workload's syscalls
  and threading model; e.g. Ruby multithreading currently livelocks under strict
  sequentialization (see `research-ruby-deadlock` / `experiments/ruby-threads`).
  Agent debugging is only as good as the set of programs Hermit can replay.
- **Overhead.** Strict deterministic mode is markedly slower than native (see
  `experiments/benchmarks`); recording cost and replay latency shape how snappy
  the agent loop feels.
- **State legibility.** The MCP tools must present state (memory, threads, logical
  time, schedules) in forms an LLM reasons about well — schedule diffs and event
  labels matter as much as raw register dumps.
- **Trust boundary.** An agent that can drive replay and read arbitrary guest
  memory needs the same care as any tool with broad read access.

### 2.8 One-line thesis

Hermit can give an agent something no live debugger can: **a bug that holds
still, a schedule space it can search, and the single scheduling decision that
causes the failure handed to it automatically** — turning concurrency debugging
from art into analysis.

---

## References

- Levin, van Kempen, Berger, Freund. *ChatDBG: Augmenting Debugging with Large
  Language Models.* arXiv:2403.16354.
- Zhong et al. *Debug like a Human: A LLM Debugger via Verifying Runtime Execution
  Step by Step* (LDB). arXiv:2402.16906.
- Chen et al. *Teaching Large Language Models to Self-Debug.* arXiv:2304.05128.
- O'Callahan et al. *Engineering Record and Replay for Deployability* (rr).
  USENIX ATC 2017.
- Musuvathi & Qadeer. *Finding and Reproducing Heisenbugs in Concurrent Programs*
  (CHESS). OSDI 2008.
- Burckhardt, Kothari, Musuvathi, Nagarakatte. *A Randomized Scheduler with
  Probabilistic Guarantees of Finding Bugs* (PCT). ASPLOS 2010.
- LLDB Model Context Protocol server: `https://lldb.llvm.org/use/mcp.html`.
- Community debugger MCP servers: `pansila/mcp_server_gdb`, `smadi0x86/GDB-MCP`,
  `dbgmcp` (GDB/LLDB/PDB), `netcoredbg-mcp`.
- Wikipedia, *Time travel debugging* (rr, Undo UDB, WinDbg TTD).
- debugg.ai: deterministic-replay-for-AI-debugging articles (deterministic bug
  repro; Time-Travel CI).
