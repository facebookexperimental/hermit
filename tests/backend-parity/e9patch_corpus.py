#!/usr/bin/env python3
"""Ratchet e9patch preprocessing parity against the golden ptrace backend.

e9patch is binary-rewriting *preprocessing* for the ptrace backend, not a
standalone Detcore backend: e9tool rewrites the guest ELF ahead of time to
pre-trap its `SYSCALL` sites, then Detcore runs the rewritten image under
ptrace. e9tool only rewrites the *main* executable, so a dynamically linked
libc program exposes zero `SYSCALL` sites in its own ELF (they live in
`libc.so`/`ld-linux`) and e9patch preprocessing becomes a no-op
(`candidate_sites=0`). The shared `run_matrix.py` guests are all dynamic libc
binaries and therefore never exercise the rewrite path. This harness instead
uses a freestanding, statically linked, raw-`syscall` corpus (x86-64) whose
`SYSCALL` sites live in the main ELF, so `candidate_sites > 0` and e9patch
actually rewrites the guest.

For each guest we compare the golden plain-ptrace run against the e9patch
preprocessing + ptrace run and enforce, per guest:

  * exit-status parity              (golden exit == e9patch exit),
  * stdout parity                   (captured from a plain --strict run;
                                      --verify diverts guest stdout for its own
                                      log comparison),
  * golden L2                       (hermit run --strict --verify verifies),
  * e9patch L2                      (hermit --backend e9patch run --strict
                                      --verify verifies),
  * full direct-AOT coverage        (mapped_sites == candidate_sites > 0),
  * no signal fallback              (b0_sites == 0; a nonzero B0 would reserve
                                      SIGILL and change guest signal semantics),
  * guest-syscall DETLOG tail-match (the golden guest-syscall sequence equals
                                      the suffix of the e9patch sequence; the
                                      removed prefix is the deterministic
                                      e9loader prologue).

Byte-identical DETLOG parity to plain ptrace is impossible by construction: the
e9patch-rewritten image carries an e9loader stub that runs a fixed, deterministic
startup prologue (readlink /proc/self/exe, open(self), arch_prctl GET/SET_FS,
N * mmap of trampoline pages, close) before the guest's own `_start`. That
prologue is a pure prefix; the achievable and enforced parity is guest-syscall
DETLOG identity *modulo* that deterministic prologue (tail-match), plus L2 and
guest-visible parity. This harness makes no claim of strict detlog identity.

Prerequisites (absent in CI, hence BLOCKED there, mirroring the KVM /dev/kvm
gate in run_matrix.py):
  * a hermit built with the `e9patch` cargo feature
    (`cargo build -p hermit --features e9patch`);
  * HERMIT_E9TOOL and HERMIT_E9PATCH_BACKEND pointing at a built e9tool/e9patch
    pair (the reverie checkout vendors them under
    `third-party/e9patch/{e9tool,e9patch}`);
  * an x86-64 host with `cc`.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPOSITORY = SCRIPT_DIR.parent.parent
CORPUS_DIR = SCRIPT_DIR / "e9patch_corpus"

# name -> (expected_exit, expected_stdout or None). stdout is exact when given.
CORPUS: dict[str, tuple[int, bytes | None]] = {
    "minimal_exit": (0, b""),
    "write_stdout": (0, b"corpus-write\n"),
    "getpid_check": (0, b""),
    "clock_gettime": (0, b""),
    "nanosleep": (0, b""),
    "getrandom": (0, b""),
    "multi_site": (0, b"multi\n"),
    "loop_write": (0, b"xxxxxxxx\n"),
    "mmap_anon": (0, b""),
    "uname": (0, b""),
    "sigmask": (0, b""),
    "compute": (0, None),
}

FREESTANDING_FLAGS = (
    "-nostdlib",
    "-static",
    "-ffreestanding",
    "-O0",
    "-fno-pie",
    "-no-pie",
)


class CorpusError(Exception):
    """A missing corpus source or a failed parity contract."""


def compile_guest(name: str, out_dir: Path) -> Path:
    source = CORPUS_DIR / f"{name}.c"
    if not source.is_file():
        raise CorpusError(f"missing corpus source: {source}")
    compiler = shutil.which(os.environ.get("CC", "cc"))
    if compiler is None:
        raise CorpusError("C compiler unavailable (set CC or install cc)")
    output = out_dir / name
    command = [compiler, *FREESTANDING_FLAGS, str(source), "-o", str(output)]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise CorpusError(f"compile failed: {command!r}\n{result.stdout}{result.stderr}")
    return output


def hermit_command(hermit: Path, e9: bool, verify: bool, guest: Path) -> list[str]:
    command = [str(hermit)]
    if e9:
        command.extend(["--backend", "e9patch"])
    command.append("run")
    command.append("--strict")
    if verify:
        command.append("--verify")
    # Guests are compiled into a temp dir under host /tmp; --tmp=/tmp keeps
    # Hermit from replacing the guest's /tmp so the binary path resolves
    # (mirrors run_matrix.py).
    command.append("--tmp=/tmp")
    command.extend(["--", str(guest)])
    return command


def run(command: list[str], timeout: int) -> tuple[int, bytes, bytes]:
    try:
        proc = subprocess.run(
            command, capture_output=True, timeout=timeout, check=False
        )
    except subprocess.TimeoutExpired:
        return 124, b"", b"<timeout>"
    return proc.returncode, proc.stdout, proc.stderr


def detlog_syscalls(hermit: Path, e9: bool, guest: Path) -> list[str]:
    """Canonical guest-syscall sequence from a --log=info plain --strict run.

    Uses the "inbound syscall:" lines (they include exit_group, which has no
    finish line), with timestamps and addresses/large integers normalized so
    the sequence is host-layout independent.
    """
    command = [str(hermit)]
    if e9:
        command.extend(["--backend", "e9patch"])
    command.extend(
        ["--log=info", "run", "--strict", "--tmp=/tmp", "--", str(guest)]
    )
    _, _, stderr = run(command, timeout=60)
    lines: list[str] = []
    for raw in stderr.decode(errors="replace").splitlines():
        match = re.search(r"inbound syscall: ([a-z_0-9]+\(.*\)) = \?$", raw)
        if not match:
            continue
        canonical = re.sub(r"0x[0-9a-f]+", "A", match.group(1))
        canonical = re.sub(r", [0-9]{4,}", ", N", canonical)
        lines.append(canonical)
    return lines


def metric(name: str, stderr: bytes) -> int | None:
    match = re.search(rf"{name}=([0-9]+)", stderr.decode(errors="replace"))
    return int(match.group(1)) if match else None


def l2_ok(stderr: bytes) -> bool:
    return b"Determinism verified" in stderr


def prerequisites(hermit: Path) -> str | None:
    if not hermit.is_file() or not os.access(hermit, os.X_OK):
        return f"hermit executable unavailable: {hermit}"
    for var in ("HERMIT_E9TOOL", "HERMIT_E9PATCH_BACKEND"):
        path = os.environ.get(var)
        if not path or not Path(path).is_file():
            return f"{var} is unset or does not point at a file"
    # A hermit built without the e9patch feature rejects --backend e9patch.
    code, _, stderr = run(
        [str(hermit), "--backend", "e9patch", "run", "--", "/bin/true"], timeout=60
    )
    text = stderr.decode(errors="replace")
    if code != 0 and "e9patch" in text and "feature" in text:
        return "hermit was not built with the e9patch cargo feature"
    return None


def run_guest(hermit: Path, name: str, out_dir: Path) -> tuple[str, str]:
    expected_exit, expected_stdout = CORPUS[name]
    guest = compile_guest(name, out_dir)

    gx, gout, _ = run(hermit_command(hermit, False, False, guest), timeout=40)
    _, _, gv = run(hermit_command(hermit, False, True, guest), timeout=60)
    ex, eout, eerr = run(hermit_command(hermit, True, False, guest), timeout=60)
    _, _, ev = run(hermit_command(hermit, True, True, guest), timeout=90)

    if gx == 124 or ex == 124:
        return "FAIL", f"timeout (golden={gx}, e9patch={ex})"
    if gx != expected_exit:
        return "FAIL", f"golden exit {gx}, expected {expected_exit}"
    if gx != ex:
        return "FAIL", f"exit divergence golden={gx} e9patch={ex}"
    if gout != eout:
        return "FAIL", f"stdout divergence golden={gout!r} e9patch={eout!r}"
    if expected_stdout is not None and gout != expected_stdout:
        return "FAIL", f"golden stdout {gout!r}, expected {expected_stdout!r}"
    if not l2_ok(gv):
        return "FAIL", "golden not L2 (no 'Determinism verified')"
    if not l2_ok(ev):
        return "FAIL", "e9patch not L2 (no 'Determinism verified')"

    cand, mapped, b0 = (
        metric("candidate_sites", eerr),
        metric("mapped_sites", eerr),
        metric("b0_sites", eerr),
    )
    if cand is None or mapped is None or b0 is None:
        return "FAIL", "missing e9patch backend metrics"
    if cand == 0:
        return "FAIL", "candidate_sites=0 (guest did not exercise the rewrite path)"
    if mapped != cand:
        return "FAIL", f"incomplete coverage mapped={mapped} candidate={cand}"
    if b0 != 0:
        return "FAIL", f"b0_sites={b0} (SIGILL signal fallback rejected)"

    golden_seq = detlog_syscalls(hermit, False, guest)
    e9_seq = detlog_syscalls(hermit, True, guest)
    prologue = len(e9_seq) - len(golden_seq)
    if prologue < 0 or e9_seq[prologue:] != golden_seq:
        return "FAIL", (
            "guest-syscall DETLOG tail mismatch "
            f"golden={golden_seq!r} e9patch={e9_seq!r}"
        )
    return "PASS_L2", (
        f"exit={gx} sites c/{cand} m/{mapped} b0/{b0} "
        f"prologue={prologue} tail_match=yes"
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--hermit",
        type=Path,
        default=REPOSITORY / "target/debug/hermit",
        help="Hermit executable (must be built --features e9patch)",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="validate the corpus contract and list guests without running",
    )
    parser.add_argument(
        "--require-backend",
        action="store_true",
        help="fail instead of reporting BLOCKED when prerequisites are absent",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    print(f"CORPUS: {len(CORPUS)} freestanding e9patch parity guests")
    for name in CORPUS:
        source = CORPUS_DIR / f"{name}.c"
        if not source.is_file():
            raise CorpusError(f"missing corpus source: {source}")
    if args.check:
        for name in CORPUS:
            print(f"  contract {name}")
        return 0

    hermit = args.hermit.resolve()
    block = prerequisites(hermit)
    if block:
        print(f"BLOCKED: {block}")
        return 1 if args.require_backend else 0

    failures = 0
    with tempfile.TemporaryDirectory(prefix="hermit-e9patch-corpus-") as tempdir:
        for name in CORPUS:
            status, detail = run_guest(hermit, name, Path(tempdir))
            print(f"{status} {name}: {detail}")
            if status != "PASS_L2":
                failures += 1
    passed = len(CORPUS) - failures
    print(f"RATCHET e9patch: {passed}/{len(CORPUS)} PASS_L2")
    return 1 if failures else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except CorpusError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        sys.exit(2)
