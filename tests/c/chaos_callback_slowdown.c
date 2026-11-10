#define _GNU_SOURCE
#include <stdio.h>
#include <time.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    volatile unsigned long sink = 7;
    for (unsigned round = 0; round < 24; ++round) {
        (void)syscall(SYS_close, -1);
        for (unsigned i = 0; i < 64; ++i) {
            sink = sink * 6364136223846793005UL + 1442695040888963407UL;
            if ((sink >> 63) != 0) sink ^= (unsigned long)i;
        }
        struct timespec now;
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return 2;
        printf("%u %lld.%09ld\n", round, (long long)now.tv_sec, now.tv_nsec);
    }
    (void)sink;
    return 0;
}
