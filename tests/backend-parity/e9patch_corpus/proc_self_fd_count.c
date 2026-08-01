/* fd-hygiene: count entries in /proc/self/fd (excluding . and ..). The absolute
 * count is environment-dependent, so the corpus asserts only golden==e9patch
 * parity (expected_stdout=None): e9patch preprocessing must not leave an extra
 * loader descriptor visible to the guest (the e9loader closes its self-fd). */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]='\n';if(!u)b[i--]='0';while(u){b[i--]='0'+(u%10);u/=10;}if(v<0)b[i--]='-';sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long d=sc(257,-100,(long)"/proc/self/fd",0200000,0,0,0); char buf[4096]; long n=sc(217,d,(long)buf,sizeof buf,0,0,0); long cnt=0,off=0; while(off<n){ unsigned short reclen=*(unsigned short*)(buf+off+16); char*nm=buf+off+19; if(!(nm[0]=='.'&&(nm[1]==0||(nm[1]=='.'&&nm[2]==0)))) cnt++; off+=reclen; } puts_("open_fds="); putn(cnt); die(0); }
