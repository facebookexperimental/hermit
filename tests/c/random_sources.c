// @lint-ignore LICENSELINT

#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/auxv.h>
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

/*
 * The vDSO leg of this contract.
 *
 * getrandom(2), /dev/urandom and /dev/random all reach the kernel through a
 * syscall Detcore intercepts. `__vdso_getrandom` (Linux 6.11+) does not: the
 * caller resolves it out of the vDSO and generates bytes from a per-thread
 * ChaCha state in USERSPACE. A test that exercises only getrandom(2) therefore
 * says nothing about this path -- it passes whether or not the fast path is
 * determinized, which is exactly why this case exists.
 *
 * The bytes are printed like every other source, so the surrounding contract
 * (byte-identical stdout across repeated runs and across backends) covers the
 * vDSO path too. If the fast path ever started producing host entropy that the
 * tool cannot see, this fixture goes red.
 *
 * Resolution is deliberately manual rather than via dlsym: the guest must reach
 * the vDSO entry itself, not whatever libc decides to do. glibc only started
 * routing getrandom() through the vDSO in 2.41, so on an older libc a normal
 * call does not exercise this path at all.
 */

#ifndef MAP_DROPPABLE
#define MAP_DROPPABLE 0x08
#endif

typedef ssize_t (*vdso_getrandom_fn)(void*, size_t, unsigned int, void*,
                                     size_t);

/* Filled by the kernel on the query call; layout from the vDSO contract. */
struct vdso_getrandom_params {
  uint32_t size_of_opaque_state;
  uint32_t mmap_prot;
  uint32_t mmap_flags;
  uint32_t reserved[13];
};

static vdso_getrandom_fn resolve_vdso_getrandom(void) {
  Elf64_Ehdr* header = (Elf64_Ehdr*)getauxval(AT_SYSINFO_EHDR);
  if (header == NULL) {
    return NULL;
  }
  Elf64_Phdr* program = (Elf64_Phdr*)((char*)header + header->e_phoff);
  uintptr_t base = (uintptr_t)header;
  uintptr_t load_offset = 0;
  Elf64_Dyn* dynamic = NULL;
  for (int index = 0; index < header->e_phnum; index++) {
    if (program[index].p_type == PT_LOAD) {
      load_offset = base + program[index].p_offset - program[index].p_vaddr;
    }
    if (program[index].p_type == PT_DYNAMIC) {
      dynamic = (Elf64_Dyn*)(base + program[index].p_offset);
    }
  }
  if (dynamic == NULL) {
    return NULL;
  }
  const char* strings = NULL;
  Elf64_Sym* symbols = NULL;
  Elf64_Word* hash = NULL;
  for (Elf64_Dyn* entry = dynamic; entry->d_tag != DT_NULL; entry++) {
    if (entry->d_tag == DT_STRTAB) {
      strings = (const char*)(load_offset + entry->d_un.d_ptr);
    } else if (entry->d_tag == DT_SYMTAB) {
      symbols = (Elf64_Sym*)(load_offset + entry->d_un.d_ptr);
    } else if (entry->d_tag == DT_HASH) {
      hash = (Elf64_Word*)(load_offset + entry->d_un.d_ptr);
    }
  }
  if (strings == NULL || symbols == NULL || hash == NULL) {
    return NULL;
  }
  for (Elf64_Word index = 0; index < hash[1]; index++) {
    const char* name = strings + symbols[index].st_name;
    if (name != NULL && strcmp(name, "__vdso_getrandom") == 0) {
      return (vdso_getrandom_fn)(load_offset + symbols[index].st_value);
    }
  }
  return NULL;
}

/*
 * Returns 1 when the fast path produced bytes, 0 when it is unavailable (no
 * symbol, or the kernel declined and the caller must use getrandom(2)), and -1
 * on an unexpected failure. "Unavailable" is a pass: it means no unintercepted
 * path exists on this host.
 */
static int fill_vdso_getrandom(uint8_t samples[SAMPLES][BYTES]) {
  vdso_getrandom_fn vdso_getrandom = resolve_vdso_getrandom();
  if (vdso_getrandom == NULL) {
    return 0;
  }
  struct vdso_getrandom_params params;
  memset(&params, 0, sizeof(params));
  ssize_t query = vdso_getrandom(NULL, 0, 0, &params, ~(size_t)0);
  if (query < 0) {
    return 0;
  }
  size_t state_size = params.size_of_opaque_state != 0
                          ? (size_t)params.size_of_opaque_state
                          : (size_t)query;
  if (state_size == 0) {
    return 0;
  }
  int prot = params.mmap_prot != 0 ? (int)params.mmap_prot
                                   : (PROT_READ | PROT_WRITE);
  int flags = params.mmap_flags != 0
                  ? (int)params.mmap_flags | MAP_PRIVATE | MAP_ANONYMOUS
                  : (MAP_DROPPABLE | MAP_PRIVATE | MAP_ANONYMOUS);
  void* state = mmap(NULL, state_size, prot, flags, -1, 0);
  if (state == MAP_FAILED) {
    /* MAP_DROPPABLE is not universal; a plain private mapping works too. */
    state = mmap(NULL, state_size, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (state == MAP_FAILED) {
      return -1;
    }
  }
  for (int sample = 0; sample < SAMPLES; sample++) {
    ssize_t drawn =
        vdso_getrandom(samples[sample], BYTES, 0, state, state_size);
    if (drawn == BYTES) {
      continue;
    }
    if (drawn < 0) {
      /* Declined mid-sequence: the caller is expected to fall back. */
      munmap(state, state_size);
      return 0;
    }
    munmap(state, state_size);
    return -1;
  }
  munmap(state, state_size);
  return 1;
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
  uint8_t vdso_samples[SAMPLES][BYTES];
  pthread_t threads[THREADS];
  struct thread_result thread_results[THREADS] = {0};
  if (check_getrandom_flags() != 0) {
    return 6;
  }
  if (check_getrandom_faults() != 0) {
    return 7;
  }

  int vdso_available = fill_vdso_getrandom(vdso_samples);
  if (vdso_available < 0) {
    return 9;
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
  if (vdso_available == 1) {
    for (int sample = 0; sample < SAMPLES; sample++) {
      print_bytes("vdso-getrandom", sample, vdso_samples[sample]);
    }
  } else {
    puts("vdso-getrandom unavailable");
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
