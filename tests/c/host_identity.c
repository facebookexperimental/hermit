/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * host_identity — backend-parity contract for the virtualized HOST IDENTITY:
 * uname(2), gethostname(2), sysinfo(2), the visible CPU count, and the
 * cpufreq files.
 *
 * WHY THIS EMITS VALUES RATHER THAN A VERDICT
 * -------------------------------------------
 * The neighbouring uname_identity fixture prints `uname ok=%d`, and
 * cpu_virtualization prints `cpu-virtualization-ok`. Backend parity is decided
 * by comparing stdout, so a constant success token makes every passing backend
 * look identical NO MATTER WHAT IT OBSERVED, and a divergence has to be
 * re-derived by hand from stderr. That is not hypothetical here: this repo
 * already records that "DBI forwards the host uname(2) nodename instead of
 * pinning it ... DBI leaks the real host name" — a leak a boolean cannot show.
 *
 * So every observation below is PRINTED, and the checks are additional rather
 * than a replacement. A backend that leaks a real host value now differs in the
 * compared stream, naming the offending field directly.
 *
 * ASSERTED vs RECORDED
 * --------------------
 * Asserted: the fields measured to be genuinely virtualized and stable.
 * Recorded but NOT asserted: fields that are stable yet still carry host
 * topology. `sysconf(_SC_NPROCESSORS_ONLN)`, /sys/devices/system/cpu/online and
 * /proc/cpuinfo all report the REAL host CPU count inside the container (316 on
 * the development host) while sched_getaffinity correctly reports 1. That is an
 * open leak, not a property this fixture should bless by asserting the leaked
 * value — but it is printed so the leak is visible and so any change to it
 * shows up as a parity diff instead of passing silently.
 *
 * CPUFREQ
 * -------
 * Asserted: the one virtual CPU runs at 1000000 kHz (its minimum, maximum and
 * current frequency) and cannot boost (boost reads 0), whatever the host's
 * values, for every spelling: absolute, through cpuN and policyN, and relative
 * to a descriptor for /sys/devices/system/cpu, however that descriptor was
 * opened: by its own name, through a symbolic link, as cpu0/.., as "." after
 * chdir, or received over a socket. A file the host does not have stays
 * absent (ENOENT), as on every backend, and prints "absent".
 *
 * OWN PROCFS DIRECTORIES
 * ----------------------
 * Asserted: readlink of the caller's own /proc/<pid>, /proc/<pid>/task and
 * /proc/<pid>/task/<tid> fails with EINVAL, as Linux fails it for a
 * directory.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/syscall.h>

#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/sysinfo.h>
#include <sys/utsname.h>
#include <unistd.h>

/* Hermit's documented virtual container identity. */
#define VIRT_NODENAME "hermetic-container.local"
#define VIRT_RELEASE "5.2.0"
#define VIRT_SYSNAME "Linux"
#define VIRT_MACHINE "x86_64"
#define VIRT_TOTALRAM 1000000000UL
#define VIRT_PROCS 1U
/* sysinfo(2) rounds positive fractional uptime up. This fixture expects its
 * initial observation in the first elapsed second after the configured
 * 120-second boot offset, so the exact integer uptime must be 121. */
#define VIRT_UPTIME 121L
#define VIRT_AFFINITY 1
#define VIRT_CPU_KHZ "1000000"
#define VIRT_BOOST "0"

static int fail(const char* field, const char* got) {
  fprintf(stderr, "host_identity: %s leaked or diverged: %s\n", field, got);
  return 1;
}

/* Reads the one-line sysfs file `path`, relative to `dirfd` (a descriptor
 * opened as `how` says), prints it, and checks it is `expected` or absent. */
static int cpufreq_as(const char* how,
                      int dirfd,
                      const char* path,
                      const char* expected) {
  int fd = openat(dirfd, path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    if (errno == ENOENT) {
      printf("cpufreq%s.%s=absent\n", how, path);
      return 0;
    }
    printf("cpufreq%s.%s=error %d\n", how, path, errno);
    return fail(path, "open failed");
  }
  char value[64] = {0};
  ssize_t length = read(fd, value, sizeof(value) - 1);
  close(fd);
  if (length < 0) {
    printf("cpufreq%s.%s=error %d\n", how, path, errno);
    return fail(path, "read failed");
  }
  value[strcspn(value, "\n")] = '\0';
  printf("cpufreq%s.%s=%s\n", how, path, value);
  return strcmp(value, expected) == 0 ? 0 : fail(path, value);
}

static int cpufreq(int dirfd, const char* path, const char* expected) {
  return cpufreq_as("", dirfd, path, expected);
}

/* The current frequency and boost, relative to `dirfd`, a descriptor for
 * /sys/devices/system/cpu opened as `how` says. */
static int cpu_directory(const char* how, int dirfd) {
  if (dirfd < 0) {
    printf("cpufreq%s=error %d\n", how, errno);
    return fail(how, "no descriptor for /sys/devices/system/cpu");
  }
  int bad =
      cpufreq_as(how, dirfd, "cpu0/cpufreq/scaling_cur_freq", VIRT_CPU_KHZ);
  bad |= cpufreq_as(how, dirfd, "cpufreq/boost", VIRT_BOOST);
  close(dirfd);
  return bad;
}

/* A copy of `fd` received through SCM_RIGHTS, or -1. Closes `fd`. */
static int received_copy(int fd) {
  int pair[2];
  if (fd < 0 || socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, pair) != 0) {
    return -1;
  }
  char byte = 'x';
  struct iovec iov = {&byte, 1};
  union {
    char buffer[CMSG_SPACE(sizeof(int))];
    struct cmsghdr align;
  } control;
  struct msghdr message = {0};
  message.msg_iov = &iov;
  message.msg_iovlen = 1;
  message.msg_control = control.buffer;
  message.msg_controllen = sizeof control.buffer;
  struct cmsghdr* header = CMSG_FIRSTHDR(&message);
  header->cmsg_level = SOL_SOCKET;
  header->cmsg_type = SCM_RIGHTS;
  header->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(header), &fd, sizeof(int));
  int copy = -1;
  if (sendmsg(pair[0], &message, 0) == 1 &&
      recvmsg(pair[1], &message, MSG_CMSG_CLOEXEC) == 1) {
    memcpy(&copy, CMSG_DATA(CMSG_FIRSTHDR(&message)), sizeof(int));
  }
  close(pair[0]);
  close(pair[1]);
  close(fd);
  return copy;
}

/* readlink of `path`, one of the caller's own procfs directories, fails
 * with EINVAL. */
static int own_directory_is_no_link(const char* what, const char* path) {
  char target[64];
  errno = 0;
  ssize_t length = readlink(path, target, sizeof(target));
  int error = errno;
  printf("readlink %s=%s\n", what, length >= 0 ? "a link" : strerror(error));
  return length < 0 && error == EINVAL ? 0 : fail(what, "not EINVAL");
}

int main(void) {
  int bad = 0;

  struct utsname u;
  if (uname(&u) != 0) {
    return fail("uname", "syscall failed");
  }
  printf("uname.sysname=%s\n", u.sysname);
  printf("uname.nodename=%s\n", u.nodename);
  printf("uname.release=%s\n", u.release);
  printf("uname.machine=%s\n", u.machine);
  if (strcmp(u.sysname, VIRT_SYSNAME) != 0)
    bad |= fail("uname.sysname", u.sysname);
  if (strcmp(u.nodename, VIRT_NODENAME) != 0)
    bad |= fail("uname.nodename", u.nodename);
  if (strcmp(u.release, VIRT_RELEASE) != 0)
    bad |= fail("uname.release", u.release);
  if (strcmp(u.machine, VIRT_MACHINE) != 0)
    bad |= fail("uname.machine", u.machine);

  char host[256] = {0};
  if (gethostname(host, sizeof(host) - 1) != 0) {
    return fail("gethostname", "syscall failed");
  }
  printf("gethostname=%s\n", host);
  if (strcmp(host, VIRT_NODENAME) != 0)
    bad |= fail("gethostname", host);

  struct sysinfo si;
  if (sysinfo(&si) != 0) {
    return fail("sysinfo", "syscall failed");
  }
  printf("sysinfo.totalram=%lu\n", (unsigned long)si.totalram);
  printf("sysinfo.procs=%u\n", si.procs);
  printf("sysinfo.uptime=%ld\n", (long)si.uptime);
  if ((unsigned long)si.totalram != VIRT_TOTALRAM)
    bad |= fail("sysinfo.totalram", "host memory size");
  if (si.procs != VIRT_PROCS)
    bad |= fail("sysinfo.procs", "host process count");
  if ((long)si.uptime != VIRT_UPTIME)
    bad |= fail("sysinfo.uptime", "host uptime");

  cpu_set_t set;
  CPU_ZERO(&set);
  if (sched_getaffinity(0, sizeof(set), &set) != 0) {
    return fail("sched_getaffinity", "syscall failed");
  }
  printf("affinity_count=%d\n", CPU_COUNT(&set));
  if (CPU_COUNT(&set) != VIRT_AFFINITY)
    bad |= fail("affinity_count", "host CPU mask");

  /* RECORDED, NOT ASSERTED: still leaks host topology (see header). */
  printf("sysconf_nprocs_onln=%ld\n", sysconf(_SC_NPROCESSORS_ONLN));

  int cpu = open("/sys/devices/system/cpu", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (cpu < 0) {
    return fail("/sys/devices/system/cpu", "open failed");
  }
  bad |= cpufreq(cpu, "cpu0/cpufreq/cpuinfo_max_freq", VIRT_CPU_KHZ);
  bad |= cpufreq(cpu, "cpu0/cpufreq/cpuinfo_min_freq", VIRT_CPU_KHZ);
  bad |= cpufreq(cpu, "cpu0/cpufreq/scaling_cur_freq", VIRT_CPU_KHZ);
  bad |= cpufreq(cpu, "cpufreq/boost", VIRT_BOOST);
  bad |= cpufreq(cpu, "cpufreq/policy0/boost", VIRT_BOOST);
  bad |= cpufreq(cpu, "cpu0/cpufreq/boost", VIRT_BOOST);
  close(cpu);
  bad |= cpufreq(AT_FDCWD,
                 "/sys/devices/system/cpu/cpufreq/policy0/scaling_max_freq",
                 VIRT_CPU_KHZ);
  bad |= cpufreq(AT_FDCWD, "/sys/devices/system/cpu/cpufreq/boost", VIRT_BOOST);

  /* The same directory by other spellings, which only the object names. */
  char link[64];
  snprintf(link, sizeof(link), "/tmp/host-identity-cpu-%d", (int)getpid());
  if (symlink("/sys/devices/system/cpu", link) != 0) {
    return fail("symlink", strerror(errno));
  }
  bad |= cpu_directory("[symlink]",
                       open(link, O_RDONLY | O_DIRECTORY | O_CLOEXEC));
  unlink(link);
  bad |= cpu_directory("[cpu0/..]", open("/sys/devices/system/cpu/cpu0/..",
                                         O_RDONLY | O_DIRECTORY | O_CLOEXEC));
  int cwd = open(".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (cwd < 0 || chdir("/sys/devices/system/cpu") != 0) {
    return fail("chdir", strerror(errno));
  }
  bad |=
      cpu_directory("[chdir .]", open(".", O_RDONLY | O_DIRECTORY | O_CLOEXEC));
  if (fchdir(cwd) != 0) {
    return fail("fchdir", strerror(errno));
  }
  close(cwd);
  bad |= cpu_directory("[received]",
                       received_copy(open("/sys/devices/system/cpu",
                                          O_RDONLY | O_DIRECTORY | O_CLOEXEC)));

  char own[64];
  int pid = (int)getpid();
  snprintf(own, sizeof(own), "/proc/%d", pid);
  bad |= own_directory_is_no_link("/proc/<pid>", own);
  snprintf(own, sizeof(own), "/proc/%d/task", pid);
  bad |= own_directory_is_no_link("/proc/<pid>/task", own);
  snprintf(own, sizeof(own), "/proc/%d/task/%ld", pid,
           (long)syscall(SYS_gettid));
  bad |= own_directory_is_no_link("/proc/<pid>/task/<tid>", own);

  return bad ? 1 : 0;
}
