/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Prints what fsync(2) on the guest's stderr returns, which depends on the
 * kind of file behind it: 0 for a regular file, EINVAL for a pipe, as an
 * abort path that flushes stderr sees it.
 *
 * usage: stderr_fsync
 */

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    int result = fsync(STDERR_FILENO);
    printf("fsync(stderr) = %s\n", result == 0 ? "0" : strerror(errno));
    return 0;
}
