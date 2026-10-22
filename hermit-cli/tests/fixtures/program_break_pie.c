/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* A position-independent image's program break starts where Linux's rule puts
 * it for wherever the image was loaded: its highest loaded segment's end at
 * the image's bias, page-aligned. */

#define _GNU_SOURCE
#include <link.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

static unsigned long expected;

static int own_image(struct dl_phdr_info *info, size_t size, void *unused) {
  (void)size;
  (void)unused;
  unsigned long end = 0;
  for (int i = 0; i < info->dlpi_phnum; i++)
    if (info->dlpi_phdr[i].p_type == PT_LOAD &&
        info->dlpi_phdr[i].p_vaddr + info->dlpi_phdr[i].p_memsz > end)
      end = info->dlpi_phdr[i].p_vaddr + info->dlpi_phdr[i].p_memsz;
  expected = (info->dlpi_addr + end + 4095) & ~4095UL;
  return 1; /* the program itself is listed first */
}

int main(void) {
  long start = syscall(SYS_brk, 0);
  dl_iterate_phdr(own_image, NULL);
  printf("start as load_elf_binary puts it: %s\n", (unsigned long)start == expected ? "yes" : "no");
  return 0;
}
