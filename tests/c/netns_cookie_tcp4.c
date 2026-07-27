#include "netns_cookie_common.h"

#include <netinet/in.h>

int main(void) {
  const int first_fd = socket(AF_INET, SOCK_STREAM, 0);
  const int second_fd = socket(AF_INET, SOCK_STREAM, 0);
  if (first_fd < 0 || second_fd < 0) {
    perror("socket(AF_INET, SOCK_STREAM)");
    close(first_fd);
    close(second_fd);
    return 1;
  }
  const int result = verify_netns_cookie(first_fd, second_fd, "tcp4");
  close(first_fd);
  close(second_fd);
  return result;
}
