#include "netlink_autobind_common.h"

int main(void) {
  return run_netlink_autobind_probe(NETLINK_USERSOCK, "usersock");
}
