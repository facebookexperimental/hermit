/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * minimal_env_identity: print the identity a guest's environment names
 * (HOME, USER, LOGNAME) next to the one its user database gives its user ID
 * (getpwuid(getuid())), then exit 0 when they agree and HOME is an existing
 * directory, or 1 naming each disagreement. An unset variable prints as
 * "(unset)".
 */

#include <pwd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static const char* shown(const char* value) {
  return value == NULL ? "(unset)" : value;
}

static int differs(const char* name, const char* value, const char* expected) {
  if (value != NULL && expected != NULL && strcmp(value, expected) == 0) {
    return 0;
  }
  printf("disagrees: %s=%s, user database %s\n", name, shown(value),
         shown(expected));
  return 1;
}

int main(void) {
  const char* home = getenv("HOME");
  const char* user = getenv("USER");
  const char* logname = getenv("LOGNAME");
  uid_t uid = getuid();
  struct passwd* entry = getpwuid(uid);
  const char* pw_name = entry == NULL ? NULL : entry->pw_name;
  const char* pw_dir = entry == NULL ? NULL : entry->pw_dir;
  struct stat status;
  int home_is_directory =
      home != NULL && stat(home, &status) == 0 && S_ISDIR(status.st_mode);

  printf("uid=%u\n", (unsigned)uid);
  printf("HOME=%s\nUSER=%s\nLOGNAME=%s\n", shown(home), shown(user),
         shown(logname));
  printf("pw_name=%s\npw_dir=%s\n", shown(pw_name), shown(pw_dir));
  printf("HOME is a directory: %s\n", home_is_directory ? "yes" : "no");

  int disagreements = 0;
  disagreements += differs("HOME", home, pw_dir);
  disagreements += differs("USER", user, pw_name);
  disagreements += differs("LOGNAME", logname, pw_name);
  if (!home_is_directory) {
    printf("disagrees: HOME is not an existing directory\n");
    ++disagreements;
  }
  if (disagreements != 0) {
    return 1;
  }
  printf("identity agrees\n");
  return 0;
}
