/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// io_uring ring-operation determinism guest. Unlike io_uring_fallback.c (which
// probes the unsupported/fallback path), this drives a real submission and
// completion queue via raw syscalls (no liburing dependency): it sets up a
// ring, submits an IORING_OP_WRITE against a temp file, waits for and reaps the
// completion, then reads the data back and folds it into an FNV-1a checksum.
// If io_uring is unavailable it prints a deterministic "unsupported" line and
// still succeeds, so the test is a determinism witness on hosts with and
// without io_uring. Prints "io_uring_ring success" as the final line.
#include <errno.h>
#include <fcntl.h>
#include <linux/io_uring.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static int io_uring_setup(unsigned entries, struct io_uring_params *p) {
    return (int)syscall(SYS_io_uring_setup, entries, p);
}

static int io_uring_enter(int fd, unsigned to_submit, unsigned min_complete,
                          unsigned flags) {
    return (int)syscall(SYS_io_uring_enter, fd, to_submit, min_complete, flags,
                        (void *)0, (size_t)0);
}

int main(void) {
    struct io_uring_params p;
    memset(&p, 0, sizeof p);
    int ring = io_uring_setup(8, &p);
    if (ring < 0) {
        printf("io_uring_setup=unsupported\n");
        printf("io_uring_ring success\n");
        return 0;
    }

    size_t sq_sz = p.sq_off.array + p.sq_entries * sizeof(unsigned);
    size_t cq_sz = p.cq_off.cqes + p.cq_entries * sizeof(struct io_uring_cqe);
    void *sq = mmap(0, sq_sz, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE,
                    ring, IORING_OFF_SQ_RING);
    void *cq = mmap(0, cq_sz, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE,
                    ring, IORING_OFF_CQ_RING);
    struct io_uring_sqe *sqes =
        mmap(0, p.sq_entries * sizeof(struct io_uring_sqe),
             PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE, ring,
             IORING_OFF_SQES);
    if (sq == MAP_FAILED || cq == MAP_FAILED || sqes == MAP_FAILED) {
        printf("io_uring_mmap=failed\n");
        return 1;
    }

    unsigned *sq_tail = (unsigned *)((char *)sq + p.sq_off.tail);
    unsigned *sq_ring_mask = (unsigned *)((char *)sq + p.sq_off.ring_mask);
    unsigned *sq_array = (unsigned *)((char *)sq + p.sq_off.array);
    unsigned *cq_head = (unsigned *)((char *)cq + p.cq_off.head);
    unsigned *cq_tail = (unsigned *)((char *)cq + p.cq_off.tail);
    unsigned *cq_ring_mask = (unsigned *)((char *)cq + p.cq_off.ring_mask);
    struct io_uring_cqe *cqes =
        (struct io_uring_cqe *)((char *)cq + p.cq_off.cqes);

    const char *path = "io_uring_ring_data.bin";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) {
        perror("open");
        return 1;
    }

    char wbuf[4096];
    for (int i = 0; i < 4096; i++) {
        wbuf[i] = (char)((i * 13 + 7) & 0xff);
    }

    unsigned tail = *sq_tail;
    unsigned idx = tail & *sq_ring_mask;
    struct io_uring_sqe *s = &sqes[idx];
    memset(s, 0, sizeof *s);
    s->opcode = IORING_OP_WRITE;
    s->fd = fd;
    s->addr = (unsigned long)wbuf;
    s->len = sizeof wbuf;
    s->off = 0;
    sq_array[idx] = idx;
    *sq_tail = tail + 1;
    __sync_synchronize();

    int r = io_uring_enter(ring, 1, 1, IORING_ENTER_GETEVENTS);
    if (r < 0) {
        printf("io_uring_enter=failed errno=%d\n", errno);
        return 1;
    }

    unsigned chead = *cq_head;
    long written = -1;
    while (chead != *cq_tail) {
        struct io_uring_cqe *e = &cqes[chead & *cq_ring_mask];
        written = e->res;
        chead++;
    }
    *cq_head = chead;
    __sync_synchronize();

    char rbuf[4096];
    ssize_t got = pread(fd, rbuf, sizeof rbuf, 0);
    uint64_t h = 1469598103934665603ULL;
    for (ssize_t i = 0; i < got; i++) {
        h ^= (uint8_t)rbuf[i];
        h *= 1099511628211ULL;
    }

    close(fd);
    unlink(path);

    printf("io_uring_setup=ok\n");
    printf("write_res=%ld read_bytes=%zd\n", written, got);
    printf("checksum=%016llx\n", (unsigned long long)h);
    if (written != 4096 || got != 4096) {
        printf("io_uring_ring MISMATCH\n");
        return 1;
    }
    printf("io_uring_ring success\n");
    return 0;
}
