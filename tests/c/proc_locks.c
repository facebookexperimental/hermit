#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>

#ifndef LOCK_API
#error "LOCK_API must select fcntl (1), lockf (2), or OFD fcntl (3)"
#endif

#define SNAPSHOT_CAP (256 * 1024)

static int take_lock(int fd) {
#if LOCK_API == 1
  struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  return fcntl(fd, F_SETLK, &lock);
#elif LOCK_API == 2
  return lockf(fd, F_TLOCK, 0);
#elif LOCK_API == 3
  struct flock lock = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
  return fcntl(fd, F_OFD_SETLK, &lock);
#else
#error "unsupported LOCK_API"
#endif
}

static ssize_t read_all(int fd, char *buffer) {
  size_t used = 0;
  while (used < SNAPSHOT_CAP - 1) {
    ssize_t count = read(fd, buffer + used, SNAPSHOT_CAP - 1 - used);
    if (count < 0 && errno == EINTR) {
      continue;
    }
    if (count <= 0) {
      if (count == 0) {
        buffer[used] = '\0';
        return (ssize_t)used;
      }
      return -1;
    }
    used += (size_t)count;
  }
  errno = EFBIG;
  return -1;
}

static int check_virtual_graph(const char *snapshot) {
  char *copy = strdup(snapshot);
  if (copy == NULL) {
    return 1;
  }
  char first_object[64] = {0};
  int rows = 0;
  int distinct_object = 0;
  char *save_line = NULL;
  for (char *line = strtok_r(copy, "\n", &save_line); line != NULL;
       line = strtok_r(NULL, "\n", &save_line)) {
    char *fields[9] = {0};
    int count = 0;
    char *save_field = NULL;
    for (char *field = strtok_r(line, " \t", &save_field);
         field != NULL && count < 9;
         field = strtok_r(NULL, " \t", &save_field)) {
      fields[count++] = field;
    }
    int waiter = count > 1 && strcmp(fields[1], "->") == 0;
    int object_index = waiter ? 6 : 5;
    if (count != (waiter ? 9 : 8) || fields[object_index] == NULL) {
      free(copy);
      return 1;
    }
    if (rows == 0) {
      snprintf(first_object, sizeof(first_object), "%s", fields[object_index]);
    } else if (strcmp(first_object, fields[object_index]) != 0) {
      distinct_object = 1;
    }
    rows++;
  }
  free(copy);
  return rows < 2 || !distinct_object;
}

static int open_and_read(const char *path, char *snapshot) {
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    return 1;
  }
  ssize_t count = read_all(fd, snapshot);
  close(fd);
  return count <= 0 || check_virtual_graph(snapshot);
}

int main(void) {
  char first_path[96];
  char second_path[96];
  snprintf(first_path, sizeof(first_path), "/tmp/hermit-proc-locks-%d-a", LOCK_API);
  snprintf(second_path, sizeof(second_path), "/tmp/hermit-proc-locks-%d-b", LOCK_API);
  int first = open(first_path, O_CREAT | O_RDWR | O_TRUNC, 0600);
  int second = open(second_path, O_CREAT | O_RDWR | O_TRUNC, 0600);
  if (first < 0 || second < 0 || take_lock(first) < 0 || take_lock(second) < 0) {
    return 1;
  }

  char *direct = calloc(SNAPSHOT_CAP, 1);
  char *alias = calloc(SNAPSHOT_CAP, 1);
  char *relative = calloc(SNAPSHOT_CAP, 1);
  if (direct == NULL || alias == NULL || relative == NULL) {
    return 1;
  }
  if (open_and_read("/proc/locks", direct) != 0 ||
      open_and_read("/proc/self/../locks", alias) != 0 ||
      strcmp(direct, alias) != 0) {
    return 1;
  }

  int proc = open("/proc", O_RDONLY | O_DIRECTORY);
  int locks = proc < 0 ? -1 : openat(proc, "locks", O_RDONLY);
  if (locks < 0 || read_all(locks, relative) <= 0 ||
      check_virtual_graph(relative) != 0 || strcmp(direct, relative) != 0) {
    return 1;
  }

  char prefix[8];
  if (lseek(locks, 0, SEEK_SET) != 0 || read(locks, prefix, sizeof(prefix)) !=
                                             (ssize_t)sizeof(prefix)) {
    return 1;
  }
  int duplicate = dup(locks);
  if (duplicate < 0 || lseek(duplicate, 0, SEEK_CUR) != (off_t)sizeof(prefix)) {
    return 1;
  }
  memset(relative, 0, SNAPSHOT_CAP);
  if (lseek(duplicate, 0, SEEK_SET) != 0 || read_all(locks, relative) <= 0 ||
      strcmp(direct, relative) != 0) {
    return 1;
  }

  char positioned[32] = {0};
  if (pread(locks, positioned, sizeof(positioned), 0) !=
          (ssize_t)sizeof(positioned) ||
      memcmp(positioned, direct, sizeof(positioned)) != 0) {
    return 1;
  }

  struct iovec vector = {.iov_base = positioned, .iov_len = sizeof(positioned)};
  errno = 0;
  if (readv(locks, &vector, 1) != -1 || errno != ENOSYS) {
    return 1;
  }
  errno = 0;
  if (preadv(locks, &vector, 1, 0) != -1 || errno != ENOSYS) {
    return 1;
  }

  puts("proc-locks-virtual-graph-and-aliases-ok");
  return 0;
}
