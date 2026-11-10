/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved. Licensed under the BSD-style license in LICENSE. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

/* A guest-handled SIGALRM against a blocking read() of a pipe whose other end
 * a child holds (dev-hermit dh-ru056 slice 2). The child is forked before the
 * handler is installed, since a process that handles SIGALRM may not fork,
 * and the parent closes its own write end before the install too, since it
 * may not close a pipe afterwards. Every line is a function of the program
 * and Linux's rules, never of host timing: times are reported only as the
 * window they fall in, relative to the alarm and the child's write.
 *
 *   eintr:   alarm(1); the child writes at 3s. The read returns -1/EINTR
 *            after the handler ran, whose frame holds -EINTR in its result
 *            register just past the syscall instruction; a second read
 *            returns the byte.
 *   data:    the child writes at 1s; alarm(2). The read returns 1 first; the
 *            handler runs after it, at the alarm, which a pause() awaits.
 *   restart: as eintr, with SA_RESTART. Linux runs the handler and restarts
 *            the read, which returns the byte at 3s.
 *   eof:     the child closes its end and exits at 1s; alarm(3). The read
 *            returns 0 with no handler run; the child's SIGCHLD, ignored by
 *            default, interrupts nothing.
 *   childexit: the parent keeps its own write end; the child exits at 0.5s
 *            without writing; alarm(1). The child's SIGCHLD, ignored by
 *            default, interrupts nothing: the read returns -1/EINTR at the
 *            alarm, after the handler.
 *   fifo:    as eintr, through a named FIFO opened before the install. The
 *            parent opens the read end with O_NONBLOCK and then the write
 *            end before the fork, so no open waits; the child writes
 *            through the inherited write end, and the parent keeps its own
 *            copy, which it may not close once it handles SIGALRM. The read
 *            end is made blocking again before the install.
 *   blocked: as eintr, with SIGALRM blocked by sigprocmask across the read.
 *            The read returns the byte at 3s with no handler run; the
 *            handler runs when the parent unblocks SIGALRM, before
 *            sigprocmask returns.
 *   setfl:   the child sets O_NONBLOCK on the shared read end with F_SETFL
 *            and then tells the parent, through a second pipe read before
 *            the install; it writes at 3s. Linux's processes share the
 *            description's flags, so the read returns -1/EAGAIN at once,
 *            with no handler run.
 *   fionbio: as setfl, with ioctl(FIONBIO).
 *   setfl_exit: the parent keeps its own write end; the child sets
 *            O_NONBLOCK on the shared read end with F_SETFL and exits at
 *            once, and the parent reaps it before the install. The status
 *            is the description's, which the parent still holds, so the read
 *            returns -1/EAGAIN at once, with no handler run.
 *   setfl_mid: as eintr, but the child sets O_NONBLOCK on the shared read
 *            end at 0.5s, while the parent's read waits. Linux's interrupted
 *            wait returns -ERESTARTSYS whatever the flags, so the read
 *            returns -1/EINTR after the handler; a second read then returns
 *            -1/EAGAIN at once.
 *   scmsetfl: the parent passes the read end to the child over a
 *            socketpair (SCM_RIGHTS) before the install; the child sets
 *            O_NONBLOCK on the end it received and tells the parent, through
 *            a second pipe; it writes at 3s. Linux's received descriptor is
 *            the same description, so the read returns -1/EAGAIN at once,
 *            with no handler run.
 *   revoke:  as eintr, but at 0.5s, while the parent's read waits, the child
 *            passes a descriptor over SCM_RIGHTS on an AF_UNIX socket that is
 *            not connected, which fails. The read returns -1/EINTR after the
 *            handler, as in eintr. In-guest, the attempt counts as a
 *            descriptor that may have left the guest.
 *   stdin:   as eintr, on standard input, the container's stdin, with no
 *            writer in the guest; the child exits at once. Under ptrace,
 *            Hermit's container gives the guest an empty standard input, so
 *            the read returns 0. In-guest refuses it: a read of the
 *            container's stdio that may wait.
 */

static volatile sig_atomic_t hits;
static volatile sig_atomic_t frame_result_is_eintr;
static volatile sig_atomic_t frame_ip_after_syscall;
static struct timespec start;
/* The child's exit status where the parent reaped it before the install
 * (setfl_exit), or -2. */
static int reaped = -2;

static long elapsed_ms(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (now.tv_sec - start.tv_sec) * 1000 + (now.tv_nsec - start.tv_nsec) / 1000000;
}

/* The window a time falls in: before the alarm at 1s, between it and the
 * write at 3s, or after that. */
static const char *window(long ms, long alarm_ms, long write_ms) {
    if (ms < alarm_ms) return "before_alarm";
    if (ms < write_ms) return "at_alarm";
    return "at_write";
}

static void on_alarm(int sig, siginfo_t *info, void *context) {
    (void)sig;
    (void)info;
    ucontext_t *uc = context;
    const unsigned char *ip = (const unsigned char *)uc->uc_mcontext.gregs[REG_RIP];
    hits++;
    frame_result_is_eintr = uc->uc_mcontext.gregs[REG_RAX] == -EINTR;
    frame_ip_after_syscall = ip[-2] == 0x0f && ip[-1] == 0x05;
}

/* Sends `fd` over SCM_RIGHTS on `socket`, with one data byte. */
static ssize_t send_fd(int socket, int fd) {
    char byte = 'f';
    struct iovec iov = {&byte, 1};
    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } control;
    memset(&control, 0, sizeof(control));
    struct msghdr msg = {0};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.buf;
    msg.msg_controllen = sizeof(control.buf);
    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
    cmsg->cmsg_level = SOL_SOCKET;
    cmsg->cmsg_type = SCM_RIGHTS;
    cmsg->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cmsg), &fd, sizeof(int));
    return sendmsg(socket, &msg, 0);
}

/* Receives one descriptor over SCM_RIGHTS on `socket`, or -1. */
static int receive_fd(int socket) {
    char byte;
    struct iovec iov = {&byte, 1};
    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } control;
    struct msghdr msg = {0};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.buf;
    msg.msg_controllen = sizeof(control.buf);
    if (recvmsg(socket, &msg, 0) != 1) return -1;
    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
    if (!cmsg || cmsg->cmsg_type != SCM_RIGHTS) return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(cmsg), sizeof(int));
    return fd;
}

static void sleep_ms(long ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000};
    while (nanosleep(&t, &t) && errno == EINTR) {
    }
}

static int install(int restart) {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = on_alarm;
    sa.sa_flags = SA_SIGINFO | (restart ? SA_RESTART : 0);
    sigemptyset(&sa.sa_mask);
    return sigaction(SIGALRM, &sa, NULL);
}

/* Cancels the alarm and restores the default action, so the parent may wait
 * for its child. */
static int finish(pid_t child) {
    alarm(0);
    signal(SIGALRM, SIG_DFL);
    if (reaped != -2) {
        printf("child_exit=%d\n", reaped);
        return 0;
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        printf("waitpid errno=%d\n", errno);
        return 1;
    }
    printf("child_exit=%d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    const char *mode = argv[1];
    int restart = !strcmp(mode, "restart");
    int fifo = !strcmp(mode, "fifo");
    int eof = !strcmp(mode, "eof");
    int data = !strcmp(mode, "data");
    int childexit = !strcmp(mode, "childexit");
    int blocked = !strcmp(mode, "blocked");
    int setfl = !strcmp(mode, "setfl");
    int fionbio = !strcmp(mode, "fionbio");
    int setfl_mid = !strcmp(mode, "setfl_mid");
    int stdin_read = !strcmp(mode, "stdin");
    int setfl_exit = !strcmp(mode, "setfl_exit");
    int scmsetfl = !strcmp(mode, "scmsetfl");
    int revoke = !strcmp(mode, "revoke");
    /* The child changes the read end's status before the read (setfl,
     * fionbio, setfl_exit, scmsetfl) or while it waits (setfl_mid). */
    int status_change = setfl || fionbio || setfl_mid || setfl_exit || scmsetfl;
    if (!restart && !fifo && !eof && !data && !childexit && !blocked && !status_change &&
        !stdin_read && !revoke && strcmp(mode, "eintr"))
        return 2;
    long write_ms = data ? 1000 : eof ? 1000 : childexit ? 500 : 3000;
    /* The windows' upper bound: the write, or 3s where the child writes
     * nothing after the alarm. */
    long window_ms = childexit ? 3000 : write_ms;
    long alarm_ms = data ? 2000 : eof ? 3000 : 1000;
    setvbuf(stdout, NULL, _IONBF, 0);
    clock_gettime(CLOCK_MONOTONIC, &start);

    int ends[2];
    /* The child's word that it changed the read end's status. */
    int sync[2] = {-1, -1};
    if ((setfl || fionbio || scmsetfl) && pipe(sync)) {
        printf("pipe errno=%d\n", errno);
        return 1;
    }
    /* The channel the parent passes the read end over (scmsetfl). */
    int channel[2] = {-1, -1};
    if (scmsetfl && socketpair(AF_UNIX, SOCK_STREAM, 0, channel)) {
        printf("socketpair errno=%d\n", errno);
        return 1;
    }
    const char *path = "sigalrm_pipe_read.fifo";
    int in = -1;
    if (fifo) {
        /* No unlink first: a stale FIFO fails mkfifo with EEXIST, a visible
         * result, rather than make two runs differ. */
        if (mkfifo(path, 0600)) {
            printf("mkfifo errno=%d\n", errno);
            return 1;
        }
        in = open(path, O_RDONLY | O_NONBLOCK);
        ends[1] = open(path, O_WRONLY);
        if (in < 0 || ends[1] < 0) {
            printf("open errno=%d\n", errno);
            return 1;
        }
        /* Unlinked while still unhandled: phase 1 refuses unlink once the
         * process handles SIGALRM. The open ends stay usable. */
        unlink(path);
    } else if (stdin_read) {
        in = 0;
    } else if (pipe(ends)) {
        printf("pipe errno=%d\n", errno);
        return 1;
    }
    pid_t child = fork();
    if (child < 0) {
        printf("fork errno=%d\n", errno);
        return 1;
    }
    if (child == 0) {
        if (stdin_read) _exit(0);
        int out = ends[1];
        if (status_change) {
            /* The read end stays open here: its status is the shared
             * description's, which the child changes. */
            if (setfl_mid) sleep_ms(500);
            int target = ends[0];
            if (scmsetfl && (target = receive_fd(channel[1])) < 0) _exit(6);
            int changed = fionbio ? ioctl(target, FIONBIO, &(int){1})
                                  : fcntl(target, F_SETFL, fcntl(target, F_GETFL) | O_NONBLOCK);
            if (changed) _exit(4);
            if (setfl_exit) _exit(0);
            if (!setfl_mid && write(sync[1], "s", 1) != 1) _exit(5);
            sleep_ms(write_ms - (setfl_mid ? 500 : 0));
            if (write(out, "x", 1) != 1) _exit(3);
            _exit(0);
        }
        close(fifo ? in : ends[0]);
        if (revoke) {
            /* A socket that is not connected: the send fails, having
             * offered a descriptor. */
            sleep_ms(500);
            int unconnected = socket(AF_UNIX, SOCK_DGRAM, 0);
            if (unconnected < 0) _exit(7);
            if (send_fd(unconnected, out) >= 0) _exit(8);
            sleep_ms(write_ms - 500);
            if (write(out, "x", 1) != 1) _exit(3);
            _exit(0);
        }
        sleep_ms(write_ms);
        if (!eof && !childexit && write(out, "x", 1) != 1) _exit(3);
        _exit(0);
    }
    if (stdin_read) {
        /* The container's descriptor, kept as it came. */
    } else if (fifo) {
        if (fcntl(in, F_SETFL, fcntl(in, F_GETFL) & ~O_NONBLOCK)) {
            printf("fcntl errno=%d\n", errno);
            return 1;
        }
    } else {
        in = ends[0];
        /* childexit and setfl_exit keep a writer, so the read could wait. */
        if (!childexit && !setfl_exit) close(ends[1]);
    }
    if (setfl_exit) {
        /* Before the install: the parent may wait for its child only while
         * SIGALRM's action is the default. */
        int status = 0;
        if (waitpid(child, &status, 0) != child) {
            printf("waitpid errno=%d\n", errno);
            return 1;
        }
        reaped = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    }
    if (scmsetfl && send_fd(channel[0], in) != 1) {
        printf("sendmsg errno=%d\n", errno);
        return 1;
    }
    if (sync[0] >= 0) {
        /* Before the install: the parent may not close a pipe afterwards. */
        char s = '?';
        if (read(sync[0], &s, 1) != 1) {
            printf("sync errno=%d\n", errno);
            return 1;
        }
        close(sync[0]);
        close(sync[1]);
    }
    sigset_t alarm_set;
    sigemptyset(&alarm_set);
    sigaddset(&alarm_set, SIGALRM);
    if (install(restart)) {
        printf("sigaction errno=%d\n", errno);
        return 1;
    }
    if (blocked && sigprocmask(SIG_BLOCK, &alarm_set, NULL)) {
        printf("sigprocmask errno=%d\n", errno);
        return 1;
    }
    alarm(alarm_ms / 1000);

    char c = '?';
    errno = 0;
    ssize_t n = read(in, &c, 1);
    int e = errno;
    long at = elapsed_ms();
    printf("read=%zd errno=%d byte=%c window=%s hits_at_return=%d\n", n, n < 0 ? e : 0,
           n == 1 ? c : '-', window(at, alarm_ms, window_ms), (int)hits);
    if (hits) {
        printf("frame result_eintr=%d ip_after_syscall=%d\n", (int)frame_result_is_eintr,
               (int)frame_ip_after_syscall);
    }
    if (blocked) {
        int unblocked = sigprocmask(SIG_UNBLOCK, &alarm_set, NULL);
        printf("unblock=%d hits_at_return=%d\n", unblocked, (int)hits);
    } else if (n < 0 && e == EINTR && !childexit && !stdin_read) {
        c = '?';
        errno = 0;
        n = read(in, &c, 1);
        printf("read2=%zd errno=%d byte=%c window=%s\n", n, n < 0 ? errno : 0, n == 1 ? c : '-',
               window(elapsed_ms(), alarm_ms, window_ms));
    } else if (data) {
        errno = 0;
        int p = pause();
        printf("pause=%d errno=%d hits=%d\n", p, errno, (int)hits);
    } else if (eof) {
        printf("alarm_left=%u\n", alarm(0));
    }
    return finish(child);
}
