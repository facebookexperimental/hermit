/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Both physical verification runs must get their own empty working directory. */
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    char cwd[4096];
    if (getcwd(cwd, sizeof(cwd)) == NULL || strcmp(cwd, "/test") != 0) {
        fprintf(stderr, "expected /test working directory\n");
        return 1;
    }
    int fd = open("physical-run-exclusive", O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (fd < 0) {
        perror("exclusive per-run file");
        return 2;
    }
    static const char contents[] = "private file contents\n";
    if (write(fd, contents, sizeof(contents) - 1) != sizeof(contents) - 1) {
        perror("write per-run file");
        return 3;
    }
    if (close(fd) != 0) {
        perror("close per-run file");
        return 4;
    }
    puts("empty per-run workdir verified");
    return 0;
}
