#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>

static int invalid_tv(void) {
  errno = 0;
  long rc = syscall(SYS_gettimeofday, (void *)-1, NULL);
  if (rc != -1 || errno != EFAULT) {
    fprintf(stderr, "invalid tv: rc=%ld errno=%d\n", rc, errno);
    return 1;
  }
  puts("invalid-tv: EFAULT");
  return 0;
}

// Maps `pages` read-write pages, or returns NULL after printing why not.
static char *map_pages(long page_size, int pages) {
  void *mapping = mmap(NULL, (size_t)page_size * (size_t)pages,
                       PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED) {
    perror("mmap");
    return NULL;
  }
  return mapping;
}

// A tv on a read-only page: Linux stores nothing and returns EFAULT, whether
// tz is NULL or itself faults.
static int readonly_tv(void) {
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf");
    return 20;
  }
  char *page = map_pages(page_size, 1);
  char *unmapped_tz = map_pages(page_size, 1);
  if (page == NULL || unmapped_tz == NULL) {
    return 21;
  }
  if (munmap(unmapped_tz, (size_t)page_size) != 0) {
    perror("munmap");
    return 22;
  }
  struct timeval *tv = (struct timeval *)page;
  tv->tv_sec = 123;
  tv->tv_usec = 456;
  if (mprotect(page, (size_t)page_size, PROT_READ) != 0) {
    perror("mprotect");
    return 23;
  }
  void *tzs[] = {NULL, unmapped_tz};
  for (size_t i = 0; i < sizeof(tzs) / sizeof(tzs[0]); i++) {
    errno = 0;
    long rc = syscall(SYS_gettimeofday, tv, tzs[i]);
    if (rc != -1 || errno != EFAULT) {
      fprintf(stderr, "readonly tv %zu: rc=%ld errno=%d\n", i, rc, errno);
      return 24;
    }
    if (tv->tv_sec != 123 || tv->tv_usec != 456) {
      fprintf(stderr, "readonly tv %zu: tv=%ld.%06ld\n", i, (long)tv->tv_sec,
              (long)tv->tv_usec);
      return 25;
    }
  }
  puts("readonly-tv: EFAULT tv unchanged");
  return 0;
}

// A tv whose tv_usec lies on a read-only page: Linux stores tv_sec, then
// faults on tv_usec and returns EFAULT.
static int straddle_tv(void) {
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf");
    return 30;
  }
  char *pages = map_pages(page_size, 2);
  if (pages == NULL) {
    return 31;
  }
  struct timeval *tv =
      (struct timeval *)(pages + page_size - sizeof(tv->tv_sec));
  tv->tv_sec = 123;
  tv->tv_usec = 456;
  if (mprotect(pages + page_size, (size_t)page_size, PROT_READ) != 0) {
    perror("mprotect");
    return 32;
  }
  struct timeval before;
  struct timeval after;
  if (syscall(SYS_gettimeofday, &before, NULL) != 0) {
    perror("gettimeofday before");
    return 33;
  }
  errno = 0;
  long rc = syscall(SYS_gettimeofday, tv, NULL);
  if (rc != -1 || errno != EFAULT) {
    fprintf(stderr, "straddle tv: rc=%ld errno=%d\n", rc, errno);
    return 34;
  }
  if (syscall(SYS_gettimeofday, &after, NULL) != 0) {
    perror("gettimeofday after");
    return 35;
  }
  if (tv->tv_usec != 456 || tv->tv_sec == 123 ||
      tv->tv_sec < before.tv_sec || tv->tv_sec > after.tv_sec) {
    fprintf(stderr, "straddle tv: tv=%ld.%06ld outside %ld..%ld\n",
            (long)tv->tv_sec, (long)tv->tv_usec, (long)before.tv_sec,
            (long)after.tv_sec);
    return 36;
  }
  puts("straddle-tv: EFAULT tv_sec stored, tv_usec unchanged");
  return 0;
}

static int faulting_tz(void) {
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    perror("sysconf");
    return 2;
  }
  void *page = mmap(NULL, (size_t)page_size, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (page == MAP_FAILED) {
    perror("mmap");
    return 3;
  }
  if (munmap(page, (size_t)page_size) != 0) {
    perror("munmap");
    return 4;
  }

  // Linux stores tv before it copies tz, so a call whose tz faults has
  // already stored the time in tv. It must be the same clock that successful
  // calls read: no earlier than the read before it and no later than the one
  // after it.
  struct timeval before;
  struct timeval after;
  struct timeval tv = {.tv_sec = 123, .tv_usec = 456};
  if (syscall(SYS_gettimeofday, &before, NULL) != 0) {
    perror("gettimeofday before");
    return 7;
  }
  errno = 0;
  long rc = syscall(SYS_gettimeofday, &tv, page);
  if (rc != -1 || errno != EFAULT) {
    fprintf(stderr, "faulting tz: rc=%ld errno=%d\n", rc, errno);
    return 5;
  }
  if (syscall(SYS_gettimeofday, &after, NULL) != 0) {
    perror("gettimeofday after");
    return 8;
  }
  if (timercmp(&tv, &before, <) || timercmp(&tv, &after, >) ||
      (tv.tv_sec == 123 && tv.tv_usec == 456)) {
    fprintf(stderr, "faulting tz: tv=%ld.%06ld outside %ld.%06ld..%ld.%06ld\n",
            (long)tv.tv_sec, (long)tv.tv_usec, (long)before.tv_sec,
            (long)before.tv_usec, (long)after.tv_sec, (long)after.tv_usec);
    return 6;
  }
  puts("faulting-tz: EFAULT tv between the surrounding reads");
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2) {
    return 64;
  }
  if (strcmp(argv[1], "invalid-tv") == 0) {
    return invalid_tv();
  }
  if (strcmp(argv[1], "readonly-tv") == 0) {
    return readonly_tv();
  }
  if (strcmp(argv[1], "straddle-tv") == 0) {
    return straddle_tv();
  }
  if (strcmp(argv[1], "faulting-tz") == 0) {
    return faulting_tz();
  }
  return 64;
}
