#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>

enum { ITERATIONS = 1000000 };

int main(void) {
    volatile uint64_t state = UINT64_C(0x9e3779b97f4a7c15);

    for (uint64_t iteration = 0; iteration < ITERATIONS; ++iteration) {
        uint64_t value = state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        value += iteration * UINT64_C(0x2545f4914f6cdd1d);
        state = value;
    }

    printf("%" PRIu64 "\n", state);
    return 0;
}
