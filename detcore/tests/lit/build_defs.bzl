# Open-source placeholders for the lit guest test macros used by BUCK in
# this directory.
#
# The lit tests here run under Meta's lit/Tpx runner internally. Open-source
# builds do not carry that runner (the shim's lit_unittest is likewise a
# no-op): the same guests are built and run here by the Cargo test-harness
# manifest flow instead. So in the OSS tree these macros define no targets.
# If an internal definition of this file exists, it must keep precedence
# over these placeholders; do not let this file silently replace it.

def cpp_lit_unittest(**_kwargs):
    pass

def rust_lit_unittest(**_kwargs):
    pass

def go_lit_unittest(**_kwargs):
    pass
