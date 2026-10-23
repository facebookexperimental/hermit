/* Native discriminator for the real random-copy fixture's credential premise.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
  int ready[2], proceed[2];
  if (pipe(ready) || pipe(proceed))
    return 90;
  volatile unsigned char byte = 165;
  pid_t child = fork();
  if (child < 0)
    return 91;
  if (!child) {
    close(ready[0]);
    close(proceed[1]);
    unsigned char token = 1;
    unsigned char random_bytes[8] = {165, 165, 165, 165, 165, 165, 165, 165};
    struct iovec random_vector = {random_bytes, 7};
    int random_fd = open("/dev/urandom", O_RDONLY);
    if (random_fd < 0)
      _exit(100);
    if (prctl(PR_SET_DUMPABLE, 1) || write(ready[1], &token, 1) != 1)
      _exit(92);
    if (read(proceed[0], &token, 1) != 1 || byte != 73)
      _exit(93);
    if (prctl(PR_SET_DUMPABLE, 0) || write(ready[1], &token, 1) != 1)
      _exit(94);
    if (read(proceed[0], &token, 1) != 1 || byte != 73)
      _exit(95);
    errno = 0;
    if (syscall(SYS_readv, random_fd, &random_vector, 1) != 7 || errno != 0 ||
        random_bytes[7] != 165)
      _exit(101);
    if (close(random_fd))
      _exit(102);
    _exit(0);
  }
  close(ready[1]);
  close(proceed[0]);
  unsigned char token = 1, value = 73;
  struct iovec local = {&value, 1}, remote = {(void*)(uintptr_t)&byte, 1};
  int failed = 0;
  for (int dumpable = 1; dumpable >= 0; --dumpable) {
    if (read(ready[0], &token, 1) != 1) {
      failed = 96;
      break;
    }
    errno = 0;
    ssize_t result = process_vm_writev(child, &local, 1, &remote, 1, 0);
    int saved_errno = errno;
    printf(
        "NATIVE_COPY dumpable=%d result=%zd errno=%d\n",
        dumpable,
        result,
        saved_errno);
    if ((dumpable && (result != 1 || saved_errno != 0)) ||
        (!dumpable && (result != -1 || saved_errno != EPERM)))
      failed = 97;
    unsigned char observed = 165;
    struct iovec read_local = {&observed, 1};
    errno = 0;
    ssize_t read_result =
        process_vm_readv(child, &read_local, 1, &remote, 1, 0);
    int read_errno = errno;
    printf(
        "NATIVE_IMPORT dumpable=%d result=%zd errno=%d byte=%u\n",
        dumpable,
        read_result,
        read_errno,
        observed);
    if ((dumpable && (read_result != 1 || read_errno != 0 || observed != 73)) ||
        (!dumpable &&
         (read_result != -1 || read_errno != EPERM || observed != 165)))
      failed = 103;
    if (write(proceed[1], &token, 1) != 1) {
      failed = 98;
      break;
    }
    value = 91; /* A denied second write must preserve the first value. */
  }
  close(ready[0]);
  close(proceed[1]);
  int status = 0;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0)
    return 99;
  printf("NATIVE_COPY child_exit=0 byte_preserved=1\n");
  printf("NATIVE_READV nondumpable=1 result=7 errno=0 sentinel=165\n");
  return failed;
}
