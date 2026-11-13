/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Synthetic autofs daemon for the privileged autofs fixture test
// (hermit-cli/tests/mount_introspection.rs).
// Usage: autofs_fixture_daemon ROOT READYFILE LOGFILE GHOSTKEYS_CSV
// Mounts an indirect autofs at ROOT (needs root in the init user
// namespace), answers v5 packets: names starting "real" or "ghost"
// get a tmpfs + READY, anything else FAIL (-ENOENT). Logs one line
// per request so tests can count daemon round-trips.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/auto_fs.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

static FILE *logf;
static void die(const char *what) { perror(what); exit(2); }

int main(int argc, char **argv) {
    if (argc != 5) { fprintf(stderr, "usage: ROOT READY LOG GHOSTS_CSV\n"); return 2; }
    const char *root = argv[1];
    logf = fopen(argv[3], "a");
    if (!logf) die("fopen log");
    setvbuf(logf, NULL, _IOLBF, 0);
    if (setpgid(0, 0) < 0) die("setpgid");
    int p[2];
    if (pipe(p) < 0) die("pipe");
    char opts[128];
    snprintf(opts, sizeof opts, "fd=%d,pgrp=%ld,minproto=5,maxproto=5,indirect",
             p[1], (long)getpgrp());
    mkdir(root, 0755);
    if (mount("g0autofs", root, "autofs", 0, opts) < 0) die("mount autofs");
    int rootfd = open(root, O_RDONLY | O_DIRECTORY);
    if (rootfd < 0) die("open root");
    // Ghost keys: pre-create positive dentries (oz mode: no trigger).
    char *ghosts = strdup(argv[4]);
    for (char *k = strtok(ghosts, ","); k; k = strtok(NULL, ",")) {
        char path[1024];
        snprintf(path, sizeof path, "%s/%s", root, k);
        if (mkdir(path, 0755) < 0 && errno != EEXIST)
            fprintf(logf, "GHOST-MKDIR-FAIL %s errno=%d\n", k, errno);
    }
    FILE *ready = fopen(argv[2], "w");
    if (!ready) die("fopen ready");
    fputs("ready\n", ready);
    fclose(ready);
    fprintf(logf, "DAEMON-READY root=%s\n", root);
    for (;;) {
        struct autofs_v5_packet pkt;
        ssize_t n = read(p[0], &pkt, sizeof pkt);
        if (n < 0) { if (errno == EINTR) continue; die("read packet"); }
        if (n == 0) break;
        char name[NAME_MAX + 1];
        memset(name, 0, sizeof name);
        size_t len = pkt.len < NAME_MAX ? pkt.len : NAME_MAX;
        memcpy(name, pkt.name, len);
        unsigned long token = pkt.wait_queue_token;
        int known = strncmp(name, "real", 4) == 0 || strncmp(name, "ghost", 5) == 0;
        fprintf(logf, "REQ type=%d name=%s known=%d token=%lu\n",
                pkt.hdr.type, name, known, token);
        if (pkt.hdr.type != autofs_ptype_missing_indirect &&
            pkt.hdr.type != autofs_ptype_missing_direct) {
            ioctl(rootfd, AUTOFS_IOC_FAIL, token);
            fprintf(logf, "ANS type=%d FAIL(non-missing)\n", pkt.hdr.type);
            continue;
        }
        if (!known) {
            ioctl(rootfd, AUTOFS_IOC_FAIL, token);
            fprintf(logf, "ANS %s FAIL\n", name);
            continue;
        }
        char path[1024];
        snprintf(path, sizeof path, "%s/%s", root, name);
        mkdir(path, 0755);
        int mrc = mount("tmpfs", path, "tmpfs", 0, "");
        if (mrc < 0 && errno != EBUSY) {
            fprintf(logf, "MOUNT-FAIL %s errno=%d\n", name, errno);
            ioctl(rootfd, AUTOFS_IOC_FAIL, token);
            continue;
        }
        char marker[1088];
        snprintf(marker, sizeof marker, "%s/marker", path);
        int mf = open(marker, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (mf >= 0) { dprintf(mf, "key=%s\n", name); close(mf); }
        int irc = ioctl(rootfd, AUTOFS_IOC_READY, token);
        fprintf(logf, "ANS %s READY mrc=%d ioctl=%d errno=%d\n", name, mrc, irc, errno);
    }
    return 0;
}
