/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Process groups and sessions, as Linux answers them to a process that
 * starts in a new PID namespace with the group and session of its parent
 * outside it (as under `unshare --pid --fork`): getpgrp, getpgid, getsid,
 * setpgid and setsid, in the cases Linux distinguishes. Ids are printed
 * relative to the process they should equal, so the output does not depend
 * on how a backend numbers processes.
 *
 * Under Hermit every backend gives the same answers, from Detcore's model
 * (slice B3a-4c of https://github.com/rrnewton/hermit/issues/4266). */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static const char* outcome(long result) {
  return result < 0 ? strerrorname_np(errno) : "ok";
}

/* `id` as the name of the process it equals, or as a number. */
static const char* named(pid_t id, pid_t self, pid_t a, pid_t b) {
  static char buffer[4][32];
  static int next;
  char* text = buffer[next++ % 4];
  if (id == 0)
    return "0";
  if (id == self)
    return "self";
  if (id == a)
    return "a";
  if (id == b)
    return "b";
  snprintf(text, sizeof(buffer[0]), "%d", (int)id);
  return text;
}

static void must(int ok, const char* what) {
  if (!ok) {
    perror(what);
    exit(2);
  }
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  pid_t self = getpid();
  printf(
      "root pgrp=%s sid=%s\n",
      named(getpgrp(), self, 0, 0),
      named(getsid(0), self, 0, 0));

  int ready[2], hold[2];
  must(pipe(ready) == 0 && pipe(hold) == 0, "pipe");
  pid_t a = fork();
  must(a >= 0, "fork");
  if (a == 0) {
    char c;
    must(read(ready[0], &c, 1) == 1, "read");
    pid_t me = getpid();
    printf(
        "a pgrp=%s sid=%s\n",
        named(getpgrp(), me, me, 0),
        named(getsid(0), me, me, 0));
    _exit(0);
  }
  long r = setpgid(a, 0);
  printf("setpgid(a, 0)=%s pgid(a)=%s\n", outcome(r), named(getpgid(a), self, a, 0));

  pid_t b = fork();
  must(b >= 0, "fork");
  if (b == 0) {
    char c;
    close(hold[1]);
    (void)!read(hold[0], &c, 1);
    _exit(0);
  }
  close(hold[0]);
  r = setpgid(b, a);
  printf("setpgid(b, a)=%s pgid(b)=%s\n", outcome(r), named(getpgid(b), self, a, b));
  r = setpgid(b, 99);
  printf("setpgid(b, no group)=%s\n", outcome(r));
  r = setpgid(0, -1);
  printf("setpgid(0, -1)=%s\n", outcome(r));
  r = getpgid(12345);
  printf("getpgid(no process)=%s\n", outcome(r));
  r = setsid();
  printf(
      "setsid=%s %s pgrp=%s sid=%s\n",
      r < 0 ? "-1" : named((pid_t)r, self, a, b),
      outcome(r),
      named(getpgrp(), self, a, b),
      named(getsid(0), self, a, b));
  r = setsid();
  printf("setsid as leader=%s\n", outcome(r));
  r = setpgid(0, 0);
  printf("setpgid(0, 0) as session leader=%s\n", outcome(r));
  r = setpgid(b, b);
  printf("setpgid(b, b) in another session=%s\n", outcome(r));

  must(write(ready[1], "x", 1) == 1, "write");
  must(waitpid(a, NULL, 0) == a, "waitpid");
  close(hold[1]);
  must(waitpid(b, NULL, 0) == b, "waitpid");

  int executed[2];
  must(pipe2(executed, O_CLOEXEC) == 0, "pipe2");
  pid_t c = fork();
  must(c >= 0, "fork");
  if (c == 0) {
    execl("/bin/true", "true", (char*)NULL);
    _exit(1);
  }
  close(executed[1]);
  /* The close-on-exec pipe reads end of file once the child has exec'd. */
  char byte;
  must(read(executed[0], &byte, 1) == 0, "read");
  r = setpgid(c, 0);
  printf("setpgid(child that exec'd)=%s\n", outcome(r));
  must(waitpid(c, NULL, 0) == c, "waitpid");
  return 0;
}
