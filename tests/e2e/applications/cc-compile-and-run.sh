#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# Compile and link a small C program with the image's cc, then run it: the
# benchmark suite's cc_run scenario, `cc -O2 hello.c -o hello && ./hello`
# with the same program, which prints 14999995. The program is started the
# way a tool that keeps a file mapped starts a child: by a small process
# that holds a read-only MAP_SHARED view of hello.c and starts ./hello with
# posix_spawn, which glibc implements with clone or clone3 and
# CLONE_VM|CLONE_VFORK. KVM refused that child until
# https://github.com/rrnewton/reverie/issues/1108, in every image.
#
# cc runs in a UTF-8 locale (the harness gives every guest LC_ALL=C). Where
# the C library ships an iconv module cache, glibc maps gconv-modules.cache
# MAP_SHARED during the gcc driver's locale setup, and the driver then starts
# cc1, as and the linker with vfork or posix_spawn while it holds that view.
# The pinned hermetic image's glibc ships no such cache, so there the
# helper's view is the one a child starts under. The helper is compiled when
# the cell is prepared, outside Hermit.
set -euo pipefail

case ${1:-} in
    --prepare)
        command -v cc >/dev/null 2>&1 || {
            echo "cc not found" >&2
            exit 1
        }
        : "${E2E_FIXTURE_DIR:?E2E_FIXTURE_DIR must be set}"
        mkdir -p -- "$E2E_FIXTURE_DIR"
        cat >"$E2E_FIXTURE_DIR/spawn-with-view.c" <<'EOF'
/* Hold a read-only MAP_SHARED view of FILE while COMMAND runs, started
 * with posix_spawnp; exit with COMMAND's status. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

int main(int argc, char **argv) {
  if (argc < 3) {
    fprintf(stderr, "usage: %s FILE COMMAND [ARG...]\n", argv[0]);
    return 2;
  }
  int fd = open(argv[1], O_RDONLY | O_CLOEXEC);
  struct stat st;
  if (fd < 0 || fstat(fd, &st) != 0 || st.st_size <= 0) {
    perror(argv[1]);
    return 3;
  }
  const unsigned char *view =
      mmap(NULL, (size_t)st.st_size, PROT_READ, MAP_SHARED, fd, 0);
  if (view == MAP_FAILED) {
    perror("mmap");
    return 4;
  }
  unsigned char first = view[0];
  pid_t child;
  int error = posix_spawnp(&child, argv[2], NULL, NULL, argv + 2, environ);
  if (error != 0) {
    fprintf(stderr, "posix_spawnp %s: %s\n", argv[2], strerror(error));
    return 5;
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 6;
  }
  if (view[0] != first || munmap((void *)view, (size_t)st.st_size) != 0) {
    fprintf(stderr, "the view of %s changed or could not be unmapped\n",
            argv[1]);
    return 7;
  }
  if (!WIFEXITED(status)) {
    fprintf(stderr, "%s: status %#x\n", argv[2], status);
    return 8;
  }
  return WEXITSTATUS(status);
}
EOF
        cc -O2 -Wall -Wextra -Werror "$E2E_FIXTURE_DIR/spawn-with-view.c" \
            -o "$E2E_FIXTURE_DIR/spawn-with-view"
        ;;
    --run)
        : "${E2E_FIXTURE_DIR:?E2E_FIXTURE_DIR must be set}"
        work="${E2E_TMPDIR:-/tmp}/hermit-cc-compile-and-run"
        mkdir -p -- "$work"
        cat >"$work/hello.c" <<'EOF'
#include <stdio.h>
int main(void){long s=0;for(long i=0;i<5000000;i++)s+=i%7;printf("%ld\n",s);return 0;}
EOF
        export LC_ALL=C.UTF-8
        cc -O2 "$work/hello.c" -o "$work/hello"
        "$E2E_FIXTURE_DIR/spawn-with-view" "$work/hello.c" "$work/hello"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
