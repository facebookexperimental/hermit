/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A shared library that declares it needs x86-64-v3
 * (GNU_PROPERTY_X86_ISA_1_NEEDED bits 1|2|4, as isa_needed.c emits
 * them), for the negative twin of cpu_profile_v2_rootfs.rs: loaded
 * under the v2 profile (as a DT_NEEDED dependency, which Hermit's exec-time
 * admission does not scan), the guest's own ld.so must refuse it.
 * Compiled -march=x86-64-v3 -shared; the declaration, not any
 * instruction it runs, is what the loader acts on.
 */

__asm__(".pushsection .note.gnu.property, \"a\"\n"
        ".p2align 3\n"
        /* namesz, descsz, NT_GNU_PROPERTY_TYPE_0, "GNU" */
        ".long 4, 16, 5\n"
        ".asciz \"GNU\"\n"
        /* pr_type, pr_datasz, pr_data (baseline|v2|v3), padding to 8 */
        ".long 0xc0008002, 4, 7, 0\n"
        ".popsection\n");

int needs_v3_answer(void) { return 42; }
