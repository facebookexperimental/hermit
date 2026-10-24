// Descriptor-table churn across threads that share one table.
//
// A backend may keep its own copy of a process's descriptor table (KVM
// republishes it after every change, copying only the entries that changed;
// https://github.com/rrnewton/hermit/issues/4287). These checks require that an
// entry other threads hold keeps behaving as one open file description while
// another thread churns the table:
//
//   eof:   a thread blocks reading a pipe while the main thread opens and
//          closes files, then writes one byte and closes the last write end;
//          the reader sees the byte and then end of file.
//   epoll: an epoll registration made before the churn is modified, waited on
//          and deleted after a churning thread has republished the table.
//   dup:   while another thread churns, a dup3 onto a live descriptor in one
//          thread is seen at its new offset in another.
//
// A descriptor received through SCM_RIGHTS and read from another thread
// belongs here too, but a received descriptor cannot be read under Hermit at
// all yet (https://github.com/rrnewton/hermit/issues/4107).
//
// It runs the three in this order, prints one line for each, and exits 0
// only if every check holds. The churn file lives in /tmp.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <unistd.h>

#define CHURN 32

static char churn_path[256];

static void die(const char* what) {
  fprintf(stderr, "%s: %s\n", what, strerror(errno));
  exit(2);
}

// Open and close a file CHURN times, changing the shared table each time.
static void churn(void) {
  for (int i = 0; i < CHURN; i++) {
    int fd = open(churn_path, O_RDONLY | O_CLOEXEC);
    if (fd < 0)
      die("open churn file");
    if (close(fd) != 0)
      die("close churn file");
  }
}

static void* churn_thread(void* unused) {
  (void)unused;
  churn();
  return NULL;
}

static int pipe_read_end;
static char eof_seen[16];

static void* eof_reader(void* unused) {
  (void)unused;
  char byte = 0;
  ssize_t first = read(pipe_read_end, &byte, 1);
  ssize_t second = read(pipe_read_end, &byte, 1);
  snprintf(eof_seen, sizeof eof_seen, "%zd,%zd", first, second);
  return NULL;
}

static int mode_eof(void) {
  int fds[2];
  if (pipe2(fds, O_CLOEXEC) != 0)
    die("pipe2");
  pipe_read_end = fds[0];
  pthread_t reader;
  if (pthread_create(&reader, NULL, eof_reader, NULL) != 0)
    die("pthread_create");
  churn();
  if (write(fds[1], "x", 1) != 1)
    die("write");
  churn();
  if (close(fds[1]) != 0)
    die("close write end");
  if (pthread_join(reader, NULL) != 0)
    die("pthread_join");
  printf("eof reads=%s\n", eof_seen);
  return strcmp(eof_seen, "1,0") == 0 ? 0 : 1;
}

static int mode_epoll(void) {
  int fds[2];
  if (pipe2(fds, O_CLOEXEC | O_NONBLOCK) != 0)
    die("pipe2");
  int epfd = epoll_create1(EPOLL_CLOEXEC);
  if (epfd < 0)
    die("epoll_create1");
  struct epoll_event event = {.events = EPOLLIN, .data.u32 = 7};
  if (epoll_ctl(epfd, EPOLL_CTL_ADD, fds[0], &event) != 0)
    die("epoll add");
  pthread_t churner;
  if (pthread_create(&churner, NULL, churn_thread, NULL) != 0)
    die("pthread_create");
  if (pthread_join(churner, NULL) != 0)
    die("pthread_join");
  event.events = EPOLLIN | EPOLLRDHUP;
  event.data.u32 = 9;
  int modified = epoll_ctl(epfd, EPOLL_CTL_MOD, fds[0], &event);
  if (write(fds[1], "y", 1) != 1)
    die("write");
  struct epoll_event ready = {0};
  int count = epoll_wait(epfd, &ready, 1, 0);
  if (pthread_create(&churner, NULL, churn_thread, NULL) != 0)
    die("pthread_create");
  if (pthread_join(churner, NULL) != 0)
    die("pthread_join");
  int deleted = epoll_ctl(epfd, EPOLL_CTL_DEL, fds[0], NULL);
  int after = epoll_wait(epfd, &ready, 1, 0);
  printf(
      "epoll mod=%d ready=%d data=%u del=%d after=%d\n",
      modified,
      count,
      count == 1 ? ready.data.u32 : 0,
      deleted,
      after);
  return modified == 0 && count == 1 && deleted == 0 && after == 0 ? 0 : 1;
}

static off_t dup_offset = -1;

static int dup_target;

static void* dup_observer(void* unused) {
  (void)unused;
  dup_offset = lseek(dup_target, 0, SEEK_CUR);
  return NULL;
}

static int mode_dup(void) {
  pthread_t churner, observer;
  if (pthread_create(&churner, NULL, churn_thread, NULL) != 0)
    die("pthread_create");
  // dup3 a file positioned at offset 2 onto a live descriptor while the
  // churner runs, then read the offset from another thread.
  int positioned = open(churn_path, O_RDONLY | O_CLOEXEC);
  dup_target = open(churn_path, O_RDONLY | O_CLOEXEC);
  if (positioned < 0 || dup_target < 0)
    die("open dup files");
  if (lseek(positioned, 2, SEEK_SET) != 2)
    die("lseek");
  if (dup3(positioned, dup_target, O_CLOEXEC) != dup_target)
    die("dup3");
  if (pthread_join(churner, NULL) != 0)
    die("pthread_join");
  if (pthread_create(&observer, NULL, dup_observer, NULL) != 0)
    die("pthread_create");
  if (pthread_join(observer, NULL) != 0)
    die("pthread_join");
  printf("dup offset=%lld\n", (long long)dup_offset);
  return dup_offset == 2 ? 0 : 1;
}

int main(void) {
  snprintf(churn_path, sizeof churn_path, "/tmp/threads-fd-churn-%d", (int)getpid());
  int fd = open(churn_path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
  if (fd < 0 || write(fd, "abcdefgh", 8) != 8 || close(fd) != 0)
    die("create churn file");
  int failed = mode_eof();
  failed |= mode_epoll();
  failed |= mode_dup();
  if (unlink(churn_path) != 0)
    die("unlink churn file");
  return failed;
}
