/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/procfs_determinism.rs: a /proc name outside the
 * run's --proc level, reached relative to a descriptor for /proc that was
 * itself opened through a descriptor link (/proc/self/fd/N, /dev/fd/N). Linux
 * follows the link to /proc, so the name is /proc/kallsyms, which
 * --proc=minimal omits: the open fails with ENOENT, as the open of
 * /proc/kallsyms does. The opens are made by the process, then by a thread of
 * a process created with CLONE_FILES and without CLONE_THREAD, which shares
 * its creator's descriptor table and leads its own thread group, so its
 * thread reads that table through /proc/self/fd/N. Prints one line per
 * open and exits 0 only if every open failed with ENOENT. Natively
 * /proc/kallsyms exists, so it exits 1.
 *
 * With --control it instead opens "kallsyms" relative to a directory outside
 * /proc that holds an ordinary file of that name, which is inside every
 * level, prints one line, and exits 0 only if the open succeeded. */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int proc = -1;

/* Opens "kallsyms" below each descriptor link of `proc`; returns how many
 * opens did not fail with ENOENT, or -1 if a link could not be opened. */
static int alias_opens(const char* who) {
  int failures = 0;
  const char* formats[] = {"/proc/self/fd/%d", "/dev/fd/%d"};
  for (size_t i = 0; i < sizeof(formats) / sizeof(formats[0]); i++) {
    char spelling[64];
    snprintf(spelling, sizeof(spelling), formats[i], proc);
    int directory = open(spelling, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (directory < 0) {
      fprintf(stderr, "%sopen(%s): %s\n", who, spelling, strerror(errno));
      return -1;
    }
    int fd = openat(directory, "kallsyms", O_RDONLY | O_CLOEXEC);
    int error = fd < 0 ? errno : 0;
    printf("%sopenat(%s, \"kallsyms\"): %s\n", who, spelling, fd < 0 ? strerror(error) : "opened");
    fflush(stdout);
    failures += !(fd < 0 && error == ENOENT);
    if (fd >= 0) {
      close(fd);
    }
    close(directory);
  }
  return failures;
}

static void* alias_thread(void* result) {
  *(int*)result = alias_opens("thread of a CLONE_FILES process: ");
  return NULL;
}

/* A fork's child creates the CLONE_FILES process, whose thread makes the
 * opens. Returns the number of failed checks. */
static int from_a_clone_files_process(void) {
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    long process = syscall(SYS_clone, CLONE_FILES | SIGCHLD, 0, 0, 0, 0);
    if (process == 0) {
      int result = -1;
      pthread_t thread;
      if (pthread_create(&thread, NULL, alias_thread, &result) != 0 ||
          pthread_join(thread, NULL) != 0) {
        _exit(2);
      }
      _exit(result == 0 ? 0 : 1);
    }
    int status = 0;
    if (process < 0 || waitpid((pid_t)process, &status, 0) != (pid_t)process) {
      fprintf(stderr, "clone(CLONE_FILES): %s\n", strerror(errno));
      _exit(2);
    }
    _exit(WIFEXITED(status) ? WEXITSTATUS(status) : 2);
  }
  int status = 0;
  if (child < 0 || waitpid(child, &status, 0) != child) {
    fprintf(stderr, "fork: %s\n", strerror(errno));
    return 1;
  }
  return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 1;
}

static const char* program;

static void* exec_thread(void* unused) {
  (void)unused;
  char* const arguments[] = {(char*)program, "--after-exec", NULL};
  execv(program, arguments);
  fprintf(stderr, "exec from a thread: %s\n", strerror(errno));
  _exit(2);
}

static void* after_exec_thread(void* result) {
  *(int*)result = alias_opens("thread after a non-leader exec: ");
  return NULL;
}

/* A fork's child execs from a thread that does not lead it; the new image's
 * thread makes the opens. Returns the number of failed checks. */
static int after_a_thread_exec(void) {
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, exec_thread, NULL) == 0) {
      pthread_join(thread, NULL);
    }
    _exit(2);
  }
  int status = 0;
  if (child < 0 || waitpid(child, &status, 0) != child) {
    fprintf(stderr, "fork: %s\n", strerror(errno));
    return 1;
  }
  return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 1;
}

static int control(void) {
  char directory[] = "/tmp/proc-alias-level-XXXXXX";
  if (mkdtemp(directory) == NULL) {
    fprintf(stderr, "mkdtemp: %s\n", strerror(errno));
    return 2;
  }
  char file[64];
  snprintf(file, sizeof(file), "%s/kallsyms", directory);
  int created = open(file, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0644);
  int fd = -1;
  int error = 0;
  if (created >= 0) {
    close(created);
    int base = open(directory, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    fd = base < 0 ? -1 : openat(base, "kallsyms", O_RDONLY | O_CLOEXEC);
    error = fd < 0 ? errno : 0;
    if (base >= 0) {
      close(base);
    }
  } else {
    error = errno;
  }
  printf("openat(<directory>, \"kallsyms\"): %s\n", fd < 0 ? strerror(error) : "opened");
  if (fd >= 0) {
    close(fd);
  }
  unlink(file);
  rmdir(directory);
  return fd >= 0 ? 0 : 1;
}

int main(int argc, char** argv) {
  program = argv[0];
  if (argc == 2 && strcmp(argv[1], "--control") == 0) {
    return control();
  }
  proc = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (proc < 0) {
    fprintf(stderr, "open(/proc): %s\n", strerror(errno));
    return 2;
  }
  if (argc == 2 && strcmp(argv[1], "--after-exec") == 0) {
    int result = -1;
    pthread_t thread;
    if (pthread_create(&thread, NULL, after_exec_thread, &result) != 0 ||
        pthread_join(thread, NULL) != 0) {
      return 2;
    }
    return result == 0 ? 0 : 1;
  }
  int failures = alias_opens("");
  if (failures < 0) {
    return 2;
  }
  failures += from_a_clone_files_process();
  failures += after_a_thread_exec();
  close(proc);
  return failures == 0 ? 0 : 1;
}
