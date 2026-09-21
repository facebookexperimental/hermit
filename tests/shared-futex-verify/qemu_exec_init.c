/*
 * Freestanding "run a userspace program" launcher for QEMU-under-Hermit.
 *
 * Runs as guest init (PID 1) inside a QEMU-emulated Linux kernel executed under
 * Hermit. After the kernel boots it fork()/execve()s a real userspace program,
 * wait4()s it, prints the child's exit status, and powers the machine off. This
 * proves Hermit can deterministically run an ordinary userspace binary -- a
 * statically linked glibc "hello world" or busybox -- inside the deterministic
 * VM, one rung past the single freestanding init used by strict_l2_test.sh.
 *
 * Two scenarios are selected at compile time:
 *   (default)          exec /hello  ["/hello"]                -> expect exit 7
 *   -DSCENARIO_BUSYBOX exec /bin/busybox ["busybox","sh","-c", SCRIPT]
 *
 * The launcher itself is freestanding (raw syscalls, no libc) so it adds no
 * loader nondeterminism; the child program is a normal libc / busybox binary.
 */

enum {
  SYS_READ = 0,
  SYS_WRITE = 1,
  SYS_CLOSE = 3,
  SYS_EXECVE = 59,
  SYS_EXIT = 60,
  SYS_WAIT4 = 61,
  SYS_FORK = 57,
  SYS_UNAME = 63,
  SYS_SYNC = 162,
  SYS_REBOOT = 169,
  SYS_PAUSE = 34,
  STDOUT_FILENO = 1,
};

enum {
  REBOOT_MAGIC1 = 0xfee1dead,
  REBOOT_MAGIC2 = 0x28121969,
  REBOOT_CMD_POWER_OFF = 0x4321fedc,
};

struct utsname {
  char sysname[65];
  char nodename[65];
  char release[65];
  char version[65];
  char machine[65];
  char domainname[65];
};

static long syscall1(long n, long a1) {
  register long rax __asm__("rax") = n;
  register long rdi __asm__("rdi") = a1;
  __asm__ volatile("syscall" : "+a"(rax) : "D"(rdi) : "rcx", "r11", "memory");
  return rax;
}
static long syscall0(long n) {
  register long rax __asm__("rax") = n;
  __asm__ volatile("syscall" : "+a"(rax) : : "rcx", "r11", "memory");
  return rax;
}
static long syscall3(long n, long a1, long a2, long a3) {
  register long rax __asm__("rax") = n;
  register long rdi __asm__("rdi") = a1;
  register long rsi __asm__("rsi") = a2;
  register long rdx __asm__("rdx") = a3;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi), "S"(rsi), "d"(rdx)
                   : "rcx", "r11", "memory");
  return rax;
}
static long syscall4(long n, long a1, long a2, long a3, long a4) {
  register long rax __asm__("rax") = n;
  register long rdi __asm__("rdi") = a1;
  register long rsi __asm__("rsi") = a2;
  register long rdx __asm__("rdx") = a3;
  register long r10 __asm__("r10") = a4;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi), "S"(rsi), "d"(rdx), "r"(r10)
                   : "rcx", "r11", "memory");
  return rax;
}

static unsigned long slen(const char *s) {
  unsigned long n = 0;
  while (s[n]) ++n;
  return n;
}
static void out(const char *s) { syscall3(SYS_WRITE, STDOUT_FILENO, (long)s, slen(s)); }
static void outn(const char *s, long n) { syscall3(SYS_WRITE, STDOUT_FILENO, (long)s, n); }
static void put_dec(char *buf, int *pos, long v) {
  char tmp[24];
  int t = 0;
  if (v < 0) {
    buf[(*pos)++] = '-';
    v = -v;
  }
  if (v == 0) tmp[t++] = '0';
  while (v > 0) {
    tmp[t++] = (char)('0' + (v % 10));
    v /= 10;
  }
  while (t > 0) buf[(*pos)++] = tmp[--t];
}

static void power_off(void) {
  syscall0(SYS_SYNC);
  syscall4(SYS_REBOOT, REBOOT_MAGIC1, REBOOT_MAGIC2, REBOOT_CMD_POWER_OFF, 0);
  for (;;) syscall0(SYS_PAUSE);
}

#ifdef SCENARIO_BUSYBOX
#ifndef BUSYBOX_SCRIPT
#define BUSYBOX_SCRIPT "echo QEMU_BUSYBOX_HELLO from $(busybox uname -s); exit 5"
#endif
static const char *g_path = "/bin/busybox";
static char *const g_argv[] = {(char *)"busybox", (char *)"sh", (char *)"-c",
                               (char *)BUSYBOX_SCRIPT, 0};
static const char *g_name = "busybox-sh";
#else
static const char *g_path = "/hello";
static char *const g_argv[] = {(char *)"/hello", 0};
static const char *g_name = "hello";
#endif

static char *const g_envp[] = {(char *)"PATH=/bin:/sbin:/usr/bin:/usr/sbin",
                               (char *)"HOME=/", (char *)"TERM=linux", 0};

void _start(void) {
  char line[256];
  int pos;

  struct utsname sys;
  if (syscall1(SYS_UNAME, (long)&sys) < 0) {
    out("SHARED_FUTEX_QEMU_UNAME_FAILED\n");
    syscall1(SYS_EXIT, 1);
  }
  out("SHARED_FUTEX_QEMU_KERNEL_OK release=");
  out(sys.release);
  out(" machine=");
  out(sys.machine);
  out("\n");

  out("QEMU_USERSPACE_LAUNCH prog=");
  out(g_path);
  out("\n");

  long pid = syscall0(SYS_FORK);
  if (pid == 0) {
    /* child: become the target userspace program */
    syscall3(SYS_EXECVE, (long)g_path, (long)g_argv, (long)g_envp);
    /* execve only returns on failure */
    out("QEMU_USERSPACE_EXEC_FAILED\n");
    syscall1(SYS_EXIT, 127);
  }
  if (pid < 0) {
    out("QEMU_USERSPACE_FORK_FAILED\n");
    power_off();
  }

  long wstatus = 0;
  long r = syscall4(SYS_WAIT4, pid, (long)&wstatus, 0, 0);
  if (r < 0) {
    out("QEMU_USERSPACE_WAIT_FAILED\n");
    power_off();
  }

  int exited = (wstatus & 0x7f) == 0;
  int code = (int)((wstatus >> 8) & 0xff);
  int sig = (int)(wstatus & 0x7f);

  pos = 0;
  {
    const char *p = "QEMU_USERSPACE_EXIT prog=";
    for (unsigned long i = 0; p[i]; ++i) line[pos++] = p[i];
  }
  for (unsigned long i = 0; g_name[i]; ++i) line[pos++] = g_name[i];
  if (exited) {
    const char *p = " exited=1 status=";
    for (unsigned long i = 0; p[i]; ++i) line[pos++] = p[i];
    put_dec(line, &pos, code);
  } else {
    const char *p = " exited=0 signal=";
    for (unsigned long i = 0; p[i]; ++i) line[pos++] = p[i];
    put_dec(line, &pos, sig);
  }
  line[pos++] = '\n';
  outn(line, pos);

  out("QEMU_USERSPACE_DONE\n");
  power_off();
}
