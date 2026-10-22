/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * Licensed under the BSD-style license in the root LICENSE file.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static char snapshot[65536];

static int check_map(const char *path, void *address, const char *wanted,
                     const char *unwanted) {
  FILE *file = fopen(path, "r");
  if (file == NULL)
    return 1;
  size_t size = fread(snapshot, 1, sizeof(snapshot) - 1, file);
  int failed = ferror(file) || !feof(file);
  failed |= fclose(file) != 0;
  snapshot[size] = 0;
  if (failed || strstr(snapshot, unwanted) != NULL)
    return 2;
  for (char *line = snapshot; line != NULL && *line != 0;) {
    char *next = strchr(line, '\n');
    if (next != NULL)
      *next++ = 0;
    unsigned long start, end;
    if (strstr(line, wanted) != NULL &&
        sscanf(line, "%lx-%lx", &start, &end) == 2 &&
        start <= (unsigned long)address && (unsigned long)address < end)
      return 0;
    line = next;
  }
  return 3;
}

int main(int argc, char **argv) {
  if (argc == 3 && strcmp(argv[1], "after-exec") == 0) {
    char byte;
    if (read(atoi(argv[2]), &byte, 1) != 0)
      return 22;
    puts("old maps descriptor has no replacement image");
    return 0;
  }
  if (argc == 2 && strcmp(argv[1], "old-exec") == 0) {
    int old = open("/proc/self/maps", O_RDONLY);
    if (old < 0)
      return 23;
    char number[32];
    snprintf(number, sizeof(number), "%d", old);
    execl(argv[0], argv[0], "after-exec", number, (char *)NULL);
    return 24;
  }
  int parent_file = memfd_create("maps-parent-visible", 0);
  int child_file = memfd_create("maps-child-visible", 0);
  if (parent_file < 0 || child_file < 0 || ftruncate(parent_file, 4096) ||
      ftruncate(child_file, 4096))
    return 10;
  void *address = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED,
                       parent_file, 0);
  if (address == MAP_FAILED)
    return 11;
  int inherited = open("/proc/self/maps", O_RDONLY);
  if (inherited < 0)
    return 18;
  int ready[2], release[2];
  if (pipe(ready) || pipe(release))
    return 12;
  pid_t child = fork();
  if (child < 0)
    return 13;
  if (child == 0) {
    close(ready[0]);
    close(release[1]);
    if (mmap(address, 4096, PROT_READ | PROT_WRITE,
             MAP_SHARED | MAP_FIXED, child_file, 0) != address)
      _exit(14);
    close(parent_file);
    close(child_file);
    // Read a dup of the inherited description: it still names the parent's mm.
    int alias = dup(inherited);
    FILE *held = alias < 0 ? NULL : fdopen(alias, "r");
    if (held == NULL)
      _exit(19);
    size_t size = fread(snapshot, 1, sizeof(snapshot) - 1, held);
    int bad = ferror(held) || !feof(held);
    bad |= fclose(held) != 0;
    snapshot[size] = 0;
    bad |= strstr(snapshot, "maps-parent-visible") == NULL;
    bad |= strstr(snapshot, "maps-child-visible") != NULL;
    close(inherited);
    if (bad)
      _exit(20);
    char byte = 'r';
    if (write(ready[1], &byte, 1) != 1)
      _exit(15);
    close(ready[1]);
    int ok = read(release[0], &byte, 1) == 1;
    close(release[0]);
    _exit(ok ? 0 : 16);
  }
  close(ready[1]);
  close(release[0]);
  close(parent_file);
  close(child_file);
  char byte;
  int ready_ok = read(ready[0], &byte, 1) == 1;
  int failed = !ready_ok;
  close(inherited);
  close(ready[0]);
  char own[64], foreign[64], task[96];
  snprintf(own, sizeof(own), "/proc/%d/maps", getpid());
  snprintf(foreign, sizeof(foreign), "/proc/%d/maps", child);
  snprintf(task, sizeof(task), "/proc/%d/task/%d/maps", child, child);
  if (!failed) {
    const char *paths[] = {"/proc/self/maps", "/proc/thread-self/maps", own,
                           foreign, task};
    for (size_t i = 0; i < sizeof(paths) / sizeof(paths[0]); ++i) {
      int remote = i >= 3;
      failed |= check_map(paths[i], address,
                          remote ? "maps-child-visible" : "maps-parent-visible",
                          remote ? "maps-parent-visible" : "maps-child-visible");
    }
  }
  byte = 'x';
  if (ready_ok)
    failed |= write(release[1], &byte, 1) != 1;
  close(release[1]);
  int status = 0;
  failed |= waitpid(child, &status, 0) != child;
  failed |= !WIFEXITED(status) || WEXITSTATUS(status) != 0;
  if (failed)
    return 17;
  puts("maps aliases retain the target's guest mapping");
  return 0;
}
