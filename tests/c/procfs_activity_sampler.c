/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// The /proc and /sys reads that iostat -d -x, vmstat -d, pidstat -d (sysstat
// and procps), lsmod (kmod), lsirq (util-linux) and mpstat -I SCPU (sysstat)
// make, made the way those tools make them, for
// command_strict_verify::activity_counter_reads_are_deterministic_under_strict_verify:
// - stdio reads (fopen and fgets: openat, fstat, read, close) of each counter
//   file, sampled before and after a one-second interval, as the tools sample
//   to compute rates. The first interval waits with alarm(2) and pause(2), as
//   sysstat does; the second with clock_nanosleep(2), as procps does.
// - Listings (getdents64) of /sys/devices/system/cpu and /sys/block, with a
//   newfstatat(2) of each CPU and each block device node and an access(2) of
//   each block directory, as iostat enumerates them, and a rewind (lseek) and
//   reread of each device's stat file.
// - uname(2), time(2) and /etc/localtime, which the sysstat tools print in
//   their headers, and the AT_CLKTCK entry of /proc/self/auxv, which procps
//   reads.
// It prints every line it reads, so a difference between the two runs of
// `hermit run --verify` shows up in its output. A file it cannot open prints
// one line naming the error. It exits 1 if the uptime did not advance by at
// least the interval across either wait.

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <time.h>
#include <unistd.h>

// The counter files the tools sample on each interval.
static const char *const sampled[] = {
    "/proc/uptime",       // iostat, pidstat, mpstat
    "/proc/stat",         // iostat, pidstat, mpstat
    "/proc/diskstats",    // vmstat -d
    "/proc/softirqs",     // mpstat -I SCPU
    "/proc/interrupts",   // lsirq, mpstat
    "/proc/self/io",      // pidstat -d, for the process itself
    "/proc/1/io",         // pidstat -d -p 1
    "/proc/1/stat",       // pidstat
    "/proc/1/schedstat",  // pidstat
    "/proc/1/status",     // pidstat
};

// The files the tools read once.
static const char *const once[] = {
    "/proc/cmdline",                  // lsmod
    "/proc/modules",                  // lsmod
    "/proc/sys/kernel/osrelease",     // vmstat
    "/sys/devices/system/cpu/online", // vmstat (get_nprocs)
};

static volatile sig_atomic_t alarmed;

static void on_alarm(int signal_number) {
  (void)signal_number;
  alarmed = 1;
}

// Prints each line of `path` behind the path, then the line count, or one
// line naming the error when the file cannot be opened.
static void dump(const char *path) {
  FILE *file = fopen(path, "r");
  if (file == NULL) {
    printf("%s: absent (%s)\n", path, strerror(errno));
    return;
  }
  char line[4096];
  long lines = 0;
  while (fgets(line, sizeof line, file) != NULL) {
    size_t length = strlen(line);
    printf("%s: %s%s", path, line,
           length > 0 && line[length - 1] == '\n' ? "" : "\n");
    lines++;
  }
  int read_error = ferror(file);
  fclose(file);
  printf("%s: %ld lines%s\n", path, lines, read_error ? ", read error" : "");
}

// The first field of /proc/uptime in hundredths of a second, or -1.
static long uptime_centiseconds(void) {
  FILE *file = fopen("/proc/uptime", "r");
  if (file == NULL) {
    return -1;
  }
  long seconds = 0;
  long hundredths = 0;
  int fields = fscanf(file, "%ld.%ld", &seconds, &hundredths);
  fclose(file);
  return fields == 2 ? seconds * 100 + hundredths : -1;
}

static int sample(const char *label) {
  // The sysstat tools stamp each sample with the local time (time(2), and
  // /etc/localtime through localtime_r).
  time_t now = time(NULL);
  struct tm local;
  char stamp[64] = "";
  if (localtime_r(&now, &local) != NULL) {
    strftime(stamp, sizeof stamp, "%Y-%m-%d %H:%M:%S %Z", &local);
  }
  printf("== sample %s, time %lld, %s\n", label, (long long)now, stamp);
  for (size_t i = 0; i < sizeof sampled / sizeof sampled[0]; i++) {
    dump(sampled[i]);
  }
  return 0;
}

// Fails when the uptime did not advance by at least one second.
static int check_advance(const char *wait, long before, long after) {
  printf("uptime across %s: %ld -> %ld centiseconds\n", wait, before, after);
  if (before < 0 || after < 0) {
    printf("FAIL: /proc/uptime could not be read around %s\n", wait);
    return 1;
  }
  if (after - before < 100) {
    printf("FAIL: the uptime advanced %ld centiseconds across %s, less than "
           "its one-second interval\n",
           after - before, wait);
    return 1;
  }
  return 0;
}

// The CPU directories, as sysstat counts the processors.
static void list_cpus(void) {
  const char *root = "/sys/devices/system/cpu";
  DIR *directory = opendir(root);
  if (directory == NULL) {
    printf("%s: absent (%s)\n", root, strerror(errno));
    return;
  }
  struct dirent *entry;
  while ((entry = readdir(directory)) != NULL) {
    unsigned cpu;
    char rest;
    if (sscanf(entry->d_name, "cpu%u%c", &cpu, &rest) != 1) {
      continue;
    }
    struct stat status;
    if (fstatat(dirfd(directory), entry->d_name, &status, 0) != 0) {
      printf("%s/%s: absent (%s)\n", root, entry->d_name, strerror(errno));
      continue;
    }
    printf("%s/%s: %s\n", root, entry->d_name,
           S_ISDIR(status.st_mode) ? "directory" : "not a directory");
    // mpstat also opens and lists each CPU's directory.
    int cpu_fd = openat(dirfd(directory), entry->d_name,
                        O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    DIR *cpu_directory = cpu_fd >= 0 ? fdopendir(cpu_fd) : NULL;
    if (cpu_directory == NULL) {
      printf("%s/%s: cannot list (%s)\n", root, entry->d_name, strerror(errno));
      if (cpu_fd >= 0) {
        close(cpu_fd);
      }
      continue;
    }
    long entries = 0;
    struct dirent *cpu_entry;
    while ((cpu_entry = readdir(cpu_directory)) != NULL) {
      if (strcmp(cpu_entry->d_name, ".") != 0 &&
          strcmp(cpu_entry->d_name, "..") != 0) {
        entries++;
      }
    }
    closedir(cpu_directory);
    printf("%s/%s: %ld entries\n", root, entry->d_name, entries);
  }
  closedir(directory);
}

// The block devices, as iostat enumerates them: each directory is checked
// with access(2), its stat file is read, rewound and read again, and its
// device node is checked with stat(2).
static void list_block_devices(void) {
  const char *root = "/sys/block";
  DIR *directory = opendir(root);
  if (directory == NULL) {
    printf("%s: absent (%s)\n", root, strerror(errno));
    return;
  }
  struct dirent *entry;
  while ((entry = readdir(directory)) != NULL) {
    if (entry->d_name[0] == '.') {
      continue;
    }
    char path[PATH_MAX];
    snprintf(path, sizeof path, "%s/%s", root, entry->d_name);
    int reachable = access(path, F_OK) == 0;
    printf("%s: %s\n", path, reachable ? "present" : strerror(errno));

    snprintf(path, sizeof path, "%s/%s/stat", root, entry->d_name);
    FILE *file = fopen(path, "r");
    if (file == NULL) {
      printf("%s: absent (%s)\n", path, strerror(errno));
    } else {
      char first[512] = "";
      char again[512] = "";
      if (fgets(first, sizeof first, file) == NULL) {
        first[0] = '\0';
      }
      rewind(file);
      if (fgets(again, sizeof again, file) == NULL) {
        again[0] = '\0';
      }
      fclose(file);
      size_t length = strlen(first);
      printf("%s: %s%s", path, first,
             length > 0 && first[length - 1] == '\n' ? "" : "\n");
      printf("%s: reread %s\n", path,
             strcmp(first, again) == 0 ? "identical" : "different");
    }

    snprintf(path, sizeof path, "/dev/%s", entry->d_name);
    struct stat status;
    if (stat(path, &status) != 0) {
      printf("%s: absent (%s)\n", path, strerror(errno));
    } else {
      printf("%s: %s\n", path,
             S_ISBLK(status.st_mode) ? "block device" : "not a block device");
    }
  }
  closedir(directory);
}

// The clock-tick rate procps reads from the auxiliary vector.
static void auxv_clock_ticks(void) {
  const char *path = "/proc/self/auxv";
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    printf("%s: absent (%s)\n", path, strerror(errno));
    return;
  }
  unsigned long pair[2];
  long ticks = -1;
  while (read(fd, pair, sizeof pair) == (ssize_t)sizeof pair && pair[0] != 0) {
    if (pair[0] == 17 /* AT_CLKTCK */) {
      ticks = (long)pair[1];
    }
  }
  close(fd);
  printf("%s: AT_CLKTCK %ld\n", path, ticks);
}

int main(void) {
  struct utsname system;
  if (uname(&system) == 0) {
    printf("uname: %s %s %s %s\n", system.sysname, system.nodename,
           system.release, system.machine);
  }
  for (size_t i = 0; i < sizeof once / sizeof once[0]; i++) {
    dump(once[i]);
  }
  // kmod probes whether the kernel decompresses modules itself.
  int probe = open("/sys/module/compression", O_RDONLY | O_CLOEXEC);
  printf("/sys/module/compression: %s\n",
         probe >= 0 ? "present" : strerror(errno));
  if (probe >= 0) {
    close(probe);
  }
  auxv_clock_ticks();
  list_cpus();
  list_block_devices();

  int failures = sample("1");

  // sysstat waits for its interval with alarm(2) and pause(2).
  struct sigaction action;
  memset(&action, 0, sizeof action);
  action.sa_handler = on_alarm;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL) != 0) {
    perror("sigaction");
    return 2;
  }
  long before = uptime_centiseconds();
  alarm(1);
  while (!alarmed) {
    pause();
  }
  long after = uptime_centiseconds();
  failures += check_advance("alarm and pause", before, after);
  failures += sample("2");

  // procps waits for its interval with clock_nanosleep(2).
  struct timespec interval = {.tv_sec = 1, .tv_nsec = 0};
  before = uptime_centiseconds();
  while (clock_nanosleep(CLOCK_REALTIME, 0, &interval, &interval) == EINTR) {
  }
  after = uptime_centiseconds();
  failures += check_advance("clock_nanosleep", before, after);
  dump("/proc/diskstats");

  return failures == 0 ? 0 : 1;
}
