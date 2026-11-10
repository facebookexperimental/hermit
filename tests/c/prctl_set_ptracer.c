// Reports how prctl(PR_SET_PTRACER) answers, for comparing a run under Hermit
// with a native run on the same host. With the Yama LSM the kernel answers 0;
// without it the option reaches prctl's default case and fails with EINVAL.
// Either way it never fails with ENOSYS, which Android's crash handler treats
// as fatal.
#include <errno.h>
#include <stdio.h>
#include <sys/prctl.h>

#ifndef PR_SET_PTRACER
#define PR_SET_PTRACER 0x59616d61
#endif
#ifndef PR_SET_PTRACER_ANY
#define PR_SET_PTRACER_ANY ((unsigned long)-1)
#endif

static const char* outcome(int rc, int err) {
  if (rc == 0) {
    return "ok";
  }
  switch (err) {
    case EINVAL:
      return "EINVAL";
    case ENOSYS:
      return "ENOSYS";
    default:
      return "other";
  }
}

int main(void) {
  int rc = prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY, 0, 0, 0);
  printf("any: %s\n", outcome(rc, errno));
  rc = prctl(PR_SET_PTRACER, 0, 0, 0, 0);
  printf("none: %s\n", outcome(rc, errno));
  return 0;
}
