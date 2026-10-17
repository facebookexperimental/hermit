/* Observe the descriptor inherited at fd 0 without replacing it. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(int argc, char** argv) {
  struct stat st;
  const char* kind = "closed";
  if (fstat(STDIN_FILENO, &st) == 0) {
    kind = S_ISREG(st.st_mode) ? "file"
        : S_ISFIFO(st.st_mode) ? "fifo"
        : S_ISSOCK(st.st_mode) ? "socket"
        : S_ISCHR(st.st_mode) ? "character" : "other";
  } else if (errno != EBADF) {
    return 2;
  }
  int flags = fcntl(STDIN_FILENO, F_GETFL);
  int tty = isatty(STDIN_FILENO);
  errno = 0;
  off_t offset = lseek(STDIN_FILENO, 0, SEEK_CUR);
  int seek_error = errno;
  printf("kind=%s access=%d nonblock=%d tty=%d seek=%lld error=%d\n",
      kind, flags < 0 ? -1 : flags & O_ACCMODE,
      flags < 0 ? -1 : !!(flags & O_NONBLOCK), tty,
      (long long)offset, seek_error);
  if (argc == 2 && strcmp(argv[1], "read") == 0) {
    unsigned char byte = 0;
    errno = 0;
    ssize_t count = read(STDIN_FILENO, &byte, 1);
    int read_error = errno;
    errno = 0;
    offset = lseek(STDIN_FILENO, 0, SEEK_CUR);
    seek_error = errno;
    printf("read=%lld byte=%u error=%d seek=%lld seek_error=%d\n",
        (long long)count, byte, read_error, (long long)offset, seek_error);
  }
  return 0;
}
