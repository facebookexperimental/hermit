/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// The vDSO a guest sees must version its exports as Linux's does. glibc's
// versioned lookup finds __vdso_clock_gettime@LINUX_2.6 and refuses
// LINUX_2.5, and the image AT_SYSINFO_EHDR names carries DT_VERDEF: Go's
// runtime walks the version definitions without checking that they exist,
// so an unversioned vDSO crashes every Go program at startup
// (https://github.com/rrnewton/reverie/issues/1001).
#define _GNU_SOURCE
#include <dlfcn.h>
#include <elf.h>
#include <link.h>
#include <stdio.h>
#include <sys/auxv.h>

int main(void) {
    const ElfW(Ehdr) *ehdr = (const void *)getauxval(AT_SYSINFO_EHDR);
    if (ehdr == NULL) {
        printf("no AT_SYSINFO_EHDR\n");
        return 1;
    }
    const char *base = (const char *)ehdr;
    const ElfW(Phdr) *phdrs = (const void *)(base + ehdr->e_phoff);
    const ElfW(Dyn) *dynamic = NULL;
    for (int i = 0; i < ehdr->e_phnum; i++) {
        if (phdrs[i].p_type == PT_DYNAMIC) {
            dynamic = (const void *)(base + phdrs[i].p_offset);
        }
    }
    int verdef = 0;
    for (const ElfW(Dyn) *entry = dynamic; entry != NULL && entry->d_tag != DT_NULL; entry++) {
        if (entry->d_tag == DT_VERDEF) {
            verdef = 1;
        }
    }

    // Find the vDSO's link map by address: glibc names it from DT_SONAME,
    // which this probe does not rely on.
    struct link_map *map = NULL;
    if (dlinfo(dlopen(NULL, RTLD_LAZY), RTLD_DI_LINKMAP, &map) != 0) {
        printf("dlinfo failed\n");
        return 1;
    }
    struct link_map *vdso = NULL;
    for (; map != NULL; map = map->l_next) {
        if ((const char *)map->l_ld == (const char *)dynamic) {
            vdso = map;
        }
    }
    if (vdso == NULL) {
        printf("no vDSO link map\n");
        return 1;
    }
    void *current = dlvsym(vdso, "__vdso_clock_gettime", "LINUX_2.6");
    void *older = dlvsym(vdso, "__vdso_clock_gettime", "LINUX_2.5");
    printf("LINUX_2.6=%s LINUX_2.5=%s DT_VERDEF=%s\n",
           current ? "found" : "missing",
           older ? "found" : "refused",
           verdef ? "present" : "absent");
    return 0;
}
