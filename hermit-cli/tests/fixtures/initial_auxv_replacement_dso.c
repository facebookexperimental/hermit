/* One admitted DT_NEEDED initializer. No executable preinit array. */
#include <stdint.h>

#define TAIL_ADDRESS UINT64_C(0x52000007f000)
#define COOKIE UINT64_C(0x3141592653589793)

static long raw6(long number, long a, long b, long c, long d, long e, long f) {
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    register long r9 __asm__("r9") = f;
    long result;
    __asm__ volatile("syscall" : "=a"(result)
                     : "a"(number), "D"(a), "S"(b), "d"(c),
                       "r"(r10), "r"(r8), "r"(r9)
                     : "rcx", "r11", "cc", "memory");
    return result;
}

__attribute__((constructor))
static void replace_scratch_tail(void) {
    /* mmap: RW, PRIVATE|ANONYMOUS|FIXED, fd=-1, offset=0. */
    long result = raw6(9, TAIL_ADDRESS, 4096, 3, 0x32, -1, 0);
    if ((uint64_t)result != TAIL_ADDRESS) {
        static const char failure[] = "DSO constructor mmap failed\n";
        (void)raw6(1, 2, (long)failure, sizeof(failure)-1, 0, 0, 0);
        (void)raw6(231, 70, 0, 0, 0, 0, 0);
        __builtin_unreachable();
    }
    *(volatile uint64_t *)(uintptr_t)TAIL_ADDRESS = COOKIE;
    static const char marker[] = "DSO constructor stored scratch-tail cookie\n";
    (void)raw6(1, 1, (long)marker, sizeof(marker)-1, 0, 0, 0);
}

uintptr_t scratch_tail_address(void) {
    return TAIL_ADDRESS;
}
