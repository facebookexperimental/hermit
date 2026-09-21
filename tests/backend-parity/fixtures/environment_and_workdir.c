#define _GNU_SOURCE

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/magic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/statfs.h>
#include <unistd.h>

extern char **environ;

static int fail(const char *message) {
    fprintf(stderr, "%s\n", message);
    return EXIT_FAILURE;
}

static const char *environment_value(const char *name) {
    size_t length = strlen(name);
    for (char **entry = environ; *entry != NULL; ++entry) {
        if (strncmp(*entry, name, length) == 0 && (*entry)[length] == '=') {
            return *entry + length + 1;
        }
    }
    return NULL;
}

static int has_prefix_and_suffix(const char *value, const char *prefix,
                                 const char *suffix) {
    if (value == NULL) {
        return 0;
    }
    size_t value_length = strlen(value);
    size_t prefix_length = strlen(prefix);
    size_t suffix_length = strlen(suffix);
    return value_length >= prefix_length + suffix_length &&
           strncmp(value, prefix, prefix_length) == 0 &&
           strcmp(value + value_length - suffix_length, suffix) == 0;
}

static int check_environment(void) {
    static const char *const expected[] = {
        "ASAN_OPTIONS",
        "E2E_FIXTURE_DIR",
        "E2E_TMPDIR",
        "HERMIT_E2E_SCHEDULED_JOBS",
        "HOME",
        "HOSTNAME",
        "LC_ALL",
        "LSAN_OPTIONS",
        "PATH",
        "TZ",
        "XDG_CONFIG_HOME",
    };
    int seen[sizeof(expected) / sizeof(expected[0])] = {0};
    size_t count = 0;

    for (char **entry = environ; *entry != NULL; ++entry) {
        const char *equals = strchr(*entry, '=');
        if (equals == NULL) {
            return fail("environment entry has no equals sign");
        }
        size_t name_length = (size_t)(equals - *entry);
        size_t index;
        for (index = 0; index < sizeof(expected) / sizeof(expected[0]); ++index) {
            if (strlen(expected[index]) == name_length &&
                strncmp(*entry, expected[index], name_length) == 0) {
                if (seen[index] != 0) {
                    return fail("environment contains a duplicate name");
                }
                seen[index] = 1;
                break;
            }
        }
        if (index == sizeof(expected) / sizeof(expected[0])) {
            return fail("environment contains a name outside the allowlist");
        }
        ++count;
    }

    if (count != sizeof(expected) / sizeof(expected[0])) {
        return fail("environment is missing an allowlisted name");
    }
    for (size_t index = 0; index < sizeof(expected) / sizeof(expected[0]); ++index) {
        if (seen[index] == 0) {
            return fail("environment is missing an allowlisted name");
        }
    }

    if (strcmp(environment_value("HOSTNAME"), "hermetic-container.local") != 0 ||
        strcmp(environment_value("PATH"),
               "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin") != 0 ||
        strcmp(environment_value("ASAN_OPTIONS"), "detect_leaks=0") != 0 ||
        strcmp(environment_value("LC_ALL"), "C") != 0 ||
        strcmp(environment_value("LSAN_OPTIONS"), "detect_leaks=0") != 0 ||
        strcmp(environment_value("TZ"), "UTC") != 0 ||
        strcmp(environment_value("E2E_TMPDIR"), "/test") != 0) {
        return fail("environment contains an unexpected fixed value");
    }

    const char *home = environment_value("HOME");
    const char *xdg = environment_value("XDG_CONFIG_HOME");
    const char *fixtures = environment_value("E2E_FIXTURE_DIR");
    if (!has_prefix_and_suffix(home, "/results/", "/home") ||
        !has_prefix_and_suffix(xdg, "/results/", "/xdg-config") ||
        !has_prefix_and_suffix(fixtures, "/results/", "/fixtures")) {
        return fail("environment path is not rooted in the pinned result mount");
    }
    size_t base_length = strlen(home) - strlen("/home");
    if (strlen(xdg) != base_length + strlen("/xdg-config") ||
        strlen(fixtures) != base_length + strlen("/fixtures") ||
        strncmp(home, xdg, base_length) != 0 ||
        strncmp(home, fixtures, base_length) != 0) {
        return fail("environment paths do not name one result cell");
    }

    const char *jobs = environment_value("HERMIT_E2E_SCHEDULED_JOBS");
    char *end = NULL;
    errno = 0;
    long parsed_jobs = jobs == NULL ? 0 : strtol(jobs, &end, 10);
    if (jobs == NULL || errno != 0 || end == jobs || *end != '\0' || parsed_jobs < 1) {
        return fail("HERMIT_E2E_SCHEDULED_JOBS is not a positive integer");
    }

    puts("minimal environment ok");
    return EXIT_SUCCESS;
}

static int check_workdir(void) {
    char physical[4096];
    if (realpath(".", physical) == NULL || strcmp(physical, "/test") != 0) {
        return fail("pwd -P is not /test");
    }

    struct statfs filesystem;
    if (statfs(".", &filesystem) != 0 || filesystem.f_type != TMPFS_MAGIC) {
        return fail("/test is not a tmpfs mount");
    }

    DIR *directory = opendir(".");
    if (directory == NULL) {
        return fail("cannot open /test");
    }
    errno = 0;
    struct dirent *entry;
    while ((entry = readdir(directory)) != NULL) {
        if (strcmp(entry->d_name, ".") != 0 && strcmp(entry->d_name, "..") != 0) {
            closedir(directory);
            return fail("/test was not empty at guest start");
        }
    }
    if (errno != 0 || closedir(directory) != 0) {
        return fail("cannot read /test");
    }

    int marker = open("myfile.txt", O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (marker < 0) {
        return fail("cannot create myfile.txt in fresh /test");
    }
    if (write(marker, "created\n", 8) != 8 || close(marker) != 0) {
        return fail("cannot write myfile.txt in fresh /test");
    }

    puts("test workdir ok");
    return EXIT_SUCCESS;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        return fail("expected environment or workdir");
    }
    if (strcmp(argv[1], "environment") == 0) {
        return check_environment();
    }
    if (strcmp(argv[1], "workdir") == 0) {
        return check_workdir();
    }
    return fail("unknown check");
}
