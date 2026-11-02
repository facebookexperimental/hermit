/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Pass a file descriptor over binder from one process to another, then use
// it in the receiver.
//
// Usage: binder_fd_passing <binderfs mount> [poll|block|ping|exhausted|writeread].
// The guest adds a device of its own to that binderfs instance through its
// binder-control file (BINDER_CTL_ADD; an existing one is reused), so it needs
// no default device and no servicemanager.
//
// In "poll" mode, the default, both processes open the device nonblocking
// and wait in poll() before each read, so no binder ioctl waits in the
// kernel. In every other mode they open it blocking, so each read waits
// inside ioctl(BINDER_WRITE_READ) until the other process sends it work: the
// mode every real binder thread pool uses. "ping" sends an empty transaction
// instead of one carrying a descriptor, so it needs nothing of the receiver
// but the round trip. "exhausted" is "ping" whose receiver first makes a
// read whose buffer is already full (read_consumed == read_size), which
// Linux makes wait for work like any other read and then return with nothing
// added. "writeread" is "ping" whose sender sends the transaction and waits
// for the reply in one BINDER_WRITE_READ, as libbinder's talkWithDriver does.
// In "block" and "exhausted" mode the sender sleeps 100 ms first, so the
// receiver's read is the one that waits.
//
// The parent becomes the device's context manager, with a node that accepts
// descriptors, and the child sends it a
// transaction to handle 0 carrying one BINDER_TYPE_FD object for a memfd
// holding "hello". Linux installs a new descriptor for it in the parent when
// the parent reads the transaction (binder_apply_fd_fixups). The parent then
// reads the file through that descriptor and replies. Both sides open the
// device nonblocking and wait in poll() before each read, so no call blocks
// inside the binder ioctl.
//
// Prints "received fd N: hello" and exits 0 on success.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/android/binder.h>
#include <linux/android/binderfs.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define MAP_SIZE (128 * 1024)

// Whether the binder device is opened nonblocking and polled before each
// read ("poll"), or opened blocking so each read waits in the kernel.
static int nonblocking = 1;

// Whether the transaction carries a file descriptor.
static int pass_fd = 1;

// Whether the receiver first reads into a buffer that is already full.
static int exhausted = 0;

// Whether the sender writes its transaction and reads the reply in one call.
static int write_and_read = 0;

// Whether the sender sleeps before it sends, so the receiver waits first.
static int sender_waits = 0;

static void die(const char* what) {
  fprintf(stderr, "binder_fd_passing: %s: %s\n", what, strerror(errno));
  exit(1);
}

static int open_device(const char* path) {
  int fd = open(path, O_RDWR | O_CLOEXEC | (nonblocking ? O_NONBLOCK : 0));
  if (fd < 0) {
    die("open");
  }
  if (mmap(NULL, MAP_SIZE, PROT_READ, MAP_PRIVATE, fd, 0) == MAP_FAILED) {
    die("mmap");
  }
  return fd;
}

static void write_commands(int fd, const void* commands, size_t size) {
  struct binder_write_read bwr = {
      .write_size = size,
      .write_buffer = (binder_uintptr_t)commands,
  };
  if (ioctl(fd, BINDER_WRITE_READ, &bwr) < 0) {
    die("BINDER_WRITE_READ (write)");
  }
}

// Read return commands into `buffer`, waiting in poll() first; returns the
// number of bytes Linux wrote.
static size_t read_commands(int fd, void* buffer, size_t size) {
  for (;;) {
    if (nonblocking) {
      struct pollfd pfd = {.fd = fd, .events = POLLIN};
      if (poll(&pfd, 1, -1) < 0) {
        die("poll");
      }
    }
    struct binder_write_read bwr = {
        .read_size = size,
        .read_buffer = (binder_uintptr_t)buffer,
    };
    if (ioctl(fd, BINDER_WRITE_READ, &bwr) < 0) {
      if (errno == EAGAIN) {
        continue;
      }
      die("BINDER_WRITE_READ (read)");
    }
    if (bwr.read_consumed > 0) {
      return bwr.read_consumed;
    }
  }
}

// Walk return commands until one of `wanted`, copying its transaction data.
static uint32_t wait_for(
    int fd,
    uint32_t wanted,
    struct binder_transaction_data* out) {
  uint8_t buffer[512];
  for (;;) {
    size_t length = read_commands(fd, buffer, sizeof(buffer));
    size_t offset = 0;
    while (offset + sizeof(uint32_t) <= length) {
      uint32_t cmd;
      memcpy(&cmd, buffer + offset, sizeof(cmd));
      offset += sizeof(cmd);
      size_t payload = _IOC_SIZE(cmd);
      if (cmd == wanted) {
        memcpy(out, buffer + offset, sizeof(*out));
        return cmd;
      }
      if (cmd == BR_DEAD_REPLY || cmd == BR_FAILED_REPLY) {
        fprintf(stderr, "binder_fd_passing: transaction failed (%#x)\n", cmd);
        exit(1);
      }
      offset += payload;
    }
  }
}

static int client(const char* path) {
  int fd = open_device(path);
  struct binder_fd_object object = {
      .hdr.type = BINDER_TYPE_FD,
  };
  if (pass_fd) {
    int file = memfd_create("binder_fd_passing", MFD_CLOEXEC);
    if (file < 0 || write(file, "hello", 5) != 5 || lseek(file, 0, SEEK_SET)) {
      die("memfd");
    }
    object.fd = file;
  }
  binder_size_t offsets[1] = {0};
  struct {
    uint32_t cmd;
    struct binder_transaction_data data;
  } __attribute__((packed)) command = {
      .cmd = BC_TRANSACTION,
      .data =
          {
              .target.handle = 0,
              .code = 1,
              .flags = TF_ACCEPT_FDS,
              .data_size = pass_fd ? sizeof(object) : 0,
              .offsets_size = pass_fd ? sizeof(offsets) : 0,
              .data.ptr.buffer = (binder_uintptr_t)&object,
              .data.ptr.offsets = (binder_uintptr_t)offsets,
          },
  };
  if (sender_waits) {
    usleep(100 * 1000);
  }
  if (write_and_read) {
    // The transaction and the wait for its reply in one call. Linux runs the
    // write half first, then the read half (binder_ioctl_write_read).
    uint8_t buffer[512];
    struct binder_write_read bwr = {
        .write_size = sizeof(command),
        .write_buffer = (binder_uintptr_t)&command,
        .read_size = sizeof(buffer),
        .read_buffer = (binder_uintptr_t)buffer,
    };
    if (ioctl(fd, BINDER_WRITE_READ, &bwr) < 0) {
      die("BINDER_WRITE_READ (write and read)");
    }
    if (bwr.write_consumed != sizeof(command)) {
      fprintf(stderr, "binder_fd_passing: the write half was not consumed\n");
      return 1;
    }
  } else {
    write_commands(fd, &command, sizeof(command));
  }
  struct binder_transaction_data reply;
  wait_for(fd, BR_REPLY, &reply);
  return 0;
}

// Add device `name` to the binderfs instance mounted at `mount`, and return
// its path.
static char* add_device(const char* mount, const char* name) {
  char control[4096];
  snprintf(control, sizeof(control), "%s/binder-control", mount);
  int fd = open(control, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    die("open binder-control");
  }
  struct binderfs_device device = {0};
  snprintf(device.name, sizeof(device.name), "%s", name);
  if (ioctl(fd, BINDER_CTL_ADD, &device) < 0 && errno != EEXIST) {
    die("BINDER_CTL_ADD");
  }
  close(fd);
  static char path[4096];
  snprintf(path, sizeof(path), "%s/%s", mount, name);
  return path;
}

int main(int argc, char** argv) {
  const char* usage = "usage: %s <binderfs mount> [poll|block|ping|exhausted|writeread]\n";
  if (argc < 2 || argc > 3) {
    fprintf(stderr, usage, argv[0]);
    return 2;
  }
  if (argc == 3) {
    if (strcmp(argv[2], "block") == 0) {
      nonblocking = 0;
      sender_waits = 1;
    } else if (strcmp(argv[2], "ping") == 0) {
      nonblocking = 0;
      pass_fd = 0;
    } else if (strcmp(argv[2], "exhausted") == 0) {
      nonblocking = 0;
      pass_fd = 0;
      exhausted = 1;
      sender_waits = 1;
    } else if (strcmp(argv[2], "writeread") == 0) {
      nonblocking = 0;
      pass_fd = 0;
      write_and_read = 1;
    } else if (strcmp(argv[2], "poll") != 0) {
      fprintf(stderr, usage, argv[0]);
      return 2;
    }
  }
  const char* path = add_device(argv[1], "binder-fd-passing");
  int fd = open_device(path);
  // The context manager's node must accept descriptors, or Linux fails a
  // transaction that carries one (binder_translate_fd).
  struct flat_binder_object manager = {
      .hdr.type = BINDER_TYPE_BINDER,
      .flags = FLAT_BINDER_FLAG_ACCEPTS_FDS,
  };
  if (ioctl(fd, BINDER_SET_CONTEXT_MGR_EXT, &manager) < 0) {
    die("BINDER_SET_CONTEXT_MGR_EXT");
  }
  uint32_t enter = BC_ENTER_LOOPER;
  write_commands(fd, &enter, sizeof(enter));

  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    _exit(client(path));
  }

  if (exhausted) {
    // A read whose buffer is already full: Linux waits for work, then finds
    // no room for it and returns with nothing added (binder_thread_read).
    uint32_t full;
    struct binder_write_read bwr = {
        .read_size = sizeof(full),
        .read_consumed = sizeof(full),
        .read_buffer = (binder_uintptr_t)&full,
    };
    if (ioctl(fd, BINDER_WRITE_READ, &bwr) < 0) {
      die("BINDER_WRITE_READ (exhausted read)");
    }
    if (bwr.read_consumed != sizeof(full)) {
      fprintf(stderr, "binder_fd_passing: the exhausted read added commands\n");
      return 1;
    }
    printf("exhausted read returned\n");
    fflush(stdout);
  }

  struct binder_transaction_data data;
  wait_for(fd, BR_TRANSACTION, &data);
  if (!pass_fd) {
    printf("received transaction from pid %d\n", data.sender_pid);
    fflush(stdout);
  } else if (data.offsets_size != sizeof(binder_size_t)) {
    fprintf(stderr, "binder_fd_passing: expected one object\n");
    return 1;
  } else {
    const binder_size_t* offsets =
        (const binder_size_t*)(uintptr_t)data.data.ptr.offsets;
    const struct binder_fd_object* object =
        (const void*)(uintptr_t)(data.data.ptr.buffer + offsets[0]);
    if (object->hdr.type != BINDER_TYPE_FD) {
      fprintf(stderr, "binder_fd_passing: not a descriptor object\n");
      return 1;
    }
    int received = (int)object->fd;
    char text[6] = {0};
    if (read(received, text, 5) != 5) {
      die("read from the received descriptor");
    }
    printf("received fd %d: %s\n", received, text);
    fflush(stdout);
  }

  // Free the transaction's buffer and reply with an empty status.
  int32_t status = 0;
  struct {
    uint32_t free_cmd;
    binder_uintptr_t buffer;
    uint32_t reply_cmd;
    struct binder_transaction_data reply;
  } __attribute__((packed)) commands = {
      .free_cmd = BC_FREE_BUFFER,
      .buffer = data.data.ptr.buffer,
      .reply_cmd = BC_REPLY,
      .reply =
          {
              .flags = TF_STATUS_CODE,
              .data_size = sizeof(status),
              .data.ptr.buffer = (binder_uintptr_t)&status,
          },
  };
  write_commands(fd, &commands, sizeof(commands));

  int wstatus;
  if (waitpid(child, &wstatus, 0) != child) {
    die("waitpid");
  }
  if (!WIFEXITED(wstatus) || WEXITSTATUS(wstatus) != 0) {
    fprintf(stderr, "binder_fd_passing: client failed (%#x)\n", wstatus);
    return 1;
  }
  return 0;
}
