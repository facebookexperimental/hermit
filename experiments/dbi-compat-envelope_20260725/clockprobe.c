#define _GNU_SOURCE
#include <stdio.h>
#include <time.h>
#include <sys/syscall.h>
#include <unistd.h>
int main(void){
    struct timespec vdso={0}, raw={0};
    clock_gettime(CLOCK_REALTIME, &vdso);              /* glibc -> vDSO fast path */
    syscall(SYS_clock_gettime, CLOCK_REALTIME, &raw);  /* forced raw syscall */
    printf("vdso_sec=%ld raw_syscall_sec=%ld\n", (long)vdso.tv_sec, (long)raw.tv_sec);
    return 0;
}
