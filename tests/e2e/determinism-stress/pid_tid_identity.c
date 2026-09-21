/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Contract fixture: pid/tid virtualization identity.
 *
 * Pins the *runtime* half of the deterministic-identity contract. Every
 * process/thread id a guest can observe must be a virtualized value that is
 * identical across two runs and across backends -- never a host id.
 *
 * This is the runtime counterpart to making `DetPid`/`DetTid` newtypes: the
 * type work makes host-id confusion a COMPILE error, this makes a host id that
 * reaches the guest a TEST failure. Neither subsumes the other. A backend can
 * type-check perfectly and still emit a raw host tid, which is exactly what was
 * observed from DBT in DETLOG records.
 *
 * Every value below is printed, so the harness's stdout comparison is the
 * assertion; a host pid leaking in on either side of a double run diverges.
 *
 * COMPLEMENTARY TO `determinism-stress-c/pid-tid`, NOT A REPLACEMENT. That
 * fixture already pins getpid/getppid/gettid across four threads. Everything
 * below is a path it does NOT cover: procfs text, wait-status, execve, pgid,
 * and a control-flow branch.
 *
 * UNVALIDATED OBSERVATION, pinned deliberately: under hermit `getpgid(0)`
 * returns 0, where the host returns a real pgid. 0 is not a valid process
 * group, so this looks like a virtualization gap rather than a virtualized
 * value. It is printed here so the fixture CATCHES A CHANGE to it -- that is
 * not an assertion that 0 is correct. See the task note; it needs its own fix.
 *
 * Deliberately covered here, because each is a distinct leak path:
 *   - getpid / gettid / getppid / getpgid on the main thread
 *   - gettid from a NON-MAIN thread (a separate task in the scheduler)
 *   - the pid field of /proc/self/stat (procfs text, a different code path
 *     from the syscall return)
 *   - a child's pid as seen by the child AND as returned by wait() to the
 *     parent (wait-status carries a pid; that is a third path again)
 *   - a fork/exec tree, so the ids survive both fork and execve
 *   - a CONTROL-FLOW BRANCH on a pid comparison, so a divergence changes the
 *     program's output shape and not merely a printed number
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <pthread.h>
#include <unistd.h>

static pid_t sys_gettid(void) {
    return (pid_t)syscall(SYS_gettid);
}

/* First field of /proc/self/stat is the pid, as TEXT -- a different path from
   the getpid() syscall return, and historically a separate leak. */
static pid_t procfs_self_pid(void) {
    FILE *f = fopen("/proc/self/stat", "r");
    if (!f) {
        return -1;
    }
    long value = -1;
    if (fscanf(f, "%ld", &value) != 1) {
        value = -1;
    }
    fclose(f);
    return (pid_t)value;
}

static void *thread_body(void *arg) {
    (void)arg;
    /* A non-main thread is a distinct scheduler task; its tid must be
       virtualized too, and must differ from the main thread's. */
    printf("thread.gettid=%d\n", (int)sys_gettid());
    fflush(stdout);
    return NULL;
}

int main(void) {
    pid_t pid = getpid();
    pid_t tid = sys_gettid();
    pid_t ppid = getppid();
    pid_t pgid = getpgid(0);
    pid_t stat_pid = procfs_self_pid();

    printf("main.getpid=%d\n", (int)pid);
    printf("main.gettid=%d\n", (int)tid);
    printf("main.getppid=%d\n", (int)ppid);
    printf("main.getpgid=%d\n", (int)pgid);
    printf("procfs.stat_pid=%d\n", (int)stat_pid);

    /* CONTRACT: the main thread's tid equals its pid, and procfs agrees with
       the syscall. Branch on the comparisons so a mismatch changes control
       flow, not just a digit -- a fixture that only prints numbers can be
       "passed" by a harness that compares loosely. */
    if (tid == pid) {
        printf("branch.tid_eq_pid=yes\n");
    } else {
        printf("branch.tid_eq_pid=no\n");
    }
    if (stat_pid == pid) {
        printf("branch.procfs_agrees=yes\n");
    } else {
        printf("branch.procfs_agrees=no\n");
    }
    /* A virtualized pid space is small and dense; a raw host pid on a busy
       box is typically large. This is a shape check, not a value check. */
    printf("branch.pid_is_small=%s\n", pid < 100000 ? "yes" : "no");
    fflush(stdout);

    pthread_t thread;
    if (pthread_create(&thread, NULL, thread_body, NULL) != 0) {
        fprintf(stderr, "pthread_create failed\n");
        return 1;
    }
    if (pthread_join(thread, NULL) != 0) {
        fprintf(stderr, "pthread_join failed\n");
        return 1;
    }

    /* Fork/exec tree: ids must survive both fork and execve. The child reports
       its own pid and its ppid (which must be the parent's virtualized pid),
       then execs so the tree spans an image change. */
    fflush(stdout);
    pid_t child = fork();
    if (child < 0) {
        fprintf(stderr, "fork failed\n");
        return 1;
    }
    if (child == 0) {
        printf("child.getpid=%d\n", (int)getpid());
        printf("child.getppid=%d\n", (int)getppid());
        printf("child.branch.ppid_eq_parent=%s\n",
               getppid() == pid ? "yes" : "no");
        fflush(stdout);

        pid_t grandchild = fork();
        if (grandchild < 0) {
            _exit(3);
        }
        if (grandchild == 0) {
            printf("grandchild.getpid=%d\n", (int)getpid());
            fflush(stdout);
            /* execve: the pid must be preserved across the image change. */
            execl("/bin/true", "true", (char *)NULL);
            _exit(4);
        }
        int gstatus = 0;
        pid_t greaped = waitpid(grandchild, &gstatus, 0);
        printf("child.reaped_grandchild=%s\n",
               greaped == grandchild ? "yes" : "no");
        printf("child.grandchild_exit=%d\n", WEXITSTATUS(gstatus));
        fflush(stdout);
        _exit(7);
    }

    int status = 0;
    pid_t reaped = waitpid(child, &status, 0);
    /* wait() hands the parent a pid; that is a third path by which a host id
       could reach the guest, distinct from getpid() and from procfs. */
    printf("parent.child_pid=%d\n", (int)child);
    printf("parent.reaped_pid=%d\n", (int)reaped);
    printf("parent.branch.reaped_matches=%s\n",
           reaped == child ? "yes" : "no");
    printf("parent.child_exit=%d\n", WEXITSTATUS(status));
    fflush(stdout);

    return 0;
}
