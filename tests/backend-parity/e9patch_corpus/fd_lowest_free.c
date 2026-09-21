/* fd-hygiene: closing a descriptor frees its number for reuse by the next
 * open (a=3, b=4, close a, c reuses 3). Regresses that the guest fd table is
 * identical under e9patch preprocessing and plain ptrace. */
static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
static long slen(const char*s){long n=0;while(s[n])n++;return n;}
static void puts_(const char*s){sc(1,1,(long)s,slen(s),0,0,0);}
static void putn(long v){char b[32];int i=31;unsigned long u=(v<0)?(unsigned long)(-v):(unsigned long)v;b[i--]='\n';if(!u)b[i--]='0';while(u){b[i--]='0'+(u%10);u/=10;}if(v<0)b[i--]='-';sc(1,1,(long)&b[i+1],(long)(32-(i+1)),0,0,0);}
void _start(void){ long a=sc(2,(long)"/dev/null",0,0,0,0,0); long b=sc(2,(long)"/dev/null",0,0,0,0,0); sc(3,a,0,0,0,0,0); long c=sc(2,(long)"/dev/null",0,0,0,0,0); puts_("a="); putn(a); puts_("b="); putn(b); puts_("c="); putn(c); die(0); }
