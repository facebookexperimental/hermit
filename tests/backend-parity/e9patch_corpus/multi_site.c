static long __attribute__((noinline)) s_write(const char*m,long n){long r;__asm__ volatile("syscall":"=a"(r):"a"(1L),"D"(1L),"S"(m),"d"(n):"rcx","r11","memory");return r;}
static long __attribute__((noinline)) s_getpid(void){long r;__asm__ volatile("syscall":"=a"(r):"a"(39L):"rcx","r11","memory");return r;}
static void __attribute__((noinline,noreturn)) s_exit(long c){__asm__ volatile("syscall"::"a"(231L),"D"(c):"rcx","r11","memory");__builtin_unreachable();}
void _start(void){ const char m[]="multi\n"; s_write(m,6); (void)s_getpid(); s_exit(0); }
