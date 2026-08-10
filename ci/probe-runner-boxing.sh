#!/usr/bin/env bash
# Can THIS machine box a CI step, and does a cgroup kill reach a setsid escapee?
#
# DIAGNOSTIC ONLY. Never fails its caller, never changes a default, never gates a lane. It answers
# one question that has been ASSUMED rather than measured: safe-ci-dag-runner skips cgroup boxing
# whenever $CI or $GITHUB_ACTIONS is set (cgroup.rs reexec_in_scope returns success WITHOUT entering
# a scope), and the real capability probe sits after that early return, so it never executes on a
# runner. Every lane therefore runs unboxed on an assumption no one has tested.
#
# Two facts, printed, from the runner itself:
#   FACT1_SCOPE_CREATED   -- can a delegated `systemd-run --user --scope` be created here at all?
#   FACT2_ESCAPEE_KILLED  -- does `cgroup.kill` terminate a setsid child that a process-group kill
#                            cannot reach? This is the property the whole teardown design rests on:
#                            setsid changes session and pgid but NOT cgroup membership.
#
# A NO on FACT1 means unboxed operation is a genuine capability limit and the current escape hatch
# is correct. A YES means boxing is being skipped by an environment-variable check on a machine that
# could enforce it. Those are different problems; this script does not choose between them.
set +e

say() { printf '%s\n' "$*"; }
say "=== runner identity ==="
say "host=$(hostname -s 2>/dev/null) uid=$(id -u) user=$(id -un)"
say "cgroup_fs=$(stat -fc %T /sys/fs/cgroup 2>/dev/null)"
say "self_cgroup=$(awk -F: '{print $3}' /proc/self/cgroup 2>/dev/null | head -1)"

say ""
say "=== prerequisites a systemd --user scope needs ==="
say "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-unset}"
say "DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS:-unset}"
ls -ld "/run/user/$(id -u)" 2>&1 | sed 's/^/run_user_dir: /'
loginctl show-user "$(id -un)" -p Linger 2>&1 | head -1 | sed 's/^/linger: /'
command -v systemd-run >/dev/null 2>&1 && say "systemd_run=present" || say "systemd_run=ABSENT"

say ""
say "=== FACT 1: can a delegated user scope be created? ==="
scope_out=$(systemd-run --user --scope --quiet --collect --unit="boxprobe-$$" \
    -p Delegate=yes bash -c '
        c=$(awk -F: "{print \$3}" /proc/self/cgroup | head -1)
        printf "scope_cgroup=%s\n" "$c"
        printf "controllers=%s\n" "$(cat "/sys/fs/cgroup$c/cgroup.controllers" 2>/dev/null)"
    ' 2>&1)
scope_rc=$?
printf '%s\n' "$scope_out"
if [ "$scope_rc" -eq 0 ]; then
    say "FACT1_SCOPE_CREATED=YES rc=0"
else
    say "FACT1_SCOPE_CREATED=NO rc=$scope_rc"
    say "FACT2_ESCAPEE_KILLED=SKIPPED (no scope to test in)"
    say "VERDICT=UNBOXED_IS_A_REAL_CAPABILITY_LIMIT_ON_THIS_RUNNER"
    exit 0
fi

say ""
say "=== FACT 2: does cgroup.kill reach a setsid child? ==="
# The inner body lives in its own file so there is no nested quoting: an earlier version mangled
# `$$` through three levels of bash -c and silently measured nothing.
ESC_PIDFILE=$(mktemp); export ESC_PIDFILE
INNER=$(mktemp); chmod +x "$INNER"
cat > "$INNER" <<'INNER_EOF'
#!/usr/bin/env bash
set +e
b="/sys/fs/cgroup$(awk -F: '{print $3}' /proc/self/cgroup | head -1)"
echo "scope_cgroup=$b"
mkdir -p "$b/victim" 2>/dev/null || { echo "FACT2_ESCAPEE_KILLED=NO reason=cannot-create-child-cgroup"; exit 0; }
# Start the escapee, THEN move it in: a process may move a child into a sub-cgroup but not itself
# out of a cgroup that is delegating controllers.
setsid bash -c 'echo $$ > "$ESC_PIDFILE"; exec sleep 120' &
sleep 0.5
pid=$(cat "$ESC_PIDFILE" 2>/dev/null)
[ -n "$pid" ] || { echo "FACT2_ESCAPEE_KILLED=INCONCLUSIVE reason=escapee-pid-not-recorded"; exit 0; }
echo "escapee_pid=$pid"
echo "escapee_sid=$(ps -o sid= -p "$pid" 2>/dev/null | tr -d ' ') probe_sid=$(ps -o sid= -p $$ 2>/dev/null | tr -d ' ')  # differing sid == it escaped the process group"
if echo "$pid" > "$b/victim/cgroup.procs" 2>/dev/null; then echo "moved_into_victim=yes"; else echo "moved_into_victim=NO"; fi
echo "escapee_cgroup=$(awk -F: '{print $3}' "/proc/$pid/cgroup" 2>/dev/null | head -1)"
kill -0 "$pid" 2>/dev/null && echo "alive_before_kill=yes" || echo "alive_before_kill=no"
if echo 1 > "$b/victim/cgroup.kill" 2>/dev/null; then echo "cgroup_kill_write=ok"; else echo "cgroup_kill_write=FAILED"; fi
sleep 1
if kill -0 "$pid" 2>/dev/null; then
    echo "FACT2_ESCAPEE_KILLED=NO (survived cgroup.kill)"
    kill -9 "$pid" 2>/dev/null
else
    echo "FACT2_ESCAPEE_KILLED=YES"
fi
INNER_EOF
systemd-run --user --scope --quiet --collect --unit="killprobe-$$" -p Delegate=yes \
    --setenv=ESC_PIDFILE="$ESC_PIDFILE" bash "$INNER" 2>&1
rm -f "$INNER" "$ESC_PIDFILE"

say ""
say "VERDICT=SCOPE_AVAILABLE — boxing is skipped by the \$CI/\$GITHUB_ACTIONS check, not by a missing capability"
exit 0
