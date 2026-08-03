// getrusage(RUSAGE_SELF) CPU-accounting determinization parity probe.
//
// A process's own resource-usage counters are host-derived state: outside
// Hermit, ru_utime/ru_stime grow with real CPU time and the fault and
// context-switch counters reflect the host kernel's scheduling of the run.
// Hermit determinizes these accounting fields to zero so a guest cannot observe
// wall-derived CPU consumption or host scheduling artifacts. This is the
// RUSAGE_SELF sibling of the process-wait "zeroed child CPU accounting" the
// wait contract already covers; getrusage itself is accepted (it must not
// spuriously fail), but the accounting it reports is the determinized zero.
//
// ru_maxrss is deliberately NOT asserted: peak resident set is a legitimate
// backend-local memory-footprint number (ptrace, DBI, and KVM each report a
// different value) and determinizing it is neither required nor claimed.
//
// The fixture first burns measurable CPU so that a native run has clearly
// nonzero user time and minor faults. Under Hermit all six checks pass (ok=6);
// native passes only the acceptance and the always-zero major-fault check
// (ok=2) because it faithfully reports real CPU time, minor faults, and an
// involuntary context switch.

#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/time.h>

int main(void) {
    // Burn measurable CPU so native accounting is unambiguously nonzero.
    volatile unsigned long acc = 0;
    for (unsigned long i = 0; i < 50000000UL; i++) acc += i;

    struct rusage ru;
    memset(&ru, 0, sizeof(ru));
    int rc = getrusage(RUSAGE_SELF, &ru);

    int ok = 0;
    // (1) getrusage(RUSAGE_SELF) is accepted (native and all backends).
    if (rc == 0) ok++;
    // (2) Determinized: user CPU time is zeroed (native reflects real time).
    if (ru.ru_utime.tv_sec == 0 && ru.ru_utime.tv_usec == 0) ok++;
    // (3) Determinized: system CPU time is zeroed.
    if (ru.ru_stime.tv_sec == 0 && ru.ru_stime.tv_usec == 0) ok++;
    // (4) Determinized: minor page-fault count is zeroed (native counts them).
    if (ru.ru_minflt == 0) ok++;
    // (5) Major page-fault count is zero (both native and Hermit for this run).
    if (ru.ru_majflt == 0) ok++;
    // (6) Determinized: voluntary and involuntary context switches are zeroed.
    if (ru.ru_nvcsw == 0 && ru.ru_nivcsw == 0) ok++;

    // Consume acc so the burn loop cannot be optimized away.
    if (acc == 0) return 2;
    printf("getrusage ok=%d\n", ok);
    return 0;
}
