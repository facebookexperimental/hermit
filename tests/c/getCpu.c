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

#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

int main() {
  unsigned cpu = UINT_MAX;
  unsigned node = UINT_MAX;
  enum { EXPECTED_CHECKS = 3 };
  int ok = 0;

  if (syscall(SYS_getcpu, &cpu, &node, NULL) == 0) {
    ok++;
  }
  /* Hermit exposes a single virtual CPU in NUMA node 0. Poison both outputs
     above so a successful call that fails to initialize either result cannot
     accidentally satisfy the contract. */
  if (cpu == 0) {
    ok++;
  }
  if (node == 0) {
    ok++;
  }

  printf("CPU %u, Node %u\n", cpu, node);
  return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
