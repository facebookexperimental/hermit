/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * In-guest LiteInst: the Detcore Tool runs inside this process, so anything it
 * does through the guest's C library heap is visible to the guest. Installing
 * a SIGALRM handler makes Detcore check the kernel's descriptor table for a
 * signalfd. If that check listed /proc/self/fd with opendir, glibc would
 * allocate its directory buffer from this process's malloc heap and leave raw
 * /proc inode numbers in it after closedir; the guest's next opendir gets the
 * same chunk back, and the getdents64 records' padding (which Linux never
 * writes) would carry those host-chosen bytes. This prints every padding byte
 * of a directory read made right after the installation, so two runs must
 * print the same thing.
 */

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

struct record {
    uint64_t ino;
    int64_t off;
    unsigned short reclen;
    unsigned char type;
    char name[];
};

static void handler(int signal) { (void)signal; }

int main(void) {
    for (int i = 0; i < 8; i++) {
        char name[32];
        snprintf(name, sizeof name, "file-%d", i);
        int fd = open(name, O_CREAT | O_WRONLY, 0644);
        if (fd < 0) {
            return 1;
        }
        close(fd);
    }
    /* As iostat does: one directory read (and its free) first. */
    DIR *earlier = opendir(".");
    if (earlier == NULL) {
        return 4;
    }
    while (readdir(earlier) != NULL) {
    }
    closedir(earlier);
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_handler = handler;
    int result = sigaction(SIGALRM, &action, NULL);
    printf("install=%d\n", result == 0 ? 0 : errno);
    DIR *directory = opendir(".");
    if (directory == NULL) {
        return 2;
    }
    struct dirent *first = readdir(directory);
    if (first == NULL) {
        return 3;
    }
    /* glibc's buffer holds the getdents64 records from the first one on. */
    const char *buffer = (const char *)first;
    size_t offset = 0;
    for (int entry = 0; entry < 10; entry++) {
        const struct record *record = (const struct record *)(buffer + offset);
        if (record->reclen == 0) {
            break;
        }
        size_t used = 19 + strlen(record->name) + 1;
        printf("record %d pad:", entry);
        for (size_t i = used; i < record->reclen; i++) {
            printf(" %02x", (unsigned char)buffer[offset + i]);
        }
        printf("\n");
        offset += record->reclen;
    }
    closedir(directory);
    return 0;
}
