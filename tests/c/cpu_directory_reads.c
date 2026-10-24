/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Lists /sys/devices/system/cpu through every way getdents64 can be served
// and reports, for each, whether any CPU other than cpu0 appears:
// - "tracked": an ordinary descriptor, read with a large buffer;
// - "received": a descriptor received through SCM_RIGHTS, read with a
//   48-byte buffer that holds one or two entries, so that a kernel buffer can
//   hold only CPUs other than cpu0. It also reports whether the entry count
//   matches the tracked one and whether every byte past each call's return
//   value, and every byte of the final call's buffer, is either untouched
//   (0xaa) or zero;
// - "received bus": the same for /sys/bus/cpu/devices, which holds only
//   cpuN entries, so its last kernel buffers hold only CPUs other than cpu0
//   and the call that ends the listing follows them;
// - "negative": an ordinary descriptor read to its end, rewound, moved to
//   offset 3, given a getdents64 whose count does not fit in an int (EINVAL),
//   then read on from where that left it.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

#define DIRECTORY "/sys/devices/system/cpu"
#define BUS_DIRECTORY "/sys/bus/cpu/devices"

struct dirent64 {
    uint64_t ino;
    int64_t off;
    unsigned short reclen;
    unsigned char type;
    char name[];
};

// Whether `name` is cpuN for some N other than 0.
static int other_cpu(const char *name) {
    if (strncmp(name, "cpu", 3) != 0 || name[3] == '\0') {
        return 0;
    }
    for (const char *c = name + 3; *c != '\0'; c++) {
        if (*c < '0' || *c > '9') {
            return 0;
        }
    }
    return strcmp(name + 3, "0") != 0;
}

struct listing {
    int entries;
    int other_cpus;
    int dirty_tail;
    int error;
};

// Reads `fd` to its end with `size`-byte getdents64 calls.
static struct listing list(int fd, size_t size) {
    struct listing result = {0, 0, 0, 0};
    unsigned char *buffer = malloc(size);
    for (int calls = 0; calls < 100000; calls++) {
        memset(buffer, 0xaa, size);
        long n = syscall(SYS_getdents64, fd, buffer, size);
        if (n < 0) {
            result.error = errno;
            break;
        }
        if (n == 0) {
            for (size_t i = 0; i < size; i++) {
                if (buffer[i] != 0xaa && buffer[i] != 0) {
                    result.dirty_tail = 1;
                }
            }
            break;
        }
        for (long i = 0; i < n;) {
            struct dirent64 *entry = (struct dirent64 *)(buffer + i);
            result.entries++;
            result.other_cpus += other_cpu(entry->name);
            i += entry->reclen;
        }
        for (size_t i = (size_t)n; i < size; i++) {
            if (buffer[i] != 0xaa && buffer[i] != 0) {
                result.dirty_tail = 1;
            }
        }
    }
    free(buffer);
    return result;
}

// A copy of `fd` received through SCM_RIGHTS, which Detcore does not track.
static int received_copy(int fd) {
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
        perror("socketpair");
        exit(2);
    }
    char byte = 'x';
    struct iovec iov = {&byte, 1};
    union {
        char buffer[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } control;
    struct msghdr message = {0};
    message.msg_iov = &iov;
    message.msg_iovlen = 1;
    message.msg_control = control.buffer;
    message.msg_controllen = sizeof control.buffer;
    struct cmsghdr *header = CMSG_FIRSTHDR(&message);
    header->cmsg_level = SOL_SOCKET;
    header->cmsg_type = SCM_RIGHTS;
    header->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(header), &fd, sizeof(int));
    if (sendmsg(pair[0], &message, 0) != 1 || recvmsg(pair[1], &message, 0) != 1) {
        perror("SCM_RIGHTS");
        exit(2);
    }
    int copy = -1;
    memcpy(&copy, CMSG_DATA(CMSG_FIRSTHDR(&message)), sizeof(int));
    close(pair[0]);
    close(pair[1]);
    return copy;
}

int main(void) {
    int fd = open(DIRECTORY, O_RDONLY | O_DIRECTORY);
    struct listing tracked = list(fd, 32768);
    close(fd);
    printf("tracked: other_cpus=%d error=%d\n", tracked.other_cpus, tracked.error);

    fd = open(DIRECTORY, O_RDONLY | O_DIRECTORY);
    int copy = received_copy(fd);
    close(fd);
    struct listing received = list(copy, 48);
    close(copy);
    printf("received: other_cpus=%d entries_match=%d dirty_tail=%d error=%d\n",
           received.other_cpus, received.entries == tracked.entries, received.dirty_tail,
           received.error);

    fd = open(BUS_DIRECTORY, O_RDONLY | O_DIRECTORY);
    copy = received_copy(fd);
    close(fd);
    struct listing bus = list(copy, 48);
    close(copy);
    printf("received bus: entries=%d other_cpus=%d dirty_tail=%d error=%d\n", bus.entries,
           bus.other_cpus, bus.dirty_tail, bus.error);

    fd = open(DIRECTORY, O_RDONLY | O_DIRECTORY);
    struct listing first = list(fd, 32768);
    lseek(fd, 0, SEEK_SET);
    lseek(fd, 3, SEEK_SET);
    char small[64];
    long rc = syscall(SYS_getdents64, fd, small, (size_t)0x80000000u);
    int saved = errno;
    struct listing after = list(fd, 32768);
    close(fd);
    printf("negative: rc=%ld %s other_cpus=%d error=%d\n", rc, rc < 0 ? strerror(saved) : "ok",
           first.other_cpus + after.other_cpus, after.error);
    return 0;
}
