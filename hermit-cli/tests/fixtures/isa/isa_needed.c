// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

/*
 * Writes "ran\n" and exits ISA_EXIT, executing no instruction above
 * baseline x86-64, while declaring in a GNU property note the x86 ISA levels
 * it needs: ISA_NEEDED, the GNU_PROPERTY_X86_ISA_1_NEEDED bits (1 baseline,
 * 2 v2, 4 v3, 8 v4). Without ISA_NEEDED it declares nothing. Linked static it
 * is an image; linked static-pie, a program interpreter, which the kernel
 * runs in place of the image naming it.
 */

#ifdef ISA_NEEDED
__asm__(".pushsection .note.gnu.property, \"a\"\n"
        ".p2align 3\n"
        /* namesz, descsz, NT_GNU_PROPERTY_TYPE_0, "GNU" */
        ".long 4, 16, 5\n"
        ".asciz \"GNU\"\n"
        /* pr_type, pr_datasz, pr_data, padding to 8 */
        ".long 0xc0008002, 4, " ISA_NEEDED ", 0\n"
        ".popsection\n");
#endif

void _start(void) {
  static const char ran[] = "ran\n";
  long result;
  __asm__ volatile("syscall"
                   : "=a"(result)
                   : "a"(1), "D"(1), "S"(ran), "d"(sizeof ran - 1)
                   : "rcx", "r11", "memory");
  __asm__ volatile("syscall" ::"a"(231), "D"(ISA_EXIT));
  __builtin_unreachable();
}
