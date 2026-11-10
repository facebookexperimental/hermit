/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * What a guest sees of System V IPC and POSIX message queues, for the private
 * IPC namespace of a Hermit run (hermit-cli/tests/cli.rs).
 *
 *   private_ipc check NAME KEY  lists /dev/mqueue, prints its mount row,
 *                               creates and removes the queue NAME through
 *                               /dev/mqueue, then tries to create a System V
 *                               shared memory segment, semaphore set and
 *                               message queue with KEY and the queue /NAME
 *                               with mq_open, each exclusively, and removes
 *                               what it created.
 *   private_ipc hold NAME KEY   creates the same objects exclusively, prints
 *                               "created: N", waits for standard input to end,
 *                               then removes them.
 *   private_ipc sees NAME KEY   opens the queue NAME through /dev/mqueue and
 *                               looks up the shared memory segment KEY,
 *                               without creating either.
 *
 * Under Detcore the System V and mq_* calls fail with ENOSYS; under
 * --namespace-only they reach the kernel.
 */

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/msg.h>
#include <sys/sem.h>
#include <sys/shm.h>
#include <unistd.h>

static char vfs_path[300];
static char mq_name[300];
static key_t key;

static const char *outcome(int ok) {
  return ok ? "created" : strerror(errno);
}

static int entries(const char *path) {
  DIR *dir = opendir(path);
  if (dir == NULL) {
    return -1;
  }
  int count = 0;
  struct dirent *entry;
  while ((entry = readdir(dir)) != NULL) {
    if (strcmp(entry->d_name, ".") != 0 && strcmp(entry->d_name, "..") != 0) {
      count++;
    }
  }
  closedir(dir);
  return count;
}

/* The per-mount options and filesystem type of the last row at /dev/mqueue. */
static void mount_row(void) {
  FILE *mountinfo = fopen("/proc/self/mountinfo", "r");
  char line[4096];
  char row[512] = "none";
  while (mountinfo != NULL && fgets(line, sizeof(line), mountinfo) != NULL) {
    char point[1024];
    char options[256];
    char fstype[64];
    char *dash = strstr(line, " - ");
    if (dash == NULL ||
        sscanf(line, "%*s %*s %*s %*s %1023s %255s", point, options) != 2 ||
        sscanf(dash + 3, "%63s", fstype) != 1) {
      continue;
    }
    if (strcmp(point, "/dev/mqueue") == 0) {
      snprintf(row, sizeof(row), "%s %s", options, fstype);
    }
  }
  if (mountinfo != NULL) {
    fclose(mountinfo);
  }
  printf("/dev/mqueue mount: %s\n", row);
}

/* Creates each object exclusively; returns how many it created. */
static int create_all(int verbose, int *shm, int *sem, int *msg, mqd_t *mq,
                      int *vfs) {
  int created = 0;
  errno = 0;
  *vfs = open(vfs_path, O_CREAT | O_EXCL | O_RDWR, 0600);
  created += *vfs >= 0;
  if (verbose) {
    printf("open(%s, O_CREAT | O_EXCL): %s\n", vfs_path, outcome(*vfs >= 0));
  }
  errno = 0;
  *shm = shmget(key, 4096, IPC_CREAT | IPC_EXCL | 0600);
  created += *shm >= 0;
  if (verbose) {
    printf("shmget(key, IPC_CREAT | IPC_EXCL): %s\n", outcome(*shm >= 0));
  }
  errno = 0;
  *sem = semget(key, 1, IPC_CREAT | IPC_EXCL | 0600);
  created += *sem >= 0;
  if (verbose) {
    printf("semget(key, IPC_CREAT | IPC_EXCL): %s\n", outcome(*sem >= 0));
  }
  errno = 0;
  *msg = msgget(key, IPC_CREAT | IPC_EXCL | 0600);
  created += *msg >= 0;
  if (verbose) {
    printf("msgget(key, IPC_CREAT | IPC_EXCL): %s\n", outcome(*msg >= 0));
  }
  /* One one-byte message at most, far below the user's RLIMIT_MSGQUEUE. */
  struct mq_attr small = {.mq_maxmsg = 1, .mq_msgsize = 1};
  errno = 0;
  *mq = mq_open(mq_name, O_CREAT | O_EXCL | O_RDWR, 0600, &small);
  created += *mq != (mqd_t)-1;
  if (verbose) {
    printf("mq_open(%s, O_CREAT | O_EXCL): %s\n", mq_name,
           outcome(*mq != (mqd_t)-1));
  }
  return created;
}

static int remove_all(int shm, int sem, int msg, mqd_t mq, int vfs) {
  int ok = 1;
  if (vfs >= 0) {
    ok &= close(vfs) == 0 && unlink(vfs_path) == 0;
  }
  if (shm >= 0) {
    ok &= shmctl(shm, IPC_RMID, NULL) == 0;
  }
  if (sem >= 0) {
    ok &= semctl(sem, 0, IPC_RMID) == 0;
  }
  if (msg >= 0) {
    ok &= msgctl(msg, IPC_RMID, NULL) == 0;
  }
  if (mq != (mqd_t)-1) {
    ok &= mq_close(mq) == 0 && mq_unlink(mq_name) == 0;
  }
  return ok;
}

static int check(void) {
  printf("/dev/mqueue entries: %d\n", entries("/dev/mqueue"));
  mount_row();
  int shm, sem, msg, vfs;
  mqd_t mq;
  create_all(1, &shm, &sem, &msg, &mq, &vfs);
  printf("removed: %s\n", remove_all(shm, sem, msg, mq, vfs) ? "yes" : "no");
  return 0;
}

static int hold(void) {
  int shm, sem, msg, vfs;
  mqd_t mq;
  printf("created: %d\n", create_all(0, &shm, &sem, &msg, &mq, &vfs));
  char buffer[64];
  while (read(STDIN_FILENO, buffer, sizeof(buffer)) > 0) {
  }
  printf("released: %s\n", remove_all(shm, sem, msg, mq, vfs) ? "yes" : "no");
  return 0;
}

static int sees(void) {
  errno = 0;
  int vfs = open(vfs_path, O_RDONLY);
  printf("open(%s): %s\n", vfs_path, vfs >= 0 ? "opened" : strerror(errno));
  errno = 0;
  int shm = shmget(key, 0, 0);
  printf("shmget(key): %s\n", shm >= 0 ? "found" : strerror(errno));
  return 0;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc != 4) {
    fprintf(stderr, "usage: %s check|hold|sees NAME KEY\n", argv[0]);
    return 2;
  }
  snprintf(vfs_path, sizeof(vfs_path), "/dev/mqueue/%s", argv[2]);
  snprintf(mq_name, sizeof(mq_name), "/%s-mq", argv[2]);
  key = (key_t)strtol(argv[3], NULL, 0);
  if (strcmp(argv[1], "check") == 0) {
    return check();
  }
  if (strcmp(argv[1], "hold") == 0) {
    return hold();
  }
  return sees();
}
