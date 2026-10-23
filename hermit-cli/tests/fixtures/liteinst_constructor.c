/* Keep the original M2 workload byte-for-byte. This separate executable adds
 * a cold-constructor observation after that workload has returned normally. */
#define main m2_main
#include "../../../reverie/reverie-liteinst/tests/fixtures/m2_stack_guest.c"
#undef main

/* A real executable export competes with the DSO's public constructor name.
 * Protected local registration must reach the genuine constructor. This uses
 * raw exit_group so an interposed entry cannot appear to pass the workload. */
__attribute__((noreturn, noinline, used, visibility("default")))
void detcore_liteinst_initialize(void) {
    __asm__ volatile("mov $231, %%eax\n\t"
                     "mov $93, %%edi\n\t"
                     "syscall\n\t"
                     "ud2"
                     : : : "rax", "rdi", "rcx", "r11", "memory");
    __builtin_unreachable();
}

struct constructor_stack_record {
    uint64_t layout_version, phase, owner_tid, extent_start, extent_end;
    uint64_t bottom, top, caller_rsp, adoption_entry, body_entry;
    uint64_t first_rust_sample_rsp, body_call_sample_rsp;
    uint64_t control_address, record_address, region_base, region_end;
};
_Static_assert(sizeof(struct constructor_stack_record) == 128, "cold stack ABI");

int main(int argc, char **argv) {
    int result = m2_main(argc, argv);
    if (result) return result;
    void *symbol = dlsym(RTLD_DEFAULT, "m3_constructor_stack_query");
    Dl_info cold_owner;
    if (!symbol || !dladdr(symbol, &cold_owner) || !cold_owner.dli_fname ||
        cold_owner.dli_fbase != owner.dli_fbase ||
        strcmp(cold_owner.dli_fname, owner.dli_fname)) {
        fputs("cold query is absent or belongs to another runtime\n", stderr);
        return 2;
    }
    int (*cold_query)(struct constructor_stack_record *, size_t);
    memcpy(&cold_query, &symbol, sizeof(cold_query));
    struct constructor_stack_record q = {0};
    int query_result = cold_query(&q, sizeof(q));
    printf("{\"schema\":1,\"owner\":"); print_string(cold_owner.dli_fname);
    printf(",\"owner_base\":%" PRIu64 ",\"query_result\":%d,\"record\":{",
           (uint64_t)(uintptr_t)cold_owner.dli_fbase, query_result);
#define U(field) printf("\"" #field "\":%" PRIu64 ",", q.field)
    U(layout_version); U(phase); U(owner_tid); U(extent_start); U(extent_end);
    U(bottom); U(top); U(caller_rsp); U(adoption_entry); U(body_entry);
    U(first_rust_sample_rsp); U(body_call_sample_rsp); U(control_address);
    U(record_address); U(region_base);
#undef U
    printf("\"region_end\":%" PRIu64 "}}\n", q.region_end);
    /* Successful capture is not a placement or byte-equality verdict. */
    return query_result ? 2 : 0;
}
