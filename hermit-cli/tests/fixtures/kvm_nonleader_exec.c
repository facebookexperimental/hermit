/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved. Licensed under the BSD-style license in LICENSE. */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static const char* program;
static const char* mode;
static const char* malformed;
static const char* missing;

static void require(int condition) {
  if (!condition)
    _exit(90);
}

static void emit(const char* text) {
  size_t length = strlen(text);
  require(write(STDOUT_FILENO, text, length) == (ssize_t)length);
}

static int replace(const char* path, int at) {
  char* const args[] = {(char*)path, "replacement", NULL};
  char* const environment[] = {"LC_ALL=C", NULL};
  if (at)
    return (int)syscall(SYS_execveat, AT_FDCWD, path, args, environment, 0);
  return execve(path, args, environment);
}

static void* worker(void* unused) {
  (void)unused;
  require(syscall(SYS_gettid) != getpid());
  if (strcmp(mode, "errors") == 0) {
    for (int at = 0; at != 2; ++at) {
      require(replace(missing, at) == -1 && errno == ENOENT);
      require(replace(malformed, at) == -1 && errno == ENOEXEC);
    }
    emit("failed-exec-errors-preserved\n");
    return NULL;
  }
  require(replace(program, strcmp(mode, "execveat") == 0) == -1);
  // Only explicit compatibility mode may reach this backend refusal errno.
  require(errno == ENOSYS);
  emit("unsupported-returned-enosys\n");
  return NULL;
}

int main(int argc, char** argv) {
  require(argc >= 2);
  if (strcmp(argv[1], "replacement") == 0) {
    require(syscall(SYS_gettid) == getpid());
    emit("replacement-ran\n");
    return 0;
  }
  program = argv[0];
  mode = argv[1];
  if (strcmp(mode, "leader") == 0) {
    replace(program, 0);
    _exit(91);
  }
  if (strcmp(mode, "errors") == 0) {
    require(argc == 4);
    malformed = argv[2];
    missing = argv[3];
  } else {
    require(argc == 2);
    require(strcmp(mode, "execve") == 0 || strcmp(mode, "execveat") == 0);
  }
  pthread_t thread;
  require(pthread_create(&thread, NULL, worker, NULL) == 0);
  require(pthread_join(thread, NULL) == 0);
  return 0;
}
