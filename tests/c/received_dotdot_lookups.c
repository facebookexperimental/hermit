/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Looks up names through `..` relative to directory descriptors for
// /tmp/d/sub: one opened by the guest, and the same directory received over
// SCM_RIGHTS, which a tracer may know no path for. Below /tmp/d are the
// guest's own proc/uptime, an ordinary file, and proc/fs, a directory: names
// Hermit decides about only below /proc, which `..` from these descriptors
// does not reach. For each it prints what a read of the file returns, or the
// errno of an fstatat and of an O_DIRECTORY openat, 0 for success. Controls:
// /proc/fs, which Hermit hides at every --proc level, absolutely; an openat
// of ../proc/uptime from a descriptor that is not open (EBADF); and an open
// of /dev/null/../../proc/uptime, whose walk stops at a file (ENOTDIR).
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <unistd.h>

static const char *errno_name(int error) {
    switch (error) {
    case 0:
        return "0";
    case ENOENT:
        return "ENOENT";
    case EACCES:
        return "EACCES";
    case ENOTDIR:
        return "ENOTDIR";
    case EBADF:
        return "EBADF";
    default:
        return strerror(error);
    }
}

// `fd`, sent over a datagram socket pair with SCM_RIGHTS and received back
// as a new descriptor, or -1.
static int received(int fd) {
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0, pair) != 0) {
        return -1;
    }
    char byte = 0;
    struct iovec iov = {.iov_base = &byte, .iov_len = 1};
    union {
        char buffer[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } control;
    memset(&control, 0, sizeof(control));
    struct msghdr message = {.msg_iov = &iov,
                             .msg_iovlen = 1,
                             .msg_control = control.buffer,
                             .msg_controllen = sizeof(control.buffer)};
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &fd, sizeof(int));
    int got = -1;
    if (sendmsg(pair[0], &message, 0) == 1) {
        memset(&control, 0, sizeof(control));
        message.msg_controllen = sizeof(control.buffer);
        if (recvmsg(pair[1], &message, MSG_CMSG_CLOEXEC) == 1 &&
            (header = CMSG_FIRSTHDR(&message)) != NULL && header->cmsg_type == SCM_RIGHTS) {
            memcpy(&got, CMSG_DATA(header), sizeof(int));
        }
    }
    close(pair[0]);
    close(pair[1]);
    return got;
}

static void read_at(const char *label, int dirfd, const char *name) {
    errno = 0;
    int fd = openat(dirfd, name, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        printf("%s %s: open=%s\n", label, name, errno_name(errno));
        return;
    }
    char contents[64] = {0};
    ssize_t length = read(fd, contents, sizeof(contents) - 1);
    close(fd);
    if (length < 0) {
        printf("%s %s: read failed\n", label, name);
        return;
    }
    contents[strcspn(contents, "\n")] = '\0';
    printf("%s %s: read=\"%s\"\n", label, name, contents);
}

static void look_up_at(const char *label, int dirfd, const char *name) {
    struct stat status;
    errno = 0;
    int stat_error = fstatat(dirfd, name, &status, 0) == 0 ? 0 : errno;
    errno = 0;
    int fd = openat(dirfd, name, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    int open_error = fd >= 0 ? 0 : errno;
    if (fd >= 0) {
        close(fd);
    }
    printf("%s %s: stat=%s open=%s\n", label, name, errno_name(stat_error),
           errno_name(open_error));
}

int main(void) {
    if (mkdir("/tmp/d", 0755) != 0 || mkdir("/tmp/d/sub", 0755) != 0 ||
        mkdir("/tmp/d/proc", 0755) != 0 || mkdir("/tmp/d/proc/fs", 0755) != 0) {
        perror("mkdir");
        return 1;
    }
    FILE *file = fopen("/tmp/d/proc/uptime", "w");
    if (file == NULL || fputs("the guest's own file\n", file) < 0 || fclose(file) != 0) {
        perror("/tmp/d/proc/uptime");
        return 1;
    }
    int opened = open("/tmp/d/sub", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    int got = opened >= 0 ? received(opened) : -1;
    if (opened < 0 || got < 0) {
        perror("descriptor for /tmp/d/sub");
        return 1;
    }
    read_at("opened", opened, "../proc/uptime");
    read_at("received", got, "../proc/uptime");
    look_up_at("opened", opened, "../proc/fs");
    look_up_at("received", got, "../proc/fs");
    look_up_at("received", got, "../../d/proc/fs");
    look_up_at("absolute", AT_FDCWD, "/proc/fs");
    read_at("not open", -1, "../proc/uptime");
    read_at("absolute", AT_FDCWD, "/dev/null/../../proc/uptime");
    return 0;
}
