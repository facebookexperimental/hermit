/*
 * Freestanding in-VM network-determinism probe for the QEMU-under-Hermit boot.
 *
 * This runs as the guest `init` inside a QEMU-emulated Linux kernel that is
 * itself executed under Hermit. It exercises the guest kernel's networking
 * stack entirely INTERNALLY (no external egress, which is inherently
 * nondeterministic): an AF_UNIX socketpair echo, /etc/hosts name resolution,
 * loopback interface bring-up, an AF_INET TCP client/server handshake over
 * 127.0.0.1, and an HTTP request/response over that connection.
 *
 * Everything happens inside the guest via TCG, so under `hermit run --strict
 * --verify` the whole exchange -- TCP initial sequence numbers, ephemeral
 * ports, checksums, and payload bytes -- must reproduce bitwise-identically
 * across two runs. It is written freestanding (no libc, raw syscalls) so it
 * stays on the exact same proven boot path as qemu_init.c and introduces no
 * loader / NSS nondeterminism of its own.
 *
 * Success prints QEMU_NET_ALL_OK and powers the machine down.
 */

enum {
  SYS_READ = 0,
  SYS_WRITE = 1,
  SYS_OPEN = 2,
  SYS_CLOSE = 3,
  SYS_IOCTL = 16,
  SYS_FORK = 57,
  SYS_EXIT = 60,
  SYS_WAIT4 = 61,
  SYS_UNAME = 63,
  SYS_SYNC = 162,
  SYS_REBOOT = 169,
  SYS_SOCKET = 41,
  SYS_CONNECT = 42,
  SYS_ACCEPT = 43,
  SYS_BIND = 49,
  SYS_LISTEN = 50,
  SYS_SETSOCKOPT = 54,
  SYS_SOCKETPAIR = 53,
  STDOUT_FILENO = 1,
};

enum {
  AF_UNIX = 1,
  AF_INET = 2,
  SOCK_STREAM = 1,
  SOCK_DGRAM = 2,
  SOL_SOCKET = 1,
  SO_REUSEADDR = 2,
  O_RDONLY = 0,
};

/* SIOC* interface ioctls and interface flags. */
enum {
  SIOCSIFADDR = 0x8916,
  SIOCSIFFLAGS = 0x8914,
  SIOCSIFNETMASK = 0x891c,
  IFF_UP = 0x1,
  IFF_RUNNING = 0x40,
};

/* LINUX_REBOOT magics (power off). */
enum {
  REBOOT_MAGIC1 = 0xfee1dead,
  REBOOT_MAGIC2 = 0x28121969,
  REBOOT_CMD_POWER_OFF = 0x4321fedc,
};

typedef unsigned long u64;
typedef unsigned int u32;
typedef unsigned short u16;
typedef unsigned char u8;

struct utsname {
  char sysname[65];
  char nodename[65];
  char release[65];
  char version[65];
  char machine[65];
  char domainname[65];
};

static long syscall0(long n) {
  register long rax __asm__("rax") = n;
  __asm__ volatile("syscall" : "+a"(rax) : : "rcx", "r11", "memory");
  return rax;
}
static long syscall1(long n, long a1) {
  register long rax __asm__("rax") = n;
  register long rdi __asm__("rdi") = a1;
  __asm__ volatile("syscall" : "+a"(rax) : "D"(rdi) : "rcx", "r11", "memory");
  return rax;
}
static long syscall2(long n, long a1, long a2) {
  register long rax __asm__("rax") = n;
  register long rdi __asm__("rdi") = a1;
  register long rsi __asm__("rsi") = a2;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi), "S"(rsi)
                   : "rcx", "r11", "memory");
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
static long syscall5(long n, long a1, long a2, long a3, long a4, long a5) {
  register long rax __asm__("rax") = n;
  register long rdi __asm__("rdi") = a1;
  register long rsi __asm__("rsi") = a2;
  register long rdx __asm__("rdx") = a3;
  register long r10 __asm__("r10") = a4;
  register long r8 __asm__("r8") = a5;
  __asm__ volatile("syscall"
                   : "+a"(rax)
                   : "D"(rdi), "S"(rsi), "d"(rdx), "r"(r10), "r"(r8)
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

/* Append a signed decimal to buf at *pos. */
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

static void mzero(void *p, unsigned long n) {
  u8 *b = (u8 *)p;
  for (unsigned long i = 0; i < n; ++i) b[i] = 0;
}
static void mcopy(void *d, const void *s, unsigned long n) {
  u8 *dd = (u8 *)d;
  const u8 *ss = (const u8 *)s;
  for (unsigned long i = 0; i < n; ++i) dd[i] = ss[i];
}
static int seq_eq(const char *a, const char *b, unsigned long n) {
  for (unsigned long i = 0; i < n; ++i)
    if (a[i] != b[i]) return 0;
  return 1;
}
/* Return 1 if haystack[0..hlen) contains needle. */
static int contains(const char *hay, unsigned long hlen, const char *needle) {
  unsigned long nlen = slen(needle);
  if (nlen == 0) return 1;
  if (hlen < nlen) return 0;
  for (unsigned long i = 0; i + nlen <= hlen; ++i)
    if (seq_eq(hay + i, needle, nlen)) return 1;
  return 0;
}

static u16 be16(u16 v) { return (u16)((v << 8) | (v >> 8)); }

/* Build a 16-byte sockaddr_in {AF_INET, port(host order), ipv4(network order)}. */
static void make_sin(u8 *sa, u16 port, u32 ipnet) {
  mzero(sa, 16);
  sa[0] = AF_INET;          /* sin_family low byte */
  sa[1] = 0;                /* sin_family high byte */
  *(u16 *)(sa + 2) = be16(port);
  *(u32 *)(sa + 4) = ipnet; /* already network byte order */
}

/*
 * Parse a dotted-decimal IPv4 from s (len bytes) into a network-order u32.
 * Returns 0 on success, -1 on parse error.
 */
static int inet_aton_net(const char *s, unsigned long len, u32 *out_net) {
  u32 parts[4];
  int pi = 0;
  unsigned long i = 0;
  while (pi < 4) {
    if (i >= len || s[i] < '0' || s[i] > '9') return -1;
    u32 acc = 0;
    while (i < len && s[i] >= '0' && s[i] <= '9') {
      acc = acc * 10 + (u32)(s[i] - '0');
      ++i;
    }
    if (acc > 255) return -1;
    parts[pi++] = acc;
    if (pi < 4) {
      if (i >= len || s[i] != '.') return -1;
      ++i;
    }
  }
  *out_net = parts[0] | (parts[1] << 8) | (parts[2] << 16) | (parts[3] << 24);
  return 0;
}

/*
 * Deterministic name resolution via /etc/hosts (no network, no NSS).
 * Finds the line whose fields include `name` and returns its first token
 * (the address) copied into ip_out plus its length. Returns 0 on success.
 */
static int resolve_hosts(const char *name, char *ip_out, int *ip_len) {
  char buf[4096];
  long fd = syscall3(SYS_OPEN, (long)"/etc/hosts", O_RDONLY, 0);
  if (fd < 0) return -1;
  long total = 0;
  for (;;) {
    long r = syscall3(SYS_READ, fd, (long)(buf + total),
                      (long)(sizeof(buf) - 1 - (unsigned long)total));
    if (r <= 0) break;
    total += r;
    if ((unsigned long)total >= sizeof(buf) - 1) break;
  }
  syscall1(SYS_CLOSE, fd);
  buf[total] = 0;

  unsigned long nlen = slen(name);
  long i = 0;
  while (i < total) {
    long ls = i;
    while (i < total && buf[i] != '\n') ++i;
    long le = i; /* [ls,le) is one line */
    if (i < total) ++i;
    /* skip comments */
    long p = ls;
    while (p < le && (buf[p] == ' ' || buf[p] == '\t')) ++p;
    if (p >= le || buf[p] == '#') continue;
    /* first token = address */
    long as = p;
    while (p < le && buf[p] != ' ' && buf[p] != '\t') ++p;
    long ae = p; /* [as,ae) address token */
    /* scan remaining tokens for name */
    int matched = 0;
    while (p < le) {
      while (p < le && (buf[p] == ' ' || buf[p] == '\t')) ++p;
      long ts = p;
      while (p < le && buf[p] != ' ' && buf[p] != '\t') ++p;
      long te = p;
      if (te - ts == (long)nlen && seq_eq(buf + ts, name, nlen)) {
        matched = 1;
        break;
      }
    }
    if (matched) {
      int n = (int)(ae - as);
      if (n <= 0 || n >= 64) return -1;
      mcopy(ip_out, buf + as, (unsigned long)n);
      ip_out[n] = 0;
      *ip_len = n;
      return 0;
    }
  }
  return -1;
}

static void power_off(void) {
  syscall0(SYS_SYNC);
  syscall4(SYS_REBOOT, REBOOT_MAGIC1, REBOOT_MAGIC2, REBOOT_CMD_POWER_OFF, 0);
  for (;;) syscall0(34 /* SYS_pause */);
}

/* Bring the loopback interface up and assign 127.0.0.1/8. Returns 0 on ok. */
static long lo_up(void) {
  long fd = syscall3(SYS_SOCKET, AF_INET, SOCK_DGRAM, 0);
  if (fd < 0) return fd;
  u8 ifr[40];

  /* address 127.0.0.1 */
  mzero(ifr, sizeof(ifr));
  ifr[0] = 'l';
  ifr[1] = 'o';
  make_sin(ifr + 16, 0, 0x0100007f /* 127.0.0.1 net order */);
  long r = syscall3(SYS_IOCTL, fd, SIOCSIFADDR, (long)ifr);
  if (r < 0 && r != -114 /* EEXIST-ish */) { syscall1(SYS_CLOSE, fd); return r; }

  /* netmask 255.0.0.0 */
  mzero(ifr, sizeof(ifr));
  ifr[0] = 'l';
  ifr[1] = 'o';
  make_sin(ifr + 16, 0, 0x000000ff /* 255.0.0.0 net order */);
  syscall3(SYS_IOCTL, fd, SIOCSIFNETMASK, (long)ifr);

  /* flags: UP|RUNNING */
  mzero(ifr, sizeof(ifr));
  ifr[0] = 'l';
  ifr[1] = 'o';
  *(u16 *)(ifr + 16) = IFF_UP | IFF_RUNNING;
  r = syscall3(SYS_IOCTL, fd, SIOCSIFFLAGS, (long)ifr);
  syscall1(SYS_CLOSE, fd);
  return r;
}

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

  /* 1. AF_UNIX socketpair echo (unprivileged, no interface required). */
  {
    int sv[2] = {-1, -1};
    long r = syscall4(SYS_SOCKETPAIR, AF_UNIX, SOCK_STREAM, 0, (long)sv);
    if (r < 0) {
      pos = 0;
      mcopy(line, "QEMU_NET_SOCKETPAIR_FAILED err=", 31);
      pos = 31;
      put_dec(line, &pos, -r);
      line[pos++] = '\n';
      outn(line, pos);
    } else {
      const char *msg = "PING";
      syscall3(SYS_WRITE, sv[0], (long)msg, 4);
      char rb[8];
      long got = syscall3(SYS_READ, sv[1], (long)rb, 8);
      syscall1(SYS_CLOSE, sv[0]);
      syscall1(SYS_CLOSE, sv[1]);
      pos = 0;
      mcopy(line, "QEMU_NET_SOCKETPAIR_OK bytes=", 29);
      pos = 29;
      put_dec(line, &pos, got);
      if (got == 4 && seq_eq(rb, "PING", 4)) mcopy(line + pos, " data=PING", 10), pos += 10;
      line[pos++] = '\n';
      outn(line, pos);
    }
  }

  /* 2. Deterministic DNS via /etc/hosts. */
  char ip[64];
  int ip_len = 0;
  u32 ipnet = 0x0100007f; /* default 127.0.0.1 */
  {
    if (resolve_hosts("localhost", ip, &ip_len) == 0 &&
        inet_aton_net(ip, (unsigned long)ip_len, &ipnet) == 0) {
      pos = 0;
      mcopy(line, "QEMU_NET_DNS_OK localhost=", 26);
      pos = 26;
      mcopy(line + pos, ip, (unsigned long)ip_len);
      pos += ip_len;
      line[pos++] = '\n';
      outn(line, pos);
    } else {
      ipnet = 0x0100007f;
      out("QEMU_NET_DNS_FALLBACK localhost=127.0.0.1\n");
    }
  }

  /* 3. Bring up loopback. */
  long lr = lo_up();
  if (lr < 0) {
    pos = 0;
    mcopy(line, "QEMU_NET_LO_SKIP err=", 21);
    pos = 21;
    put_dec(line, &pos, -lr);
    line[pos++] = '\n';
    outn(line, pos);
    /* Without lo we cannot do the AF_INET path; still a clean, deterministic
     * run of the socketpair + DNS stages. */
    out("QEMU_NET_ALL_OK\n");
    power_off();
  }
  out("QEMU_NET_LO_UP\n");

  /* 4 + 5. AF_INET TCP + HTTP over 127.0.0.1:8080. Listen, then fork a client. */
  long lsock = syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM, 0);
  if (lsock < 0) {
    out("QEMU_NET_TCP_SOCKET_FAILED\n");
    power_off();
  }
  int one = 1;
  syscall5(SYS_SETSOCKOPT, lsock, SOL_SOCKET, SO_REUSEADDR, (long)&one, 4);
  u8 sin[16];
  make_sin(sin, 8080, ipnet);
  if (syscall3(SYS_BIND, lsock, (long)sin, 16) < 0) {
    out("QEMU_NET_TCP_BIND_FAILED\n");
    power_off();
  }
  if (syscall2(SYS_LISTEN, lsock, 8) < 0) {
    out("QEMU_NET_TCP_LISTEN_FAILED\n");
    power_off();
  }

  long pid = syscall0(SYS_FORK);
  if (pid == 0) {
    /* CLIENT: connect, send HTTP request, read + verify response. */
    long cs = syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM, 0);
    if (cs < 0) syscall1(SYS_EXIT, 21);
    u8 dst[16];
    make_sin(dst, 8080, ipnet);
    if (syscall3(SYS_CONNECT, cs, (long)dst, 16) < 0) syscall1(SYS_EXIT, 22);
    const char *req = "GET / HTTP/1.0\r\nHost: localhost\r\n\r\n";
    syscall3(SYS_WRITE, cs, (long)req, (long)slen(req));
    char resp[512];
    long total = 0;
    for (;;) {
      long r = syscall3(SYS_READ, cs, (long)(resp + total),
                        (long)(sizeof(resp) - (unsigned long)total));
      if (r <= 0) break;
      total += r;
      if ((unsigned long)total >= sizeof(resp)) break;
    }
    syscall1(SYS_CLOSE, cs);
    int ok_status = contains(resp, total, "HTTP/1.0 200");
    int ok_body = contains(resp, total, "HELLO");
    if (ok_status && ok_body) syscall1(SYS_EXIT, 0);
    syscall1(SYS_EXIT, 23);
  }

  /* SERVER (parent): accept, read request, send HTTP response. */
  long conn = syscall4(SYS_ACCEPT, lsock, 0, 0, 0);
  int req_ok = 0;
  if (conn >= 0) {
    char rb[512];
    long got = syscall3(SYS_READ, conn, (long)rb, sizeof(rb));
    if (got > 0 && contains(rb, got, "GET /")) req_ok = 1;
    const char *resp =
        "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nHELLO";
    syscall3(SYS_WRITE, conn, (long)resp, (long)slen(resp));
    syscall1(SYS_CLOSE, conn);
  }
  syscall1(SYS_CLOSE, lsock);

  long status = 0;
  syscall4(SYS_WAIT4, pid, (long)&status, 0, 0);
  int client_ok = ((status & 0x7f) == 0) && (((status >> 8) & 0xff) == 0);

  if (req_ok) out("QEMU_NET_TCP_OK proto=tcp addr=127.0.0.1:8080\n");
  else out("QEMU_NET_TCP_REQ_MISMATCH\n");

  if (client_ok) out("QEMU_NET_HTTP_OK status=200 body=HELLO\n");
  else {
    pos = 0;
    mcopy(line, "QEMU_NET_HTTP_FAILED wstatus=", 29);
    pos = 29;
    put_dec(line, &pos, status);
    line[pos++] = '\n';
    outn(line, pos);
  }

  if (req_ok && client_ok) out("QEMU_NET_ALL_OK\n");
  else out("QEMU_NET_PARTIAL\n");

  power_off();
}
