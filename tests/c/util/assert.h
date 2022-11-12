// @lint-ignore LICENSELINT
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

#ifndef DETTRACE_TESTS_UTIL_H
#define DETTRACE_TESTS_UTIL_H

#include <errno.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

void abort_if_failed(
    bool cond,
    int error,
    const char* file,
    int line,
    const char* func,
    const char* desc) {
  if (!cond) {
    fprintf(
        stderr, "%s: %s:%d: Assertion `%s' failed.\n", func, file, line, desc);
    if (errno > 0) {
      fprintf(stderr, "           os error (%d): %s\n", errno, strerror(errno));
    }
    raise(SIGABRT);
    abort();
  }
}

#ifdef __cplusplus
}
#endif

// The assert() from <assert.h> will get compiled out in release mode, so we
// define our own here.
#ifdef assert
#undef assert
#endif

#define assert(cond) \
  abort_if_failed(!!(cond), errno, __FILE__, __LINE__, __func__, #cond)

#endif // DETTRACE_TESTS_UTIL_H
