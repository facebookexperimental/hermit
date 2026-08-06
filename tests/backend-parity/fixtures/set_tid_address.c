// Cross-backend parity contract: set_tid_address(2) caller-TID identity.
//
// set_tid_address has no glibc wrapper, so it is issued raw. The kernel stores
// the pointer as the thread's clear_child_tid and always succeeds, returning
// the caller's own thread ID. This contract exercises that identity without
// baking any absolute TID into the golden output: it only compares the return
// value to itself across repeated calls and to gettid(), and prints just the
// pass count. Hermit virtualizes thread IDs, so the interesting property is
// that set_tid_address and gettid report the *same* virtualized caller TID and
// that the value is stable across calls.
//
// Everything is a property of the calling thread. There is no file, no memory
// mapping, no data transfer, and no blocking wait, so the fixture is idempotent
// under --verify and safe for the DBT cooperative scheduler.
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    int ok = 0;
    int slot_a = 0;
    int slot_b = 0;

    // (1) set_tid_address returns the caller's TID (> 0) and always succeeds.
    long r1 = syscall(SYS_set_tid_address, &slot_a);
    if (r1 > 0) ok++;

    // (2) calling again returns the same caller TID regardless of the pointer.
    long r2 = syscall(SYS_set_tid_address, &slot_b);
    if (r2 == r1) ok++;

    // (3) a NULL pointer clears clear_child_tid but still returns the same TID.
    long r3 = syscall(SYS_set_tid_address, (void *)0);
    if (r3 == r1) ok++;

    // (4) the returned TID matches gettid(): both name the calling thread, and
    //     Hermit virtualizes them consistently.
    long tid = syscall(SYS_gettid);
    if (tid == r1) ok++;

    printf("set_tid_address ok=%d\n", ok);
    return 0;
}
