/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * PINS CURRENT BEHAVIOUR. It does NOT assert that this behaviour is correct.
 *
 * Detcore has handlers for the WRITE side of session and process-group
 * identity -- handle_setsid and handle_setpgid in detcore/src/syscalls/misc.rs
 * inject the call and then mirror the result into Detcore's own model
 * (dispatched at detcore/src/lib.rs:2158-2159). It has NO handler for the READ
 * side: getsid, getpgid and getpgrp have no handle_* function and no dispatch
 * arm, and they pass straight through to the kernel.
 *
 * THAT IS NOT A MISCLASSIFICATION -- classify_syscall says so plainly. All
 * three sit in the PASS-THRU arm, whose comment reads: "These existing and
 * triaged passthroughs are conditionally repeatable under Hermit's
 * fixed-container, stable-filesystem, and serialization assumptions."
 *
 * ⚠️ THIS CELL IS THE TEST FOR THAT STATED CONDITION. The classification makes
 * repeatability conditional on the fixed container and nothing checks the
 * condition. Here, it is checked.
 *
 * MEASURED 2026-08-25, ptrace backend, three consecutive runs, identical:
 *     before sid=0 pgid=0
 *     setpgid rc=0 errno=0 then pgid=3
 *     setsid rc=-1 errno=1 then sid=0 pgid=3
 * Natively the same program prints per-run host values (sid=81608 pgid=82040
 * on one run), so the observable really does discriminate.
 *
 * ⚠️ WHAT THIS CELL DEFENDS, STATED SO IT IS NOT OVER-READ. The stability
 * below is produced by the PID and user NAMESPACE, not by a Detcore handler:
 * `3` is this process's pid inside the namespace, and `0` is what the kernel
 * reports for a session whose leader lives outside it. Removing Detcore's
 * setsid/setpgid mirroring would NOT necessarily red this cell, because the
 * values it observes are passthrough reads. It defends the CONTAINER's
 * identity surface -- exactly the "fixed-container assumption" the
 * classification rests on -- and it is not evidence that the read side is
 * determinized.
 *
 * ⚠️ AND IT DOES NOT PRESUPPOSE THE DESIGN. There is an open question about
 * whether Hermit should model process groups, sessions and supplementary
 * groups at all. If that is answered by building a model, these values are
 * expected to CHANGE and this cell is expected to be updated with them. A red
 * here after such a change is the cell doing its job, not a defect.
 */

/* getsid and getpgid need the POSIX feature-test macro; the harness compiles
 * with -Werror=implicit-function-declaration and a stricter -std than a bare
 * `gcc file.c`, so an implicit declaration here is a build error rather than
 * a warning. */
#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
    printf("before sid=%d pgid=%d\n", getsid(0), getpgid(0));

    /* Exercises handle_setpgid: the kernel validates, Detcore mirrors. */
    int pg = setpgid(0, 0);
    printf("setpgid rc=%d errno=%d then pgid=%d\n", pg, pg == 0 ? 0 : errno,
           getpgid(0));

    /*
     * Exercises handle_setsid. This is EXPECTED TO FAIL with EPERM: the
     * setpgid above makes this process a group leader, and Linux refuses
     * setsid from a group leader. The refusal is the pinned observation --
     * a success here would mean the group-leader precondition changed.
     */
    int sd = setsid();
    printf("setsid rc=%d errno=%d then sid=%d pgid=%d\n", sd < 0 ? -1 : 0,
           sd < 0 ? errno : 0, getsid(0), getpgid(0));

    return 0;
}
