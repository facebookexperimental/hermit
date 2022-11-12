// @lint-ignore-every LICENSELINT
/*
Copyright (c) 2017-2019 joint authorship:

 * Baojun Wang <wangbj@gmail.com>
 * Joe Devietti <devietti@cis.upenn.edu>
 * Kelly Shiptoski <kship@seas.upenn.edu>
 * Nicholas Renner <nrenner@seas.upenn.edu>
 * Omar Salvador Navarro Leija <gatoWololo@gmail.com>
 * Ryan Rhodes Newton <rrnewton@gmail.com>
 * Ryan Glenn Scott <ryan.gl.scott@gmail.com>

MIT LICENSE:
-------------------------
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

#include "util/assert.h"

static _Atomic int thread_should_exit;

static void thread_exit(int signum, siginfo_t* info, void* uctxt) {
  write(STDOUT_FILENO, "caught SIGTERM, preparing exit\n", 31);
  atomic_store(&thread_should_exit, 1);
}

static void* second_thread(void* param) {
  pthread_t thread_suspend = *(pthread_t*)param;

  write(STDOUT_FILENO, "2. sending SIGTERM\n", 19);
  pthread_kill(thread_suspend, SIGTERM);

  return NULL;
}

static void* first_thread(void* param) {
  sigset_t set;
  siginfo_t siginfo;
  struct timespec tp = {1, 0};

  sigemptyset(&set);
  sigaddset(&set, SIGTERM);

  write(STDOUT_FILENO, "1. sigtimedwait timeout one second\n", 35);
  sigtimedwait(&set, &siginfo, &tp);
  write(STDOUT_FILENO, "1. sigtimedwait finished\n", 25);

  return NULL;
}

int main(int argc, char* argv[]) {
  sigset_t set, oldset;
  pthread_t threads[2];

  sigemptyset(&set);

  sigaddset(&set, SIGTERM);
  sigprocmask(SIG_BLOCK, &set, &oldset);

  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_sigaction = thread_exit;
  sa.sa_flags = SA_RESTART | SA_RESETHAND | SA_SIGINFO;

  assert(sigaction(SIGTERM, &sa, NULL) == 0);

  assert(pthread_create(&threads[0], NULL, first_thread, NULL) == 0);
  assert(pthread_create(&threads[1], NULL, second_thread, &threads[0]) == 0);

  pthread_join(threads[0], NULL);
  pthread_join(threads[1], NULL);

  return 0;
}
