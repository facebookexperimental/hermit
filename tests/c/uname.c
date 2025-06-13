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
#include <string.h>
#include <sys/utsname.h>

int main() {
  struct utsname buf;
  int ret = uname(&buf);
  if (ret == -1) {
    printf("Uname failed\nReason: %s\n", strerror(errno));
  }

  printf("Operating name: %s\n", buf.sysname);
  printf("Node name: %s\n", buf.nodename);
  printf("Operating system release: %s\n", buf.release);
  printf("Operating system version: %s\n", buf.version);
  printf("Hardware identifier: %s\n", buf.machine);
}
