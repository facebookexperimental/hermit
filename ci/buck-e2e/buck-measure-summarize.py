#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Write one validation node's Buck measurement sidecar (schema hermit-buck-measure/v2).

A node that may run buck2 records what it ran and what it cost, and changes nothing it builds or
decides. ci/buck-e2e/buck-measure.sh gathers the raw facts and calls this to type them. The
sidecar holds:

- node, attempt: the DAG step tag and its DAGRUN_STEP_STARTED_MONOTONIC_NS;
- buck2: the resolved path, its sha256, whether that file is the executable or a DotSlash
  launcher (whose hash is the launcher's, not the executable's), `--version`, how it was chosen;
- daemon: where the buck2 daemon ran, from `buck2 status` while it still ran (the `daemon`
  subcommand below):
  - `step_cgroup`: in this node's cgroup (or below it). dagrun already records that cgroup's
    CPU and peak memory as the step's, so the daemon adds nothing to the step's row;
  - `scope`: in a systemd scope outside the step's cgroup (the stock runner's
    buck2_resource_control): the scope's CPU (usage, user, system microseconds) over the node,
    from cgroup v2 `cpu.stat`, and its `memory.peak`;
  - `unmeasured`, with the reason;
- bootstraps: each step that prepares a Buck build, with its exit status and CPU seconds, or
  unmeasured with the reason. They run in the node's cgroup, so they are part of its row;
- actions: every executed action's kind, counted from buck2's event logs
  (/Event/data/SpanEnd/data/ActionExecution/execution_kind, buck2's ActionExecutionKind), with
  the decoded bytes read. Each log must end with buck2's final `Result` record, and there must be
  as many logs as the node ran invocations (--expected-event-logs); otherwise the actions are
  unmeasured, with the reason, and no partial count is written;
- outputs: each built output's target, path and sha256, from `buck2 build --show-output`;
- disk: buck-out's allocated and apparent bytes, where the node asked for them;
- cache: the action cache's identity, or `configured: false`.

A fact that cannot be typed is recorded as unmeasured with the reason: a wrong number is worse
than none, and the comparison (ci/buck-build-compare.py) refuses a node whose daemon or actions
are unmeasured. Exit status: 0 once the sidecar is written; 2 for a usage error or a sidecar that
cannot be written.

`buck-measure-summarize.py daemon --status FILE --out FILE` reads `buck2 status` output (FILE;
empty when no daemon ran) and writes where the daemon runs, compared with this process's own
cgroup, which is the calling node's. It keeps only the daemon's pid, forkserver pid and cgroup
paths: the status also names the daemon's auth token, which is never written.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import sys
import zlib
from pathlib import Path
from typing import Iterator

SCHEMA = "hermit-buck-measure/v2"

# buck2's ActionExecutionKind (app/buck2_data/data.proto). 5 is reserved (the deprecated
# SKIPPED). An unknown value is counted under its number, never dropped or folded into another
# kind.
EXECUTION_KINDS = {
    1: "local",
    2: "remote",
    3: "action_cache",
    4: "simple",
    5: "reserved_5",
    6: "deferred",
    7: "local_dep_file",
    8: "local_worker",
    9: "remote_dep_file_cache",
    10: "local_action_cache",
    11: "remote_worker",
    12: "remote_served_from_cache",
    13: "remote_deduplicated",
}
# Kinds whose result came from a cache rather than from running the action here or remotely.
CACHE_KINDS = {
    "action_cache",
    "local_dep_file",
    "remote_dep_file_cache",
    "local_action_cache",
    "remote_served_from_cache",
}


class Unmeasured(Exception):
    """A fact that cannot be typed; recorded with its reason, never as a number."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def cgroup_of(pid: str) -> str:
    """The cgroup v2 path of a process (`0::<path>` in /proc/<pid>/cgroup)."""
    path = Path("/proc") / pid / "cgroup"
    try:
        lines = path.read_text().splitlines()
    except OSError as error:
        raise Unmeasured(f"{path} is unreadable: {error}") from error
    for line in lines:
        if line.startswith("0::"):
            return line[3:]
    raise Unmeasured(f"{path} has no cgroup v2 (0::) line")


def within(path: str, ancestor: str) -> bool:
    return path == ancestor or path.startswith(ancestor.rstrip("/") + "/")


def daemon_placement(status_text: str) -> dict:
    """Where the daemon runs, from `buck2 status` output, against this process's cgroup."""
    if not status_text.strip():
        raise Unmeasured("no buck2 daemon was running when it was measured (`buck2 status`)")
    try:
        status = json.loads(status_text)
    except json.JSONDecodeError as error:
        raise Unmeasured(f"`buck2 status` printed no JSON: {error}") from error
    info = status.get("process_info") if isinstance(status, dict) else None
    pid = info.get("pid") if isinstance(info, dict) else None
    if not isinstance(pid, int) or isinstance(pid, bool) or pid <= 0:
        raise Unmeasured("`buck2 status` named no daemon pid")
    daemon = cgroup_of(str(pid))
    node = cgroup_of("self")
    facts = {
        "pid": pid,
        "cgroup": daemon,
        "node_cgroup": node,
        "allprocs_cgroup_path": status.get("allprocs_cgroup_path") or "",
    }
    forkserver = status.get("forkserver_pid")
    if isinstance(forkserver, int) and not isinstance(forkserver, bool) and forkserver > 0:
        facts["forkserver_pid"] = forkserver
        facts["forkserver_cgroup"] = cgroup_of(str(forkserver))
    return facts


def cpu_stat(path: Path) -> dict[str, int]:
    """The usage, user and system microseconds of a cgroup v2 cpu.stat file."""
    values: dict[str, int] = {}
    for number, line in enumerate(path.read_text().splitlines(), 1):
        parts = line.split()
        if len(parts) != 2 or not parts[1].isdigit():
            raise Unmeasured(f"{path}:{number}: not a `key value` cpu.stat line: {line!r}")
        values[parts[0]] = int(parts[1])
    missing = [key for key in ("usage_usec", "user_usec", "system_usec") if key not in values]
    if missing:
        raise Unmeasured(f"{path}: cpu.stat lacks {', '.join(missing)}")
    return values


def daemon_facts(args: argparse.Namespace) -> dict:
    try:
        if args.daemon_unmeasured_reason:
            raise Unmeasured(args.daemon_unmeasured_reason)
        if args.daemon_scope:
            if args.scope_cpu_start is None or args.scope_cpu_end is None or args.scope_memory_peak is None:
                raise Unmeasured("a daemon scope was named without its cpu.stat start, end and memory.peak readings")
            before, after = cpu_stat(args.scope_cpu_start), cpu_stat(args.scope_cpu_end)
            delta = {key: after[key] - before[key] for key in ("usage_usec", "user_usec", "system_usec")}
            negative = [key for key, value in delta.items() if value < 0]
            if negative:
                raise Unmeasured(f"the daemon scope's {', '.join(negative)} went backwards: was the scope replaced?")
            text = args.scope_memory_peak.read_text().strip()
            if not text.isdigit():
                raise Unmeasured(f"{args.scope_memory_peak}: memory.peak is {text!r}, not a byte count")
            return {
                "placement": "scope",
                "scope": args.daemon_scope,
                "cpu_usec": delta["usage_usec"],
                "user_usec": delta["user_usec"],
                "system_usec": delta["system_usec"],
                "memory_peak_bytes": int(text),
            }
        if args.daemon_facts is None:
            raise Unmeasured("where the daemon ran was not recorded")
        facts = json.loads(args.daemon_facts.read_text())
        if not isinstance(facts, dict):
            raise Unmeasured(f"{args.daemon_facts}: not a JSON object")
        if facts.get("unmeasured"):
            raise Unmeasured(str(facts["unmeasured"]))
        daemon, node = facts.get("cgroup"), facts.get("node_cgroup")
        if not isinstance(daemon, str) or not isinstance(node, str) or not daemon or not node:
            raise Unmeasured(f"{args.daemon_facts}: no daemon and node cgroup paths")
        if not within(daemon, node):
            raise Unmeasured(
                f"the daemon (pid {facts.get('pid')}) ran in cgroup {daemon}, outside this node's cgroup "
                f"{node}, and no systemd scope reading was taken: its CPU is in no step's row"
            )
        forkserver = facts.get("forkserver_cgroup")
        if isinstance(forkserver, str) and forkserver and not within(forkserver, node):
            raise Unmeasured(f"the daemon's forkserver ran in cgroup {forkserver}, outside this node's cgroup {node}")
        return {"placement": "step_cgroup", "cgroup": daemon, "node_cgroup": node, "pid": facts.get("pid")}
    except (Unmeasured, OSError, ValueError) as error:
        return {"placement": "unmeasured", "reason": str(error)}


def log_lines(log: Path) -> Iterator[bytes]:
    if log.name.endswith(".gz"):
        with gzip.open(log, "rb") as handle:
            yield from handle
    else:
        with log.open("rb") as handle:
            yield from handle


def action_counts(event_logs: list[Path], expected: int | None) -> dict:
    """Count executed actions by kind, streaming each log; unmeasured unless every log is whole."""
    by_kind: dict[str, int] = {}
    decoded = 0
    try:
        if expected is not None and len(event_logs) != expected:
            raise Unmeasured(f"the node ran {expected} buck2 invocation(s) with event logs, and {len(event_logs)} were decoded")
        if not event_logs:
            raise Unmeasured("no buck2 event log was kept")
        for log in event_logs:
            last = b""
            try:
                for number, line in enumerate(log_lines(log), 1):
                    decoded += len(line)
                    if not line.strip():
                        continue
                    if last:
                        count_action(json.loads(last), by_kind, log, number - 1)
                    last = line
            except (OSError, EOFError, gzip.BadGzipFile, zlib.error) as error:
                raise Unmeasured(f"{log}: not a readable buck2 event log: {error}") from error
            except ValueError as error:  # a JSONDecodeError, or a UnicodeDecodeError from bytes not UTF-8
                raise Unmeasured(f"{log}: invalid JSON before its last line: {error}") from error
            try:
                final = json.loads(last) if last else None
            except ValueError:  # as above: also a copy cut inside a multibyte character
                final = None
            if not isinstance(final, dict) or "Result" not in final:
                raise Unmeasured(f"{log} does not end with buck2's Result record: the log was cut")
    except Unmeasured as error:
        return {"measured": False, "reason": str(error), "decoded_bytes": decoded}
    total = sum(by_kind.values())
    cached = sum(count for name, count in by_kind.items() if name in CACHE_KINDS)
    return {
        "measured": True,
        "event_logs": len(event_logs),
        "decoded_bytes": decoded,
        "total": total,
        "cached": cached,
        "by_kind": dict(sorted(by_kind.items())),
    }


def count_action(event: object, by_kind: dict[str, int], log: Path, number: int) -> None:
    if not isinstance(event, dict):
        raise Unmeasured(f"{log}:{number}: a record that is not a JSON object")
    data = event.get("Event")
    for key in ("data", "SpanEnd", "data"):
        data = data.get(key) if isinstance(data, dict) else None
    action = data.get("ActionExecution") if isinstance(data, dict) else None
    if action is None:
        return
    kind = action.get("execution_kind") if isinstance(action, dict) else None
    if not isinstance(kind, int) or isinstance(kind, bool):
        raise Unmeasured(f"{log}:{number}: an ActionExecution without an integer execution_kind")
    name = EXECUTION_KINDS.get(kind, f"unknown_{kind}")
    by_kind[name] = by_kind.get(name, 0) + 1


def outputs(show_output: Path | None, root: Path) -> dict:
    if show_output is None:
        return {"measured": False, "reason": "the node kept no --show-output"}
    found = []
    try:
        for number, line in enumerate(show_output.read_text().splitlines(), 1):
            parts = line.split(maxsplit=1)
            if not parts:
                continue
            if len(parts) != 2:
                raise Unmeasured(f"{show_output}:{number}: not a `target path` line: {line!r}")
            target, path = parts
            file = Path(path) if os.path.isabs(path) else root / path
            if not file.is_file():
                raise Unmeasured(f"{show_output}:{number}: {target}'s output {file} is not a file")
            found.append({"target": target, "path": str(file), "sha256": sha256_file(file)})
    except (Unmeasured, OSError, UnicodeDecodeError) as error:
        return {"measured": False, "reason": str(error)}
    return {"measured": True, "files": found}


def bootstraps(tsv: Path | None) -> list[dict]:
    if tsv is None or not tsv.exists():
        return []
    rows = []
    for number, line in enumerate(tsv.read_text().splitlines(), 1):
        parts = line.split("\t")
        row: dict = {"name": parts[0]}
        try:
            if len(parts) != 4:
                raise Unmeasured(f"{tsv}:{number}: not a `name rc user_s sys_s` row: {line!r}")
            row["rc"] = int(parts[1])
            if parts[2] == "unmeasured":
                raise Unmeasured(parts[3])
            row["user_s"], row["sys_s"] = float(parts[2]), float(parts[3])
            row["measured"] = True
        except (Unmeasured, ValueError) as error:
            row.update(measured=False, reason=str(error))
        rows.append(row)
    return rows


def disk(buck_out: Path | None) -> dict:
    if buck_out is None:
        return {"measured": False, "reason": "this node does not measure buck-out"}
    if not buck_out.is_dir():
        return {"measured": False, "reason": f"{buck_out} is not a directory"}
    allocated = apparent = 0
    for directory, _, files in os.walk(buck_out):
        for name in files:
            try:
                status = os.lstat(os.path.join(directory, name))
            except FileNotFoundError:
                continue
            apparent += status.st_size
            allocated += status.st_blocks * 512
    return {"measured": True, "buck_out_apparent_bytes": apparent, "buck_out_allocated_bytes": allocated}


def buck2_identity(path: str, version: str, chosen_by: str) -> dict:
    record = {"version": version, "chosen_by": chosen_by}
    try:
        executable = Path(path).resolve(strict=True)
        with executable.open("rb") as handle:
            head = handle.read(256)
        record.update(path=str(executable), sha256=sha256_file(executable))
        first = head.split(b"\n", 1)[0]
        # A DotSlash file is a JSON descriptor behind a `#!/usr/bin/env dotslash` line.
        record["file"] = "dotslash_launcher" if first.startswith(b"#!") and b"dotslash" in first else "executable"
    except OSError as error:
        record.update(path=path, file="unreadable", reason=str(error))
    return record


def write_atomically(out: Path, record: dict) -> None:
    out.parent.mkdir(parents=True, exist_ok=True)
    temporary = out.with_name(out.name + ".tmp")
    temporary.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
    temporary.replace(out)


def daemon_main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="buck-measure-summarize.py daemon")
    parser.add_argument("--status", required=True, type=Path, help="`buck2 status` stdout; empty when no daemon ran")
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        facts = daemon_placement(args.status.read_text(errors="replace"))
    except (Unmeasured, OSError) as error:
        facts = {"unmeasured": str(error)}
    try:
        write_atomically(args.out, facts)
    except OSError as error:
        print(f"buck-measure: cannot write {args.out}: {error}", file=sys.stderr)
        return 2
    return 0


def main(argv: list[str]) -> int:
    if argv[:1] == ["daemon"]:
        return daemon_main(argv[1:])
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--node", required=True, help="the DAG step tag")
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--attempt", default="", help="DAGRUN_STEP_STARTED_MONOTONIC_NS; empty outside dagrun")
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--buck2", default="")
    parser.add_argument("--buck2-version", default="")
    parser.add_argument("--buck2-chosen-by", default="")
    parser.add_argument("--daemon-facts", type=Path, help="the `daemon` subcommand's output")
    parser.add_argument("--daemon-scope", default="")
    parser.add_argument("--daemon-unmeasured-reason", default="")
    parser.add_argument("--scope-cpu-start", type=Path)
    parser.add_argument("--scope-cpu-end", type=Path)
    parser.add_argument("--scope-memory-peak", type=Path)
    parser.add_argument("--event-log", type=Path, action="append", default=[])
    parser.add_argument("--expected-event-logs", type=int)
    parser.add_argument("--show-output", type=Path)
    parser.add_argument("--bootstraps", type=Path)
    parser.add_argument("--buck-out", type=Path)
    parser.add_argument("--cache", default="", help="the action cache's identity; empty when none is configured")
    args = parser.parse_args(argv)
    attempt = args.attempt if args.attempt.isdigit() else None
    try:
        record = {
            "schema": SCHEMA,
            "node": args.node,
            "attempt": attempt,
            "buck2": buck2_identity(args.buck2, args.buck2_version, args.buck2_chosen_by),
            "daemon": daemon_facts(args),
            "bootstraps": bootstraps(args.bootstraps),
            "actions": action_counts(args.event_log, args.expected_event_logs),
            "outputs": outputs(args.show_output, args.root),
            "disk": disk(args.buck_out),
            "cache": {"configured": True, "identity": args.cache} if args.cache else {"configured": False},
        }
        write_atomically(args.out, record)
    except Exception as error:  # A measurement never raises into the node: name it and stop.
        print(f"buck-measure: {args.node}: no sidecar written: {type(error).__name__}: {error}", file=sys.stderr)
        return 2
    daemon, actions = record["daemon"], record["actions"]
    daemon_text = daemon["placement"] + (f" ({daemon['reason']})" if daemon["placement"] == "unmeasured" else "")
    actions_text = (f"{actions['total']} ({actions['cached']} from cache)" if actions["measured"]
                    else f"unmeasured ({actions['reason']})")
    print(f"buck-measure: {args.node}: daemon {daemon_text}; actions {actions_text} -> {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
