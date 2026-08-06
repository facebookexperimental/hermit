/*
 * Process execution-domain round-trip via personality(2).
 *
 * The contract asserts only process-local relational state, never the host's
 * absolute starting persona:
 *   1. query and save the starting persona;
 *   2. toggle UNAME26 and require personality(target) to return the start;
 *   3. query and require the exact target persona;
 *   4. restore the start and require personality(start) to return the target;
 *   5. query and require the exact starting persona again.
 *
 * Hermit already enables ADDR_NO_RANDOMIZE, so OR-ing that flag can be a
 * no-op. XOR-ing UNAME26 guarantees that a successful run exercises a real
 * state transition. The starting value is never printed, which keeps the
 * fixed success oracle portable: "pers ok=5".
 */

#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/personality.h>

int main(void) {
    enum { EXPECTED_CHECKS = 5 };
    int ok = 0;

    int rc = personality(0xffffffffUL);
    if (rc == -1) {
        printf("pers ok=0\n");
        return EXIT_FAILURE;
    }
    unsigned int start = (unsigned int)rc;
    ok++;

    unsigned int target = start ^ UNAME26;
#ifdef HERMIT_TEST_PERSONALITY_NO_TRANSITION
    target = start; /* plant the vacuous target used by the old contract */
#endif
    if (target == start) {
        printf("pers ok=%d\n", ok);
        return EXIT_FAILURE;
    }

    rc = personality(target);
    bool target_may_be_active = rc != -1;
    if (target_may_be_active && (unsigned int)rc == start) {
        ok++;
    }

    rc = personality(0xffffffffUL);
    bool target_query_matches = rc != -1 && (unsigned int)rc == target;
#ifdef HERMIT_TEST_PERSONALITY_POST_SET_FAILURE
    target_query_matches = false; /* bracket a mismatch after the state change */
#endif
    if (target_query_matches) {
        ok++;
    }

    /* Restore best-effort even when the set return or follow-up query was bad. */
    rc = personality(start);
    if (target_may_be_active && rc != -1 && (unsigned int)rc == target) {
        ok++;
    }

    rc = personality(0xffffffffUL);
    if (rc != -1 && (unsigned int)rc == start) {
        ok++;
    }

#ifdef HERMIT_TEST_ORACLE_NEGATIVE
    ok--; /* stable wrong stdout must be rejected by the normal exit oracle */
#endif
    printf("pers ok=%d\n", ok);
    return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
