/* Execute 4 MiB of text so DBT must translate the full code path. */
__asm__(
    ".pushsection .text.benchmark_padding,\"ax\",@progbits\n"
    ".balign 16\n"
    ".global benchmark_padding\n"
    ".type benchmark_padding,@function\n"
    "benchmark_padding:\n"
    ".rept 4194304\n"
    "nop\n"
    ".endr\n"
    "ret\n"
    ".size benchmark_padding,.-benchmark_padding\n"
    ".popsection\n");

extern void benchmark_padding(void);

int main(void) {
    benchmark_padding();
    return 0;
}
