// @lint-ignore LICENSELINT

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/random.h>
#include <sys/syscall.h>
#include <unistd.h>

enum { BYTES = 16, SAMPLES = 5, THREADS = 4 };

struct sample {
  uint8_t getrandom_bytes[BYTES];
  uint8_t urandom_bytes[BYTES];
};

struct thread_result {
  struct sample sample;
  int error;
};

static int read_exact(int fd, uint8_t* buffer, size_t length) {
  size_t offset = 0;
  while (offset < length) {
    ssize_t count = read(fd, buffer + offset, length - offset);
    if (count <= 0) {
      return -1;
    }
    offset += (size_t)count;
  }
  return 0;
}

static int fill_getrandom(uint8_t buffer[BYTES]) {
  return getrandom(buffer, BYTES, 0) == BYTES ? 0 : -1;
}

static int check_getrandom_flags(void) {
  static const unsigned int valid_flags[] = {
      GRND_NONBLOCK,
      GRND_RANDOM,
      GRND_NONBLOCK | GRND_RANDOM,
  };
  uint8_t buffer[BYTES];

  for (size_t index = 0; index < sizeof(valid_flags) / sizeof(valid_flags[0]);
       index++) {
    if (getrandom(buffer, sizeof(buffer), valid_flags[index]) !=
        (ssize_t)sizeof(buffer)) {
      return -1;
    }
  }

  if (syscall(SYS_getrandom, buffer, sizeof(buffer), 1ULL << 32) !=
      (ssize_t)sizeof(buffer)) {
    return -1;
  }

  errno = 0;
  if (getrandom(buffer, sizeof(buffer), 0x80000000u) != -1 ||
      errno != EINVAL) {
    return -1;
  }

  errno = 0;
  if (syscall(
          SYS_getrandom,
          buffer,
          sizeof(buffer),
          (1ULL << 32) | 0x80000000ULL) != -1 ||
      errno != EINVAL) {
    return -1;
  }

#ifdef GRND_INSECURE
  errno = 0;
  if (getrandom(buffer, sizeof(buffer), GRND_RANDOM | GRND_INSECURE) != -1 ||
      errno != EINVAL) {
    return -1;
  }
#endif

  errno = 0;
  if (syscall(SYS_getrandom, NULL, 0, 0) != 0 || errno != 0) {
    return -1;
  }

  puts("getrandom-flags ok");
  return 0;
}

static int check_getrandom_faults(void) {
  errno = 0;
  if (syscall(SYS_getrandom, (void*)1, SIZE_MAX, 0) != -1 ||
      errno != EFAULT) {
    return -1;
  }

  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    return -1;
  }
  size_t page = (size_t)page_size;
  uint8_t* mapping =
      mmap(NULL, page * 2, PROT_READ | PROT_WRITE,
           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED) {
    return -1;
  }
  if (mprotect(mapping + page, page, PROT_NONE) != 0) {
    munmap(mapping, page * 2);
    return -1;
  }

  long result = syscall(SYS_getrandom, mapping + page - 8, 16, 0);
  long later_chunk_result =
      syscall(SYS_getrandom, mapping, page + 8, 0);
  int unmap_result = munmap(mapping, page * 2);
  if (result != 8 || later_chunk_result != (long)page ||
      unmap_result != 0) {
    return -1;
  }

  puts("getrandom-faults ok");
  return 0;
}

static int fill_device(const char* path, uint8_t buffer[BYTES]) {
  int fd = open(path, O_RDONLY);
  if (fd < 0) {
    return -1;
  }
  int result = read_exact(fd, buffer, BYTES);
  if (close(fd) != 0) {
    return -1;
  }
  return result;
}

static void print_bytes(const char* source,
                        int index,
                        const uint8_t buffer[BYTES]) {
  printf("%s[%d]=", source, index);
  for (int byte = 0; byte < BYTES; byte++) {
    printf("%02x", buffer[byte]);
  }
  putchar('\n');
}

static void* thread_main(void* argument) {
  struct thread_result* result = argument;
  if (fill_getrandom(result->sample.getrandom_bytes) != 0 ||
      fill_device("/dev/urandom", result->sample.urandom_bytes) != 0) {
    result->error = 1;
  }
  return NULL;
}

int main(int argc, char** argv) {
  int root_only = argc == 2 && strcmp(argv[1], "--root-only") == 0;
  if (argc != 1 && !root_only) {
    return 8;
  }
  uint8_t getrandom_samples[SAMPLES][BYTES];
  uint8_t urandom_samples[SAMPLES][BYTES];
  uint8_t random_samples[SAMPLES][BYTES];
  pthread_t threads[THREADS];
  struct thread_result thread_results[THREADS] = {0};
  if (check_getrandom_flags() != 0) {
    return 6;
  }
  if (check_getrandom_faults() != 0) {
    return 7;
  }

  for (int sample = 0; sample < SAMPLES; sample++) {
    if (fill_getrandom(getrandom_samples[sample]) != 0 ||
        fill_device("/dev/urandom", urandom_samples[sample]) != 0 ||
        fill_device("/dev/random", random_samples[sample]) != 0) {
      return 2;
    }
  }

  if (!root_only) {
    for (int thread = 0; thread < THREADS; thread++) {
      if (pthread_create(&threads[thread], NULL, thread_main,
                         &thread_results[thread]) != 0) {
        return 3;
      }
    }
    for (int thread = 0; thread < THREADS; thread++) {
      if (pthread_join(threads[thread], NULL) != 0 ||
          thread_results[thread].error != 0) {
        return 4;
      }
      for (int previous = 0; previous < thread; previous++) {
        if (memcmp(&thread_results[thread].sample,
                   &thread_results[previous].sample,
                   sizeof(struct sample)) == 0) {
          return 5;
        }
      }
    }
  }

  for (int sample = 0; sample < SAMPLES; sample++) {
    print_bytes("getrandom", sample, getrandom_samples[sample]);
    print_bytes("urandom", sample, urandom_samples[sample]);
    print_bytes("random", sample, random_samples[sample]);
  }
  if (!root_only) {
    for (int thread = 0; thread < THREADS; thread++) {
      print_bytes("thread-getrandom", thread,
                  thread_results[thread].sample.getrandom_bytes);
      print_bytes("thread-urandom", thread,
                  thread_results[thread].sample.urandom_bytes);
    }
  }
  return 0;
}
