/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * inotify_read_watch DIRECTORY NAME: watch DIRECTORY with inotify, open
 * DIRECTORY/NAME, read it to its end, close it, and print the events the
 * watch queued for NAME, in order, one name per event.
 *
 * Linux queues an inotify event synchronously in the syscall that causes it
 * (fsnotify_open in fs/namei.c, fsnotify_access in fs/read_write.c,
 * fsnotify_close in fs/file_table.c), so a nonblocking read after the close
 * finds every event the guest's own three calls caused, and any other open
 * or read of the file meanwhile: natively, "OPEN ACCESS CLOSE_NOWRITE".
 */

#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <string.h>
#include <sys/inotify.h>
#include <unistd.h>

int main(int argc, char** argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: %s DIRECTORY NAME\n", argv[0]);
    return 2;
  }
  int watch = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  if (watch < 0 || inotify_add_watch(watch, argv[1], IN_ALL_EVENTS) < 0) {
    perror("inotify");
    return 1;
  }
  char path[PATH_MAX];
  snprintf(path, sizeof path, "%s/%s", argv[1], argv[2]);
  int file = open(path, O_RDONLY);
  if (file < 0) {
    perror("open");
    return 1;
  }
  char bytes[4096];
  while (read(file, bytes, sizeof bytes) > 0) {
  }
  close(file);

  char events[16384] __attribute__((aligned(__alignof__(struct inotify_event))));
  ssize_t length = read(watch, events, sizeof events);
  if (length < 0) {
    perror("read inotify");
    return 1;
  }
  static const struct {
    unsigned mask;
    const char* name;
  } names[] = {
      {IN_ACCESS, "ACCESS"},
      {IN_MODIFY, "MODIFY"},
      {IN_ATTRIB, "ATTRIB"},
      {IN_CLOSE_WRITE, "CLOSE_WRITE"},
      {IN_CLOSE_NOWRITE, "CLOSE_NOWRITE"},
      {IN_OPEN, "OPEN"},
  };
  const char* separator = "";
  for (char* at = events; at < events + length;) {
    struct inotify_event* event = (struct inotify_event*)at;
    if (event->len > 0 && strcmp(event->name, argv[2]) == 0) {
      for (size_t index = 0; index < sizeof names / sizeof names[0]; index++) {
        if (event->mask & names[index].mask) {
          printf("%s%s", separator, names[index].name);
          separator = " ";
        }
      }
    }
    at += sizeof(struct inotify_event) + event->len;
  }
  printf("\n");
  return 0;
}
