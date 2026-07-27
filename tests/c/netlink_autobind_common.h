#ifndef HERMIT_TESTS_C_NETLINK_AUTOBIND_COMMON_H
#define HERMIT_TESTS_C_NETLINK_AUTOBIND_COMMON_H

#include <linux/netlink.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int bind_netlink_socket(int protocol, struct sockaddr_nl* observed) {
  const int fd = socket(AF_NETLINK, SOCK_RAW, protocol);
  if (fd < 0) {
    perror("socket");
    return -1;
  }

  struct sockaddr_nl requested;
  memset(&requested, 0, sizeof(requested));
  requested.nl_family = AF_NETLINK;
  if (bind(
          fd,
          (const struct sockaddr*)&requested,
          sizeof(requested)) != 0) {
    perror("bind");
    close(fd);
    return -1;
  }

  memset(observed, 0, sizeof(*observed));
  socklen_t observed_length = sizeof(*observed);
  if (getsockname(
          fd,
          (struct sockaddr*)observed,
          &observed_length) != 0) {
    perror("getsockname");
    close(fd);
    return -1;
  }
  if (observed_length != sizeof(*observed) ||
      observed->nl_family != AF_NETLINK || observed->nl_pid == 0 ||
      observed->nl_groups != 0) {
    fprintf(
        stderr,
        "invalid netlink autobind: family=%u pid=%u groups=%u length=%u\n",
        observed->nl_family,
        observed->nl_pid,
        observed->nl_groups,
        observed_length);
    close(fd);
    return -1;
  }
  return fd;
}

static int run_netlink_autobind_probe(int protocol, const char* label) {
  struct sockaddr_nl first;
  const int first_fd = bind_netlink_socket(protocol, &first);
  if (first_fd < 0) {
    return 1;
  }

  struct sockaddr_nl second;
  const int second_fd = bind_netlink_socket(protocol, &second);
  if (second_fd < 0) {
    close(first_fd);
    return 1;
  }

  close(first_fd);
  close(second_fd);
  if (first.nl_pid == second.nl_pid) {
    fprintf(stderr, "netlink autobind reused live port ID %u\n", first.nl_pid);
    return 1;
  }

  printf("%s=%u,%u\n", label, first.nl_pid, second.nl_pid);
  return 0;
}

#endif
