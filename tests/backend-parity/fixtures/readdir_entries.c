// Backend-parity contract: directory enumeration via readdir/getdents.
//
// Creates a temporary directory with three known regular files, enumerates it
// with opendir/readdir (which is backed by getdents64), sorts the returned
// names in the fixture so the result is independent of on-disk directory
// order, and verifies the count, the sorted names, and that each entry is a
// regular file. The temporary tree is removed before exit so repeated (and
// --verify) runs are idempotent.
//
// _GNU_SOURCE is supplied by the harness compile flags (see run_matrix.py);
// do not define it here (it would collide with -D_GNU_SOURCE under -Werror).
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

enum { PATH_CAP = 128, JOIN_CAP = 64, MAX_ENTRIES = 16 };

static int name_cmp(const void *a, const void *b) {
  return strcmp(*(const char *const *)a, *(const char *const *)b);
}

// Create a one-byte regular file at path; return 0 on success, -1 otherwise.
static int make_file(const char *path) {
  int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
  if (fd < 0) {
    return -1;
  }
  ssize_t n = write(fd, "x", 1);
  if (close(fd) != 0 || n != 1) {
    return -1;
  }
  return 0;
}

int main(void) {
  int ok = 0;
  char joined[JOIN_CAP] = {0};

  char root[] = "/tmp/rddirXXXXXX";
  if (mkdtemp(root) == NULL) {
    printf("readdir ok=%d names=%s\n", ok, joined);
    return 0;
  }

  // Create the entries in an order that is NOT the sorted order.
  const char *want[] = {"gamma", "alpha", "beta"};
  int created = 0;
  for (int i = 0; i < 3; i++) {
    char path[PATH_CAP];
    snprintf(path, sizeof path, "%s/%s", root, want[i]);
    if (make_file(path) == 0) {
      created++;
    }
  }

  char *names[MAX_ENTRIES];
  int n = 0;
  DIR *dir = opendir(root);
  if (dir != NULL) {
    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL && n < MAX_ENTRIES) {
      if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0) {
        continue;
      }
      names[n++] = strdup(entry->d_name);
    }
    closedir(dir);
  }

  if (created == 3 && n == 3) {
    ok++; // 1: exactly the three created entries are enumerated
  }

  qsort(names, n, sizeof names[0], name_cmp);
  if (n == 3 && strcmp(names[0], "alpha") == 0 &&
      strcmp(names[1], "beta") == 0 && strcmp(names[2], "gamma") == 0) {
    ok++; // 2: sorted enumeration matches the known set
  }

  int all_regular = (n == 3);
  for (int i = 0; i < n; i++) {
    char path[PATH_CAP];
    snprintf(path, sizeof path, "%s/%s", root, names[i]);
    struct stat st;
    if (stat(path, &st) != 0 || !S_ISREG(st.st_mode)) {
      all_regular = 0;
    }
  }
  if (all_regular) {
    ok++; // 3: every entry resolves to a regular file
  }

  for (int i = 0; i < n; i++) {
    strncat(joined, names[i], sizeof joined - strlen(joined) - 1);
    if (i + 1 < n) {
      strncat(joined, ",", sizeof joined - strlen(joined) - 1);
    }
  }

  // Remove the temporary tree so repeated runs are idempotent.
  for (int i = 0; i < n; i++) {
    char path[PATH_CAP];
    snprintf(path, sizeof path, "%s/%s", root, names[i]);
    unlink(path);
    free(names[i]);
  }
  rmdir(root);

  printf("readdir ok=%d names=%s\n", ok, joined);
  return 0;
}
