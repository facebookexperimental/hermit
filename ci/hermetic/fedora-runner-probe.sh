#!/usr/bin/env bash
# READ-ONLY probe for the self-hosted runner box. Answers runner questions that
# cannot be answered from the other validate host.
#
# WHAT IT DOES: lists containers, reads their config, and runs `exec` inside them
# to look at /dev/kvm, the PMU settings and network reachability.
# WHAT IT DOES NOT DO: it does not register, deregister, start, stop, restart or
# modify any runner or container; it pulls no images; and it CREATES NO FILES
# AND DELETES NOTHING -- there is no `rm`, no redirect to a file, and no temp
# directory anywhere in it. Every container command is `ps`, `inspect`, or
# `exec` of a read-only shell command; there are no run/pull/rm/stop/start
# calls. Three things do execute: `curl` (to /dev/null), where perf exists
# `perf stat -e branches true`, and a direct `perf_event_open` availability
# probe. All are reads, but as root they run as root inside the container, so
# they are named here rather than glossed.
#
# SAFE TO RUN AS ROOT, and you probably must -- see the rootless warning it
# prints if it cannot see root-owned containers.
#
#   usage:  ./fedora-runner-probe.sh          (run as whoever can talk to the runtime;
#                                              if the containers are root-owned that
#                                              probably means sudo)

set -uo pipefail

# There is deliberately NO temp directory and NO `rm` anywhere in this file.
# An earlier draft created one with `mktemp -d` and removed it on exit, and
# nothing ever wrote to it. That is the shape of a known near-miss on this
# project -- an `rm -rf` on a path built from a variable that could come back
# empty -- so it is gone rather than guarded. The only occurrences of that
# verb left in this file are in this paragraph.

say()  { printf '\n%s\n' "$*"; }
line() { printf '  %s\n' "$*"; }

# ---------------------------------------------------------------- runtime ----
# Do not assume podman. Prefer whichever one actually has containers.
RT=""
for candidate in podman docker; do
    command -v "$candidate" >/dev/null 2>&1 || continue
    if "$candidate" ps --format '{{.Names}}' >/dev/null 2>&1; then RT=$candidate; break; fi
done
if [[ -z "$RT" ]]; then
    echo "FAIL: neither podman nor docker is usable here." >&2
    echo "  Both were tried. If the containers are root-owned, re-run with sudo." >&2
    exit 2
fi

echo "================ runner probe ================"
line "container runtime: $RT ($($RT --version 2>/dev/null | head -1))"
line "host: $(hostname)  kernel: $(uname -r)  as: $(id -un)"

# ROOTLESS BLINDNESS. A rootless podman sees ONLY this user's containers and
# gives no hint that root-owned ones exist. Measured: run as an ordinary user on
# a box whose hermit runners run as root, this script found five unrelated
# user-owned containers and reported on them confidently. A probe that presents
# a partial picture as the whole is worse than one that finds nothing, so say so
# loudly and unconditionally rather than only when the list is empty.
ROOTLESS="unknown"
if [[ $(id -u) -eq 0 ]]; then
    ROOTLESS="no (running as root)"
elif [[ $RT == podman ]]; then
    case "$(podman info --format '{{.Host.Security.Rootless}}' 2>/dev/null)" in
        true)  ROOTLESS="YES" ;;
        false) ROOTLESS="no (rootful podman as non-root)" ;;
        *)     ROOTLESS="probably (non-root user, could not query podman)" ;;
    esac
else
    ROOTLESS="unknown (docker client as non-root; the daemon may still be root)"
fi
line "rootless: $ROOTLESS"

mapfile -t ALL < <($RT ps --format '{{.Names}}' 2>/dev/null)
line "containers visible to this user: ${#ALL[@]}${ALL:+ -- ${ALL[*]}}"
if [[ $ROOTLESS == YES* || $ROOTLESS == probably* ]]; then
    echo
    echo "  ****************************************************************"
    echo "  * WARNING: this is a ROOTLESS container runtime.                *"
    echo "  * Root-owned containers are INVISIBLE to it. If the runners run *"
    echo "  * as root -- and hermit's do -- they will NOT appear below, and *"
    echo "  * any containers that DO appear are probably not the runners.   *"
    echo "  * RE-RUN WITH sudo BEFORE BELIEVING ANY ANSWER HERE.            *"
    echo "  ****************************************************************"
fi
if [[ ${#ALL[@]} -eq 0 ]]; then
    line "no running containers visible to this user"
fi

# Identify which containers are actually running a GitHub Actions runner.
RUNNERS=()
for c in "${ALL[@]:-}"; do
    [[ -n "$c" ]] || continue
    if $RT exec "$c" sh -c 'ps -eo args 2>/dev/null | grep -q "[R]unner.Listener"' 2>/dev/null; then
        RUNNERS+=("$c")
    fi
done

# ---------------------------------------------- Q1: containerized, as root? --
say "Q1. Are the runners containerized, and running as root?"
if [[ ${#RUNNERS[@]} -eq 0 ]]; then
    line "ANSWER: no running container has a Runner.Listener process."
    line "Either the runners are not containerized here, or they are not visible to $(id -un)."
    line "Runner.Listener processes on the HOST (outside any container):"
    # shellcheck disable=SC2009  # ps, not pgrep: the report wants the USER column too.
    ps -eo pid,user,args 2>/dev/null | grep "[R]unner.Listener" | sed 's/^/    /' || line "    none"
else
    line "ANSWER: yes -- ${#RUNNERS[@]} container(s) are running a runner."
    for c in "${RUNNERS[@]}"; do
        # shellcheck disable=SC2016  # single quotes are deliberate: $(pgrep ...) must
        # expand INSIDE the container, not on the host running this script.
        u=$($RT exec "$c" sh -c 'ps -o user= -p $(pgrep -f "[R]unner.Listener" | head -1) 2>/dev/null' 2>/dev/null | tr -d ' ')
        uid=$($RT exec "$c" id -u 2>/dev/null | tr -d ' ')
        priv=$($RT inspect --format '{{.HostConfig.Privileged}}' "$c" 2>/dev/null)
        line "$c: runner process user=${u:-?} (exec uid=${uid:-?})  privileged=${priv:-?}"
    done
fi

# ------------------------------------------------------- Q2: which image? ----
say "Q2. What image do they run? (could it be replaced by the pinned one?)"
if [[ ${#RUNNERS[@]} -eq 0 ]]; then
    line "not applicable -- no runner containers found"
else
    for c in "${RUNNERS[@]}"; do
        img=$($RT inspect --format '{{.Config.Image}}' "$c" 2>/dev/null)
        iid=$($RT inspect --format '{{.Image}}' "$c" 2>/dev/null)
        line "$c: image=${img:-?}"
        line "    image id/digest=${iid:-?}"
    done
    line "Replaceable if the runner is launched from a compose/systemd unit naming this image;"
    line "the launch definition is what would change, not anything inside the container."
fi

# --------------------------------------------------- Q3: ghcr reachability ---
# 401 is the CORRECT response from a live registry to an anonymous request.
# 000 means the request never completed, i.e. unreachable.
say "Q3. Can the runners reach ghcr.io? (401 = reachable, 000 = unreachable)"
probe_ghcr() {  # $1 = label, rest = command prefix
    local label=$1; shift
    local code
    code=$("$@" curl -s -o /dev/null -w '%{http_code}' --max-time 20 https://ghcr.io/v2/ 2>/dev/null)
    line "$label: ${code:-000} $( [[ ${code:-000} == 401 ]] && echo '(reachable)' || echo '(NOT reachable)')"
}
line "from the fedora HOST:"
probe_ghcr "  direct        " env
if command -v with-proxy >/dev/null 2>&1; then
    probe_ghcr "  via with-proxy" with-proxy
else
    line "  via with-proxy: with-proxy not present on this host"
fi
if [[ ${#RUNNERS[@]} -gt 0 ]]; then
    for c in "${RUNNERS[@]}"; do
        line "from INSIDE $c:"
        code=$($RT exec "$c" sh -c 'command -v curl >/dev/null 2>&1 && curl -s -o /dev/null -w "%{http_code}" --max-time 20 https://ghcr.io/v2/ || echo NOCURL' 2>/dev/null)
        line "  direct        : ${code:-000} $( [[ ${code:-000} == 401 ]] && echo '(reachable)' || echo '(NOT reachable)')"
        code=$($RT exec "$c" sh -c 'command -v with-proxy >/dev/null 2>&1 && with-proxy curl -s -o /dev/null -w "%{http_code}" --max-time 20 https://ghcr.io/v2/ || echo NOPROXYWRAPPER' 2>/dev/null)
        line "  via with-proxy: ${code:-000}"
    done
fi

# ------------------------------------------------ Q4: /dev/kvm and the PMU ---
# Deliberately checked from INSIDE the container. What the fedora host can see
# says nothing about what the container is given.
say "Q4. Do the runners have /dev/kvm and PMU access FROM INSIDE the container?"
if [[ ${#RUNNERS[@]} -eq 0 ]]; then
    line "not applicable -- no runner containers found; host values for reference:"
    line "  /dev/kvm on host: $(ls -l /dev/kvm 2>/dev/null || echo absent)"
    line "  perf_event_paranoid: $(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo '?')"
else
    for c in "${RUNNERS[@]}"; do
        line "$c:"
        line "  /dev/kvm: $($RT exec "$c" sh -c 'ls -l /dev/kvm 2>/dev/null || echo ABSENT' 2>/dev/null)"
        line "  /dev/kvm read+write: $($RT exec "$c" sh -c '[ -r /dev/kvm ] && [ -w /dev/kvm ] && echo YES || echo NO' 2>/dev/null)"
        line "  perf_event_paranoid: $($RT exec "$c" sh -c 'cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo unreadable' 2>/dev/null)"
        line "  capabilities (CapEff): $($RT exec "$c" sh -c 'grep CapEff /proc/self/status 2>/dev/null' 2>/dev/null)"
        # A real PMU test beats reading preconditions, when perf happens to exist.
        pmu=$($RT exec "$c" sh -c 'command -v perf >/dev/null 2>&1 && (perf stat -e branches true >/dev/null 2>&1 && echo COUNTED || echo "NONZERO (see Q5)") || echo "perf not installed"' 2>/dev/null)
        line "  perf stat -e branches: ${pmu:-?}"
    done
    line "NOTE: the perf CLI result alone is not Hermit's PMU availability check."
    line "Q5 makes Hermit's initial perf_event_open probe directly and reports its errno."
fi

# ------------------------- Q5: WHY did perf exit nonzero, if it did? ----------
# Q4 answers "did `perf stat` exit 0". That is NOT the same question as "does
# Hermit's initial PMU availability probe pass", and the first run conflated them.
#
#   1. Q4 discards perf's stderr and reports only COUNTED/REFUSED from the exit
#      status. A perf binary that is simply the wrong version for the running
#      kernel, or an event name the PMU does not expose, exits nonzero and reads
#      as "REFUSED" -- which sounds like a permission denial and is not one.
#   2. NOTHING IN HERMIT RUNS THE `perf` TOOL. reverie calls perf_event_open(2)
#      directly (reverie-ptrace/src/perf.rs), with exclude_kernel=1 set
#      explicitly to lower the permission bar. So the only measurement that
#      settles it is that exact syscall, with those exact attributes.
#
# This section makes the syscall itself, twice, and prints the errno by name.
# The pair distinguishes the common availability cases -- read the table at
# the bottom of the output. It does not replace Hermit's fuller PMU validation.
#
# WHY PYTHON AND NOT rust-script. The repo convention prefers rust-script, but
# this code has to execute INSIDE whatever the runner container happens to be,
# where rust-script and cargo are not present and cannot be installed by a
# read-only probe. python3 is in the runner image, and ctypes needs no compiler
# and writes no file. If python3 is absent the section says so rather than
# guessing.
PERF_PY='
import ctypes, ctypes.util, platform, struct
NR = {"x86_64": 298, "aarch64": 241}.get(platform.machine())
if NR is None:
    print("arch-unsupported " + platform.machine()); raise SystemExit(0)
libc = ctypes.CDLL(ctypes.util.find_library("c") or "libc.so.6", use_errno=True)
libc.syscall.restype = ctypes.c_long
def probe(exclude_kernel):
    SIZE = 128
    buf = bytearray(SIZE)
    struct.pack_into("<IIQQQQQ", buf, 0,
                     0,                        # type   = PERF_TYPE_HARDWARE
                     SIZE,                     # size
                     0,                        # config = PERF_COUNT_HW_INSTRUCTIONS
                     1 << 60,                  # sample_period (reverie DISABLE_SAMPLE_PERIOD)
                     0, 0,                     # sample_type, read_format
                     (1 << 5) if exclude_kernel else 0)   # bit 5 = exclude_kernel
    cbuf = (ctypes.c_char * SIZE).from_buffer(buf)
    ctypes.set_errno(0)
    fd = libc.syscall(ctypes.c_long(NR), ctypes.byref(cbuf),
                      ctypes.c_long(0),   # pid = 0, this thread
                      ctypes.c_long(-1),  # cpu = -1, any
                      ctypes.c_long(-1),  # group_fd
                      ctypes.c_ulong(8))  # PERF_FLAG_FD_CLOEXEC
    if fd >= 0:
        libc.close(ctypes.c_int(fd)); return "OK"
    import errno as E
    e = ctypes.get_errno()
    return E.errorcode.get(e, "errno=%d" % e)
print("exclude_kernel=1 -> %s   exclude_kernel=0 -> %s" % (probe(True), probe(False)))
'
say "Q5. If perf exits nonzero, what does Hermit's initial PMU probe report?"
q5_one() {  # $1 = human label, rest = command prefix to run a shell inside the target
    local label=$1; shift
    line "$label"
    line "  user namespace  : $("$@" sh -c 'readlink /proc/self/ns/user' 2>/dev/null)"
    line "  uid_map         : $("$@" sh -c 'tr "\n" "|" < /proc/self/uid_map' 2>/dev/null)"
    line "  seccomp         : $("$@" sh -c 'grep -E "^Seccomp" /proc/self/status | tr "\n" " "' 2>/dev/null)"
    line "  paranoid        : $("$@" sh -c 'cat /proc/sys/kernel/perf_event_paranoid' 2>/dev/null)"
    line "  lockdown        : $("$@" sh -c 'cat /sys/kernel/security/lockdown 2>/dev/null || echo "(not exposed)"' 2>/dev/null)"
    line "  PMU event srcs  : $("$@" sh -c 'ls /sys/bus/event_source/devices 2>/dev/null | tr "\n" " " || echo "(none)"' 2>/dev/null)"
    line "  hypervisor flag : $("$@" sh -c 'grep -qw hypervisor /proc/cpuinfo && echo "PRESENT (this is a VM; vPMU may be absent)" || echo "absent (bare metal)"' 2>/dev/null)"
    # shellcheck disable=SC2016  # single quotes are deliberate, same reason as Q1:
    # $(uname -r) and $(perf --version) must expand INSIDE the target, not here.
    line "  kernel / perf   : $("$@" sh -c 'printf "%s / %s" "$(uname -r)" "$(perf --version 2>/dev/null || echo "perf not installed")"' 2>/dev/null)"
    line "  perf_event_open : $("$@" python3 -c "$PERF_PY" 2>/dev/null || echo '(python3 unavailable -- cannot make the syscall)')"
    line "  perf stat stderr: $("$@" sh -c 'command -v perf >/dev/null 2>&1 && (perf stat -e branches true 2>&1 | grep -v "^$" | tail -3 | tr "\n" "|") || echo "(perf not installed)"' 2>/dev/null)"
}
line "on the fedora HOST, for comparison:"
q5_one "  host" env
if [[ ${#RUNNERS[@]} -eq 0 ]]; then
    line "no runner containers to compare against"
else
    for c in "${RUNNERS[@]}"; do
        q5_one "inside $c:" "$RT" exec "$c"
    done
fi
say "Q5b. Does the image digest split explain which containers have perf?"
if [[ ${#RUNNERS[@]} -eq 0 ]]; then
    line "not applicable -- no runner containers found"
else
    line "one row per container, so the correlation is read off the output, not guessed:"
    for c in "${RUNNERS[@]}"; do
        iid=$($RT inspect --format '{{.Image}}' "$c" 2>/dev/null)
        has=$($RT exec "$c" sh -c 'command -v perf >/dev/null 2>&1 && echo yes || echo no' 2>/dev/null)
        line "  $c  image=${iid:-?}  perf-installed=${has:-?}"
    done
    line "NOTE: ci-hub/runners/Containerfile has NEVER installed perf or linux-tools"
    line "(checked over its whole git history). So a container that HAS perf did not"
    line "get it from either image build, and the digest split cannot be the whole story."
fi
say "HOW TO READ Q5 -- the pair reports the initial hardware-event availability check"
line "exclude_kernel=1 OK,     exclude_kernel=0 EACCES"
line "     -> NORMAL for perf_event_paranoid=2. Hermit's initial availability"
line "        probe uses exclude_kernel=1 (reverie-ptrace/src/perf.rs:817) and"
line "        would pass. Full Hermit PMU validation remains a separate check."
line "        A nonzero perf TOOL result alongside this is not evidence that"
line "        perf_event_open was denied."
line "BOTH ENOENT / ENODEV / EOPNOTSUPP"
line "     -> the hardware-instructions event used by the availability probe is"
line "        unavailable. Cross-check the hypervisor flag and PMU event-source"
line "        list; the errno alone does not prove that every PMU event is absent."
line "BOTH EACCES"
line "     -> Hermit's initial availability probe would fail an access check."
line "        Compare the user namespace with the host: a container in its own"
line "        userns can report CapEff 000001ffffffffff without holding the same"
line "        capability in the initial namespace, so CapEff alone proves nothing."
line "BOTH EPERM"
line "     -> inspect seccomp, LSM/lockdown, and unsupported attribute policies;"
line "        this errno alone does not identify which layer refused the request."

say "================ end of probe ================"
line "Nothing was modified. No images pulled, no containers started or stopped."
