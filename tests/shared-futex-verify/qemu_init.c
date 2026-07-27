enum {
  SYS_WRITE = 1,
  SYS_PAUSE = 34,
  SYS_EXIT = 60,
  SYS_UNAME = 63,
  SYS_SYNC = 162,
  SYS_REBOOT = 169,
  STDOUT_FILENO = 1,
};

struct utsname {
  char sysname[65];
  char nodename[65];
  char release[65];
  char version[65];
  char machine[65];
  char domainname[65];
};

static long syscall0(long number) {
  register long rax __asm__("rax") = number;
  __asm__ volatile("syscall" : "+a"(rax) : : "rcx", "r11", "memory");
  return rax;
}

static long syscall1(long number, long arg1) {
  register long rax __asm__("rax") = number;
  register long rdi __asm__("rdi") = arg1;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi)
                   : "rcx", "r11", "memory");
  return rax;
}

static long syscall3(long number, long arg1, long arg2, long arg3) {
  register long rax __asm__("rax") = number;
  register long rdi __asm__("rdi") = arg1;
  register long rsi __asm__("rsi") = arg2;
  register long rdx __asm__("rdx") = arg3;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi), "S"(rsi), "d"(rdx)
                   : "rcx", "r11", "memory");
  return rax;
}

static long syscall4(long number, long arg1, long arg2, long arg3, long arg4) {
  register long rax __asm__("rax") = number;
  register long rdi __asm__("rdi") = arg1;
  register long rsi __asm__("rsi") = arg2;
  register long rdx __asm__("rdx") = arg3;
  register long r10 __asm__("r10") = arg4;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi), "S"(rsi), "d"(rdx), "r"(r10)
                   : "rcx", "r11", "memory");
  return rax;
}

static unsigned long text_length(const char *text) {
  unsigned long length = 0;
  while (text[length] != '\0') {
    ++length;
  }
  return length;
}

static void write_text(const char *text) {
  syscall3(SYS_WRITE, STDOUT_FILENO, (long)text, text_length(text));
}

void _start(void) {
  struct utsname system;
  if (syscall1(SYS_UNAME, (long)&system) < 0) {
    write_text("SHARED_FUTEX_QEMU_UNAME_FAILED\n");
    syscall1(SYS_EXIT, 1);
  }

  write_text("SHARED_FUTEX_QEMU_KERNEL_OK release=");
  write_text(system.release);
  write_text(" machine=");
  write_text(system.machine);
  write_text("\n");
  syscall0(SYS_SYNC);
  syscall4(SYS_REBOOT, 0xfee1dead, 0x28121969, 0x4321fedc, 0);
  for (;;) {
    syscall0(SYS_PAUSE);
  }
}
