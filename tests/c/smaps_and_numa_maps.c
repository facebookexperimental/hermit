/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Reads /proc/self/maps and smaps (or smaps_rollup), checks what Linux's
 * fs/proc/task_mmu.c prints in them (show_smap, __show_smap and
 * show_smaps_rollup in v6.17), and checks that /proc/self/numa_maps does
 * not exist, as on the kernel built without NUMA that Hermit presents
 * (https://github.com/rrnewton/hermit/issues/5269; Linux registers the
 * file only under CONFIG_NUMA, fs/proc/base.c). The guest reports which
 * it found (`numa absent` under Hermit, `numa present` on a NUMA host)
 * instead of failing, so a native run still gives the smaps lines the
 * Hermit runs are compared against; the tests require `absent`.
 *
 * - every smaps block's header is a maps line, in the same order, and its
 *   fields are Linux's, in Linux's order: Size, KernelPageSize, MMUPageSize,
 *   the accounting fields, THPeligible, ProtectionKey where any block has it,
 *   and VmFlags, whose rd, wr and ex follow the line's permissions;
 * - Size is the range's, THPeligible 0 or 1, and [stack]'s flags have gd;
 * - whether opening numa_maps fails with ENOENT.
 *
 * The program makes a mapping of each kind it reports first: anonymous
 * memory, a file (its own executable), the heap (brk), and it has its stack
 * and the vDSO. It reports, per kind, whether maps lists one; the count of
 * accounting values that are not zero (a sanitizer zeroes them all); and the
 * stack's details: its field labels, flags, THPeligible and ProtectionKey,
 * then its whole smaps block with the
 * range and size left out, which runs on different backends can compare.
 * Before those, the stack's range, which a test that knows a backend's
 * layout can check THPeligible against.
 *
 * Modes:
 *   files    the above;
 *   rollup   smaps_rollup's header spans the first maps line's start to the
 *            last one's end but the gate area's, and its fields are Linux's;
 *   split    first makes a stack page below the stack pointer PROT_NONE,
 *            splitting the stack's mapping, then as files;
 *   advise   first gives the whole stack mapping MADV_RANDOM, which keeps
 *            it one mapping and sets VM_RAND_READ (rr), then as files;
 *   other    first gives the anonymous mapping, not the stack, MADV_RANDOM
 *            and MADV_DONTNEED, then as files;
 *   discard  runs on a stack of its own, discards every page of the initial
 *            stack with MADV_DONTNEED, and checks numa_maps is still absent:
 *            the discard reaches a listed mapping, which is what used to
 *            make its rendered row unanswerable on KVM.
 *
 * Everything is read into static buffers before anything is printed, so no
 * allocation (stdio's own included) adds a mapping between the reads.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#define FILE_BYTES (1 << 20)
#define MAX_LINES 4096

static char maps_text[FILE_BYTES];
static char smaps_text[FILE_BYTES];
static char out_buffer[1 << 16];

static char* maps_lines[MAX_LINES];
static char* smaps_lines[MAX_LINES];

static const char* const LABELS[] = {
    "Size",          "KernelPageSize", "MMUPageSize",    "Rss",
    "Pss",           "Pss_Dirty",      "Shared_Clean",   "Shared_Dirty",
    "Private_Clean", "Private_Dirty",  "Referenced",     "Anonymous",
    "KSM",           "LazyFree",       "AnonHugePages",  "ShmemPmdMapped",
    "FilePmdMapped", "Shared_Hugetlb", "Private_Hugetlb", "Swap",
    "SwapPss",       "Locked",
};
#define LABEL_COUNT (sizeof(LABELS) / sizeof(LABELS[0]))

static const char* const ROLLUP_LABELS[] = {
    "Rss",           "Pss",            "Pss_Dirty",      "Pss_Anon",
    "Pss_File",      "Pss_Shmem",      "Shared_Clean",   "Shared_Dirty",
    "Private_Clean", "Private_Dirty",  "Referenced",     "Anonymous",
    "KSM",           "LazyFree",       "AnonHugePages",  "ShmemPmdMapped",
    "FilePmdMapped", "Shared_Hugetlb", "Private_Hugetlb", "Swap",
    "SwapPss",       "Locked",
};
#define ROLLUP_LABEL_COUNT (sizeof(ROLLUP_LABELS) / sizeof(ROLLUP_LABELS[0]))

static void fail(const char* what, const char* detail) {
  printf("FAIL: %s: %s\n", what, detail);
  fflush(stdout);
  exit(1);
}

/* The whole of `path` into `buffer`, NUL-terminated; its length. */
static size_t slurp(const char* path, char* buffer, size_t size) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    fail(path, strerror(errno));
  }
  size_t used = 0;
  for (;;) {
    ssize_t got = read(fd, buffer + used, size - 1 - used);
    if (got < 0) {
      fail(path, strerror(errno));
    }
    if (got == 0) {
      break;
    }
    used += (size_t)got;
    if (used == size - 1) {
      fail(path, "too long");
    }
  }
  close(fd);
  buffer[used] = '\0';
  return used;
}

/* Whether numa_maps is absent, as on the kernel built without NUMA that
 * Hermit presents, which registers none in any task's directory. */
static int numa_maps_absent(void) {
  int fd = open("/proc/self/numa_maps", O_RDONLY | O_CLOEXEC);
  if (fd >= 0) {
    close(fd);
    return 0;
  }
  if (errno != ENOENT) {
    fail("/proc/self/numa_maps", strerror(errno));
  }
  return 1;
}

/* Split `text` into its lines, in place; their count. */
static size_t split_lines(char* text, char** lines) {
  size_t count = 0;
  char* line = text;
  while (*line != '\0') {
    char* end = strchr(line, '\n');
    if (end == NULL) {
      fail("split", "a line without its newline");
    }
    *end = '\0';
    if (count == MAX_LINES) {
      fail("split", "too many lines");
    }
    lines[count++] = line;
    line = end + 1;
  }
  return count;
}

/* Whether `line` starts with a mapping range, `start-end `. */
static int range_of(const char* line, unsigned long* start, unsigned long* end) {
  char* after = NULL;
  *start = strtoul(line, &after, 16);
  if (after == line || *after != '-') {
    return 0;
  }
  const char* second = after + 1;
  *end = strtoul(second, &after, 16);
  return after != second && *after == ' ';
}

/* The text after the five fields of a maps line (range, permissions, offset,
 * device, inode) and the spaces after them: its name, maybe empty. */
static const char* name_of(const char* line) {
  const char* cursor = line;
  for (int field = 0; field < 5; ++field) {
    while (*cursor == ' ') {
      ++cursor;
    }
    while (*cursor != ' ' && *cursor != '\0') {
      ++cursor;
    }
  }
  while (*cursor == ' ') {
    ++cursor;
  }
  return cursor;
}

/* The maps line whose range holds `address`, or -1. */
static long line_holding(char** lines, size_t count, unsigned long address) {
  for (size_t index = 0; index < count; ++index) {
    unsigned long start = 0;
    unsigned long end = 0;
    if (range_of(lines[index], &start, &end) && start <= address &&
        address < end) {
      return (long)index;
    }
  }
  return -1;
}

static int is_gate(const char* line) {
  return strcmp(name_of(line), "[vsyscall]") == 0;
}

/* Whether `value` (after a label's colon) is a number and " kB". */
static int kilobytes(const char* value, unsigned long* amount) {
  char* after = NULL;
  while (*value == ' ' || *value == '\t') {
    ++value;
  }
  *amount = strtoul(value, &after, 10);
  return after != value && strcmp(after, " kB") == 0;
}

static unsigned long accounting_nonzero;

/* Check the smaps block whose header is smaps line `at`, for maps line
 * `line`; the index of the line after it. Fills `labels` with its labels. */
static size_t check_block(
    size_t at,
    size_t count,
    const char* line,
    int* protection_key,
    char* labels,
    size_t labels_size) {
  unsigned long start = 0;
  unsigned long end = 0;
  range_of(line, &start, &end);
  if (strcmp(smaps_lines[at], line) != 0) {
    fail("smaps header is not the maps line", smaps_lines[at]);
  }
  labels[0] = '\0';
  size_t index = at + 1;
  size_t expected = 0;
  int saw_key = 0;
  const char* permissions = strchr(line, ' ') + 1;
  for (; index < count; ++index) {
    unsigned long ignored_start = 0;
    unsigned long ignored_end = 0;
    if (range_of(smaps_lines[index], &ignored_start, &ignored_end)) {
      break;
    }
    char* colon = strchr(smaps_lines[index], ':');
    if (colon == NULL) {
      fail("smaps field without a colon", smaps_lines[index]);
    }
    size_t label_length = (size_t)(colon - smaps_lines[index]);
    char label[64];
    if (label_length >= sizeof(label)) {
      fail("smaps label too long", smaps_lines[index]);
    }
    memcpy(label, smaps_lines[index], label_length);
    label[label_length] = '\0';
    const char* value = colon + 1;
    if (strlen(labels) + label_length + 2 < labels_size) {
      if (labels[0] != '\0') {
        strcat(labels, " ");
      }
      strcat(labels, label);
    }
    if (expected < LABEL_COUNT) {
      if (strcmp(label, LABELS[expected]) != 0) {
        fail("smaps field out of Linux's order", smaps_lines[index]);
      }
      unsigned long amount = 0;
      if (!kilobytes(value, &amount)) {
        fail("smaps field is not a size", smaps_lines[index]);
      }
      if (expected == 0 && amount != (end - start) / 1024) {
        fail("smaps Size is not the range's", smaps_lines[index]);
      }
      if (expected >= 3 && amount != 0) {
        ++accounting_nonzero;
      }
      ++expected;
    } else if (expected == LABEL_COUNT) {
      if (strcmp(label, "THPeligible") != 0) {
        fail("smaps THPeligible missing", smaps_lines[index]);
      }
      unsigned long eligible = strtoul(value, NULL, 10);
      if (eligible > 1) {
        fail("smaps THPeligible is not 0 or 1", smaps_lines[index]);
      }
      ++expected;
    } else if (strcmp(label, "ProtectionKey") == 0 && !saw_key) {
      saw_key = 1;
    } else if (strcmp(label, "VmFlags") == 0) {
      int read_flag = strstr(value, " rd ") != NULL;
      int write_flag = strstr(value, " wr ") != NULL;
      int exec_flag = strstr(value, " ex ") != NULL;
      if (read_flag != (permissions[0] == 'r') ||
          write_flag != (permissions[1] == 'w') ||
          exec_flag != (permissions[2] == 'x')) {
        fail("smaps VmFlags disagree with the permissions", line);
      }
      if (strcmp(name_of(line), "[stack]") == 0 &&
          strstr(value, " gd ") == NULL) {
        fail("the stack's VmFlags lack gd", value);
      }
      ++index;
      break;
    } else {
      fail("smaps field Linux does not print here", smaps_lines[index]);
    }
  }
  if (expected != LABEL_COUNT + 1 ||
      (index <= count && strncmp(smaps_lines[index - 1], "VmFlags:", 8) != 0)) {
    fail("smaps block is not Linux's", line);
  }
  if (*protection_key == -1) {
    *protection_key = saw_key;
  } else if (*protection_key != saw_key) {
    fail("ProtectionKey in some blocks only", line);
  }
  return index;
}

static void print_kind(
    const char* kind,
    char** lines,
    size_t count,
    unsigned long address) {
  printf(
      "kind %s %s\n",
      kind,
      line_holding(lines, count, address) >= 0 ? "listed" : "not-listed");
}

/* discard mode: the stack it runs on, and the initial stack's range, outside
 * the initial stack, which it discards. */
static unsigned char alternate_stack[1 << 20] __attribute__((aligned(16)));
static unsigned long discard_start;
static unsigned long discard_end;

/* Called on alternate_stack: discard every page of the initial stack, then
 * report whether numa_maps is absent. Never returns, as the frames below
 * the stack it discarded; it reads nothing there (argv, the environment and
 * the auxiliary vector are). */
__attribute__((noreturn, noinline)) static void discard_and_list(void) {
  if (syscall(SYS_madvise, discard_start, discard_end - discard_start, MADV_DONTNEED) != 0) {
    fail("madvise MADV_DONTNEED of the stack", strerror(errno));
  }
  printf("discard ok\nnuma %s\n", numa_maps_absent() ? "absent" : "present");
  fflush(stdout);
  syscall(SYS_exit_group, 0);
  __builtin_unreachable();
}

static int rollup(void) {
  slurp("/proc/self/maps", maps_text, sizeof(maps_text));
  slurp("/proc/self/smaps_rollup", smaps_text, sizeof(smaps_text));
  size_t maps_count = split_lines(maps_text, maps_lines);
  size_t rollup_count = split_lines(smaps_text, smaps_lines);
  unsigned long first = 0;
  unsigned long last = 0;
  int found = 0;
  for (size_t index = 0; index < maps_count; ++index) {
    unsigned long start = 0;
    unsigned long end = 0;
    if (!range_of(maps_lines[index], &start, &end)) {
      fail("maps line", maps_lines[index]);
    }
    if (is_gate(maps_lines[index])) {
      continue;
    }
    if (!found) {
      first = start;
      found = 1;
    }
    last = end;
  }
  if (rollup_count != ROLLUP_LABEL_COUNT + 1) {
    fail("smaps_rollup", "not Linux's line count");
  }
  char header[64];
  snprintf(header, sizeof(header), "%08lx-%08lx ---p 00000000 00:00 0 ", first, last);
  if (strncmp(smaps_lines[0], header, strlen(header)) != 0 ||
      strcmp(name_of(smaps_lines[0]), "[rollup]") != 0) {
    fail("smaps_rollup header", smaps_lines[0]);
  }
  for (size_t index = 0; index < ROLLUP_LABEL_COUNT; ++index) {
    const char* line = smaps_lines[index + 1];
    size_t length = strlen(ROLLUP_LABELS[index]);
    unsigned long amount = 0;
    if (strncmp(line, ROLLUP_LABELS[index], length) != 0 ||
        line[length] != ':' || !kilobytes(line + length + 1, &amount)) {
      fail("smaps_rollup field out of Linux's order", line);
    }
    if (amount != 0) {
      ++accounting_nonzero;
    }
  }
  printf("rollup ok\naccounting nonzero %lu\n", accounting_nonzero);
  return 0;
}

int main(int argc, char** argv) {
  setvbuf(stdout, out_buffer, _IOFBF, sizeof(out_buffer));
  const char* mode = argc > 1 ? argv[1] : "files";
  if (strcmp(mode, "rollup") == 0) {
    return rollup();
  }
  if (strcmp(mode, "discard") == 0) {
    char here = 0;
    slurp("/proc/self/maps", maps_text, sizeof(maps_text));
    size_t count = split_lines(maps_text, maps_lines);
    long at_stack = line_holding(maps_lines, count, (unsigned long)&here);
    if (at_stack < 0 || !range_of(maps_lines[at_stack], &discard_start, &discard_end)) {
      fail("discard", "no maps line holds the stack");
    }
#if defined(__x86_64__)
    void* top = alternate_stack + sizeof(alternate_stack);
    __asm__ volatile("mov %0, %%rsp\n\tcall *%1"
                     :
                     : "r"(top), "r"(discard_and_list)
                     : "memory");
    __builtin_unreachable();
#else
    fail("discard", "x86-64 only");
#endif
  }
  if (strcmp(mode, "files") != 0 && strcmp(mode, "split") != 0 &&
      strcmp(mode, "advise") != 0 && strcmp(mode, "other") != 0) {
    fail("usage", "files|rollup|split|advise|other|discard");
  }
  long page = sysconf(_SC_PAGESIZE);
  // Anonymous memory, its first page written.
  char* anonymous = mmap(
      NULL,
      (size_t)(3 * page),
      PROT_READ | PROT_WRITE,
      MAP_PRIVATE | MAP_ANONYMOUS,
      -1,
      0);
  if (anonymous == MAP_FAILED) {
    fail("mmap anonymous", strerror(errno));
  }
  anonymous[0] = 1;
  if (strcmp(mode, "other") == 0) {
    // Advice that sets a flag, and advice that discards pages, on a mapping
    // that is not the stack.
    if (madvise(anonymous, (size_t)(3 * page), MADV_RANDOM) != 0 ||
        madvise(anonymous, (size_t)(3 * page), MADV_DONTNEED) != 0) {
      fail("madvise the anonymous mapping", strerror(errno));
    }
  }
  // A file: the program's own first page.
  int fd = open(argv[0], O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    fail("open the program", strerror(errno));
  }
  char* file = mmap(NULL, (size_t)page, PROT_READ, MAP_PRIVATE, fd, 0);
  if (file == MAP_FAILED) {
    fail("mmap the program", strerror(errno));
  }
  close(fd);
  volatile char touched = file[0];
  (void)touched;
  // The heap.
  char* heap = sbrk(0);
  if (heap == (void*)-1 || brk(heap + page) != 0) {
    fail("brk", strerror(errno));
  }
  heap[0] = 1;
  // The stack, and a page of it below the stack pointer for split and advise.
  char here = 0;
  unsigned long stack = (unsigned long)&here;
  unsigned long below = (stack & ~((unsigned long)page - 1)) - 16 * (unsigned long)page;
  if (strcmp(mode, "split") == 0 &&
      mprotect((void*)below, (size_t)page, PROT_NONE) != 0) {
    fail("mprotect", strerror(errno));
  }
  if (strcmp(mode, "advise") == 0) {
    // The whole stack mapping, which stays one mapping, with VM_RAND_READ.
    slurp("/proc/self/maps", maps_text, sizeof(maps_text));
    size_t count = split_lines(maps_text, maps_lines);
    long at_stack = line_holding(maps_lines, count, stack);
    unsigned long start = 0;
    unsigned long end = 0;
    if (at_stack < 0 || !range_of(maps_lines[at_stack], &start, &end)) {
      fail("advise", "no maps line holds the stack");
    }
    if (madvise((void*)start, end - start, MADV_RANDOM) != 0) {
      fail("madvise", strerror(errno));
    }
  }
  unsigned long vdso = getauxval(AT_SYSINFO_EHDR);

  slurp("/proc/self/maps", maps_text, sizeof(maps_text));
  slurp("/proc/self/smaps", smaps_text, sizeof(smaps_text));
  int numa_absent = numa_maps_absent();
  size_t maps_count = split_lines(maps_text, maps_lines);
  size_t smaps_count = split_lines(smaps_text, smaps_lines);

  print_kind("anonymous", maps_lines, maps_count, (unsigned long)anonymous);
  print_kind("file", maps_lines, maps_count, (unsigned long)file);
  print_kind("stack", maps_lines, maps_count, stack);
  print_kind("heap", maps_lines, maps_count, (unsigned long)heap);
  print_kind("vdso", maps_lines, maps_count, vdso);
  long stack_at = line_holding(maps_lines, maps_count, stack);
  unsigned long stack_start = 0;
  unsigned long stack_end = 0;
  if (stack_at < 0 || !range_of(maps_lines[stack_at], &stack_start, &stack_end)) {
    fail("maps", "no line holds the stack");
  }
  printf("stack range %lx-%lx\n", stack_start, stack_end);

  int protection_key = -1;
  size_t at = 0;
  long stack_line = line_holding(maps_lines, maps_count, stack);
  char stack_labels[1024] = "";
  size_t stack_block = 0;
  size_t stack_block_end = 0;
  for (size_t index = 0; index < maps_count; ++index) {
    char labels[1024];
    if (at >= smaps_count) {
      fail("smaps lacks a block for", maps_lines[index]);
    }
    size_t next =
        check_block(at, smaps_count, maps_lines[index], &protection_key, labels, sizeof(labels));
    if ((long)index == stack_line) {
      snprintf(stack_labels, sizeof(stack_labels), "%s", labels);
      stack_block = at;
      stack_block_end = next;
    }
    at = next;
  }
  // smaps lists the uprobes page a maps rendered from a memory map leaves
  // out (detcore/src/procfs/memory_map.rs).
  while (at < smaps_count && strstr(smaps_lines[at], " [uprobes]") != NULL) {
    char labels[1024];
    at = check_block(at, smaps_count, smaps_lines[at], &protection_key, labels, sizeof(labels));
  }
  if (at != smaps_count) {
    fail("smaps has a block maps does not list", smaps_lines[at]);
  }
  printf("blocks ok\nnuma %s\n", numa_absent ? "absent" : "present");
  printf("accounting nonzero %lu\n", accounting_nonzero);
  printf("stack labels %s\n", stack_labels);
  printf("stack block begin\n");
  for (size_t index = stack_block; index < stack_block_end; ++index) {
    const char* line = smaps_lines[index];
    if (index == stack_block) {
      // The fields after the range, which the backend's layout decides, as
      // are the spaces that pad the name to its column.
      char fields[5][256];
      int got = sscanf(
          line,
          "%*s %255s %255s %255s %255s %255s",
          fields[0],
          fields[1],
          fields[2],
          fields[3],
          fields[4]);
      if (got != 5) {
        fail("the stack's header", line);
      }
      printf(
          "%s %s %s %s %s\n", fields[0], fields[1], fields[2], fields[3], fields[4]);
    } else if (strncmp(line, "Size:", 5) == 0) {
      printf("Size: (the range's)\n");
    } else {
      printf("%s\n", line);
    }
  }
  printf("stack block end\n");
  fflush(stdout);
  return 0;
}
