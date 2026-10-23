#!/usr/bin/env python3
"""Summarize where a `buck2 test` invocation's cells spent their time.

usage: stages.py [--top N] EVENTS
       buck2 log show EVENT_LOG | stages.py [--top N] -

EVENTS is the JSON-lines form of a Buck event log (`buck2 log show EVENT_LOG`), one
event per line; `-` reads it from stdin. Every cell execution is a TestRun span, and the
executor stages Buck opens below it (Re.Queue, Re.WorkerDownload, Re.Execute, ReUpload,
CacheQuery, Local.Queued, Local.Execute, ...) are timed and charged to that cell. Stages
can nest, so a cell's stages are not a partition of its span.

Prints the invocation's span and cell count, then one row per stage over every cell
(count, sum, mean, p50, p90 and max seconds), then the last N cells to finish (default
20) with their offset from the first event, their own span, whether their result was a
cache hit, and their four longest stages. That names the cells a slow invocation waited
on, and the stage they waited in. Lines that are not events are skipped; the summary of
an event log cut short covers what it holds, and a cell still running when it ends is
counted to the last event. So is a stage still open when it ends, or to its cell's end
if that came first: that time is a lower bound, so the stage table counts the cells in
which a stage was still open, and a cell lists such a stage as NAME>=SECONDS.

ci/buck-e2e/validate-node writes this summary beside each invocation's event log it
keeps (see its E2E_RESULT_ROOT).
"""

from __future__ import annotations

import argparse
import collections
import json
import statistics
import sys


def stage_name(inner):
    """The name of the stage a span below a TestRun is, from its SpanStart data."""
    kind = next(iter(inner))
    value = inner[kind]
    if kind == "ExecutorStage" and isinstance(value, dict) and isinstance(value.get("stage"), dict):
        stage = value["stage"]
        executor = next(iter(stage), None)
        if executor in ("Re", "Local") and isinstance(stage[executor], dict):
            sub = stage[executor].get("stage")
            if isinstance(sub, dict) and sub:
                return f"{executor}.{next(iter(sub))}"
        if executor is not None:
            return executor
    return kind


def cell_name(test_run):
    suite = test_run.get("suite") or {}
    return str(suite.get("suite_name", "?")).rsplit(":", 1)[-1]


def cache_hit(test_run):
    """True or False when the TestRun's SpanEnd reports a remote command, else None."""
    try:
        command = test_run["command_report"]["details"]["command_kind"]["command"]
    except (KeyError, TypeError):
        return None
    remote = command.get("RemoteCommand") if isinstance(command, dict) else None
    if not isinstance(remote, dict) or "cache_hit" not in remote:
        return None
    return bool(remote["cache_hit"])


def summarize(lines):
    """Per-cell spans and stage seconds from the JSON lines of an event log."""
    runs = {}  # TestRun span id -> {"name", "start", "end", "cache_hit", "open": stages still open}
    owner = {}  # span id -> the TestRun span id it is below
    open_stages = {}  # span id -> (stage name, start)
    stages = collections.defaultdict(lambda: collections.defaultdict(float))
    first = last = None
    for line in lines:
        try:
            event = json.loads(line)["Event"]
            seconds, nanos = event["timestamp"]
            now = seconds + nanos / 1e9
            data = event["data"]
            kind = next(iter(data))
            value = data[kind]
        except (ValueError, KeyError, TypeError, StopIteration):
            continue
        # Buck writes events roughly, not strictly, in time order.
        first = now if first is None else min(first, now)
        last = now if last is None else max(last, now)
        inner = value.get("data") if isinstance(value, dict) else None
        if not isinstance(inner, dict) or not inner:
            continue
        inner_kind = next(iter(inner))
        span, parent = event.get("span_id"), event.get("parent_id")
        if kind == "SpanStart":
            if inner_kind == "TestRun":
                runs[span] = {"name": cell_name(inner[inner_kind]), "start": now, "end": None,
                              "cache_hit": None, "open": set()}
                owner[span] = span
            elif parent in owner:
                owner[span] = owner[parent]
                open_stages[span] = (stage_name(inner), now)
        elif kind == "SpanEnd":
            if inner_kind == "TestRun" and span in runs:
                runs[span]["end"] = now
                runs[span]["cache_hit"] = cache_hit(inner[inner_kind])
            elif span in open_stages:
                name, start = open_stages.pop(span)
                stages[owner[span]][name] += now - start
    # A stage the log ends in: count what it shows, up to the last event or the cell's end.
    for span, (name, start) in open_stages.items():
        run = runs[owner[span]]
        end = last if run["end"] is None else min(run["end"], last)
        stages[owner[span]][name] += max(0.0, end - start)
        run["open"].add(name)
    return runs, stages, first, last


def report(runs, stages, first, last, top):
    if first is None:
        return "no events\n"
    out = [f"event span {last - first:.1f}s, {len(runs)} cell executions\n"]
    by_stage = collections.defaultdict(list)
    still_open = collections.Counter()  # stage name -> cells it was still open in at the end
    for span, per_cell in stages.items():
        for name, seconds in per_cell.items():
            by_stage[name].append(seconds)
        still_open.update(runs[span]["open"])
    out.append(f"{'stage':28} {'n':>5} {'sum_s':>9} {'mean':>7} {'p50':>7} {'p90':>7} {'max':>7}\n")
    for name, values in sorted(by_stage.items(), key=lambda item: (-sum(item[1]), item[0])):
        values.sort()
        out.append(f"{name:28} {len(values):5} {sum(values):9.1f} {statistics.mean(values):7.2f} "
                   f"{values[len(values) // 2]:7.2f} {values[int(len(values) * 0.9)]:7.2f} {values[-1]:7.2f}"
                   + (f"  still open in {still_open[name]}, a lower bound" if still_open[name] else "") + "\n")
    finished = sorted(((run["end"] or last) - first, run["name"], span) for span, run in runs.items())
    top = max(0, min(top, len(finished)))
    out.append(f"\nlast {top} cell executions to finish "
               "(offset from the first event, own span, cache hit, longest stages):\n")
    for offset, name, span in finished[max(0, len(finished) - top):]:
        run = runs[span]
        hit = {True: "hit", False: "miss", None: "-"}[run["cache_hit"]]
        longest = sorted(stages[span].items(), key=lambda item: (-item[1], item[0]))[:4]
        out.append(f"{offset:7.1f} {(run['end'] or last) - run['start']:6.1f}s {hit:4} {name} "
                   + " ".join(f"{stage}{'>=' if stage in run['open'] else '='}{seconds:.1f}"
                              for stage, seconds in longest) + "\n")
    return "".join(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--top", type=int, default=20, help="cells to list by finishing time (default 20)")
    ap.add_argument("events", help="`buck2 log show` JSON lines, or - for stdin")
    a = ap.parse_args()
    if a.events == "-":
        result = summarize(sys.stdin)
    else:
        with open(a.events, encoding="utf-8", errors="replace") as stream:
            result = summarize(stream)
    sys.stdout.write(report(*result, a.top))


if __name__ == "__main__":
    main()
