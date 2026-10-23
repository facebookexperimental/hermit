// Issues argv[1] raw getppid system calls and exits 0. The KVM host-syscall
// budget test runs it twice, with N and with 0, and divides the difference
// in Hermit's host system calls by N.
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(int argc, char **argv) {
    long count = argc > 1 ? strtol(argv[1], NULL, 10) : 0;
    for (long i = 0; i < count; i++) {
        syscall(SYS_getppid);
    }
    return 0;
}
