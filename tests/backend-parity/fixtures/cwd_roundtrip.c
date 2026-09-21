#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/*
 * Process working-directory round-trip via chdir/getcwd/fchdir. It asserts only
 * backend-invariant relational properties -- never an absolute host path -- so
 * the golden stdout is portable across hosts and backends:
 *   1. getcwd captures the starting directory.
 *   2. chdir into a fresh mkdtemp directory succeeds.
 *   3. after chdir, getcwd differs from the start (the process actually moved).
 *   4. fchdir back through a directory fd opened on the start restores it.
 *   5. getcwd now equals the saved start again (the fchdir round-trip).
 *   6. chdir back to the start by absolute path also restores it.
 *
 * The starting path is never printed, so a container- or host-specific working
 * directory cannot leak into the golden output; only the relational count does.
 */
int main(void) {
    enum { EXPECTED_CHECKS = 6 };
    int ok = 0;
    char start[4096];
    if (getcwd(start, sizeof(start)) == NULL) {
        printf("cwd ok=0\n");
        return EXIT_FAILURE;
    }
    ok++;
    int startfd = open(start, O_RDONLY | O_DIRECTORY);

    char tmpl[] = "/tmp/cwd_roundtrip_XXXXXX";
    char *dir = mkdtemp(tmpl);
    if (dir != NULL && chdir(dir) == 0) {
        ok++;
    }
    char moved[4096];
    /* Load-bearing, not cosmetic: `moved` is read below to derive moved_dir, so
     * it must be initialised even when this getcwd fails. */
    moved[0] = '\0';
    int moved_differs = (getcwd(moved, sizeof(moved)) != NULL && strcmp(moved, start) != 0);
    if (moved_differs) {
        ok++;
    }
    if (startfd >= 0 && fchdir(startfd) == 0) {
        ok++;
    }
    char back1[4096];
    int back1_eq = (getcwd(back1, sizeof(back1)) != NULL && strcmp(back1, start) == 0);
    if (back1_eq) {
        ok++;
    }
    /* leave via a second chdir to confirm absolute-path chdir round-trips too */
    int abs_roundtrip = 0;
    if (dir != NULL && chdir(dir) == 0 && chdir(start) == 0) {
        char back2[4096];
        abs_roundtrip = (getcwd(back2, sizeof(back2)) != NULL && strcmp(back2, start) == 0);
        if (abs_roundtrip) {
            ok++;
        }
    }
    if (startfd >= 0) {
        close(startfd);
    }
    if (dir != NULL) {
        rmdir(dir);
    }
#ifdef HERMIT_TEST_ORACLE_NEGATIVE
    ok--; /* plant one failed contract check to bracket the exit oracle */
#endif
    /*
     * EMIT AN OBSERVED VALUE, NOT ONLY THE CHECK TALLY. Check 3 only asks that
     * the moved-to path DIFFERS from the start, so it holds for any differing
     * path: a backend that VIRTUALISES the path namespace (returning, say,
     * "/virtual/tmp/cwd_roundtrip_ab12cd") and one that does not both satisfy
     * every check above and both printed the identical byte stream "cwd ok=6".
     *
     * The raw path is not run-stable -- mkdtemp randomises the final six
     * characters and hermit isolates the guest /tmp per repeat -- and stdout is
     * double-run compared under `--strict --verify`. So emit the DIRNAME of the
     * observed path. That drops the random component entirely, is the constant
     * "/tmp" on a non-virtualising backend, and changes exactly when a backend
     * rewrites the namespace: value-bearing, run-stable, host-independent.
     */
    char moved_dir[4096];
    snprintf(moved_dir, sizeof(moved_dir), "%s", moved);
    char *cut = strrchr(moved_dir, '/');
    if (cut == moved_dir) {
        cut[1] = '\0'; /* the observed path was at the root: report "/" */
    } else if (cut != NULL) {
        *cut = '\0';
    } else {
        snprintf(moved_dir, sizeof(moved_dir), "%s", "<no-slash>");
    }
    /*
     * ...AND DE-ALIAS THE SUM. These two are not redundant, they cover disjoint
     * classes and neither subsumes the other:
     *   moved_dir  distinguishes what was OBSERVED on a fully passing run --
     *              measured "/tmp" vs "/vroot/tmp" against a consistent
     *              path-virtualising backend where both sides reach ok=6.
     *   the flags  attribute WHICH check failed on a failing run -- `ok=%d` is a
     *              sum, so two backends failing DIFFERENT checks alias to the
     *              same total; measured back1_eq=0 abs_roundtrip=1 where the
     *              tally alone shows only ok=4.
     */
    printf("cwd ok=%d moved_dir=%s moved_differs=%d back1_eq=%d abs_roundtrip=%d\n",
           ok, moved_dir, moved_differs, back1_eq, abs_roundtrip);
    return ok == EXPECTED_CHECKS ? EXIT_SUCCESS : EXIT_FAILURE;
}
