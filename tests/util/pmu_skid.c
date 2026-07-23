/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <cpuid.h>
#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <inttypes.h>
#include <linux/perf_event.h>
#include <sched.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define DEFAULT_ITERATIONS 200
#define DEFAULT_PERIOD 1000000
#define MAX_ITERATIONS 1000000
#define PERF_SIGNAL SIGUSR1

struct options {
  size_t iterations;
  uint64_t period;
  int cpu;
};

struct cpu_info {
  char vendor[13];
  char brand[49];
  unsigned family;
  unsigned model;
  unsigned stepping;
  bool precise_ip;
  uint64_t rcb_event;
};

static void usage(const char *program) {
  fprintf(stderr,
          "Usage: %s [--iterations N] [--period RCB] [--cpu CPU]\n"
          "\n"
          "Measure retired-conditional-branch PMU overflow skid.\n"
          "Defaults: --iterations %d --period %d --cpu current\n",
          program, DEFAULT_ITERATIONS, DEFAULT_PERIOD);
}

static bool parse_u64(const char *value, uint64_t *result) {
  char *end = NULL;
  errno = 0;
  unsigned long long parsed = strtoull(value, &end, 0);
  if (errno != 0 || end == value || *end != '\0') {
    return false;
  }
  *result = parsed;
  return true;
}

static struct options parse_options(int argc, char **argv) {
  struct options options = {
      .iterations = DEFAULT_ITERATIONS,
      .period = DEFAULT_PERIOD,
      .cpu = -1,
  };
  static const struct option long_options[] = {
      {"iterations", required_argument, NULL, 'i'},
      {"period", required_argument, NULL, 'p'},
      {"cpu", required_argument, NULL, 'c'},
      {"help", no_argument, NULL, 'h'},
      {NULL, 0, NULL, 0},
  };

  int option;
  while ((option = getopt_long(argc, argv, "i:p:c:h", long_options, NULL)) !=
         -1) {
    uint64_t parsed;
    switch (option) {
    case 'i':
      if (!parse_u64(optarg, &parsed) || parsed == 0 ||
          parsed > MAX_ITERATIONS) {
        fprintf(stderr, "Invalid iteration count: %s\n", optarg);
        exit(EXIT_FAILURE);
      }
      options.iterations = (size_t)parsed;
      break;
    case 'p':
      if (!parse_u64(optarg, &parsed) || parsed == 0 || parsed > INT64_MAX) {
        fprintf(stderr, "Invalid RCB period: %s\n", optarg);
        exit(EXIT_FAILURE);
      }
      options.period = parsed;
      break;
    case 'c':
      if (!parse_u64(optarg, &parsed) || parsed >= CPU_SETSIZE) {
        fprintf(stderr, "Invalid CPU index: %s\n", optarg);
        exit(EXIT_FAILURE);
      }
      options.cpu = (int)parsed;
      break;
    case 'h':
      usage(argv[0]);
      exit(EXIT_SUCCESS);
    default:
      usage(argv[0]);
      exit(EXIT_FAILURE);
    }
  }

  if (optind != argc) {
    usage(argv[0]);
    exit(EXIT_FAILURE);
  }
  return options;
}

static void pin_to_cpu(int cpu) {
  cpu_set_t cpuset;
  CPU_ZERO(&cpuset);
  CPU_SET(cpu, &cpuset);
  if (sched_setaffinity(0, sizeof(cpuset), &cpuset) != 0) {
    perror("sched_setaffinity");
    exit(EXIT_FAILURE);
  }
}

static void trim_brand(char *brand) {
  char *start = brand;
  while (*start == ' ') {
    ++start;
  }
  if (start != brand) {
    memmove(brand, start, strlen(start) + 1);
  }
  size_t length = strlen(brand);
  while (length > 0 && brand[length - 1] == ' ') {
    brand[--length] = '\0';
  }
}

static struct cpu_info read_cpu_info(void) {
  struct cpu_info info = {0};
  unsigned eax;
  unsigned ebx;
  unsigned ecx;
  unsigned edx;

  if (!__get_cpuid(0, &eax, &ebx, &ecx, &edx)) {
    fprintf(stderr, "CPUID leaf 0 is unavailable\n");
    exit(EXIT_FAILURE);
  }
  memcpy(info.vendor, &ebx, sizeof(ebx));
  memcpy(info.vendor + 4, &edx, sizeof(edx));
  memcpy(info.vendor + 8, &ecx, sizeof(ecx));

  if (!__get_cpuid(1, &eax, &ebx, &ecx, &edx)) {
    fprintf(stderr, "CPUID leaf 1 is unavailable\n");
    exit(EXIT_FAILURE);
  }
  unsigned base_family = (eax >> 8) & 0xf;
  unsigned base_model = (eax >> 4) & 0xf;
  unsigned extended_family = (eax >> 20) & 0xff;
  unsigned extended_model = (eax >> 16) & 0xf;
  info.family =
      base_family == 0xf ? base_family + extended_family : base_family;
  info.model = (base_family == 0x6 || base_family == 0xf)
                   ? base_model + (extended_model << 4)
                   : base_model;
  info.stepping = eax & 0xf;
  info.precise_ip = (edx & (1u << 21)) != 0;

  unsigned maximum_extended = __get_cpuid_max(0x80000000, NULL);
  if (maximum_extended >= 0x80000004) {
    unsigned brand[12];
    __get_cpuid(0x80000002, &brand[0], &brand[1], &brand[2], &brand[3]);
    __get_cpuid(0x80000003, &brand[4], &brand[5], &brand[6], &brand[7]);
    __get_cpuid(0x80000004, &brand[8], &brand[9], &brand[10], &brand[11]);
    memcpy(info.brand, brand, sizeof(brand));
    trim_brand(info.brand);
  } else {
    strcpy(info.brand, "unknown");
  }

  if (strcmp(info.vendor, "GenuineIntel") == 0) {
    info.rcb_event = 0x5101c4;
  } else if (strcmp(info.vendor, "AuthenticAMD") == 0) {
    info.rcb_event = 0x5100d1;
  } else {
    fprintf(stderr, "Unsupported CPU vendor: %s\n", info.vendor);
    exit(EXIT_FAILURE);
  }
  return info;
}

static int open_counter(pid_t pid, uint64_t event, uint64_t sample_period,
                        bool precise_ip) {
  struct perf_event_attr attr = {0};
  attr.type = PERF_TYPE_RAW;
  attr.size = sizeof(attr);
  attr.config = event;
  attr.sample_period = sample_period;
  attr.disabled = 1;
  attr.pinned = 1;
  attr.exclude_kernel = 1;
  attr.exclude_hv = 1;
  attr.exclude_guest = 1;
  attr.wakeup_events = 1;
  attr.precise_ip = precise_ip ? 1 : 0;

  return (int)syscall(SYS_perf_event_open, &attr, pid, -1, -1,
                      PERF_FLAG_FD_CLOEXEC);
}

static void configure_signal_delivery(int fd, pid_t tid) {
  struct f_owner_ex owner = {
      .type = F_OWNER_TID,
      .pid = tid,
  };
  if (fcntl(fd, F_SETOWN_EX, &owner) != 0 ||
      fcntl(fd, F_SETSIG, PERF_SIGNAL) != 0) {
    perror("configure perf signal delivery");
    exit(EXIT_FAILURE);
  }
  int flags = fcntl(fd, F_GETFL);
  if (flags < 0 || fcntl(fd, F_SETFL, flags | O_ASYNC | O_NONBLOCK) != 0) {
    perror("enable asynchronous perf notification");
    exit(EXIT_FAILURE);
  }
}

__attribute__((noreturn)) static void branch_loop(void) {
  const unsigned one = 1;
  for (;;) {
    __asm__ volatile("test %[one], %[one]\n\t"
                     "jnz 1f\n\t"
                     "1:"
                     :
                     : [one] "r"(one)
                     : "cc");
  }
}

__attribute__((noreturn)) static void child_main(void) {
  if (ptrace(PTRACE_TRACEME, 0, NULL, NULL) != 0) {
    _exit(2);
  }
  if (raise(SIGSTOP) != 0) {
    _exit(3);
  }
  branch_loop();
}

static void terminate_child(pid_t child) {
  if (ptrace(PTRACE_KILL, child, NULL, NULL) != 0 && errno != ESRCH) {
    perror("PTRACE_KILL");
  }
  while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {
  }
}

static void checked_ioctl(int fd, unsigned long request,
                          const char *operation) {
  if (ioctl(fd, request, 0) != 0) {
    perror(operation);
    exit(EXIT_FAILURE);
  }
}

static void set_period(int fd, uint64_t period) {
  if (ioctl(fd, PERF_EVENT_IOC_PERIOD, &period) != 0) {
    perror("set timer period");
    exit(EXIT_FAILURE);
  }
}

static uint64_t read_counter(int fd) {
  uint64_t value;
  ssize_t bytes;
  do {
    bytes = read(fd, &value, sizeof(value));
  } while (bytes < 0 && errno == EINTR);
  if (bytes == 0) {
    fprintf(stderr, "Pinned perf counter was descheduled\n");
    exit(EXIT_FAILURE);
  }
  if (bytes != (ssize_t)sizeof(value)) {
    perror("read perf counter");
    exit(EXIT_FAILURE);
  }
  return value;
}

static int compare_i64(const void *left, const void *right) {
  int64_t a = *(const int64_t *)left;
  int64_t b = *(const int64_t *)right;
  return (a > b) - (a < b);
}

static uint64_t recommended_margin(uint64_t maximum) {
  uint64_t margin = maximum > UINT64_MAX / 2 ? UINT64_MAX : maximum * 2;
  return margin < 100 ? 100 : margin;
}

static void print_results(const struct options *options,
                          const struct cpu_info *cpu, const int64_t *samples) {
  int64_t minimum = samples[0];
  int64_t maximum = samples[options->iterations - 1];
  size_t p99_index = (99 * options->iterations + 99) / 100 - 1;
  long double sum = 0;
  for (size_t i = 0; i < options->iterations; ++i) {
    sum += samples[i];
  }

  printf("CPU: %s\n", cpu->brand);
  printf("Vendor: %s family=0x%x model=0x%x stepping=0x%x cpu=%d\n",
         cpu->vendor, cpu->family, cpu->model, cpu->stepping, options->cpu);
  printf("RCB event: 0x%" PRIx64 ", precise_ip=%u\n", cpu->rcb_event,
         cpu->precise_ip ? 1 : 0);
  printf("Iterations: %zu, programmed period: %" PRIu64 " RCB\n",
         options->iterations, options->period);
  printf("Skid (RCB): min=%" PRId64 " max=%" PRId64 " mean=%.2Lf p99=%" PRId64
         "\n",
         minimum, maximum, sum / options->iterations, samples[p99_index]);
  printf("Recommended margin: %" PRIu64
         " RCB (2x observed max, minimum 100; empirical, not a hard bound)\n",
         recommended_margin(maximum > 0 ? (uint64_t)maximum : 0));
}

int main(int argc, char **argv) {
  struct options options = parse_options(argc, argv);
  if (options.cpu < 0) {
    options.cpu = sched_getcpu();
    if (options.cpu < 0) {
      perror("sched_getcpu");
      return EXIT_FAILURE;
    }
  }
  pin_to_cpu(options.cpu);
  struct cpu_info cpu = read_cpu_info();

  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return EXIT_FAILURE;
  }
  if (child == 0) {
    child_main();
  }

  int status;
  pid_t waited;
  do {
    waited = waitpid(child, &status, 0);
  } while (waited < 0 && errno == EINTR);
  if (waited != child || !WIFSTOPPED(status) || WSTOPSIG(status) != SIGSTOP) {
    fprintf(stderr, "Child failed to enter its initial ptrace stop\n");
    terminate_child(child);
    return EXIT_FAILURE;
  }
  if (ptrace(PTRACE_SETOPTIONS, child, NULL, PTRACE_O_EXITKILL) != 0) {
    perror("PTRACE_SETOPTIONS");
    terminate_child(child);
    return EXIT_FAILURE;
  }

  int timer_fd =
      open_counter(child, cpu.rcb_event, options.period, cpu.precise_ip);
  if (timer_fd < 0) {
    fprintf(stderr,
            "perf_event_open failed: %s. Check perf_event_paranoid and PMU "
            "access.\n",
            strerror(errno));
    terminate_child(child);
    return EXIT_FAILURE;
  }
  int clock_fd = open_counter(child, cpu.rcb_event, 0, false);
  if (clock_fd < 0) {
    fprintf(stderr,
            "perf_event_open failed: %s. Check perf_event_paranoid and PMU "
            "access.\n",
            strerror(errno));
    close(timer_fd);
    terminate_child(child);
    return EXIT_FAILURE;
  }
  configure_signal_delivery(timer_fd, child);

  int64_t *samples = calloc(options.iterations, sizeof(*samples));
  if (samples == NULL) {
    perror("calloc");
    terminate_child(child);
    return EXIT_FAILURE;
  }

  for (size_t i = 0; i < options.iterations; ++i) {
    checked_ioctl(clock_fd, PERF_EVENT_IOC_RESET, "reset clock counter");
    checked_ioctl(timer_fd, PERF_EVENT_IOC_RESET, "reset timer counter");
    set_period(timer_fd, options.period);
    checked_ioctl(clock_fd, PERF_EVENT_IOC_ENABLE, "enable clock counter");
    checked_ioctl(timer_fd, PERF_EVENT_IOC_ENABLE, "enable timer counter");

    if (ptrace(PTRACE_CONT, child, NULL, NULL) != 0) {
      perror("PTRACE_CONT");
      terminate_child(child);
      return EXIT_FAILURE;
    }
    do {
      waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    if (waited != child) {
      perror("waitpid");
      terminate_child(child);
      return EXIT_FAILURE;
    }

    checked_ioctl(timer_fd, PERF_EVENT_IOC_DISABLE, "disable timer counter");
    checked_ioctl(clock_fd, PERF_EVENT_IOC_DISABLE, "disable clock counter");
    if (!WIFSTOPPED(status) || WSTOPSIG(status) != PERF_SIGNAL) {
      fprintf(stderr, "Unexpected child stop at iteration %zu: status=0x%x\n",
              i, status);
      terminate_child(child);
      return EXIT_FAILURE;
    }
    siginfo_t signal_info;
    if (ptrace(PTRACE_GETSIGINFO, child, NULL, &signal_info) != 0) {
      perror("PTRACE_GETSIGINFO");
      terminate_child(child);
      return EXIT_FAILURE;
    }
    if (signal_info.si_signo != PERF_SIGNAL || signal_info.si_fd != timer_fd) {
      fprintf(stderr, "Unexpected perf signal at iteration %zu\n", i);
      terminate_child(child);
      return EXIT_FAILURE;
    }

    uint64_t observed = read_counter(clock_fd);
    if (observed > INT64_MAX || options.period > INT64_MAX) {
      fprintf(stderr, "Counter value exceeds the signed reporting range\n");
      terminate_child(child);
      return EXIT_FAILURE;
    }
    samples[i] = (int64_t)observed - (int64_t)options.period;
  }

  terminate_child(child);
  close(timer_fd);
  close(clock_fd);
  qsort(samples, options.iterations, sizeof(*samples), compare_i64);
  print_results(&options, &cpu, samples);
  free(samples);
  return EXIT_SUCCESS;
}
