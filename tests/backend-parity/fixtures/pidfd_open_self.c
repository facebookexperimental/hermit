// Cross-backend parity contract: pidfd_open(2) on the calling process.
//
// pidfd_open has no glibc wrapper, so it is issued raw. It returns a new file
// descriptor that refers to a process; here the process is the caller's own,
// named through getpid(). Hermit virtualizes PIDs, so the interesting property
// is that pidfd_open accepts the *virtualized* self PID and returns a usable,
// deterministic descriptor consistently across every backend -- historically an
// open question because a backend that failed to translate the virtual PID
// would diverge here.
//
// The contract is purely relational: it never bakes an absolute PID or fd
// number into the golden output. It checks that two self pidfds open, that they
// are distinct open descriptions, that an invalid flags argument is rejected
// with EINVAL, and that both descriptors close. There is no file, no memory
// mapping, no data transfer, and no blocking wait, so the fixture is idempotent
// under --verify and safe for the DBI cooperative scheduler.
#include <errno.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    int ok = 0;

    // (1) open a pidfd referring to this process via its (virtual) PID.
    long fd1 = syscall(SYS_pidfd_open, getpid(), 0);
    if (fd1 >= 0) ok++;

    // (2) a second self pidfd also opens.
    long fd2 = syscall(SYS_pidfd_open, getpid(), 0);
    if (fd2 >= 0) ok++;

    // (3) the two are distinct open file descriptions (relational, not absolute).
    if (fd1 >= 0 && fd2 >= 0 && fd2 != fd1) ok++;

    // (4) an invalid flags argument is rejected with EINVAL (faithful error path).
    long bad = syscall(SYS_pidfd_open, getpid(), 0xFFFFFFFF);
    if (bad == -1 && errno == EINVAL) ok++;

    // (5),(6) both descriptors close cleanly.
    if (fd1 >= 0 && close((int)fd1) == 0) ok++;
    if (fd2 >= 0 && close((int)fd2) == 0) ok++;

    printf("pidfd ok=%d\n", ok);
    return 0;
}
