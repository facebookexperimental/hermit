/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A shared library whose constructor changes the in-guest LiteInst runtime's
 * settings before the runtime's own constructor runs: the program links it,
 * and the dynamic loader initializes a program's libraries before the
 * preloaded runtime. IN_GUEST_TRAP_FIXTURE_CONSTRUCTOR selects what it does:
 *
 * - `set`: sets REVERIE_LITEINST_SITE_PATCHING=1;
 * - `unset`: removes REVERIE_LITEINST_SITE_PATCHING;
 * - `set-then-restore`: sets it to 1, and the library's `getenv`, which
 *   interposes on the C library's, sets it back to 0 at the first lookup of
 *   another name after the runtime has looked up
 *   REVERIE_LITEINST_SITE_PATCHING: the runtime reads its settings through
 *   Rust's std::env, which calls getenv, so that lookup comes after it has
 *   read 1 and before it constructs its Tool. (The hook used to be the
 *   runtime's mprotect of the vDSO, which no longer goes through the C
 *   library.);
 * - `no-stats`: removes REVERIE_LITEINST_STATS_COORDINATOR, so the runtime
 *   collects no statistics;
 * - anything else: changes nothing.
 */

#define _GNU_SOURCE
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static const char* mode = "none";
static int restore_pending = 0;
static int site_patching_read = 0;
static int restored = 0;

__attribute__((constructor)) static void change_settings(void) {
  const char* requested = getenv("IN_GUEST_TRAP_FIXTURE_CONSTRUCTOR");
  if (requested == NULL) {
    return;
  }
  if (strcmp(requested, "set") == 0) {
    setenv("REVERIE_LITEINST_SITE_PATCHING", "1", 1);
    mode = "set";
  } else if (strcmp(requested, "unset") == 0) {
    unsetenv("REVERIE_LITEINST_SITE_PATCHING");
    mode = "unset";
  } else if (strcmp(requested, "set-then-restore") == 0) {
    setenv("REVERIE_LITEINST_SITE_PATCHING", "1", 1);
    restore_pending = 1;
    mode = "set-then-restore";
  } else if (strcmp(requested, "no-stats") == 0) {
    unsetenv("REVERIE_LITEINST_STATS_COORDINATOR");
    mode = "no-stats";
  }
}

extern char** environ;

/* Looks the name up in the environment block itself, as the C library's
 * getenv does, so it needs no other symbol. */
char* getenv(const char* name) {
  static const char site_patching[] = "REVERIE_LITEINST_SITE_PATCHING";
  if (restore_pending) {
    if (strcmp(name, site_patching) == 0) {
      site_patching_read = 1;
    } else if (site_patching_read) {
      restore_pending = 0;
      setenv(site_patching, "0", 1);
      restored = 1;
    }
  }
  size_t length = strlen(name);
  for (char** entry = environ; entry != NULL && *entry != NULL; entry++) {
    if (strncmp(*entry, name, length) == 0 && (*entry)[length] == '=') {
      return *entry + length + 1;
    }
  }
  return NULL;
}

const char* in_guest_trap_fixture_mode(void) {
  return mode;
}

int in_guest_trap_fixture_restored(void) {
  return restored;
}
