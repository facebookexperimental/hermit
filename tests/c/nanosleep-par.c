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

#include <stdatomic.h>
#include <stdio.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include "util/assert.h"

static _Atomic unsigned long counter_loc;
static volatile _Atomic unsigned long* pcounter = &counter_loc;

#define DELAY_CYCLES 10000000

static void delay(void) {
  for (int i = 0; i < DELAY_CYCLES; i++) {
    atomic_fetch_add(pcounter, 1);
  }
}

static void run_parent_process(void) {
  const struct timespec req = {0, 100000000};
  struct timespec rem = {0, 0};

  for (int i = 0; i < 10; i++) {
    printf("parent\n");
    assert(nanosleep(&req, &rem) == 0);
    delay();
  }
}

static void run_child_process(pid_t pid) {
  const struct timespec req = {0, 100000000};
  struct timespec rem = {0, 0};

  for (int i = 0; i < 10; i++) {
    printf("child\n");
    assert(nanosleep(&req, &rem) == 0);
    delay();
  }
}

int main(int argc, char* argv[]) {
  pid_t pid = fork();

  if (pid == 0) { /* child */
    run_child_process(pid);
  } else if (pid > 0) { /* parent */
    int status;

    run_parent_process();
    waitpid(pid, &status, 0);
    printf("global counter: %lu\n", atomic_load(pcounter));
  } else {
    perror("fork():");
  }
  return 0;
}
