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
 * setpgid and setsid, in the cases Linux distinguishes, including a process
 * already reaped (automatically, with SIGCHLD ignored) and the namespace's
 * init, pid 1, whose group and session are outside too. Ids are printed
 * relative to the process they should equal, so the output does not depend
 * on how a backend numbers processes.
 *
 * Under Hermit every backend gives the same answers, from Detcore's model
 * (slice B3a-4c of https://github.com/rrnewton/hermit/issues/4266).
 *
 * usage: process_groups [reaped [--host]]
 *   --host: natively. A child reaped automatically can still be found for a
 *   moment after wait reports no child, so the guest waits (up to 2 s) for
 *   getpgid to stop finding it before it checks. */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
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

/* A group leader that its parent reaps automatically: once it is gone, it
 * has no group or session, and its group, which had no other member, no
 * longer exists. Run as `process_groups reaped`. */
static int reaped_group_leader(int host) {
  setvbuf(stdout, NULL, _IONBF, 0);
  long r;
  must(signal(SIGCHLD, SIG_IGN) != SIG_ERR, "signal");
  int release[2];
  must(pipe(release) == 0, "pipe");
  pid_t e = fork();
  must(e >= 0, "fork");
  if (e == 0) {
    char byte;
    close(release[1]);
    (void)!read(release[0], &byte, 1);
    _exit(0);
  }
  close(release[0]);
  r = setpgid(e, 0);
  printf("setpgid(e, 0)=%s\n", outcome(r));
  close(release[1]);
  /* With SIGCHLD ignored, a wait for the child returns ECHILD once it is
   * gone. */
  r = waitpid(e, NULL, 0);
  printf("waitpid(e)=%s\n", r == e ? "e" : outcome(r));
  /* Linux reaps such a child asynchronously to its parent's wait: exit marks
   * it EXIT_DEAD, which wait skips (so it reports ECHILD), and only then
   * release_task detaches its pid, group and session (Linux v7.1.3
   * kernel/exit.c:766, :777, :1465, :141-147). So natively getpgid can still
   * find it for a moment; wait for that to end. Under Hermit the first
   * answer must already be ESRCH. */
  for (int tries = 0; host && tries < 2000 && getpgid(e) >= 0; tries++) {
    usleep(1000);
  }
  long g = getpgid(e);
  printf("getpgid(reaped e)=%s ", outcome(g));
  long s = getsid(e);
  printf("getsid(reaped e)=%s\n", outcome(s));
  must(pipe(release) == 0, "pipe");
  pid_t f = fork();
  must(f >= 0, "fork");
  if (f == 0) {
    char byte;
    close(release[1]);
    (void)!read(release[0], &byte, 1);
    _exit(0);
  }
  close(release[0]);
  r = setpgid(f, e);
  printf("setpgid(f, reaped e's group)=%s\n", outcome(r));
  close(release[1]);
  r = waitpid(f, NULL, 0);
  printf("waitpid(f)=%s\n", r == f ? "f" : outcome(r));
  return 0;
}

int main(int argc, char** argv) {
  if (argc >= 2 && strcmp(argv[1], "reaped") == 0)
    return reaped_group_leader(argc == 3 && strcmp(argv[2], "--host") == 0);
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

  /* The kernel takes a zero group as the pid before checking its sign. */
  r = setpgid(-5, 0);
  printf("setpgid(-5, 0)=%s\n", outcome(r));
  /* The namespace's init: its group and session are outside the namespace. */
  printf(
      "init pgrp=%s sid=%s\n",
      named(getpgid(1), self, 0, 0),
      named(getsid(1), self, 0, 0));

  return 0;
}
