/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

/*
 * Syscall numbers past the end of Hermit's pinned x86-64 table (which ends
 * at 461, lsm_list_modules), plus one number no kernel wires. The libc
 * headers on the build host may not name the newest of these yet, so they
 * are spelled as raw numbers with #ifndef fallbacks, like the
 * copy_file_range probe: 462 mseal, 463 setxattrat, 464 getxattrat,
 * 465 listxattrat, 466 removexattrat, 467 open_tree_attr, 468 file_getattr,
 * 469 file_setattr, 470 listns.
 *
 * Under Hermit every one of them must answer ENOSYS before any argument is
 * looked at: natively, setxattrat with a NULL path would be EFAULT, but the
 * point of the probe is that the number never reaches the kernel at all.
 * Natively this program is only a probe and simply reports what the host
 * kernel does with each number.
 */
#ifndef SYS_mseal
#define SYS_mseal 462
#endif
#ifndef SYS_setxattrat
#define SYS_setxattrat 463
#endif
#ifndef SYS_getxattrat
#define SYS_getxattrat 464
#endif
#ifndef SYS_listxattrat
#define SYS_listxattrat 465
#endif
#ifndef SYS_removexattrat
#define SYS_removexattrat 466
#endif
#ifndef SYS_open_tree_attr
#define SYS_open_tree_attr 467
#endif
#ifndef SYS_file_getattr
#define SYS_file_getattr 468
#endif
#ifndef SYS_file_setattr
#define SYS_file_setattr 469
#endif
#ifndef SYS_listns
#define SYS_listns 470
#endif

int main(void) {
  const long numbers[] = {
      SYS_mseal,     SYS_setxattrat,  SYS_getxattrat,   SYS_listxattrat,
      SYS_removexattrat, SYS_open_tree_attr, SYS_file_getattr, SYS_file_setattr,
      SYS_listns,    999,
  };
  int all_enosys = 1;
  for (size_t i = 0; i < sizeof(numbers) / sizeof(numbers[0]); i++) {
    errno = 0;
    long result = syscall(numbers[i], 0, 0, 0, 0, 0, 0);
    int seen = errno;
    printf("nr %ld ret %ld errno %d (%s)\n", numbers[i], result, seen,
           strerror(seen));
    if (result != -1 || seen != ENOSYS) {
      all_enosys = 0;
    }
  }
  if (!all_enosys) {
    fprintf(stderr, "some unknown syscall did not return ENOSYS\n");
    return 1;
  }
  puts("unknown-syscall-enosys-ok");
  return 0;
}
