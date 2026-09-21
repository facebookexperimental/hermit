/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

import java.security.SecureRandom;

public final class RuntimeRandom {
    public static void main(String[] args) {
        byte[] bytes = new byte[16];
        new SecureRandom().nextBytes(bytes);
        StringBuilder output = new StringBuilder();
        for (byte value : bytes) {
            output.append(String.format("%02x", value & 0xff));
        }
        System.out.println(output);
    }
}
