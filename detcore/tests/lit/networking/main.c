// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

// NOTE: This test cannot run in parallel with itself since it binds to a fixed
// port. Instead, it should always be ran within a networking namespace.

// Use a bogus 'RUN' directive so this test isn't disabled:
// RUN: true

#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define handle_error(msg) \
  do {                    \
    perror(msg);          \
    exit(EXIT_FAILURE);   \
  } while (0)

void ipv4_zero_port() {
  int sfd;
  struct sockaddr_in my_sockaddr;

  sfd = socket(AF_INET, SOCK_STREAM, 0);
  if (sfd == -1)
    handle_error("ipv4 socket");

  memset(&my_sockaddr, 0, sizeof(my_sockaddr));
  my_sockaddr.sin_family = AF_INET;
  my_sockaddr.sin_port = htons(0);
  my_sockaddr.sin_addr.s_addr = htonl(INADDR_ANY);

  if (bind(sfd, (struct sockaddr*)&my_sockaddr, sizeof(my_sockaddr)) == -1)
    handle_error("ipv4 bind 2");

  close(sfd);
}

void ipv4_fixed_port() {
  int sfd;
  struct sockaddr_in my_sockaddr;

  sfd = socket(AF_INET, SOCK_STREAM, 0);
  if (sfd == -1)
    handle_error("ipv4 socket");

  memset(&my_sockaddr, 0, sizeof(my_sockaddr));
  my_sockaddr.sin_family = AF_INET;
  my_sockaddr.sin_port = htons(1299);
  my_sockaddr.sin_addr.s_addr = htonl(INADDR_ANY);

  if (bind(sfd, (struct sockaddr*)&my_sockaddr, sizeof(my_sockaddr)) == -1)
    handle_error("ipv4 bind 1");

  close(sfd);
}

void ipv6_fixed_port() {
  int sfd;
  struct sockaddr_in6 my_sockaddr;

  sfd = socket(AF_INET6, SOCK_STREAM, 0);
  if (sfd == -1)
    handle_error("ipv6 socket");

  memset(&my_sockaddr, 0, sizeof(my_sockaddr));
  my_sockaddr.sin6_family = AF_INET6;
  my_sockaddr.sin6_port = htons(1299);
  my_sockaddr.sin6_addr = in6addr_any;

  if (bind(sfd, (struct sockaddr*)&my_sockaddr, sizeof(my_sockaddr)) == -1)
    handle_error("ipv6 bind1");

  close(sfd);
}

void ipv6_zero_port() {
  int sfd;
  struct sockaddr_in6 my_sockaddr;

  sfd = socket(AF_INET6, SOCK_STREAM, 0);
  if (sfd == -1)
    handle_error("ipv6 socket");

  memset(&my_sockaddr, 0, sizeof(my_sockaddr));
  my_sockaddr.sin6_family = AF_INET6;
  my_sockaddr.sin6_port = htons(0);
  my_sockaddr.sin6_addr = in6addr_any;

  if (bind(sfd, (struct sockaddr*)&my_sockaddr, sizeof(my_sockaddr)) == -1)
    handle_error("ipv6 bind1");

  close(sfd);
}

int main(int argc, char* argv[]) {
  ipv4_zero_port();
  ipv4_fixed_port();
  ipv6_fixed_port();
  ipv6_zero_port();

  return 0;
}
