/*
 * epoll_readiness: cross-backend parity for the epoll readiness interface.
 *
 * The io_uring fallback row only checks that epoll_create1 succeeds. This row
 * exercises the full non-blocking readiness cycle: register a pre-armed eventfd
 * with epoll_ctl, observe it ready via a zero-timeout epoll_wait, deregister it,
 * and observe the empty set. epoll_wait is called with timeout 0 throughout, so
 * it never blocks -- a blocking wait would livelock the single-threaded DBI
 * backend against the deterministic scheduler.
 */
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <unistd.h>

int main(void) {
  int ok = 0;

  int ep = epoll_create1(EPOLL_CLOEXEC);
  int ev = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);

  /* 1: both descriptors are created. */
  if (ep >= 0 && ev >= 0) {
    ok++;
  }

  /* 2: arm the eventfd so it reports readable. */
  uint64_t one = 1;
  if (write(ev, &one, sizeof one) == (ssize_t)sizeof one) {
    ok++;
  }

  /* 3: register the eventfd for read readiness. */
  struct epoll_event add;
  memset(&add, 0, sizeof add);
  add.events = EPOLLIN;
  add.data.fd = ev;
  if (epoll_ctl(ep, EPOLL_CTL_ADD, ev, &add) == 0) {
    ok++;
  }

  /* 4: a zero-timeout wait reports exactly the armed eventfd as readable. */
  struct epoll_event got[4];
  memset(got, 0, sizeof got);
  int ready = epoll_wait(ep, got, 4, 0);
  if (ready == 1 && got[0].data.fd == ev && (got[0].events & EPOLLIN)) {
    ok++;
  }

  /* 5: deregister the eventfd from the interest set. */
  if (epoll_ctl(ep, EPOLL_CTL_DEL, ev, NULL) == 0) {
    ok++;
  }

  /* 6: with nothing registered, a zero-timeout wait reports no readiness. */
  int empty = epoll_wait(ep, got, 4, 0);
  if (empty == 0) {
    ok++;
  }

  close(ev);
  close(ep);

  printf("epoll ok=%d\n", ok);
  return 0;
}
