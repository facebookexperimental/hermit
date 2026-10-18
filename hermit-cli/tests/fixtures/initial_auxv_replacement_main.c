#include <stdint.h>
#include <unistd.h>

extern uintptr_t scratch_tail_address(void);

static long raw3(long number, long a, long b, long c) {
    long result;
    __asm__ volatile("syscall" : "=a"(result)
                     : "a"(number), "D"(a), "S"(b), "d"(c)
                     : "rcx", "r11", "cc", "memory");
    return result;
}

int main(void) {
    static const char good[] = "main read scratch-tail cookie\n";
    static const char bad[] = "main read wrong scratch-tail cookie\n";
    static const char unmapped[] = "main-unmapped: scratch-tail page is absent\n";
    static const char probe_error[] = "main mincore returned unexpected error\n";
    uintptr_t address = scratch_tail_address();
    unsigned char resident = 0;
    long present = raw3(27, address, 4096, (long)&resident);
    if (present == -12) { /* Linux mincore ENOMEM. This stays a failing exit. */
        (void)write(1, unmapped, sizeof(unmapped)-1);
        return 42;
    }
    if (present != 0) {
        (void)write(2, probe_error, sizeof(probe_error)-1);
        return 43;
    }
    uint64_t value = *(volatile const uint64_t *)address;
    if (address != UINT64_C(0x52000007f000)
            || value != UINT64_C(0x3141592653589793)) {
        (void)write(2, bad, sizeof(bad)-1);
        return 71;
    }
    return write(1, good, sizeof(good)-1) == sizeof(good)-1 ? 0 : 72;
}
