#include "socket_cookie_common.h"

#include <sys/un.h>

int main(void) {
  int pair[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
    perror("socketpair(AF_UNIX, SOCK_STREAM)");
    return 1;
  }
  const int result = verify_socket_cookies(pair[0], pair[1], "unix");
  close(pair[0]);
  close(pair[1]);
  return result;
}
