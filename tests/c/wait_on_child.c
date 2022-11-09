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
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#include <sys/types.h>
#include <sys/wait.h>

// Program testing ordering for processes, have child wait for child to finish.
// Parent should print pid of 1.
// Child should print pid of 2.
int main(void) {
  pid_t pid = fork();
  if (pid == -1) {
    printf("Fork failed, reason:\n%s\n", strerror(errno));
    exit(1);
  }

  if (pid == 0) {
    printf("Child: My pid: %d\n", getpid());
  } else {
    int status;
    if (-1 == wait(&status)) {
      fprintf(stderr, "Child: wait failed with %s\n", strerror(errno));
      exit(1);
    }
    printf("Parent: My pid: %d\n", getpid());
  }
  return 0;
}
