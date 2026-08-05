/*
 * Backend-parity contract: CPU virtualization.
 *
 * Detcore serializes every guest thread onto a single virtual CPU (CPU 0,
 * NUMA node 0) regardless of the physical topology of the host. This fixture
 * pins that guest-visible contract so it is byte-identical across the ptrace,
 * DBI, and KVM backends and across repeated --verify runs: a passthrough of
 * getcpu / sched_getaffinity would otherwise leak the host's CPU count and the
 * scheduler's placement decisions, both of which vary run to run and host to
 * host.
 *
 * Every query is issued through the raw syscall (never the glibc/vDSO fast
 * path) so it is guaranteed to cross the backend and reach Detcore's handlers
 * on all three backends.
 *
 * _GNU_SOURCE is supplied by the harness compile flags (-D_GNU_SOURCE); it is
 * deliberately not redefined here to avoid a -Werror redefinition collision.
 */
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    /* getcpu: Detcore always reports CPU 0 / NUMA node 0. */
    unsigned int cpu = 0xffffffffu;
    unsigned int node = 0xffffffffu;
    long rc = syscall(SYS_getcpu, &cpu, &node, (void *)0);
    if (rc != 0) {
        fprintf(stderr, "getcpu failed: rc=%ld\n", rc);
        return 1;
    }
    if (cpu != 0 || node != 0) {
        fprintf(stderr, "getcpu leaked topology: cpu=%u node=%u\n", cpu, node);
        return 2;
    }

    /* sched_getaffinity: Detcore reports a single-CPU affinity mask (only
     * CPU 0 set), independent of the host's real CPU count. */
    cpu_set_t mask;
    CPU_ZERO(&mask);
    rc = syscall(SYS_sched_getaffinity, (pid_t)0, sizeof(mask), &mask);
    if (rc <= 0) {
        fprintf(stderr, "sched_getaffinity failed: rc=%ld\n", rc);
        return 3;
    }
    if (!CPU_ISSET(0, &mask)) {
        fprintf(stderr, "sched_getaffinity: CPU 0 not set\n");
        return 4;
    }
    if (CPU_COUNT(&mask) != 1) {
        fprintf(stderr, "sched_getaffinity leaked topology: count=%d\n",
                CPU_COUNT(&mask));
        return 5;
    }

    printf("cpu-virtualization-ok\n");
    return 0;
}
