static long sc(long n,long a,long b,long c,long d,long e,long f){long r;register long r10 __asm__("r10")=d;register long r8 __asm__("r8")=e;register long r9 __asm__("r9")=f;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
__attribute__((noreturn)) static void die(int s){sc(231,s,0,0,0,0,0);__builtin_unreachable();}
void _start(void){ long p=sc(9,0,4096,3,0x22,-1,0); if(p<0) die(1); volatile char*b=(char*)p; b[0]=7; long r=sc(11,p,4096,0,0,0,0); die(r==0?0:1); }
