#define _GNU_SOURCE

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

enum { ITERATIONS = 10000, COMPUTE_STEPS = 128 };

int main(void) {
    volatile uint64_t state = UINT64_C(0xd1b54a32d192ed03);

    for (uint64_t iteration = 0; iteration < ITERATIONS; ++iteration) {
        uint64_t value = state;
        for (uint64_t step = 0; step < COMPUTE_STEPS; ++step) {
            value ^= value >> 12;
            value ^= value << 25;
            value ^= value >> 27;
            value *= UINT64_C(0x2545f4914f6cdd1d);
            value += iteration + step;
        }
        const long pid = syscall(SYS_getpid);
        if (pid < 0) {
            perror("getpid");
            return 1;
        }
        state = value;
    }

    printf("%" PRIu64 "\n", state);
    return 0;
}
