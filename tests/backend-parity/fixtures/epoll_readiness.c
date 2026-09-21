/*
 * epoll_readiness: cross-backend parity for the epoll readiness interface.
 *
 * The io_uring fallback row only checks that epoll_create1 succeeds. This row
 * exercises the full non-blocking readiness cycle: register a pre-armed eventfd
 * with epoll_ctl, observe it ready via a zero-timeout epoll_wait, deregister it,
 * and observe the empty set. epoll_wait is called with timeout 0 throughout, so
 * it never blocks -- a blocking wait would livelock the single-threaded DBT
 * backend against the deterministic scheduler.
 *
 * THE READY COUNTS ARE PRINTED. "epoll ok=6" collapsed six checks into one
 * scalar, so a backend that reported the wrong number of ready descriptors and
 * a backend that failed to deregister both printed "epoll ok=5" and compared
 * EQUAL. The existing exit-status guard catches the lower total, but does not
 * identify which observation was wrong.
 * The two readiness counts are the substance of this contract -- armed set
 * reports exactly 1, empty set reports exactly 0 -- and both are determined by
 * the guest's own actions on descriptors it just created, so emitting them adds
 * no host state to the byte stream.
 */
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <unistd.h>

int main(void) {
  enum { EXPECTED_CHECKS = 6 };

  int ep = epoll_create1(EPOLL_CLOEXEC);
  int ev = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);

  /* 1: both descriptors are created. */
  int fds_created = ep >= 0 && ev >= 0;

  /* 2: arm the eventfd so it reports readable. */
  uint64_t one = 1;
  int armed = write(ev, &one, sizeof one) == (ssize_t)sizeof one;

  /* 3: register the eventfd for read readiness. */
  struct epoll_event add;
  memset(&add, 0, sizeof add);
  add.events = EPOLLIN;
  add.data.fd = ev;
  int registered = epoll_ctl(ep, EPOLL_CTL_ADD, ev, &add) == 0;

  /* 4: a zero-timeout wait reports exactly the armed eventfd as readable. */
  struct epoll_event got[4];
  memset(got, 0, sizeof got);
  int ready = epoll_wait(ep, got, 4, 0);
  int ready_is_armed_fd =
      ready == 1 && got[0].data.fd == ev && (got[0].events & EPOLLIN);

  /* 5: deregister the eventfd from the interest set. */
  int deregistered = epoll_ctl(ep, EPOLL_CTL_DEL, ev, NULL) == 0;

  /* 6: with nothing registered, a zero-timeout wait reports no readiness. */
  int empty = epoll_wait(ep, got, 4, 0);

  close(ev);
  close(ep);

  int ok = fds_created + armed + registered + ready_is_armed_fd + deregistered +
      (empty == 0);
  printf(
      "epoll ok=%d fds_created=%d armed=%d registered=%d ready=%d "
      "ready_is_armed_fd=%d deregistered=%d empty=%d\n",
      ok,
      fds_created,
      armed,
      registered,
      ready,
      ready_is_armed_fd,
      deregistered,
      empty);
  return ok == EXPECTED_CHECKS ? 0 : 1;
}
