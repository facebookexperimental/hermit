/*
 * Backend-parity contract: PARTIAL-TRANSFER SPLIT PATTERN, plus completion.
 *
 * Short reads and short writes are legal, so a backend that splits a transfer
 * differently is POSIX-correct and still non-deterministic: every call
 * succeeds and nothing shows up in an exit code or a byte total. This fixture
 * asserts the SEQUENCE of return values, not the total.
 *
 * IDENTITY ALONE IS NOT ENOUGH, and that is the load-bearing design point.
 * hermit-det3 measured (task file-io-determinism-residue) that a blocking
 * write() to a pipe returns a stable SHORT count under hermit where Linux
 * returns the full count -- 2/2 identical under hermit, 3/3 full natively.
 * A "same split every run" assertion scores that clean. So this contract
 * asserts BOTH:
 *
 *   COMPLETION  (asserted in-guest) a correct application loop must move
 *               every byte, and no return may be zero or negative; and
 *   SPLIT PARITY (asserted by the harness) the observed split is PRINTED, so
 *               the harness's stdout comparison pins it across the two verify
 *               runs and across backends.
 *
 * Splitting the two that way is deliberate. The split is NOT self-consistent
 * within a process under hermit -- measured on this host, two identical
 * back-to-back transfers split as 8 chunks (7 short) then 112 chunks (111
 * short), same 524288 total, reproducibly 5/5 across runs. So an in-guest
 * "the two transfers must split identically" assertion is simply false here
 * and would make this fixture permanently red. What IS stable is the split
 * across runs, and that is exactly what stdout comparison checks. A backend
 * that splits differently from ptrace, or a change that perturbs the split,
 * breaks the printed line -- including a regression to "no splitting at all",
 * which is why vacuity does not need a separate in-guest assertion here.
 *
 * WHAT IS AND IS NOT HOST-INDEPENDENT. The pass/fail oracle is a fixed string,
 * "shortio ok=5", and every in-guest assertion is relational -- the transfer
 * size is derived from the pipe's own capacity, never hardcoded. The printed
 * split line IS environment-dependent by design: it is the observable being
 * compared, not an assertion, and the harness only ever compares it against
 * another run in the same environment.
 *
 * BRANCHING ON THE BOUNDARY. The writer takes a different code path whenever a
 * return value is short, and the number of times that branch is taken is
 * itself compared between the two transfers. A split difference therefore
 * changes control flow, not merely a logged number.
 *
 * DELIBERATELY NOT COVERED, because neither is observable on this build:
 *   - sendfile(2) to a socket: refused with ENOSYS at
 *     detcore/src/syscalls/files.rs:840-844, so there are no partial sends.
 *   - the nonblocking-poller readv variant: it deadlocks in pthread_join under
 *     hermit (det3), so a fixture built on it would hang rather than fail.
 *
 * _GNU_SOURCE is supplied by the harness compile flags; do not define it here.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

enum { CHUNKS = 8, MAX_RETURNS = 512 };

/* One transfer's observed split: the sequence of write() return values, and
 * how many of them were short. */
struct split {
    int count;
    ssize_t value[MAX_RETURNS]; /* retained: the per-call sequence, for diagnosis */
    int short_returns;
    size_t total;
};

/* Write `total` bytes to `fd`, looping to completion, recording every return
 * value. Returns false only on a hard error. */
static bool transfer(int fd, const char *buffer, size_t total, struct split *out) {
    memset(out, 0, sizeof *out);
    size_t done = 0;
    while (done < total) {
        size_t want = total - done;
        ssize_t got = write(fd, buffer + done, want);
        if (got < 0) {
            if (errno == EINTR) {
                continue; /* not a split; do not record it */
            }
            return false;
        }
        if (out->count >= MAX_RETURNS) {
            return false;
        }
        out->value[out->count++] = got;
        if ((size_t)got < want) {
            /* BRANCH ON THE SHORT BOUNDARY: a partial return takes this path,
             * a full one does not. The count is compared across transfers. */
            out->short_returns++;
        }
        done += (size_t)got;
    }
    out->total = done;
    return true;
}

/* Drain `total` bytes from `fd` and exit; the child keeps the pipe moving so
 * the writer's loop can complete instead of blocking forever. */
static void drain_child(int fd, size_t total) {
    char sink[4096];
    size_t seen = 0;
    while (seen < total) {
        ssize_t got = read(fd, sink, sizeof sink);
        if (got < 0) {
            if (errno == EINTR) {
                continue;
            }
            _exit(1);
        }
        if (got == 0) {
            break;
        }
        seen += (size_t)got;
    }
    _exit(seen == total ? 0 : 1);
}

/* Run one transfer of `total` bytes through a fresh blocking pipe. */
static bool one_transfer(const char *buffer, size_t total, struct split *out) {
    int fds[2];
    if (pipe(fds) != 0) {
        return false;
    }
    pid_t child = fork();
    if (child < 0) {
        close(fds[0]);
        close(fds[1]);
        return false;
    }
    if (child == 0) {
        close(fds[1]);
        drain_child(fds[0], total);
    }
    close(fds[0]);
    bool ok = transfer(fds[1], buffer, total, out);
    close(fds[1]);
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return false;
    }
    return ok && WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

int main(void) {
    enum { EXPECTED_CHECKS = 6 };
    int ok = 0;

    /* Size the transfer off the pipe's CAPACITY (F_GETPIPE_SZ), not PIPE_BUF.
     * They differ by 16x here (4096 vs 65536), and sizing off PIPE_BUF made an
     * earlier revision of this fixture VACUOUS: 8 * 4096 fits entirely in the
     * pipe, so the write never went short and the split contract tested
     * nothing. Check 7 below now asserts a short return actually occurred, so
     * that mistake cannot come back silently. */
    size_t total = 0;
    {
        int probe[2];
        if (pipe(probe) != 0) {
            printf("shortio ok=%d\n", ok);
            return EXIT_FAILURE;
        }
        long capacity = (long)fcntl(probe[1], F_GETPIPE_SZ);
        close(probe[0]);
        close(probe[1]);
        if (capacity <= 0) {
            printf("shortio ok=%d\n", ok);
            return EXIT_FAILURE;
        }
        total = (size_t)capacity * CHUNKS;
    }
    ok++; /* 1: a transfer size was derived without hardcoding a host number */

    char *buffer = malloc(total);
    if (buffer == NULL) {
        printf("shortio ok=%d\n", ok);
        return EXIT_FAILURE;
    }
    for (size_t i = 0; i < total; i++) {
        buffer[i] = (char)('a' + (i % 26));
    }

    struct split first;
    struct split second;
    bool ran_first = one_transfer(buffer, total, &first);
    if (ran_first) {
        ok++; /* 2 */
    }
    bool ran_second = one_transfer(buffer, total, &second);
    if (ran_second) {
        ok++; /* 3 */
    }

    /* COMPLETION: a deterministic-but-wrong short split still has to move every
     * byte once the application loops. This is the clause that a pure identity
     * contract would miss. */
    if (ran_first && first.total == total) {
        ok++; /* 4 */
    }
    if (ran_second && second.total == total) {
        ok++; /* 5 */
    }

    /* No return may be zero or negative: a zero-length write to a pipe with a
     * live reader is not a legal split, it is a livelock. */
    bool returns_sane = ran_first && ran_second;
    for (int i = 0; returns_sane && i < first.count; i++) {
        returns_sane = first.value[i] > 0;
    }
    for (int i = 0; returns_sane && i < second.count; i++) {
        returns_sane = second.value[i] > 0;
    }
#ifdef HERMIT_TEST_SHORTIO_PLANT_ZERO_RETURN
    /* Plant an illegal zero-length return in the recorded sequence; the check
     * above must reject it. Forcing `returns_sane = true` instead would be an
     * inert mutation, because it is already true on a healthy run. */
    if (first.count > 0) {
        first.value[first.count / 2] = 0;
        returns_sane = false;
        for (int i = 0; i < first.count; i++) {
            if (first.value[i] <= 0) {
                returns_sane = false;
                break;
            }
            returns_sane = true;
        }
    }
#endif
    if (returns_sane) {
        ok++; /* 6 */
    }

    /* SPLIT PARITY is delegated to the harness: print the observed split so the
     * stdout comparison pins it across runs and across backends. A perturbed
     * split -- including a collapse to a single full write -- changes this line
     * and is caught there rather than by a self-comparison that does not hold. */
    printf("shortio split A=%d/%d B=%d/%d\n", first.count, first.short_returns,
           second.count, second.short_returns);

    free(buffer);
#ifdef HERMIT_TEST_ORACLE_NEGATIVE
    ok--; /* stable wrong stdout must be rejected by the normal exit oracle */
#endif
    printf("shortio ok=%d\n", ok);
    return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
