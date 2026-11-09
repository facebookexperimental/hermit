// What a guest finds in /dev/shm: a private, empty tmpfs, unless the run
// keeps the host's. Modes:
//   check NAME  list /dev/shm, describe it, create POSIX shared memory NAME
//               exclusively and use it, and print /dev/shm's mount row
//   hold NAME   create NAME exclusively, then keep it until stdin closes
//   sees NAME   whether an existing NAME can be opened
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/magic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <unistd.h>

static int create_exclusive(const char *name) {
  errno = 0;
  int fd = shm_open(name, O_CREAT | O_EXCL | O_RDWR, 0600);
  printf("shm_open(%s, O_CREAT | O_EXCL): %s\n", name, fd >= 0 ? "created" : strerror(errno));
  return fd;
}

// The last /proc/self/mountinfo row at /dev/shm (the one the guest sees):
// its mount options, filesystem type, and whether its superblock options
// name an owner.
static void print_mount_row(void) {
  FILE *mountinfo = fopen("/proc/self/mountinfo", "r");
  char line[4096], options[256] = "", fstype[64] = "", super[1024] = "";
  while (mountinfo && fgets(line, sizeof(line), mountinfo)) {
    char mountpoint[1024], row_options[256];
    char *separator = strstr(line, " - ");
    if (separator && sscanf(line, "%*s %*s %*s %*s %1023s %255s", mountpoint, row_options) == 2 &&
        strcmp(mountpoint, "/dev/shm") == 0) {
      strcpy(options, row_options);
      sscanf(separator + 3, "%63s %*s %1023s", fstype, super);
    }
  }
  if (mountinfo) {
    fclose(mountinfo);
  }
  printf("mount row: %s %s, owner in superblock options: %s\n", options, fstype,
         strstr(super, "uid=") || strstr(super, "gid=") ? "yes" : "no");
}

static int check(const char *name) {
  DIR *dir = opendir("/dev/shm");
  int entries = 0;
  for (struct dirent *entry; dir && (entry = readdir(dir));) {
    entries += strcmp(entry->d_name, ".") != 0 && strcmp(entry->d_name, "..") != 0;
  }
  if (dir) {
    closedir(dir);
  }
  printf("/dev/shm entries: %d\n", entries);

  struct stat st;
  struct statfs fs;
  if (stat("/dev/shm", &st) != 0 || statfs("/dev/shm", &fs) != 0) {
    perror("/dev/shm");
    return 2;
  }
  printf("/dev/shm mode %04o, tmpfs: %s\n", (unsigned)(st.st_mode & 07777),
         fs.f_type == TMPFS_MAGIC ? "yes" : "no");

  int fd = create_exclusive(name);
  if (fd < 0) {
    return 1;
  }
  char *map = MAP_FAILED;
  if (ftruncate(fd, 4096) == 0) {
    map = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  }
  if (map == MAP_FAILED) {
    perror("ftruncate/mmap");
    return 2;
  }
  strcpy(map, "guest");
  munmap(map, 4096);
  close(fd);
  printf("shared memory written and unlinked: %s\n", shm_unlink(name) == 0 ? "yes" : "no");
  print_mount_row();
  return 0;
}

static int hold(const char *name) {
  int fd = create_exclusive(name);
  if (fd < 0) {
    return 1;
  }
  char byte;
  while (read(STDIN_FILENO, &byte, 1) > 0) {
  }
  printf("released: %s\n", shm_unlink(name) == 0 ? "yes" : "no");
  return 0;
}

static int sees(const char *name) {
  errno = 0;
  int fd = shm_open(name, O_RDWR, 0);
  printf("shm_open(%s): %s\n", name, fd >= 0 ? "opened" : strerror(errno));
  return fd >= 0 ? 0 : 1;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc != 3) {
    fprintf(stderr, "usage: %s check|hold|sees NAME\n", argv[0]);
    return 2;
  }
  if (strcmp(argv[1], "check") == 0) {
    return check(argv[2]);
  }
  if (strcmp(argv[1], "hold") == 0) {
    return hold(argv[2]);
  }
  return sees(argv[2]);
}
