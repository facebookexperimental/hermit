# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Reverse DAP requests for a Hermit replay managed by hermit-dap.

The first implementation deliberately trades speed for simplicity: invisible
source breakpoints record the executed source-line path, and each reverse
request starts the deterministic replay again and runs forward to an earlier
position on that path.
"""

import importlib
import json
import os
import re
import select
import shutil
import subprocess
import time

import gdb
from gdb.dap.server import capability, request
from gdb.dap.startup import DAPException
from gdb.dap.state import set_thread


server = importlib.import_module("gdb.dap.server")
frames = importlib.import_module("gdb.dap.frames")

# hermit-dap.rs defines these names before it execs this script.
_replay_command = HERMIT_REPLAY_COMMAND  # noqa: F821
_replay_target = HERMIT_REPLAY_TARGET  # noqa: F821
_client_fds = HERMIT_CLIENT_FDS  # noqa: F821
_replay_process = None
_setpriv = None
_history = []
_line_breakpoints = []
_arrival_leaders = {}
_stop_address_breakpoints = {}
_line_program = None
# Why the hidden line breakpoints cannot record the program's source-line
# path, or None. stepBack and reverseContinue refuse while it is set: without
# that record they would rewind to the wrong point in time without saying so.
_line_breakpoint_problem = None
_suppress_events = False
_advancing = False
_last_stopped = None

# How long a refusal waits for the client's first request before exiting.
_REFUSAL_REQUEST_TIMEOUT = 30.0
# How many stops that are not a line arrival (signals, for example) stepBack
# tolerates while it runs the replay forward to the next line arrival.
_ADVANCE_LIMIT = 100


def _read_client_messages(fd, timeout):
    # Yield the Content-Length framed DAP messages that arrive on FD within
    # TIMEOUT seconds. A message that is not a JSON object yields None, and so
    # does a header block without a usable Content-Length, after which the
    # stream cannot be framed and nothing more is read.
    deadline = time.monotonic() + timeout
    data = b""
    while True:
        header_end = data.find(b"\r\n\r\n")
        if header_end >= 0:
            length = None
            for line in data[:header_end].split(b"\r\n"):
                name, _, value = line.partition(b":")
                if name.strip().lower() == b"content-length":
                    try:
                        length = int(value.strip())
                    except ValueError:
                        length = None
            if length is None or length < 0:
                yield None
                return
            body = data[header_end + 4 :]
            if len(body) >= length:
                data = body[length:]
                try:
                    message = json.loads(body[:length])
                except ValueError:
                    message = None
                yield message if isinstance(message, dict) else None
                continue
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return
        ready, _, _ = select.select([fd], [], [], remaining)
        if not ready:
            return
        chunk = os.read(fd, 65536)
        if not chunk:
            return
        data += chunk


def _write_all(fd, data):
    while data:
        data = data[os.write(fd, data) :]


def _write_message(fd, message):
    body = json.dumps(message).encode()
    _write_all(fd, "Content-Length: {}\r\n\r\n".format(len(body)).encode() + body)


def _refuse(message):
    # Stop at startup and tell the client why.
    #
    # By the time this script runs, GDB's DAP interpreter has pointed fds 1 and
    # 2 at a pipe that only its server loop drains, and that loop has not
    # started yet. gdb.write, sys.stderr and os.write(2) all vanish here (GDB
    # 17.2 measured: zero bytes reach either stream). hermit-dap.rs therefore
    # hands this script duplicates of its own stdin, stdout and stderr: the
    # message goes to stderr, and the client's first request, normally
    # `initialize`, gets a failed DAP response that carries it. A client that
    # sends no request in time, or a header block this cannot frame, gets the
    # message as an `output` event instead.
    text = "hermit-dap: " + message
    if _client_fds is None:
        # hermit-dap.rs could not duplicate its streams and has said so on its
        # own stderr. GDB's stderr is the only remaining route; it may not
        # reach the client, but it is all there is.
        gdb.write(text + "\n", gdb.STDERR)
        os._exit(1)
    client_in, client_out, client_err = _client_fds
    try:
        _write_all(client_err, (text + "\n").encode())
    except OSError:
        pass
    request = None
    try:
        for incoming in _read_client_messages(client_in, _REFUSAL_REQUEST_TIMEOUT):
            if incoming is None or incoming.get("type") == "request":
                request = incoming
                break
    except OSError:
        request = None
    if request is not None:
        reply = {
            "seq": 1,
            "type": "response",
            "request_seq": request.get("seq", 0),
            "command": request.get("command", ""),
            "success": False,
            "message": text,
            "body": {"error": {"id": 1, "format": text, "showUser": True}},
        }
    else:
        reply = {
            "seq": 1,
            "type": "event",
            "event": "output",
            "body": {"category": "important", "output": text + "\n"},
        }
    try:
        _write_message(client_out, reply)
    except OSError:
        pass
    os._exit(1)


def _unsupported_gdb(problem):
    # Fail at startup, before the client sees an adapter that lacks reverse
    # requests, and say which GDB is wrong rather than leaving a traceback.
    _refuse(
        "managed replay does not support this GDB ({}): {}. "
        "It is tested with GDB 17.2; use --gdb to select a GDB whose DAP "
        "interpreter provides these hooks.".format(gdb.VERSION, problem)
    )


# The reverse requests below rely on these parts of GDB's DAP implementation;
# none of them is a stable public interface, so check them all up front.
_missing_hooks = [
    name
    for name in (
        "send_event",
        "call_function_later",
        "send_gdb",
        "send_gdb_with_response",
        "_commands",
    )
    if not hasattr(server, name)
]
if _missing_hooks or not hasattr(gdb, "with_parameter"):
    _unsupported_gdb(
        "gdb.dap.server lacks {}".format(
            ", ".join(_missing_hooks or ["(gdb.with_parameter)"])
        )
    )
if not callable(getattr(frames, "_clear_frame_ids", None)):
    _unsupported_gdb("gdb.dap.frames lacks _clear_frame_ids")
for _command in ("attach", "disconnect"):
    if _command not in server._commands:
        _unsupported_gdb("gdb.dap has no '{}' request".format(_command))


# GDB's DAP modules do not expose a supported event-suppression interface. Keep
# their handlers connected so they can maintain thread and frame state, but
# hide the process churn caused by an internal replay restart from the client.
#
# Every event leaves through one Server method. GDB 17.2 names it _send_event
# (send_event_maybe_later and the module-level send_event both call it at call
# time); older DAP servers named it send_event. Wrap whichever this GDB has, so
# a renamed sink stops the adapter here instead of leaking restart events.
_event_sink_name = next(
    (
        name
        for name in ("_send_event", "send_event")
        if callable(getattr(server.Server, name, None))
    ),
    None,
)
if _event_sink_name is None:
    _unsupported_gdb("gdb.dap.server.Server has neither _send_event nor send_event")
_original_send_event = getattr(server.Server, _event_sink_name)


def _send_event(self, event, body=None):
    global _last_stopped
    if _suppress_events:
        if event == "stopped":
            _last_stopped = dict(body or {})
        return
    if event == "stopped" and body is not None and "hitBreakpointIds" in body:
        body = dict(body)
        body["hitBreakpointIds"] = [
            breakpoint_id
            for breakpoint_id in body["hitBreakpointIds"]
            if breakpoint_id > 0
        ]
    _original_send_event(self, event, body)


setattr(server.Server, _event_sink_name, _send_event)


def _recording():
    return not _suppress_events or _advancing


# The frame identity holds the newest _FRAME_IDENTITY_DEPTH frames: frame 0
# and up to 7 of its callers. The cap exists because the identity is taken at
# every line arrival the history records during forward execution, and GDB
# spends about 60 microseconds unwinding each frame. A full chain therefore
# makes a forward run cost the square of the stack depth: one continue past a
# recursion 1000 deep took 121.79 s with the full chain and 4.77 s with the
# cap (GDB 17.2, on the host recorded in docs/TESTING_ENVIRONMENTS.md under
# "Named measurement hosts"). With the cap the cost per arrival is bounded.
# hermit_dap_replay_step_back_refuses_an_activation_that_differs_only_at_frame_7
# in hermit-cli/tests/cli.rs fails with any smaller value, and
# hermit_dap_replay_continue_through_deep_recursion_is_fast_and_lands_exactly
# fails without a cap.
_FRAME_IDENTITY_DEPTH = 8


def _frame_identity():
    # The frame identity of a history entry: a (pc, stack pointer) pair for
    # each of the newest _FRAME_IDENTITY_DEPTH frames of the stopped thread,
    # newest first. An older frame's pc is its return address and its stack
    # pointer is the canonical frame address of the frame it called. A
    # deterministic replay reaches the same execution point with the same
    # chain every time.
    #
    # The chain tells apart two activations of one function at different call
    # depths (recursion: the stack pointers differ) and two calls from
    # different call sites at the same depth (sibling calls: a caller's
    # return address differs), as long as that caller is among the newest
    # _FRAME_IDENTITY_DEPTH frames. It cannot tell apart two passes of a loop
    # in one activation, two calls from the same call site at the same depth
    # (a call inside a loop), or two arrivals whose newest frames are all
    # equal and which differ only further out (the same chain of
    # _FRAME_IDENTITY_DEPTH or more nested helper calls, entered from two call
    # sites in one caller): their chains are equal. GDB stops unwinding at
    # main by default, so frames that called main are never part of the chain.
    identity = []
    try:
        frame = gdb.newest_frame()
        while frame is not None and len(identity) < _FRAME_IDENTITY_DEPTH:
            identity.append((int(frame.pc()), int(frame.read_register("sp"))))
            frame = frame.older()
    except (gdb.error, AttributeError, ValueError):
        # The unwinder gave up at this frame. That happens at the same frame
        # on every pass through the same execution point, so the partial
        # chain is still an identity.
        pass
    return tuple(identity) or None


def _describe_frames(identity):
    if identity is None:
        return "unknown"
    return " <- ".join("{:#x}/sp {:#x}".format(pc, sp) for pc, sp in identity)


# TEST-ONLY fault injection, read by nothing but this extension:
# HERMIT_DAP_TEST_ONLY_DROP_LINE_ARRIVAL=LINE:K makes the line breakpoints
# leave the K-th arrival they record at source line LINE out of the history,
# as a debugger stub that loses a breakpoint would. The history then holds
# one arrival too few, which is how hermit's tests drive the frame check and
# its recovery paths. A stop the client sees is still recorded, by
# _remember_stop. Never set it outside a test.
_TEST_ONLY_DROP_VARIABLE = "HERMIT_DAP_TEST_ONLY_DROP_LINE_ARRIVAL"
_test_only_drop = None
_test_only_drop_seen = 0


def _parse_test_only_drop():
    value = os.environ.get(_TEST_ONLY_DROP_VARIABLE)
    if value is None:
        return None
    match = re.fullmatch(r"([1-9][0-9]*):([1-9][0-9]*)", value)
    if match is None:
        return value
    return (int(match.group(1)), int(match.group(2)))


def _test_only_dropped(line):
    global _test_only_drop_seen
    if _test_only_drop is None or _test_only_drop[0] != line:
        return False
    _test_only_drop_seen += 1
    return _test_only_drop_seen == _test_only_drop[1]


def _append_line(pc, file, line, thread_id):
    # One entry per arrival at a line address. Reverse requests count earlier
    # entries with the same pc to choose which hit of a temporary breakpoint
    # to stop at, so an extra or a missing entry moves the target in time.
    if _test_only_dropped(line):
        return
    _history.append(
        {
            "pc": pc,
            "frame": _frame_identity(),
            "file": os.path.realpath(file),
            "line": line,
            "thread_id": thread_id,
            "breakpoint": False,
            "breakpoint_ids": [],
            "counted": False,
        }
    )


def _arrival_leader(pc):
    # GDB often resolves several source lines to one address (a function's
    # opening line and its first statement both land after the prologue) and
    # calls stop() once for each of those breakpoints on a single arrival.
    # Only one line breakpoint at the address records it: the one with the
    # lowest number, which for GDB's negative internal numbers is the one
    # created last. None means no line breakpoint stops at PC, so the
    # history does not hold every arrival there.
    if pc not in _arrival_leaders:
        _arrival_leaders[pc] = min(
            (
                breakpoint.number
                for breakpoint in _line_breakpoints
                if breakpoint.is_valid()
                and any(
                    location.address is not None and int(location.address) == pc
                    for location in breakpoint.locations
                )
            ),
            default=None,
        )
    return _arrival_leaders[pc]


class _LineBreakpoint(gdb.Breakpoint):
    def __init__(self, file, line):
        # An explicit location: a linespec string would misparse a file name
        # that contains a space or a colon.
        super().__init__(source=file, line=line, internal=True)
        self.silent = True
        self.file = file
        self.line = line

    def stop(self):
        if not _recording():
            return False
        try:
            pc = int(gdb.newest_frame().pc())
            if _arrival_leader(pc) in (None, self.number):
                _append_line(
                    pc,
                    self.file,
                    self.line,
                    gdb.selected_thread().global_num,
                )
        except (gdb.error, AttributeError):
            pass
        # _advance_to_a_line runs the replay to the next recorded line.
        return _advancing


class _StopAddressBreakpoint(gdb.Breakpoint):
    # Records every later arrival at an address where the client stopped but
    # no line breakpoint does: the return address a stepOut lands on, the
    # second line-table row a next ends at, an instruction breakpoint. Without
    # it the history holds only the arrivals the client stopped at, and the
    # occurrence count would choose the wrong pass through the address.
    def __init__(self, pc):
        super().__init__("*{:#x}".format(pc), internal=True)
        self.silent = True
        self.pc = pc

    def stop(self):
        if not _recording():
            return False
        try:
            if int(gdb.newest_frame().pc()) == self.pc:
                position = _source_position(None) or {
                    "pc": self.pc,
                    "frame": _frame_identity(),
                    "file": None,
                    "line": 0,
                    "thread_id": gdb.selected_thread().global_num,
                    "breakpoint": False,
                    "breakpoint_ids": [],
                }
                # A bookkeeping entry: it counts arrivals, but stepBack does
                # not stop at it unless the client also stopped there.
                position["counted"] = True
                _history.append(position)
        except (gdb.error, AttributeError):
            pass
        return False


def _line_table_addresses(program):
    # The address of every row in PROGRAM's DWARF line tables. Only the
    # address is taken from readelf. The file name it prints is a line-table
    # file entry's name without the entry's directory, and which entry
    # differs between releases: for GCC 11's DWARF 5 output, binutils 2.35
    # printed file entry 0, where GCC stores the full path, while 2.46 prints
    # the bare name of entry 1; for GCC 15's output both print a bare name.
    # A bare name resolved against GDB's working directory named no source
    # file, so every line breakpoint stayed pending and nothing recorded the
    # source-line path.
    decoded = subprocess.run(
        # --wide keeps each row on one line, however long its file name.
        ["readelf", "--wide", "--debug-dump=decodedline", program],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    ).stdout
    addresses = set()
    for row in decoded.splitlines():
        # A row is a file name, a line number, an address and optional view
        # and statement columns. An end-of-sequence row prints "-" for its
        # line number (binutils 2.46) or repeats the last one (2.35). Its
        # address can resolve to the table of the code that follows, which
        # that code's own rows reach anyway.
        match = re.search(r"(?:^|\s)\d+\s+0x([0-9a-fA-F]+)(?:\s|$)", row)
        if match is not None:
            addresses.add(int(match.group(1), 16))
    return addresses


def _source_lines(addresses):
    # Every (file, line) pair in the GDB symbol tables that hold ADDRESSES,
    # with file and line as GDB itself names them, so that a breakpoint on
    # the pair resolves however readelf printed the row. A table's lines
    # include those whose rows are all non-statement rows, as readelf's do.
    #
    # The addresses are link-time addresses. new_objfile reports the program
    # when the DAP attach request loads it, before GDB connects to the replay
    # and relocates a position-independent program, so link-time addresses
    # are still the addresses GDB knows them by here.
    progspace = gdb.current_progspace()
    symtabs = {}
    for address in addresses:
        symtab = progspace.find_pc_line(address).symtab
        if symtab is not None:
            # A header has one symbol table in every compilation unit that
            # uses it; the unit's static block tells those tables apart.
            block = symtab.static_block()
            key = (symtab.fullname(), block.start, block.end)
            symtabs.setdefault(key, symtab)
    source_lines = set()
    for (file, _, _), symtab in symtabs.items():
        source_lines.update((file, line) for line in symtab.linetable().source_lines())
    return source_lines


def _unknown_source_file(file, line):
    # GDB's own explanation when it does not recognise FILE as a source file
    # name, or None when it does.
    try:
        gdb.decode_line('"{}":{}'.format(file, line))
    except gdb.error as error:
        if str(error).startswith("No source file named"):
            return str(error)
    return None


def _install_line_breakpoints(event=None):
    global _line_breakpoint_problem
    global _line_program
    global _suppress_events

    program = gdb.current_progspace().filename
    if program is None or program == _line_program or not os.path.isfile(program):
        return
    _line_program = program
    _line_breakpoint_problem = None

    problem = None
    try:
        addresses = _line_table_addresses(program)
    except (OSError, subprocess.CalledProcessError) as error:
        addresses = set()
        problem = "failed to read the line tables of {}: {}".format(program, error)
    source_lines = _source_lines(addresses)
    if problem is None and not addresses:
        problem = "readelf lists no line-table rows in {}; build it with -g".format(
            program
        )
    elif problem is None and not source_lines:
        problem = "GDB has no source line at any of the {} addresses in {}".format(
            len(addresses), program
        )

    previous_suppression = _suppress_events
    _suppress_events = True
    _arrival_leaders.clear()
    placed = set()
    unplaced = {}
    try:
        for file, line in sorted(source_lines):
            try:
                breakpoint = _LineBreakpoint(file, line)
            except gdb.error:
                unplaced.setdefault(file, line)
                continue
            if breakpoint.pending:
                # GDB places a line with no statement row at the next line
                # that has one, and leaves it pending when no later line
                # does. Such a line can never be a stop, so its breakpoint
                # records nothing; GDB would retry it at every library load.
                breakpoint.delete()
                unplaced.setdefault(file, line)
            else:
                placed.add(file)
                _line_breakpoints.append(breakpoint)
    finally:
        _suppress_events = previous_suppression

    if problem is None and not placed:
        problem = "GDB placed none of the {} source-line breakpoints in {}".format(
            len(source_lines), program
        )
    if problem is None:
        # A file none of whose lines got a breakpoint is benign when all its
        # rows are non-statement rows, and a defect when GDB does not
        # recognise the name it gave the file.
        for file in sorted(set(unplaced) - placed):
            unknown = _unknown_source_file(file, unplaced[file])
            if unknown is not None:
                problem = "no source-line breakpoint could be set in {}: {}".format(
                    file, unknown
                )
                break
    if problem is not None:
        _line_breakpoint_problem = problem
        # An output event, not gdb.write: the attach request loads the
        # program with gdb.execute(..., to_string=True), so whatever is
        # written meanwhile reaches only GDB's own DAP log, never the client.
        server.send_event(
            "output",
            {
                "category": "stderr",
                "output": "hermit-dap: stepBack and reverseContinue are "
                "unavailable: {}\n".format(problem),
            },
        )


def _require_line_breakpoints():
    # Refuse a reverse request, before it touches the replay, when the
    # source-line path it rewinds along was never recorded.
    if _line_breakpoint_problem is not None:
        raise DAPException(
            "Hermit reverse execution is unavailable: " + _line_breakpoint_problem
        )


def _source_position(event):
    try:
        frame = gdb.newest_frame()
        sal = frame.find_sal()
        if sal.symtab is None or sal.line <= 0:
            return None
        breakpoint_ids = []
        if isinstance(event, gdb.BreakpointEvent):
            breakpoint_ids = [
                breakpoint.number
                for breakpoint in event.breakpoints
                if breakpoint.visible
            ]
        return {
            "pc": int(frame.pc()),
            "frame": _frame_identity(),
            "file": os.path.realpath(sal.symtab.fullname()),
            "line": sal.line,
            "thread_id": gdb.selected_thread().global_num,
            "breakpoint": bool(breakpoint_ids),
            "breakpoint_ids": breakpoint_ids,
            "counted": False,
        }
    except (gdb.error, AttributeError):
        return None


def _remember_stop(event):
    global _suppress_events

    if _suppress_events:
        return

    position = _source_position(event)
    if position is None:
        return
    # A stop at the address a line or stop-address breakpoint just recorded
    # is that same arrival, even when the recording breakpoint names another
    # source line that resolves to the same address.
    if (
        _history
        and _history[-1]["pc"] == position["pc"]
        and _history[-1]["thread_id"] == position["thread_id"]
    ):
        _history[-1].update(position)
    else:
        _history.append(position)

    pc = position["pc"]
    if _arrival_leader(pc) is None and pc not in _stop_address_breakpoints:
        _suppress_events = True
        try:
            _stop_address_breakpoints[pc] = _StopAddressBreakpoint(pc)
        except gdb.error:
            pass
        finally:
            _suppress_events = False


def _wait_for_replay(timeout=5.0):
    global _replay_process
    if _replay_process is None:
        return
    try:
        _replay_process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        _replay_process.kill()
        _replay_process.wait()
    _replay_process = None


def _terminate_replay():
    if _replay_process is None:
        return
    _replay_process.kill()
    _wait_for_replay(0)


def _start_replay():
    global _replay_process
    _replay_process = subprocess.Popen(
        [_setpriv, "--pdeathsig", "SIGKILL", "--"] + _replay_command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        close_fds=True,
    )
    time.sleep(0.5)
    status = _replay_process.poll()
    if status is not None:
        _replay_process = None
        raise gdb.error("Hermit replay exited before GDB attached: {}".format(status))


def _connect_replay():
    last_error = None
    for _ in range(200):
        try:
            gdb.execute(
                "target remote " + _replay_target, from_tty=False, to_string=True
            )
            return
        except gdb.error as error:
            last_error = error
            time.sleep(0.05)
    raise gdb.error("Hermit replay did not accept a GDB connection: " + str(last_error))


def _recording_breakpoints():
    return [
        breakpoint
        for breakpoint in (gdb.breakpoints() or [])
        if breakpoint.is_valid()
        and breakpoint.enabled
        and (
            breakpoint.visible
            or isinstance(breakpoint, (_LineBreakpoint, _StopAddressBreakpoint))
        )
    ]


def _restart_replay():
    # Replace the replay with a fresh one stopped at its entry.
    with gdb.with_parameter("confirm", False):
        try:
            gdb.execute("kill", from_tty=False, to_string=True)
        except gdb.error:
            pass
    _wait_for_replay()
    try:
        gdb.execute("disconnect", from_tty=False, to_string=True)
    except gdb.error:
        pass

    _start_replay()
    _connect_replay()
    # GDB's DAP layer caches frames per thread and drops the cache only
    # when the inferior resumes. A restart that stops at the replay's
    # entry never resumes, so drop the killed inferior's frames here.
    frames._clear_frame_ids(None)


class _WrongFrame(Exception):
    # A rewind stopped at the right address but in another frame than the
    # history entry it was aiming for, so the arrival count it relied on was
    # wrong. Reported to the client as an error instead of a stop at the
    # wrong time.
    pass


def _run_to(pc, occurrence, frame=None):
    # Continue the replay to the OCCURRENCE-th arrival at PC since it started,
    # and check that it stopped with the frame identity FRAME (when FRAME is
    # known; see _frame_identity).
    target = gdb.Breakpoint(
        "*{:#x}".format(pc),
        type=gdb.BP_BREAKPOINT,
        internal=True,
        temporary=True,
    )
    try:
        target.ignore_count = max(0, occurrence - 1)
        gdb.execute("continue", from_tty=False, to_string=True)
        if int(gdb.newest_frame().pc()) != pc:
            raise gdb.error("replay did not stop at the requested source position")
        landed = _frame_identity()
        if frame is not None and landed != frame:
            raise _WrongFrame(
                "arrival {} at {:#x} has the frames {}, but the history entry "
                "was recorded with {}".format(
                    occurrence,
                    pc,
                    _describe_frames(landed),
                    _describe_frames(frame),
                )
            )
    finally:
        if target.is_valid():
            target.delete()


class _ArrivalCounter(gdb.Breakpoint):
    def __init__(self, pc):
        super().__init__("*{:#x}".format(pc), internal=True)
        self.silent = True
        self.pc = pc
        self.arrivals = 0

    def stop(self):
        try:
            if int(gdb.newest_frame().pc()) == self.pc:
                self.arrivals += 1
        except gdb.error:
            pass
        return False


def _count_arrivals(pc, marker):
    # Restart the replay and count its arrivals at PC before it reaches
    # MARKER, a (pc, occurrence, frame) triple, or before it exits when MARKER is
    # None.
    global _suppress_events

    disabled_breakpoints = []
    counter = None
    _suppress_events = True
    try:
        disabled_breakpoints = _recording_breakpoints()
        for breakpoint in disabled_breakpoints:
            breakpoint.enabled = False
        _restart_replay()
        counter = _ArrivalCounter(pc)
        if marker is None:
            gdb.execute("continue", from_tty=False, to_string=True)
            if gdb.selected_inferior().pid != 0:
                raise gdb.error("replay stopped before it exited")
        else:
            _run_to(*marker)
        return counter.arrivals
    finally:
        if counter is not None and counter.is_valid():
            counter.delete()
        for breakpoint in disabled_breakpoints:
            if breakpoint.is_valid():
                breakpoint.enabled = True
        _suppress_events = False


def _advance_to_a_line():
    # Run the live replay forward, recording as usual, until it arrives at a
    # line breakpoint (or exits). Returns whether it is still alive.
    global _advancing
    global _suppress_events

    disabled_breakpoints = [
        breakpoint
        for breakpoint in (gdb.breakpoints() or [])
        if breakpoint.is_valid() and breakpoint.enabled and breakpoint.visible
    ]
    _suppress_events = True
    _advancing = True
    try:
        for breakpoint in disabled_breakpoints:
            breakpoint.enabled = False
        gdb.execute("continue", from_tty=False, to_string=True)
    finally:
        for breakpoint in disabled_breakpoints:
            if breakpoint.is_valid():
                breakpoint.enabled = True
        _advancing = False
        _suppress_events = False
    return gdb.selected_inferior().pid != 0


def _line_entry_after(index):
    return next(
        (
            later
            for later in range(index + 1, len(_history))
            if _arrival_leader(_history[later]["pc"]) is not None
        ),
        None,
    )


def _line_entry_reached_after(index):
    # The index of a line entry after INDEX, running the live replay forward,
    # recording as usual, until the history holds one. None when the replay
    # exits first; the history then holds every arrival after INDEX.
    marker_index = _line_entry_after(index)
    for _ in range(_ADVANCE_LIMIT):
        if marker_index is not None:
            return marker_index
        if gdb.selected_inferior().pid == 0 or not _advance_to_a_line():
            return None
        marker_index = _line_entry_after(index)
    if marker_index is None:
        raise gdb.error("replay did not reach a source line")
    return marker_index


def _occurrence(index, advance=True):
    # Which arrival at its pc, counted from the start of the replay, the
    # history entry at INDEX is. ADVANCE=False refuses to run the live replay
    # forward (below), for when the live replay is not where the history
    # ends.
    entry = _history[index]
    pc = entry["pc"]
    if _arrival_leader(pc) is not None:
        # A line breakpoint records every arrival at a line address, as long
        # as the debugger stub keeps its int3 in place. Reverie's gdbstub
        # did not always: it restored a whole 8-byte word when it removed a
        # software breakpoint, which could erase the int3 of a line
        # breakpoint up to 7 bytes after it, and the count came out short
        # (fixed in https://github.com/rrnewton/reverie/pull/890). A short or
        # long count makes _run_to land at another arrival, and its frame
        # check refuses that landing whenever the frame identity differs.
        return sum(1 for earlier in _history[: index + 1] if earlier["pc"] == pc)

    # No line breakpoint covers PC: the history holds the arrival the client
    # first stopped at and, through its _StopAddressBreakpoint, every later
    # one, but none before. So replay from the start to a later line entry,
    # whose own occurrence is exact, counting arrivals at PC on the way, and
    # subtract the ones the history holds between this entry and that one.
    # With no line entry after this one yet, run the live replay forward to
    # the next line arrival first: counting to the replay's exit instead
    # would also count the arrivals after the current position, which the
    # history does not hold.
    if advance:
        marker_index = _line_entry_reached_after(index)
    else:
        marker_index = _line_entry_after(index)
        if marker_index is None:
            raise gdb.error("no later source line to count arrivals against")
    if marker_index is None:
        arrivals = _count_arrivals(pc, None)
        later = _history[index + 1 :]
    else:
        marker = _history[marker_index]
        arrivals = _count_arrivals(
            pc, (marker["pc"], _occurrence(marker_index), marker["frame"])
        )
        later = _history[index + 1 : marker_index]
    occurrence = arrivals - sum(1 for entry_after in later if entry_after["pc"] == pc)
    if occurrence < 1:
        raise gdb.error("replay never reached the requested source position")
    return occurrence


def _rewind(target_index, reason, breakpoint_ids=None, occurrence=None):
    # Restart the replay and stop it where the history entry at TARGET_INDEX
    # was, or at the replay's entry when TARGET_INDEX is negative. OCCURRENCE
    # is the entry's arrival count when the caller has it already. Returns the
    # body of the stopped event. Raises _WrongFrame, with the history left
    # alone, when the replay stops in another frame than the entry's.
    global _last_stopped
    global _suppress_events

    position = None
    if target_index >= 0:
        position = _history[target_index]
        if occurrence is None:
            occurrence = _occurrence(target_index)
    disabled_breakpoints = []
    _last_stopped = None
    _suppress_events = True
    try:
        disabled_breakpoints = _recording_breakpoints()
        for breakpoint in disabled_breakpoints:
            breakpoint.enabled = False
        _restart_replay()
        if position is not None:
            _run_to(position["pc"], occurrence, position["frame"])

        body = dict(_last_stopped or {})
        body["reason"] = reason
        body["threadId"] = gdb.selected_thread().global_num
        body["allThreadsStopped"] = True
        if breakpoint_ids is not None:
            body["hitBreakpointIds"] = breakpoint_ids
        else:
            body.pop("hitBreakpointIds", None)
    finally:
        for breakpoint in disabled_breakpoints:
            if breakpoint.is_valid():
                breakpoint.enabled = True
        _suppress_events = False
    if target_index >= 0:
        del _history[target_index + 1 :]
    else:
        _history.clear()
    return body


class _StayedAtCurrentStop(Exception):
    # A reverse request could not reach its target exactly and put the replay
    # back at the client's current stop. BODY is the stopped event for that
    # stop: the restart invalidated the client's frame ids.
    def __init__(self, message, body):
        super().__init__(message)
        self.body = body


def _rewind_or_stay(target_index, reason, breakpoint_ids=None):
    # _rewind, except that when the replay lands in the wrong frame it is put
    # back at the client's current stop, the last history entry, and the
    # request fails with _StayedAtCurrentStop instead of reporting a stop at
    # the wrong time.
    current_index = len(_history) - 1
    current_occurrence = None
    if (
        target_index >= 0
        and current_index >= 0
        and _arrival_leader(_history[current_index]["pc"]) is None
    ):
        # The current stop is not a line start (a stepOut's return address,
        # say), so counting its arrival needs a line entry after it (see
        # _occurrence). Once the rewind below has restarted the replay,
        # nothing can record one, so record it now, while the live replay is
        # still at the current stop. If the replay exits first, the history
        # holds every arrival after the current stop, and the count is taken
        # now, against the exit.
        if _line_entry_reached_after(current_index) is None:
            current_occurrence = _occurrence(current_index)
    try:
        return _rewind(target_index, reason, breakpoint_ids)
    except _WrongFrame as wrong:
        problem = str(wrong)
    if current_index < 0:
        raise gdb.error("the rewind landed in the wrong frame: " + problem)
    # The live replay is now somewhere else, so counting the current stop's
    # arrival must not run it forward. Entries after CURRENT_INDEX came from
    # the real path and may be used.
    if current_occurrence is None:
        current_occurrence = _occurrence(current_index, advance=False)
    body = _rewind(current_index, "step", None, current_occurrence)
    body["description"] = "Returned to the current stop"
    raise _StayedAtCurrentStop(
        "Hermit could not reach the earlier stop exactly, so the replay stays "
        "at the current stop ({}).".format(problem),
        body,
    )


def _report_reverse_failure(operation, error):
    server.send_event(
        "output",
        {
            "category": "stderr",
            "output": "hermit-dap: {} failed: {}\n".format(operation, error),
        },
    )
    server.send_event("terminated")


def _reverse_request(operation, run):
    # Run a reverse request's rewind on GDB's thread while the request is still
    # in flight, so that a failure becomes the request's failed response. The
    # request is registered with defer_events=False: the events of the replay
    # restart must reach _send_event while _suppress_events still hides them,
    # not after the response. The stopped event goes out after the response.
    # The delayed functions bind their values as default arguments: Python
    # deletes an except clause's "as" name when the clause ends, before the
    # delayed function runs.
    try:
        body = server.send_gdb_with_response(run)
    except _StayedAtCurrentStop as stayed:
        server.call_function_later(
            lambda body=stayed.body: server.send_event("stopped", body)
        )
        raise DAPException(str(stayed))
    except DAPException:
        # Raised before the replay was touched (bad arguments).
        raise
    except BaseException as error:
        # The replay is in an unknown state: end the session.
        server.call_function_later(
            lambda error=error: _report_reverse_failure(operation, error)
        )
        raise
    server.call_function_later(lambda: server.send_event("stopped", body))


def _step_back(thread_id, granularity):
    if granularity != "statement":
        raise DAPException("Hermit stepBack currently supports statement granularity")
    _require_line_breakpoints()
    set_thread(thread_id)
    matching = [
        index
        for index, position in enumerate(_history)
        if position["thread_id"] == thread_id and not position["counted"]
    ]
    target_index = matching[-2] if len(matching) >= 2 else -1
    return _rewind_or_stay(target_index, "step")


@capability("supportsStepBack")
@request("stepBack", on_dap_thread=True, defer_events=False)
def step_back(
    *, threadId: int, singleThread: bool = False, granularity: str = "statement", **args
):
    if singleThread:
        raise DAPException("Hermit reverse execution restarts the whole replay")
    _reverse_request("stepBack", lambda: _step_back(threadId, granularity))


def _visible_breakpoints_by_pc():
    result = {}
    for breakpoint in gdb.breakpoints() or []:
        if (
            not breakpoint.is_valid()
            or not breakpoint.enabled
            or not breakpoint.visible
        ):
            continue
        for location in breakpoint.locations:
            if location.enabled and location.address is not None:
                result.setdefault(int(location.address), []).append(breakpoint.number)
    return result


def _reverse_continue(thread_id):
    _require_line_breakpoints()
    set_thread(thread_id)
    breakpoints = _visible_breakpoints_by_pc()
    target_index = next(
        (
            index
            for index in range(len(_history) - 2, -1, -1)
            if _history[index]["pc"] in breakpoints
        ),
        -1,
    )
    if target_index >= 0:
        return _rewind_or_stay(
            target_index, "breakpoint", breakpoints[_history[target_index]["pc"]]
        )
    return _rewind_or_stay(target_index, "entry")


@request("reverseContinue", on_dap_thread=True, defer_events=False)
def reverse_continue(*, threadId: int, singleThread: bool = False, **args):
    if singleThread:
        raise DAPException("Hermit reverse execution restarts the whole replay")
    _reverse_request("reverseContinue", lambda: _reverse_continue(threadId))


def _kill_replay_for_disconnect():
    global _suppress_events
    previous_suppression = _suppress_events
    _suppress_events = True
    try:
        with gdb.with_parameter("confirm", False):
            try:
                gdb.execute("kill", from_tty=False, to_string=True)
            except gdb.error:
                pass
        _wait_for_replay()
    finally:
        _suppress_events = previous_suppression


_original_attach = server._commands["attach"]
_original_disconnect = server._commands["disconnect"]


def _attach(**args):
    if args.get("target") != _replay_target:
        raise DAPException(
            "managed replay requires DAP target {}".format(_replay_target)
        )
    return _original_attach(**args)


def _disconnect(**args):
    try:
        server.send_gdb_with_response(_kill_replay_for_disconnect)
    except Exception:
        pass
    args["terminateDebuggee"] = False
    return _original_disconnect(**args)


server._commands["attach"] = _attach
server._commands["disconnect"] = _disconnect


def _cleanup(event):
    _terminate_replay()


_setpriv = (
    "/usr/bin/setpriv"
    if os.path.isfile("/usr/bin/setpriv")
    else shutil.which("setpriv")
)
_missing_tools = [
    name
    for name, path in (("readelf", shutil.which("readelf")), ("setpriv", _setpriv))
    if path is None
]
if _missing_tools:
    _refuse(
        "managed replay requires readelf and setpriv on PATH (missing: {})".format(
            ", ".join(_missing_tools)
        )
    )
_test_only_drop = _parse_test_only_drop()
if isinstance(_test_only_drop, str):
    _refuse(
        "{} must be LINE:K with positive integers, not {!r}".format(
            _TEST_ONLY_DROP_VARIABLE, _test_only_drop
        )
    )

gdb.events.new_objfile.connect(_install_line_breakpoints)
gdb.events.stop.connect(_remember_stop)
gdb.events.gdb_exiting.connect(_cleanup)
try:
    _start_replay()
except Exception as error:
    _refuse("failed to start replay: {}".format(error))

# Startup succeeded; from here on GDB's DAP server owns the client's streams.
if _client_fds is not None:
    for _fd in _client_fds:
        try:
            os.close(_fd)
        except OSError:
            pass
    _client_fds = None
