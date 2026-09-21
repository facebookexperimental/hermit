/* output-hygiene: a single writev with three iovecs emits "ABC\n" atomically
 * and returns the total byte count (4). Regresses gathered-write output
 * parity between e9patch preprocessing and plain ptrace. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]='\n';if(!u)b[i--]='0';while(u){b[i--]='0'+(u%10);u/=10;}if(v<0)b[i--]='-';sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
struct iovec{void*base;unsigned long len;};
void _start(void){ struct iovec v[3]; v[0].base=(void*)"AB"; v[0].len=2; v[1].base=(void*)"C"; v[1].len=1; v[2].base=(void*)"\n"; v[2].len=1; long r=sc(20,1,(long)v,3,0,0,0); puts_("wrote="); putn(r); die(0); }
