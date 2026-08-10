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
    SCOPE_OK=yes
else
    say "FACT1_SCOPE_CREATED=NO rc=$scope_rc"
    say "FACT2_ESCAPEE_KILLED=SKIPPED (no scope to test in)"
    # DELIBERATELY NOT EXITING. A failed systemd route does not mean containment is impossible --
    # treating it as though it did is the exact conflation this probe exists to break. FACT 3
    # needs neither a scope nor systemd, so it must still run.
    SCOPE_OK=no
fi

if [ "$SCOPE_OK" = yes ]; then
say ""
say "=== FACT 2: does cgroup.kill reach a setsid child? (inside the systemd scope) ==="
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
fi

FACT3_INNER=$(mktemp); chmod +x "$FACT3_INNER"
F3_PIDFILE=$(mktemp); export F3_PIDFILE
cat > "$FACT3_INNER" <<'F3_EOF'
#!/usr/bin/env bash
# Containment WITHOUT systemd, in whatever cgroup we already occupy.
set +e
b="/sys/fs/cgroup$(awk -F: '{print $3}' /proc/self/cgroup | head -1)"
echo "own_cgroup=$b"
echo "own_cgroup_type=$(cat "$b/cgroup.type" 2>/dev/null)"
echo "controllers_available_here=$(cat "$b/cgroup.controllers" 2>/dev/null)"
echo "subtree_control_now=$(cat "$b/cgroup.subtree_control" 2>/dev/null)"
echo "cgroupfs_writable=$([ -w "$b" ] && echo yes || echo no)"

# TWO-LEVEL, exactly like the runner: an outer cgroup we own, with per-step children under it.
# Everything happens under probe-root so the CALLER'S cgroup is never mutated. An earlier revision
# wrote subtree_control on the caller's own cgroup; that enabled a controller there, and because
# cgroup-v2 forbids internal processes in a cgroup that delegates, the NEXT run could no longer move
# a process into its child and reported a confident wrong FACT3B=NO. A probe must not poison the
# environment it measures.
root="$b/probe-root"
if mkdir -p "$root" 2>/dev/null; then
    echo "FACT3A_CHILD_CGROUP_CREATED=YES"
else
    echo "FACT3A_CHILD_CGROUP_CREATED=NO"
    echo "FACT3B_ESCAPEE_KILLED_NO_SYSTEMD=SKIPPED (no child cgroup)"
    echo "FACT3C_CONTROLLERS_DELEGABLE=SKIPPED (no child cgroup)"
    exit 0
fi

# ORDER IS LOAD-BEARING: 3B RUNS FIRST, BEFORE ANY subtree_control WRITE.
# Enabling a controller on the parent makes it a cgroup that delegates while still holding this
# job's own processes, and cgroup-v2 then refuses to move a process into any of its children --
# so 3B's escapee could no longer enter the victim cgroup and would report a false NO. Measured
# exactly that when 3C ran first: escapee_moved_into_victim=NO and FACT3B=NO, on a machine where
# both had just been YES. Measure the kill path on an untouched hierarchy, then perturb it.

# ---- 3B: does cgroup.kill reach a setsid escapee in a per-step child? cgroup.kill is a CORE v2
#          file needing no controller delegation, so this can pass even when 3C does not.
mkdir -p "$root/victim" 2>/dev/null
echo "victim_has_cgroup_kill=$([ -e "$root/victim/cgroup.kill" ] && echo yes || echo NO)"
setsid bash -c 'echo $$ > "$F3_PIDFILE"; exec sleep 120' &
sleep 0.5
pid=$(cat "$F3_PIDFILE" 2>/dev/null)
if [ -z "$pid" ]; then
    echo "FACT3B_ESCAPEE_KILLED_NO_SYSTEMD=INCONCLUSIVE reason=escapee-pid-not-recorded"
else
    esid=$(ps -o sid= -p "$pid" 2>/dev/null | tr -d ' '); psid=$(ps -o sid= -p $$ 2>/dev/null | tr -d ' ')
    echo "escapee_pid=$pid escapee_sid=$esid probe_sid=$psid"
    if [ -n "$esid" ] && [ "$esid" != "$psid" ]; then
        echo "ESCAPEE_LEFT_PROCESS_GROUP=YES (differing session id — a killpg cannot reach it)"
    else
        echo "ESCAPEE_LEFT_PROCESS_GROUP=UNPROVEN (sid did not differ; this sub-fact is not established)"
    fi
    if echo "$pid" > "$root/victim/cgroup.procs" 2>/dev/null; then echo "escapee_moved_into_victim=YES"; else echo "escapee_moved_into_victim=NO"; fi
    echo "escapee_cgroup_now=$(awk -F: '{print $3}' "/proc/$pid/cgroup" 2>/dev/null | head -1)"
    echo "victim_members=[$(tr '\n' ' ' < "$root/victim/cgroup.procs" 2>/dev/null)]"
    kill -0 "$pid" 2>/dev/null && echo "alive_before_kill=yes" || echo "alive_before_kill=no"
    if echo 1 > "$root/victim/cgroup.kill" 2>/dev/null; then echo "cgroup_kill_write=ok"; else echo "cgroup_kill_write=FAILED"; fi
    sleep 1
    if kill -0 "$pid" 2>/dev/null; then
        echo "FACT3B_ESCAPEE_KILLED_NO_SYSTEMD=NO (survived cgroup.kill)"; kill -9 "$pid" 2>/dev/null
    else
        echo "FACT3B_ESCAPEE_KILLED_NO_SYSTEMD=YES"
    fi
fi

# ---- 3C: delegation is a TWO-LEVEL question and the level matters.
# A cgroup's children receive ONLY the controllers its PARENT lists in cgroup.subtree_control.
# The previous revision wrote probe-root/cgroup.subtree_control -- which governs probe-root's
# CHILDREN -- and never the parent, so probe-root itself had no controllers to hand down and the
# NO was guaranteed regardless of what this machine permits. Enable on the PARENT first, re-read
# what probe-root actually received, and only then delegate one level further.
#
# Each controller is written SEPARATELY: an atomic multi-controller write fails wholesale, so a
# single refused controller would otherwise hide the ones that would have been granted.
parent_before=$(cat "$b/cgroup.subtree_control" 2>/dev/null)
echo "parent_subtree_control_before=[$parent_before]"
parent_got=""
for c in memory cpu pids; do
    err=$(sh -c "printf '%s' '+$c' > '$b/cgroup.subtree_control'" 2>&1); rc=$?
    if [ $rc -eq 0 ]; then parent_got="$parent_got $c"; else echo "  parent +$c REFUSED: ${err##*: }"; fi
done
echo "parent_enabled_by_us=[${parent_got# }]"
echo "probe_root_controllers_after_parent_write=$(cat "$root/cgroup.controllers" 2>/dev/null)"

# Now the child level: what can probe-root hand to a per-step cgroup beneath it?
# Remove the leftover victim cgroup first: an existing child is one plausible reason a
# subtree_control write is refused, and leaving it in place would confound the answer.
rmdir "$root/victim" 2>/dev/null
echo "probe_root_children_before_delegate=[$(ls -d "$root"/*/ 2>/dev/null | wc -l)]"
echo "probe_root_procs=[$(tr '\n' ' ' < "$root/cgroup.procs" 2>/dev/null)]"
child_got=""
for c in memory cpu pids; do
    err=$(sh -c "printf '%s' '+$c' > '$root/cgroup.subtree_control'" 2>&1); rc=$?
    if [ $rc -eq 0 ]; then child_got="$child_got $c"; else echo "  probe-root +$c REFUSED: ${err##*: }"; fi
done
echo "probe_root_enabled_for_children=[${child_got# }]"

# Per-step CAPS need memory and cpu specifically; `pids` alone is not resource capping.
case " $child_got " in
    *" memory "*) case " $child_got " in
        *" cpu "*) echo "FACT3C_CONTROLLERS_DELEGABLE=YES (memory+cpu reach a per-step cgroup: full caps possible without systemd)" ;;
        *) echo "FACT3C_CONTROLLERS_DELEGABLE=PARTIAL (memory but no cpu)" ;;
    esac ;;
    *) if [ -n "$child_got" ]; then
           echo "FACT3C_CONTROLLERS_DELEGABLE=PARTIAL (only [${child_got# }]; no memory/cpu, so no per-step resource caps)"
       else
           echo "FACT3C_CONTROLLERS_DELEGABLE=NO (nothing reached a per-step cgroup even after writing the parent)"
       fi ;;
esac

rmdir "$root/victim" 2>/dev/null
# RESTORE the parent exactly as found, then prove it. Leaving a controller enabled is what made an
# earlier revision report a confident wrong FACT3B=NO on its second invocation.
for c in $parent_got; do
    case " $parent_before " in *" $c "*) : ;; *) echo "-$c" > "$b/cgroup.subtree_control" 2>/dev/null ;; esac
done
echo "parent_subtree_control_restored=[$(cat "$b/cgroup.subtree_control" 2>/dev/null)] (was [$parent_before])"
rmdir "$root" 2>/dev/null   # leave no trace in the caller's cgroup
F3_EOF

say ""
say "=== FACT 3: containment WITHOUT systemd — direct cgroupfs in our own cgroup ==="
bash "$FACT3_INNER" 2>&1
rm -f "$FACT3_INNER" "$F3_PIDFILE"

say ""
say "SCOPE_ROUTE_AVAILABLE=$SCOPE_OK"
say "READ THE VERDICT FROM FACT3B/FACT3C, NOT FROM FACT1: the systemd route failing says nothing"
say "about whether this machine can contain a step. 3B answers whether a setsid escapee can be"
say "killed; 3C answers whether per-step memory/CPU caps are possible. They can differ."
exit 0
