/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Contract: WHAT EACH PROCESS OBSERVES THROUGH A SHARED MAPPING is deterministic.
 *
 * This is a different question from thread scheduling. A backend can
 * sequentialize threads identically -- same thread running at the same point
 * every run -- and still expose DIFFERENT INTERMEDIATE STATES through a shared
 * mapping, because what a reader sees depends on when the writer's stores
 * become visible, not only on who was scheduled. Pinning the schedule does not
 * pin the memory.
 *
 * Adjacent cells exist and none of them run: determinism-stress-c/
 * mmap-fork-shared, backend-parity-c/memfd-create and
 * backend-parity-c/msync-writeback are all ci=false in all five modes. So this
 * surface is guarded on paper and unguarded in practice.
 *
 * WHAT IS ASSERTED.
 *
 * The program prints the VALUES IT OBSERVED and the BRANCHES it took on them --
 * never a summary like ok=N. A pass/fail count is exactly the shape that hides
 * a change: two runs can both report ok=5 while observing entirely different
 * intermediate states. Every observation is emitted as its own line carrying
 * the actual bytes seen, so a divergence names the value that moved.
 *
 * NO BARRIERS, NO SLEEPS, NO SYNCHRONISATION ADDED. A barrier before reading
 * would make the reader see the completed write every time, which deletes the
 * interleaving that is the entire subject. The single waitpid is at the very
 * END, after all sampling, and exists only to reap -- it never precedes an
 * observation. A TORN OR PARTIAL READ IS A VALID OBSERVATION HERE; the contract
 * is that the same tear happens every run, not that no tear happens.
 *
 * NON-VACUITY. A fixture can go green by observing nothing -- a recent vdso fix
 * passed by emitting no bytes at all. So the program counts the observations it
 * actually made and prints OBSERVATIONS <n> as its last line, and it returns
 * non-zero if that count is zero. A run that reaches no shared memory fails
 * loudly instead of passing quietly.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

enum { PAGE = 4096, NWORD = 8 };

static int observations = 0;

/* Emit one observation: the label, the raw value seen, and the branch taken on
 * it. The branch is printed as its own token so a control-flow change is
 * visible even when the value is unchanged. */
static void observe_u64(const char* label, unsigned long long v, const char* branch) {
  printf("OBS %-26s value=0x%016llx branch=%s\n", label, v, branch);
  observations++;
}

static void observe_words(const char* label, const volatile unsigned long long* p, int n) {
  printf("OBS %-26s words=", label);
  for (int i = 0; i < n; i++) {
    printf("%s%llx", i ? "," : "", (unsigned long long)p[i]);
  }
  /* Which prefix of the record is populated: this is the PARTIAL-UPDATE
   * observation. A reader that catches 3 of 8 words has seen a real, legitimate
   * intermediate state; the contract is that it catches the same 3 every run. */
  int filled = 0;
  while (filled < n && p[filled] != 0) {
    filled++;
  }
  printf(" filled=%d branch=%s\n", filled,
         filled == 0 ? "none" : (filled == n ? "complete" : "PARTIAL"));
  observations++;
}

int main(void) {
  /* --- A: MAP_SHARED anonymous, across fork ------------------------------ */
  volatile unsigned long long* anon =
      mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (anon == MAP_FAILED) {
    printf("SETUP mmap anon shared FAILED %s\n", strerror(errno));
    return 2;
  }
  anon[0] = 0x1111111111111111ULL;

  /* --- B: MAP_SHARED file-backed ----------------------------------------- */
  int fd = open("shm_backing.bin", O_CREAT | O_RDWR | O_TRUNC, 0644);
  if (fd < 0) {
    printf("SETUP open backing FAILED %s\n", strerror(errno));
    return 2;
  }
  if (ftruncate(fd, PAGE) != 0) {
    printf("SETUP ftruncate FAILED %s\n", strerror(errno));
    return 2;
  }
  volatile unsigned long long* filemap =
      mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (filemap == MAP_FAILED) {
    printf("SETUP mmap file shared FAILED %s\n", strerror(errno));
    return 2;
  }

  /* --- C: memfd, shared across fork -------------------------------------- */
  int mfd = memfd_create("shm_coherency", 0);
  volatile unsigned long long* memmap = MAP_FAILED;
  if (mfd >= 0 && ftruncate(mfd, PAGE) == 0) {
    memmap = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
  }
  printf("SETUP memfd %s\n", memmap == MAP_FAILED ? "UNAVAILABLE" : "ok");

  /* --- D: MAP_PRIVATE, for COW divergence -------------------------------- */
  volatile unsigned long long* priv =
      mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (priv == MAP_FAILED) {
    printf("SETUP mmap private FAILED %s\n", strerror(errno));
    return 2;
  }
  priv[0] = 0xAAAAAAAAAAAAAAAAULL;

  pid_t pid = fork();
  if (pid < 0) {
    printf("SETUP fork FAILED %s\n", strerror(errno));
    return 2;
  }

  if (pid == 0) {
    /* CHILD: writer. Writes the multi-word record ONE WORD AT A TIME with no
     * fence, so a concurrent reader can legitimately catch a prefix. */
    for (int i = 0; i < NWORD; i++) {
      anon[i] = 0x2200000000000000ULL + (unsigned long long)(i + 1);
    }
    for (int i = 0; i < NWORD; i++) {
      filemap[i] = 0x3300000000000000ULL + (unsigned long long)(i + 1);
    }
    if (memmap != MAP_FAILED) {
      for (int i = 0; i < NWORD; i++) {
        memmap[i] = 0x4400000000000000ULL + (unsigned long long)(i + 1);
      }
    }
    /* COW: the child's write must NOT be visible to the parent. */
    priv[0] = 0xBBBBBBBBBBBBBBBBULL;
    observe_u64("child.private.after_write", priv[0],
                priv[0] == 0xBBBBBBBBBBBBBBBBULL ? "child-sees-own" : "UNEXPECTED");
    /* msync: push the file-backed mapping and report the result, since a
     * backend can make msync a no-op and still look correct on read-back. */
    int rc = msync((void*)filemap, PAGE, MS_SYNC);
    observe_u64("child.msync.rc", (unsigned long long)(long long)rc,
                rc == 0 ? "synced" : "failed");
    printf("OBSERVATIONS %d\n", observations);
    _exit(observations > 0 ? 0 : 3);
  }

  /* PARENT: reader. Samples IMMEDIATELY, with nothing added to stabilise it. */
  observe_words("parent.anon.sample", anon, NWORD);
  observe_words("parent.filemap.sample", filemap, NWORD);
  if (memmap != MAP_FAILED) {
    observe_words("parent.memfd.sample", memmap, NWORD);
  }

  /* COW divergence: the parent must still see its own value. */
  observe_u64("parent.private.after_child", priv[0],
              priv[0] == 0xAAAAAAAAAAAAAAAAULL ? "COW-isolated"
                                               : "LEAKED-child-write");

  int status = 0;
  waitpid(pid, &status, 0); /* reap only; every observation above already happened */

  /* After reaping, the writes must all be visible -- the settled state. */
  observe_words("parent.anon.settled", anon, NWORD);
  observe_words("parent.filemap.settled", filemap, NWORD);
  if (memmap != MAP_FAILED) {
    observe_words("parent.memfd.settled", memmap, NWORD);
  }
  observe_u64("parent.private.settled", priv[0],
              priv[0] == 0xAAAAAAAAAAAAAAAAULL ? "COW-isolated"
                                               : "LEAKED-child-write");

  /* Read the backing FILE, not the mapping: msync's visible effect. */
  unsigned long long via_file[NWORD];
  memset(via_file, 0, sizeof(via_file));
  if (pread(fd, via_file, sizeof(via_file), 0) == (ssize_t)sizeof(via_file)) {
    observe_words("parent.file.readback", (volatile unsigned long long*)via_file, NWORD);
  } else {
    printf("OBS parent.file.readback UNREADABLE %s\n", strerror(errno));
  }

  printf("CHILD exited=%d status=%d\n", WIFEXITED(status), WEXITSTATUS(status));
  close(fd);
  if (mfd >= 0) {
    close(mfd);
  }
  unlink("shm_backing.bin");

  /* NON-VACUITY GATE. A run that observed nothing must FAIL, not pass quietly. */
  printf("OBSERVATIONS %d\n", observations);
  if (observations == 0) {
    printf("VACUOUS: no shared-memory observation was made\n");
    return 3;
  }
  return 0;
}
