#define _GNU_SOURCE

#include <dlfcn.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

__asm__(".text\n"
        ".p2align 4\n"
        ".global hermit_liteinst_activation_getpid\n"
        ".type hermit_liteinst_activation_getpid,@function\n"
        "hermit_liteinst_activation_getpid:\n"
        "mov $39, %eax\n"
        ".global hermit_liteinst_activation_getpid_site\n"
        "hermit_liteinst_activation_getpid_site:\n"
        "syscall\n"
        "nop\n"
        "nop\n"
        "nop\n"
        "ret\n"
        ".size hermit_liteinst_activation_getpid, "
        ".-hermit_liteinst_activation_getpid\n");

extern long hermit_liteinst_activation_getpid(void);
extern unsigned char hermit_liteinst_activation_getpid_site;

typedef uint64_t (*count_fn)(uint64_t);

static count_fn load_count(const char *name) {
  count_fn function = (count_fn)dlsym(RTLD_DEFAULT, name);
  if (function == NULL) {
    fprintf(stderr, "missing %s: %s\n", name, dlerror());
    exit(20);
  }
  return function;
}

int main(void) {
  long expected = -1;
  for (unsigned int index = 0; index < 32; ++index) {
    long observed = hermit_liteinst_activation_getpid();
    if (expected == -1) {
      expected = observed;
    } else if (expected != observed) {
      return 21;
    }
  }
  uint64_t address =
      (uint64_t)(uintptr_t)&hermit_liteinst_activation_getpid_site;
  uint64_t traps =
      load_count("reverie_liteinst_site_trap_count")(address);
  uint64_t hooks =
      load_count("reverie_liteinst_site_hook_count")(address);
  printf("calls=32 traps=%" PRIu64 " hooks=%" PRIu64 "\n", traps, hooks);
  return traps == 1 && hooks == 31 ? 0 : 22;
}
