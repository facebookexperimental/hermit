// @lint-ignore LICENSELINT

#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/random.h>
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

int main(void) {
  uint8_t getrandom_samples[SAMPLES][BYTES];
  uint8_t urandom_samples[SAMPLES][BYTES];
  uint8_t random_samples[SAMPLES][BYTES];
  pthread_t threads[THREADS];
  struct thread_result thread_results[THREADS] = {0};

  for (int sample = 0; sample < SAMPLES; sample++) {
    if (fill_getrandom(getrandom_samples[sample]) != 0 ||
        fill_device("/dev/urandom", urandom_samples[sample]) != 0 ||
        fill_device("/dev/random", random_samples[sample]) != 0) {
      return 2;
    }
  }

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

  for (int sample = 0; sample < SAMPLES; sample++) {
    print_bytes("getrandom", sample, getrandom_samples[sample]);
    print_bytes("urandom", sample, urandom_samples[sample]);
    print_bytes("random", sample, random_samples[sample]);
  }
  for (int thread = 0; thread < THREADS; thread++) {
    print_bytes("thread-getrandom", thread,
                thread_results[thread].sample.getrandom_bytes);
    print_bytes("thread-urandom", thread,
                thread_results[thread].sample.urandom_bytes);
  }
  return 0;
}
