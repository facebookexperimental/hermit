// getrusage(RUSAGE_SELF) CPU-accounting determinization parity probe.
//
// A process's own resource-usage counters are host-derived state: outside
// Hermit, ru_utime/ru_stime grow with real CPU time and the fault and
// context-switch counters reflect the host kernel's scheduling of the run.
// Hermit must not let a guest observe wall-derived CPU consumption or host
// scheduling artifacts. Zero was reproducible but frozen: it made getrusage
// contradict times(2), which already derives CPU accounting from virtual time.
// The CPU fields now advance from that same virtual clock; fault and
// context-switch counters remain zero because no virtual model exists for them.
// This is the
// RUSAGE_SELF sibling of the process-wait "zeroed child CPU accounting" the
// wait contract already covers; getrusage itself is accepted (it must not
// spuriously fail), and its CPU accounting must advance deterministically.
//
// ru_maxrss is deliberately NOT asserted: peak resident set is a legitimate
// backend-local memory-footprint number (ptrace, DBT, and KVM each report a
// different value) and determinizing it is neither required nor claimed.
//
// The fixture first burns measurable CPU so native and Hermit have nonzero total
// CPU time. Under Hermit all six checks pass (ok=6). Native typically scores
// ok=5: it also advances CPU time, but exposes host page faults and context switches.

#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/time.h>
#include <sys/times.h>
#include <unistd.h>

static int timeval_to_ticks(const struct timeval *value, long ticks_per_second,
                            clock_t *ticks) {
    if (ticks_per_second <= 0 || value->tv_sec < 0 || value->tv_usec < 0 ||
        value->tv_usec >= 1000000) {
        return 0;
    }
    unsigned long long whole =
        (unsigned long long)value->tv_sec * (unsigned long long)ticks_per_second;
    unsigned long long partial =
        (unsigned long long)value->tv_usec * (unsigned long long)ticks_per_second /
        1000000ULL;
    *ticks = (clock_t)(whole + partial);
    return 1;
}

static int usage_matches_times(const struct rusage *usage,
                               const struct tms *before,
                               const struct tms *after,
                               long ticks_per_second) {
    clock_t user_ticks = 0;
    clock_t system_ticks = 0;
    return (before->tms_utime > 0 || before->tms_stime > 0) &&
           timeval_to_ticks(&usage->ru_utime, ticks_per_second, &user_ticks) &&
           timeval_to_ticks(&usage->ru_stime, ticks_per_second, &system_ticks) &&
           user_ticks >= before->tms_utime && user_ticks <= after->tms_utime &&
           system_ticks >= before->tms_stime && system_ticks <= after->tms_stime;
}

int main(void) {
    // Burn measurable CPU so native accounting is unambiguously nonzero.
    volatile unsigned long acc = 0;
    for (unsigned long i = 0; i < 50000000UL; i++) acc += i;

    struct tms before;
    struct tms after;
    memset(&before, 0, sizeof(before));
    memset(&after, 0, sizeof(after));
    clock_t before_rc = times(&before);

    struct rusage ru;
    memset(&ru, 0, sizeof(ru));
    int rc = getrusage(RUSAGE_SELF, &ru);
    clock_t after_rc = times(&after);
    long ticks_per_second = sysconf(_SC_CLK_TCK);

    // Negative bracket: the previously accepted frozen 1us value must not be
    // mistaken for accounting that tracks the already-positive times(2) clock.
    struct rusage frozen;
    memset(&frozen, 0, sizeof(frozen));
    frozen.ru_utime.tv_usec = 1;
    if (before_rc == (clock_t)-1 || after_rc == (clock_t)-1 ||
        usage_matches_times(&frozen, &before, &after, ticks_per_second)) {
        return 3;
    }

    int ok = 0;
    // (1) getrusage(RUSAGE_SELF) is accepted (native and all backends).
    if (rc == 0) ok++;
    // (2) getrusage and times report the same advancing logical CPU clock.
    if (rc == 0 && usage_matches_times(&ru, &before, &after, ticks_per_second)) ok++;
    // (3) A runaway value would indicate that host time leaked into the guest.
    // Exact determinism is checked by the backend-parity double execution.
    if (rc == 0 && ru.ru_utime.tv_sec < 60) ok++;
    // (4) Determinized: minor page-fault count is zeroed (native counts them).
    if (ru.ru_minflt == 0) ok++;
    // (5) Major page-fault count is zero (both native and Hermit for this run).
    if (ru.ru_majflt == 0) ok++;
    // (6) Determinized: voluntary and involuntary context switches are zeroed.
    if (ru.ru_nvcsw == 0 && ru.ru_nivcsw == 0) ok++;

    // Consume acc so the burn loop cannot be optimized away.
    if (acc == 0) return 2;
    printf("getrusage ok=%d\n", ok);
    return ok == 6 ? 0 : 1;
}
