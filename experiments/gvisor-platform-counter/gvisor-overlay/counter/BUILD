load("//tools:defs.bzl", "go_binary")

package(
    default_applicable_licenses = ["//:license"],
    licenses = ["notice"],
)

go_binary(
    name = "counter",
    pure = True,
    srcs = ["main.go"],
    visibility = ["//:sandbox"],
    deps = [
        "//pkg/context",
        "//pkg/cpuid",
        "//pkg/hostarch",
        "//pkg/memutil",
        "//pkg/sentry/arch",
        "//pkg/sentry/limits",
        "//pkg/sentry/memmap",
        "//pkg/sentry/mm",
        "//pkg/sentry/pgalloc",
        "//pkg/sentry/platform",
        "//pkg/sentry/platform/kvm",
        "//pkg/sentry/platform/systrap",
        "//pkg/usermem",
        "@org_golang_x_sys//unix:go_default_library",
    ],
)
