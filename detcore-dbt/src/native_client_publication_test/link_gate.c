/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

int
unlink(const char *path)
{
    int (*real_unlink)(const char *) = dlsym(RTLD_NEXT, "unlink");
    if (real_unlink == NULL)
        _exit(110);
    int result = real_unlink(path);
    int saved_errno = errno;
    const char *basename = strrchr(path, '/');
    basename = basename == NULL ? path : basename + 1;
    const char *endpoint = getenv("PUBLICATION_GATE");
    if (result == 0 && endpoint != NULL &&
        strcmp(basename, "libreverie_dbt_client.so") == 0) {
        struct sockaddr_un address = { .sun_family = AF_UNIX };
        if (strlen(endpoint) >= sizeof(address.sun_path))
            _exit(111);
        strcpy(address.sun_path, endpoint);
        int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
        if (fd < 0 || connect(fd, (struct sockaddr *)&address, sizeof(address)) != 0 ||
            write(fd, "U", 1) != 1)
            _exit(112);
        char release;
        ssize_t count;
        do {
            count = read(fd, &release, 1);
        } while (count < 0 && errno == EINTR);
        /* EOF also releases the linker during an assertion unwind. */
        if (count < 0)
            _exit(113);
        close(fd);
    }
    errno = saved_errno;
    return result;
}
