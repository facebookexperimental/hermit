#!/usr/bin/env bash
# Assert that the calling environment has NO network, or (with --expect-network)
# that it HAS one. Both directions matter: a probe that only ever runs where the
# answer is "no network" cannot distinguish a genuinely isolated environment
# from a broken probe that always says no. Run it on the host with
# --expect-network as the NEGATIVE CONTROL and the assertion becomes evidence
# instead of a claim.
#
# WHY THIS EXISTS. The test phase of the split validate must be UNABLE to reach
# the network, not merely uninterested in it. Network at test time is an
# uncontrolled input to the very thing being measured for determinism. Network
# at BUILD time is fine, because Cargo.lock pins versions and content checksums,
# so what the build fetches is content-determined. See README.md.
#
#   usage: assert-no-network.sh [--expect-network]
#     default          exit 0 if the network is unreachable, 1 if reachable
#     --expect-network exit 0 if the network IS reachable, 1 if not
#
# Three INDEPENDENT probes, so one mechanism silently failing cannot fake a
# pass: the kernel's own route table, DNS resolution, and a raw TCP connect to a
# literal address (which needs no DNS at all).

set -uo pipefail

expect_network=0
case "${1:-}" in
    --expect-network) expect_network=1 ;;
    "") ;;
    *) echo "assert-no-network: unexpected argument '$1'" >&2; exit 2 ;;
esac

reachable=0
report=()

# PROBE 1 -- the kernel route table. `--network=none` gives the container a
# network namespace with loopback only, so there is no route to anywhere and no
# non-loopback interface. Read /proc and /sys directly: `ip` is not in the
# pinned root, and depending on it would make the probe untestable there.
routes=$(awk 'NR>1 && $1!="lo" {n++} END{print n+0}' /proc/net/route 2>/dev/null || echo 0)
ifaces=$(ls /sys/class/net 2>/dev/null | grep -cv '^lo$' || true)
if [[ ${routes:-0} -gt 0 || ${ifaces:-0} -gt 0 ]]; then
    reachable=1
    report+=("route-table: REACHABLE (${routes} non-loopback route(s), ${ifaces} non-loopback interface(s))")
else
    report+=("route-table: no route (0 non-loopback routes, 0 non-loopback interfaces)")
fi

# PROBE 2 -- DNS. Named explicitly because these are the two hosts the BUILD
# phase legitimately needs; the test phase must not be able to see them.
for host in github.com crates.io static.crates.io; do
    if python3 -c "
import socket,sys
socket.setdefaulttimeout(4)
try:
    socket.getaddrinfo('$host', 443)
except Exception as e:
    print('dns $host: refused (%s)' % type(e).__name__); sys.exit(1)
print('dns $host: RESOLVED'); sys.exit(0)
" 2>/dev/null; then
        reachable=1
        report+=("dns $host: RESOLVED")
    else
        report+=("dns $host: refused")
    fi
done

# PROBE 3 -- raw TCP to a literal address. This is the probe that cannot be
# fooled by a broken resolver: no DNS is involved, so a refusal here means the
# packet genuinely has nowhere to go.
for addr in 1.1.1.1:443 8.8.8.8:53; do
    ip=${addr%:*}; port=${addr#*:}
    if python3 -c "
import socket,sys
s=socket.socket(); s.settimeout(4)
try:
    s.connect(('$ip', $port))
except Exception as e:
    sys.exit(1)
sys.exit(0)
" 2>/dev/null; then
        reachable=1
        report+=("tcp $addr: CONNECTED")
    else
        report+=("tcp $addr: refused")
    fi
done

printf 'assert-no-network: %s\n' "${report[@]}" >&2

if [[ $expect_network -eq 1 ]]; then
    if [[ $reachable -eq 1 ]]; then
        echo "assert-no-network: NEGATIVE CONTROL PASSED -- network is reachable here, so the probe can detect one." >&2
        exit 0
    fi
    echo "assert-no-network: NEGATIVE CONTROL FAILED -- expected a reachable network and found none." >&2
    echo "  The probe cannot be trusted to detect network, so a 'no network' result from it proves nothing." >&2
    exit 1
fi

if [[ $reachable -eq 1 ]]; then
    echo "assert-no-network: FAILED -- this environment can reach the network." >&2
    echo "  The test phase must be unable to reach the network: it is an uncontrolled" >&2
    echo "  input to the determinism being measured. Refusing to run." >&2
    exit 1
fi
echo "assert-no-network: OK -- no route, no DNS, no raw TCP. This environment is isolated." >&2
exit 0
