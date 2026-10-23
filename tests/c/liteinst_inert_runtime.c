/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Exports the required strong, protected, 16-byte initializer shape but
 * registers no constructor, so preloading it would run the guest without
 * installing Detcore. Hermit must reject the missing registration before
 * dispatch, after checking the export itself.
 */
__asm__(".text\n"
        ".p2align 4\n"
        ".globl detcore_liteinst_initialize\n"
        ".protected detcore_liteinst_initialize\n"
        ".type detcore_liteinst_initialize,@function\n"
        "detcore_liteinst_initialize:\n"
        "ret\n"
        ".fill 15,1,0x90\n"
        ".size detcore_liteinst_initialize,.-detcore_liteinst_initialize\n");
