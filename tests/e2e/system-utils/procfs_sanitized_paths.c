/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

/*
 * Only statm and loadavg have whole-file stability assertions: their valid
 * inputs are replaced wholesale with fixed normalized forms.  The other 16
 * snapshots retain legitimate process identity, virtual time, topology, or
 * host configuration, so whole-file equality would deliberately build a
 * flaky test.  They are checked against sanitizer-specific invariants.
 *
 * The assertions live in this guest.  A two-run comparator cannot by itself
 * defend a control whose purpose is to make both runs agree.
 */

static char error_message[512];

static bool failf(const char *format, ...) {
  va_list args;
  va_start(args, format);
  vsnprintf(error_message, sizeof(error_message), format, args);
  va_end(args);
  return false;
}

static bool rooted_path(char *out, size_t size, const char *root,
                        const char *absolute) {
  int written = root == NULL ? snprintf(out, size, "%s", absolute)
                             : snprintf(out, size, "%s%s", root, absolute);
  return written >= 0 && (size_t)written < size;
}

static char *read_path(const char *root, const char *label,
                       const char *absolute, size_t *length, bool allow_empty) {
  char path[4096];
  if (!rooted_path(path, sizeof(path), root, absolute)) {
    failf("%s path was too long", label);
    return NULL;
  }

  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    failf("%s read failed at %s: %s", label, absolute, strerror(errno));
    return NULL;
  }

  size_t capacity = 4096;
  size_t used = 0;
  char *contents = malloc(capacity + 1);
  if (contents == NULL) {
    close(fd);
    failf("%s allocation failed", label);
    return NULL;
  }

  for (;;) {
    if (used == capacity) {
      if (capacity >= 16 * 1024 * 1024) {
        free(contents);
        close(fd);
        failf("%s exceeded the 16 MiB guest limit", label);
        return NULL;
      }
      capacity *= 2;
      char *grown = realloc(contents, capacity + 1);
      if (grown == NULL) {
        free(contents);
        close(fd);
        failf("%s allocation failed", label);
        return NULL;
      }
      contents = grown;
    }
    ssize_t got = read(fd, contents + used, capacity - used);
    if (got < 0) {
      int saved_errno = errno;
      free(contents);
      close(fd);
      failf("%s read failed at %s: %s", label, absolute, strerror(saved_errno));
      return NULL;
    }
    if (got == 0) {
      break;
    }
    used += (size_t)got;
  }
  close(fd);

  if (used == 0 && !allow_empty) {
    free(contents);
    failf("%s was empty at %s", label, absolute);
    return NULL;
  }
  contents[used] = '\0';
  *length = used;
  return contents;
}

static char *read_required(const char *root, const char *label,
                           const char *absolute, size_t *length) {
  return read_path(root, label, absolute, length, false);
}

static bool parse_u64(const char *text, unsigned long long *value) {
  if (*text == '\0') {
    return false;
  }
  char *end = NULL;
  errno = 0;
  unsigned long long parsed = strtoull(text, &end, 10);
  if (errno != 0 || end == text || *end != '\0') {
    return false;
  }
  *value = parsed;
  return true;
}

static bool require_zero(const char *label, const char *value) {
  unsigned long long parsed = 0;
  if (!parse_u64(value, &parsed)) {
    return failf("%s contained a non-decimal counter: %s", label, value);
  }
  if (parsed != 0) {
    return failf("%s retained a nonzero host counter: %s", label, value);
  }
  return true;
}

static bool write_text(const char *root, const char *absolute,
                       const char *contents) {
  char path[4096];
  if (!rooted_path(path, sizeof(path), root, absolute)) {
    return failf("fixture path was too long");
  }
  int fd = open(path, O_WRONLY | O_TRUNC | O_CLOEXEC);
  if (fd < 0) {
    return failf("cannot mutate fixture %s: %s", absolute, strerror(errno));
  }
  size_t remaining = strlen(contents);
  const char *cursor = contents;
  while (remaining != 0) {
    ssize_t wrote = write(fd, cursor, remaining);
    if (wrote <= 0) {
      int saved_errno = errno;
      close(fd);
      return failf("cannot mutate fixture %s: %s", absolute,
                   strerror(saved_errno));
    }
    cursor += wrote;
    remaining -= (size_t)wrote;
  }
  close(fd);
  return true;
}

static bool check_stable(const char *root, const char *label,
                         const char *absolute, const char *expected) {
  size_t first_length = 0;
  char *first = read_required(root, label, absolute, &first_length);
  if (first == NULL) {
    return false;
  }
  const char *mutate = getenv("PROCFS_PROBE_MUTATE_LABEL");
  if (root != NULL && mutate != NULL && strcmp(mutate, label) == 0 &&
      !write_text(root, absolute, "changed\n")) {
    free(first);
    return false;
  }
  size_t second_length = 0;
  char *second = read_required(root, label, absolute, &second_length);
  if (second == NULL) {
    free(first);
    return false;
  }
  bool equal =
      first_length == second_length && memcmp(first, second, first_length) == 0;
  bool expected_match =
      expected == NULL || (first_length == strlen(expected) &&
                           memcmp(first, expected, first_length) == 0);
  free(first);
  free(second);
  if (!equal) {
    return failf("%s changed between adjacent reads at %s", label, absolute);
  }
  if (!expected_match) {
    return failf("%s did not have its fixed normalized form", label);
  }
  return true;
}

static bool is_zero_kb_field(const char *name) {
  static const char *fields[] = {
      "Rss",           "Pss",        "Pss_Dirty",    "Pss_Anon",
      "Pss_File",      "Pss_Shmem",  "Shared_Clean", "Shared_Dirty",
      "Private_Clean", "Referenced", "KSM",          "SwapPss",
  };
  for (size_t index = 0; index < sizeof(fields) / sizeof(fields[0]); ++index) {
    if (strcmp(name, fields[index]) == 0) {
      return true;
    }
  }
  return false;
}

static bool check_smaps(const char *root) {
  size_t length = 0;
  char *contents =
      read_required(root, "self-smaps", "/proc/self/smaps", &length);
  if (contents == NULL) {
    return false;
  }
  size_t mappings = 0;
  size_t normalized = 0;
  char *save = NULL;
  for (char *line = strtok_r(contents, "\n", &save); line != NULL;
       line = strtok_r(NULL, "\n", &save)) {
    unsigned long long start = 0;
    unsigned long long end = 0;
    if (sscanf(line, "%llx-%llx", &start, &end) == 2 && start < end) {
      ++mappings;
      continue;
    }
    char name[64];
    unsigned long long value = 0;
    char unit[8];
    if (sscanf(line, "%63[^:]: %llu %7s", name, &value, unit) == 3 &&
        is_zero_kb_field(name)) {
      if (value != 0 || strcmp(unit, "kB") != 0) {
        free(contents);
        return failf("self-smaps left %s unnormalized", name);
      }
      ++normalized;
    }
  }
  free(contents);
  return (mappings != 0 && normalized != 0) ||
         failf("self-smaps contained no mapping/accounting evidence");
}

static bool check_statm(const char *root) {
  return check_stable(root, "self-statm", "/proc/self/statm",
                      "0 0 0 0 0 0 0\n");
}

static int io_field_index(const char *name) {
  static const char *fields[] = {"rchar",
                                 "wchar",
                                 "syscr",
                                 "syscw",
                                 "read_bytes",
                                 "write_bytes",
                                 "cancelled_write_bytes"};
  for (size_t index = 0; index < sizeof(fields) / sizeof(fields[0]); ++index) {
    if (strcmp(name, fields[index]) == 0) {
      return (int)index;
    }
  }
  return -1;
}

static bool check_io(const char *root) {
  size_t length = 0;
  char *contents = read_required(root, "self-io", "/proc/self/io", &length);
  if (contents == NULL) {
    return false;
  }
  unsigned seen = 0;
  char *save = NULL;
  for (char *line = strtok_r(contents, "\n", &save); line != NULL;
       line = strtok_r(NULL, "\n", &save)) {
    char name[64];
    char value[64];
    if (sscanf(line, "%63[^:]: %63s", name, value) != 2) {
      continue;
    }
    int index = io_field_index(name);
    if (index >= 0) {
      char label[96];
      snprintf(label, sizeof(label), "self-io %s", name);
      if (!require_zero(label, value)) {
        free(contents);
        return false;
      }
      seen |= 1U << (unsigned)index;
    }
  }
  free(contents);
  return seen == 0x7fU || failf("self-io omitted normalized counters");
}

static bool has_raw_temp_root(const char *root) {
  static const char *prefixes[] = {"/tmpvol/.tmp", "/tmp/.tmp"};
  for (size_t index = 0; index < sizeof(prefixes) / sizeof(prefixes[0]);
       ++index) {
    size_t length = strlen(prefixes[index]);
    if (strncmp(root, prefixes[index], length) != 0) {
      continue;
    }
    const char *suffix = root + length;
    for (int digit = 0; digit < 6; ++digit) {
      if (!isalnum((unsigned char)suffix[digit])) {
        return false;
      }
    }
    return suffix[6] == '\0' || suffix[6] == '/';
  }
  return false;
}

static bool check_mountinfo(const char *root) {
  size_t length = 0;
  char *contents =
      read_required(root, "self-mountinfo", "/proc/self/mountinfo", &length);
  if (contents == NULL) {
    return false;
  }
  bool saw_mount = false;
  bool saw_hermit_root = false;
  char *save_line = NULL;
  for (char *line = strtok_r(contents, "\n", &save_line); line != NULL;
       line = strtok_r(NULL, "\n", &save_line)) {
    if (strstr(line, " - ") == NULL) {
      free(contents);
      return failf("self-mountinfo contained a malformed row");
    }
    char *save_field = NULL;
    char *field = strtok_r(line, " ", &save_field);
    for (int index = 0; index < 3 && field != NULL; ++index) {
      field = strtok_r(NULL, " ", &save_field);
    }
    if (field == NULL) {
      free(contents);
      return failf("self-mountinfo omitted the mount root");
    }
    saw_mount = true;
    if (has_raw_temp_root(field)) {
      free(contents);
      return failf("self-mountinfo leaked a private temporary root");
    }
    if (strncmp(field, "/tmpvol/.hermit", strlen("/tmpvol/.hermit")) == 0) {
      saw_hermit_root = true;
    }
  }
  free(contents);
  return (saw_mount && (root != NULL || saw_hermit_root)) ||
         failf("self-mountinfo did not expose the normalized Hermit root");
}

static bool is_numa_counter(const char *field) {
  const char *equal = strchr(field, '=');
  if (equal == NULL) {
    return false;
  }
  size_t length = (size_t)(equal - field);
  static const char *names[] = {"active", "anon",      "dirty",    "mapped",
                                "mapmax", "swapcache", "writeback"};
  for (size_t index = 0; index < sizeof(names) / sizeof(names[0]); ++index) {
    if (strlen(names[index]) == length &&
        strncmp(field, names[index], length) == 0) {
      return true;
    }
  }
  if (length < 2 || field[0] != 'N') {
    return false;
  }
  for (size_t index = 1; index < length; ++index) {
    if (!isdigit((unsigned char)field[index])) {
      return false;
    }
  }
  return true;
}

static bool check_numa_maps(const char *root) {
  size_t length = 0;
  char *contents =
      read_required(root, "self-numa-maps", "/proc/self/numa_maps", &length);
  if (contents == NULL) {
    return false;
  }
  size_t rows = 0;
  char *save_line = NULL;
  for (char *line = strtok_r(contents, "\n", &save_line); line != NULL;
       line = strtok_r(NULL, "\n", &save_line)) {
    char *save_field = NULL;
    char *field = strtok_r(line, " ", &save_field);
    if (field == NULL) {
      free(contents);
      return failf("self-numa-maps contained an empty row");
    }
    char *end = NULL;
    (void)strtoull(field, &end, 16);
    if (end == field || *end != '\0') {
      free(contents);
      return failf("self-numa-maps contained a malformed mapping address");
    }
    while ((field = strtok_r(NULL, " ", &save_field)) != NULL) {
      if (is_numa_counter(field)) {
        free(contents);
        return failf("self-numa-maps retained host page accounting");
      }
    }
    ++rows;
  }
  free(contents);
  return rows != 0 || failf("self-numa-maps contained no mapping rows");
}

static bool check_arch_status(const char *root) {
  size_t length = 0;
  char *contents = read_required(root, "self-arch-status",
                                 "/proc/self/arch_status", &length);
  if (contents == NULL) {
    return false;
  }
  char *elapsed = strstr(contents, "AVX512_elapsed_ms:");
  if (elapsed != NULL) {
    elapsed += strlen("AVX512_elapsed_ms:");
    while (isspace((unsigned char)*elapsed)) {
      ++elapsed;
    }
    char *end = elapsed;
    while (*end != '\0' && !isspace((unsigned char)*end)) {
      ++end;
    }
    char saved = *end;
    *end = '\0';
    bool valid = strcmp(elapsed, "-1") == 0 || strcmp(elapsed, "0") == 0;
    *end = saved;
    if (!valid) {
      free(contents);
      return failf("self-arch-status retained positive AVX-512 elapsed time");
    }
  }
  free(contents);
  return true;
}

static bool all_numeric_fields_zero(char *cursor, const char *label) {
  char *save = NULL;
  for (char *field = strtok_r(cursor, " \t", &save); field != NULL;
       field = strtok_r(NULL, " \t", &save)) {
    if (!require_zero(label, field)) {
      return false;
    }
  }
  return true;
}

static bool check_system_stat(const char *root) {
  size_t length = 0;
  char *contents = read_required(root, "stat", "/proc/stat", &length);
  if (contents == NULL) {
    return false;
  }
  bool saw_cpu = false;
  unsigned seen = 0;
  static const char *names[] = {"intr",          "ctxt",          "processes",
                                "procs_running", "procs_blocked", "softirq"};
  char *save_line = NULL;
  for (char *line = strtok_r(contents, "\n", &save_line); line != NULL;
       line = strtok_r(NULL, "\n", &save_line)) {
    char *space = strpbrk(line, " \t");
    if (space == NULL) {
      continue;
    }
    *space++ = '\0';
    if (strncmp(line, "cpu", 3) == 0 &&
        (line[3] == '\0' || isdigit((unsigned char)line[3]))) {
      char *save = NULL;
      char *first = strtok_r(space, " \t", &save);
      if (first == NULL || !parse_u64(first, &(unsigned long long){0}) ||
          !all_numeric_fields_zero(save, "stat CPU")) {
        free(contents);
        return false;
      }
      saw_cpu = true;
      continue;
    }
    for (size_t index = 0; index < sizeof(names) / sizeof(names[0]); ++index) {
      if (strcmp(line, names[index]) == 0) {
        if (!all_numeric_fields_zero(space, "stat counter")) {
          free(contents);
          return false;
        }
        seen |= 1U << index;
      }
    }
  }
  free(contents);
  return (saw_cpu && seen == 0x3fU) || failf("stat omitted normalized rows");
}

static bool check_two_column_zeros(const char *root, const char *label,
                                   const char *absolute) {
  size_t length = 0;
  char *contents = read_required(root, label, absolute, &length);
  if (contents == NULL) {
    return false;
  }
  size_t rows = 0;
  char *save = NULL;
  for (char *line = strtok_r(contents, "\n", &save); line != NULL;
       line = strtok_r(NULL, "\n", &save)) {
    char name[256];
    char value[64];
    char extra[2];
    if (sscanf(line, "%255s %63s %1s", name, value, extra) != 2 ||
        !require_zero(label, value)) {
      free(contents);
      return false;
    }
    ++rows;
  }
  free(contents);
  return rows != 0 || failf("%s contained no counter rows", label);
}

static bool check_zoneinfo(const char *root) {
  size_t length = 0;
  char *contents = read_required(root, "zoneinfo", "/proc/zoneinfo", &length);
  if (contents == NULL) {
    return false;
  }
  static const char *names[] = {"pages free", "min", "low", "high", "managed"};
  unsigned seen = 0;
  char *save = NULL;
  for (char *line = strtok_r(contents, "\n", &save); line != NULL;
       line = strtok_r(NULL, "\n", &save)) {
    while (isspace((unsigned char)*line)) {
      ++line;
    }
    for (size_t index = 0; index < sizeof(names) / sizeof(names[0]); ++index) {
      size_t name_length = strlen(names[index]);
      if (strncmp(line, names[index], name_length) == 0 &&
          isspace((unsigned char)line[name_length])) {
        char *value = line + name_length;
        while (isspace((unsigned char)*value)) {
          ++value;
        }
        if (!require_zero("zoneinfo", value)) {
          free(contents);
          return false;
        }
        seen |= 1U << index;
      }
    }
  }
  free(contents);
  return seen == 0x1fU || failf("zoneinfo omitted normalized fields");
}

static bool check_diskstats(const char *root) {
  size_t length = 0;
  char *contents = read_required(root, "diskstats", "/proc/diskstats", &length);
  if (contents == NULL) {
    return false;
  }
  size_t rows = 0;
  char *save_line = NULL;
  for (char *line = strtok_r(contents, "\n", &save_line); line != NULL;
       line = strtok_r(NULL, "\n", &save_line)) {
    char *save_field = NULL;
    char *field = strtok_r(line, " \t", &save_field);
    for (int index = 0; index < 3 && field != NULL; ++index) {
      field = strtok_r(NULL, " \t", &save_field);
    }
    size_t counter = 0;
    for (; field != NULL;
         field = strtok_r(NULL, " \t", &save_field), ++counter) {
      unsigned long long actual = 0;
      unsigned long long expected = counter == 0 || counter == 4   ? 1
                                    : counter == 2 || counter == 6 ? 8
                                                                   : 0;
      if (!parse_u64(field, &actual) || actual != expected) {
        free(contents);
        return failf("diskstats retained an unexpected counter");
      }
    }
    if (counter == 0) {
      free(contents);
      return failf("diskstats contained a malformed row");
    }
    ++rows;
  }
  free(contents);
  return rows != 0 || failf("diskstats contained no device rows");
}

static bool check_modules(const char *root) {
  size_t length = 0;
  char *contents = read_path(root, "modules", "/proc/modules", &length, true);
  if (contents == NULL) {
    return false;
  }
  char *save_line = NULL;
  for (char *line = strtok_r(contents, "\n", &save_line); line != NULL;
       line = strtok_r(NULL, "\n", &save_line)) {
    char *fields[4] = {0};
    char *save_field = NULL;
    for (size_t index = 0; index < 4; ++index) {
      fields[index] = strtok_r(index == 0 ? line : NULL, " \t", &save_field);
    }
    if (fields[3] == NULL) {
      free(contents);
      return failf("modules contained a malformed row");
    }
    unsigned long long use_count = 0;
    if (!parse_u64(fields[2], &use_count)) {
      free(contents);
      return failf("modules contained a malformed use count");
    }
    size_t holders = 0;
    if (strcmp(fields[3], "-") != 0) {
      const char *cursor = fields[3];
      while (*cursor != '\0') {
        while (*cursor == ',') {
          ++cursor;
        }
        if (*cursor == '\0') {
          break;
        }
        ++holders;
        while (*cursor != '\0' && *cursor != ',') {
          ++cursor;
        }
      }
    }
    if (use_count != holders) {
      free(contents);
      return failf("modules retained a host reference count");
    }
  }
  free(contents);
  return true;
}

static bool check_swaps(const char *root) {
  size_t length = 0;
  char *contents = read_required(root, "swaps", "/proc/swaps", &length);
  if (contents == NULL) {
    return false;
  }
  char *save_line = NULL;
  char *line = strtok_r(contents, "\n", &save_line);
  if (line == NULL ||
      strcmp(line, "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority") != 0) {
    free(contents);
    return failf("swaps contained a malformed header");
  }
  while ((line = strtok_r(NULL, "\n", &save_line)) != NULL) {
    char *save_field = NULL;
    char *field = strtok_r(line, " \t", &save_field);
    for (int index = 0; index < 3 && field != NULL; ++index) {
      field = strtok_r(NULL, " \t", &save_field);
    }
    if (field == NULL || !require_zero("swaps Used", field)) {
      free(contents);
      return false;
    }
  }
  free(contents);
  return true;
}

static bool check_interrupts(const char *root, const char *label,
                             const char *absolute) {
  size_t length = 0;
  char *contents = read_path(root, label, absolute, &length, true);
  if (contents == NULL) {
    return false;
  }
  char *save_line = NULL;
  for (char *line = strtok_r(contents, "\n", &save_line); line != NULL;
       line = strtok_r(NULL, "\n", &save_line)) {
    char *colon = strchr(line, ':');
    if (colon == NULL) {
      continue;
    }
    char *save_field = NULL;
    char *field = strtok_r(colon + 1, " \t", &save_field);
    size_t counters = 0;
    while (field != NULL) {
      unsigned long long value = 0;
      if (!parse_u64(field, &value)) {
        break;
      }
      if (value != 0) {
        free(contents);
        return failf("%s retained a host interrupt counter", label);
      }
      ++counters;
      field = strtok_r(NULL, " \t", &save_field);
    }
  }
  free(contents);
  return true;
}

static bool check_buddyinfo(const char *root) {
  size_t length = 0;
  char *contents = read_required(root, "buddyinfo", "/proc/buddyinfo", &length);
  if (contents == NULL) {
    return false;
  }
  size_t rows = 0;
  char *save_line = NULL;
  for (char *line = strtok_r(contents, "\n", &save_line); line != NULL;
       line = strtok_r(NULL, "\n", &save_line)) {
    char *save_field = NULL;
    char *field = strtok_r(line, " \t", &save_field);
    for (int index = 0; index < 4 && field != NULL; ++index) {
      field = strtok_r(NULL, " \t", &save_field);
    }
    if (field == NULL || !require_zero("buddyinfo", field) ||
        !all_numeric_fields_zero(save_field, "buddyinfo")) {
      free(contents);
      return false;
    }
    ++rows;
  }
  free(contents);
  return rows != 0 || failf("buddyinfo contained no zone rows");
}

static bool check_schedstat(const char *root) {
  size_t length = 0;
  char *contents = read_required(root, "schedstat", "/proc/schedstat", &length);
  if (contents == NULL) {
    return false;
  }
  bool saw_timestamp = false;
  bool saw_cpu = false;
  char *save_line = NULL;
  for (char *line = strtok_r(contents, "\n", &save_line); line != NULL;
       line = strtok_r(NULL, "\n", &save_line)) {
    char *save_field = NULL;
    char *name = strtok_r(line, " \t", &save_field);
    if (name == NULL) {
      continue;
    }
    if (strcmp(name, "timestamp") == 0) {
      if (!all_numeric_fields_zero(save_field, "schedstat timestamp")) {
        free(contents);
        return false;
      }
      saw_timestamp = true;
    } else if (strncmp(name, "cpu", 3) == 0 &&
               isdigit((unsigned char)name[3])) {
      if (!all_numeric_fields_zero(save_field, "schedstat CPU")) {
        free(contents);
        return false;
      }
      saw_cpu = true;
    } else if (strncmp(name, "domain", 6) == 0 &&
               isdigit((unsigned char)name[6])) {
      (void)strtok_r(NULL, " \t", &save_field);
      (void)strtok_r(NULL, " \t", &save_field);
      if (!all_numeric_fields_zero(save_field, "schedstat domain")) {
        free(contents);
        return false;
      }
    }
  }
  free(contents);
  return (saw_timestamp && saw_cpu) ||
         failf("schedstat omitted timestamp or CPU rows");
}

static bool check_key_users(const char *root) {
  size_t length = 0;
  char *contents = read_required(root, "key-users", "/proc/key-users", &length);
  if (contents == NULL) {
    return false;
  }
  size_t rows = 0;
  char *save_line = NULL;
  for (char *line = strtok_r(contents, "\n", &save_line); line != NULL;
       line = strtok_r(NULL, "\n", &save_line)) {
    char uid[64];
    char usage[64];
    char counts[64];
    char key_quota[64];
    char byte_quota[64];
    if (sscanf(line, "%63s %63s %63s %63s %63s", uid, usage, counts, key_quota,
               byte_quota) != 5 ||
        uid[strlen(uid) - 1] != ':' ||
        !require_zero("key-users usage", usage) || strcmp(counts, "0/0") != 0) {
      free(contents);
      return failf("key-users retained host usage or key counts");
    }
    char *slash = strchr(key_quota, '/');
    char *byte_slash = strchr(byte_quota, '/');
    if (slash == NULL || byte_slash == NULL) {
      free(contents);
      return failf("key-users contained a malformed quota");
    }
    *slash = '\0';
    *byte_slash = '\0';
    if (!require_zero("key-users key quota", key_quota) ||
        !require_zero("key-users byte quota", byte_quota)) {
      free(contents);
      return false;
    }
    ++rows;
  }
  free(contents);
  return rows != 0 || failf("key-users contained no user rows");
}

static bool run_check(const char *root, const char *label,
                      bool (*check)(const char *)) {
  if (!check(root)) {
    return false;
  }
  const char *suffix =
      strcmp(label, "self-statm") == 0 || strcmp(label, "loadavg") == 0
          ? "normalized-and-stable"
          : "normalized";
  printf("%s=%s\n", label, suffix);
  return true;
}

static bool check_loadavg(const char *root) {
  return check_stable(root, "loadavg", "/proc/loadavg",
                      "0.00 0.00 0.00 1/1 1\n");
}

static bool check_vmstat(const char *root) {
  return check_two_column_zeros(root, "vmstat", "/proc/vmstat");
}

static bool check_interrupts_file(const char *root) {
  return check_interrupts(root, "interrupts", "/proc/interrupts");
}

static bool check_softirqs(const char *root) {
  return check_interrupts(root, "softirqs", "/proc/softirqs");
}

static bool check_all(const char *root) {
  struct Check {
    const char *label;
    bool (*function)(const char *);
  } checks[] = {
      {"self-smaps", check_smaps},
      {"self-statm", check_statm},
      {"self-io", check_io},
      {"self-mountinfo", check_mountinfo},
      {"self-numa-maps", check_numa_maps},
      {"self-arch-status", check_arch_status},
      {"stat", check_system_stat},
      {"vmstat", check_vmstat},
      {"zoneinfo", check_zoneinfo},
      {"loadavg", check_loadavg},
      {"diskstats", check_diskstats},
      {"modules", check_modules},
      {"swaps", check_swaps},
      {"interrupts", check_interrupts_file},
      {"softirqs", check_softirqs},
      {"buddyinfo", check_buddyinfo},
      {"schedstat", check_schedstat},
      {"key-users", check_key_users},
  };
  for (size_t index = 0; index < sizeof(checks) / sizeof(checks[0]); ++index) {
    if (!run_check(root, checks[index].label, checks[index].function)) {
      return false;
    }
  }
  return true;
}

int main(int argc, char **argv) {
  const char *root = NULL;
  if (argc == 3 && strcmp(argv[1], "--fixture-root") == 0) {
    root = argv[2];
  } else if (argc != 1 && !(argc == 2 && strcmp(argv[1], "--run") == 0)) {
    fprintf(stderr, "usage: %s [--run|--fixture-root DIR]\n", argv[0]);
    return 2;
  }
  if (!check_all(root)) {
    fprintf(stderr, "procfs-sanitized-paths FAIL: %s\n", error_message);
    return 1;
  }
  return 0;
}
