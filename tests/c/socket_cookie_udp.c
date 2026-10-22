#include "socket_cookie_common.h"

#include <netinet/in.h>

int main(void) {
  const int first_fd = socket(AF_INET, SOCK_DGRAM, 0);
  const int second_fd = socket(AF_INET, SOCK_DGRAM, 0);
  if (first_fd < 0 || second_fd < 0) {
    perror("socket(AF_INET, SOCK_DGRAM)");
    close(first_fd);
    close(second_fd);
    return 1;
  }
  const int result = verify_socket_cookies(first_fd, second_fd, "udp");
  close(first_fd);
  close(second_fd);
  return result;
}
