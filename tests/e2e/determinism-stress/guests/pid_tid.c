/* @lint-ignore-every LICENSELINT */

#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

enum { THREADS = 4 };

struct thread_result {
  pid_t pid;
  pid_t tid;
};

static void *thread_main(void *opaque) {
  struct thread_result *result = opaque;
  result->pid = getpid();
  result->tid = (pid_t)syscall(SYS_gettid);
  return NULL;
}

int main(void) {
  printf("root pid=%ld ppid=%ld tid=%ld\n", (long)getpid(), (long)getppid(),
         syscall(SYS_gettid));

  pthread_t threads[THREADS];
  struct thread_result results[THREADS] = {0};
  for (int id = 0; id < THREADS; id++) {
    if (pthread_create(&threads[id], NULL, thread_main, &results[id]) != 0) {
      return 1;
    }
  }
  for (int id = 0; id < THREADS; id++) {
    if (pthread_join(threads[id], NULL) != 0 || results[id].pid != getpid() ||
        results[id].tid == getpid()) {
      return 2;
    }
    printf("thread[%d] pid=%ld tid=%ld\n", id, (long)results[id].pid,
           (long)results[id].tid);
  }

  int pipe_fds[2];
  if (pipe(pipe_fds) != 0) {
    return 3;
  }
  pid_t child = fork();
  if (child < 0) {
    return 4;
  }
  if (child == 0) {
    close(pipe_fds[0]);
    pid_t identity[3] = {getpid(), getppid(), (pid_t)syscall(SYS_gettid)};
    ssize_t written = write(pipe_fds[1], identity, sizeof(identity));
    _exit(written == (ssize_t)sizeof(identity) ? 0 : 1);
  }

  close(pipe_fds[1]);
  pid_t identity[3] = {0};
  ssize_t count = read(pipe_fds[0], identity, sizeof(identity));
  close(pipe_fds[0]);
  int status = 0;
  if (waitpid(child, &status, 0) != child ||
      count != (ssize_t)sizeof(identity) || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0 || identity[0] != child ||
      identity[1] != getpid() || identity[2] != child) {
    return 5;
  }
  printf("child pid=%ld ppid=%ld tid=%ld\n", (long)identity[0],
         (long)identity[1], (long)identity[2]);
  return 0;
}
