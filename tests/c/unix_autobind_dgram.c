#include "unix_autobind_common.h"

int main(void) {
  return run_unix_autobind_probe(SOCK_DGRAM, "dgram");
}
