/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A process that holds MAP_SHARED views of an ordinary file creates children
// that share its address space until they exec or exit: vfork(2), and
// posix_spawn(3), which glibc implements with clone or clone3 and
// CLONE_VM|CLONE_VFORK. This is what a C compiler driver does: glibc maps its
// iconv module cache (gconv-modules.cache) MAP_SHARED during locale setup,
// then the gcc driver vforks cc1, and collect2 and make posix_spawn their
// children. KVM refused every such child until the shared-file vfork slice
// (https://github.com/rrnewton/reverie/issues/1108).
//
// Every check settles when the creating call returns or through waitpid, so
// it holds whatever order the processes run in:
// - vfork, then execve of this program, whose new image maps the file itself
//   and bumps a counter in it;
// - vfork, then a store through the writable view, then _exit;
// - vfork whose execve fails: the child records errno in the view and exits,
//   as execvp's callers do;
// - a signal the vfork child sends itself, whose handler stores into the
//   view, and, with the argument "parent-signal", one it sends its parent,
//   handled once the parent resumes (KVM refuses a signal to another process
//   by name, with or without shared views, so the cells leave it out);
// - a vfork child killed by a signal;
// - a nested vfork: a vfork child vforks a grandchild that execs;
// - posix_spawn of this program, as in the first case.
// Afterwards the parent's two views still show exactly the file's bytes.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

enum { PAGE = 4096, PAGES = 2 };

static const char path[] = "shared-file-vfork.data";

// The first page of the file, as the writable view shows it.
struct record {
    uint32_t exec_count;    // bumped once by each new image of this program
    uint32_t exit_store;    // stored by a vfork child before _exit
    int32_t failed_errno;   // errno of a vfork child's failed execve
    uint32_t self_signal;   // stored by a handler in a vfork child
    uint32_t parent_signal; // stored by the parent's handler
    uint32_t nested;        // stored by a nested vfork child and its grandchild
};

static volatile struct record *record;

#define REQUIRE(condition, step)                                       \
    do {                                                               \
        if (!(condition)) {                                            \
            fprintf(stderr, "%s failed errno=%d\n", step, errno);      \
            _exit(1);                                                  \
        }                                                              \
    } while (0)

static unsigned char pattern(size_t index) {
    return (unsigned char)(index * 7 + 3);
}

static int wait_status(pid_t child) {
    int status = 0x5a5a5a5a;
    pid_t waited;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    REQUIRE(waited == child, "waitpid");
    return status;
}

static void wait_ok(pid_t child, const char *step) {
    int status = wait_status(child);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "%s status=%#x\n", step, status);
        _exit(1);
    }
}

// The new image: map the file afresh and bump the counter.
static int helper(void) {
    int fd = open(path, O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        return 2;
    }
    struct record *view =
        mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (view == MAP_FAILED) {
        return 3;
    }
    view->exec_count += 1;
    if (munmap(view, PAGE) != 0 || close(fd) != 0) {
        return 4;
    }
    return 0;
}

static void exec_helper(char *program) {
    char *child_argv[] = {program, "helper", NULL};
    execve(program, child_argv, environ);
}

static void on_signal(int signal) {
    if (signal == SIGUSR1) {
        record->self_signal = 1;
    } else if (signal == SIGUSR2) {
        record->parent_signal = 1;
    }
}

static long raw_kill(long pid, int signal) {
    return syscall(SYS_kill, pid, signal);
}

// Maps the file twice: the whole of it writable, and its second page
// read-only, as glibc maps its iconv cache.
static void map_views(unsigned char **writable, unsigned char **readonly) {
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
    REQUIRE(fd >= 0, "create file");
    unsigned char bytes[PAGES * PAGE];
    for (size_t i = 0; i < sizeof(bytes); i++) {
        bytes[i] = i < PAGE ? 0 : pattern(i);
    }
    REQUIRE(write(fd, bytes, sizeof(bytes)) == (ssize_t)sizeof(bytes),
            "write file");
    *writable = mmap(NULL, PAGES * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED,
                     fd, 0);
    REQUIRE(*writable != MAP_FAILED, "map writable view");
    *readonly = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, fd, PAGE);
    REQUIRE(*readonly != MAP_FAILED, "map read-only view");
    REQUIRE(close(fd) == 0, "close file");
    record = (volatile struct record *)*writable;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "helper") == 0) {
        return helper();
    }
    int parent_signal = argc == 2 && strcmp(argv[1], "parent-signal") == 0;
    REQUIRE(argc == 1 || parent_signal, "usage");
    REQUIRE(sysconf(_SC_PAGESIZE) == PAGE, "page size");
    unsigned char *writable;
    unsigned char *readonly;
    map_views(&writable, &readonly);
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = on_signal;
    action.sa_flags = SA_RESTART;
    sigemptyset(&action.sa_mask);
    REQUIRE(sigaction(SIGUSR1, &action, NULL) == 0 &&
                sigaction(SIGUSR2, &action, NULL) == 0,
            "sigaction");

    // A vfork child execs a new image, which maps the file itself.
    pid_t child = vfork();
    REQUIRE(child >= 0, "vfork exec");
    if (child == 0) {
        exec_helper(argv[0]);
        _exit(127);
    }
    wait_ok(child, "exec child");
    REQUIRE(record->exec_count == 1, "exec child bumped the counter");

    // A vfork child stores through the view and exits.
    child = vfork();
    REQUIRE(child >= 0, "vfork exit");
    if (child == 0) {
        record->exit_store = 0x5a5a;
        _exit(0);
    }
    wait_ok(child, "exit child");
    REQUIRE(record->exit_store == 0x5a5a, "exit child's store");

    // A vfork child's execve fails; it goes on in the shared address space,
    // records the error in the view and exits.
    child = vfork();
    REQUIRE(child >= 0, "vfork failed exec");
    if (child == 0) {
        char *missing_argv[] = {"missing", NULL};
        execve("/nonexistent/shared-file-vfork-helper", missing_argv, environ);
        record->failed_errno = errno;
        _exit(0);
    }
    wait_ok(child, "failed-exec child");
    REQUIRE(record->failed_errno == ENOENT, "failed exec errno");

    // Signals during the window: the child's own, handled in the child, and
    // with "parent-signal" one to the parent, handled once the parent resumes.
    child = vfork();
    REQUIRE(child >= 0, "vfork signal");
    if (child == 0) {
        if (raw_kill(syscall(SYS_getpid), SIGUSR1) != 0) {
            _exit(10 + errno);
        }
        if (parent_signal && raw_kill(syscall(SYS_getppid), SIGUSR2) != 0) {
            _exit(100 + errno);
        }
        _exit(0);
    }
    wait_ok(child, "signal child");
    REQUIRE(record->self_signal == 1, "child's handler stored into the view");
    REQUIRE(record->parent_signal == (uint32_t)parent_signal,
            "parent's handler ran");

    // A vfork child killed by a signal.
    child = vfork();
    REQUIRE(child >= 0, "vfork killed");
    if (child == 0) {
        raw_kill(syscall(SYS_getpid), SIGTERM);
        _exit(1);
    }
    int status = wait_status(child);
    REQUIRE(WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM,
            "killed child's status");

    // A vfork child vforks a grandchild, which stores and execs.
    child = vfork();
    REQUIRE(child >= 0, "vfork nested");
    if (child == 0) {
        pid_t grandchild = vfork();
        if (grandchild < 0) {
            _exit(1);
        }
        if (grandchild == 0) {
            record->nested = 1;
            exec_helper(argv[0]);
            _exit(127);
        }
        int grandchild_status = wait_status(grandchild);
        if (!WIFEXITED(grandchild_status) ||
            WEXITSTATUS(grandchild_status) != 0 || record->nested != 1 ||
            record->exec_count != 2) {
            _exit(1);
        }
        record->nested = 2;
        _exit(0);
    }
    wait_ok(child, "nested child");
    REQUIRE(record->nested == 2 && record->exec_count == 2, "nested vfork");

    // posix_spawn: glibc's clone or clone3 with CLONE_VM|CLONE_VFORK.
    char *spawn_argv[] = {argv[0], "helper", NULL};
    REQUIRE(posix_spawn(&child, argv[0], NULL, NULL, spawn_argv, environ) == 0,
            "posix_spawn");
    wait_ok(child, "spawned child");
    REQUIRE(record->exec_count == 3, "spawned child bumped the counter");

    // The views still show exactly the file's bytes.
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    REQUIRE(fd >= 0, "reopen file");
    unsigned char bytes[PAGES * PAGE];
    REQUIRE(pread(fd, bytes, sizeof(bytes), 0) == (ssize_t)sizeof(bytes),
            "read file");
    REQUIRE(memcmp(bytes, writable, sizeof(bytes)) == 0, "writable view");
    REQUIRE(memcmp(bytes + PAGE, readonly, PAGE) == 0, "read-only view");
    for (size_t i = PAGE; i < sizeof(bytes); i++) {
        REQUIRE(bytes[i] == pattern(i), "file pattern");
    }
    REQUIRE(close(fd) == 0, "close file");
    printf("shared-file vfork exec=ok exit=ok failed-exec=ok signals=%s "
           "killed=ok nested=ok spawn=ok views=ok\n",
           parent_signal ? "child+parent" : "child");
    return 0;
}
