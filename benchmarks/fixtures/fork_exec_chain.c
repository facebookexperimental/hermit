#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static unsigned int parse_depth(const char *value) {
    char *end = NULL;
    const unsigned long parsed = strtoul(value, &end, 10);

    if (value[0] == '\0' || *end != '\0' || parsed > 1000) {
        fprintf(stderr, "depth must be an integer from 0 through 1000\n");
        exit(2);
    }
    return (unsigned int)parsed;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s DEPTH\n", argv[0]);
        return 2;
    }

    const unsigned int depth = parse_depth(argv[1]);
    if (depth == 0) {
        return 0;
    }

    const pid_t child = fork();
    if (child < 0) {
        perror("fork");
        return 1;
    }
    if (child == 0) {
        char next_depth[32];
        const int written = snprintf(next_depth, sizeof(next_depth), "%u", depth - 1);
        if (written < 0 || (size_t)written >= sizeof(next_depth)) {
            _exit(125);
        }
        execl(argv[0], argv[0], next_depth, (char *)NULL);
        perror("execl");
        _exit(127);
    }

    int status = 0;
    while (waitpid(child, &status, 0) < 0) {
        if (errno != EINTR) {
            perror("waitpid");
            return 1;
        }
    }
    if (!WIFEXITED(status)) {
        fprintf(stderr, "child did not exit normally\n");
        return 1;
    }
    return WEXITSTATUS(status);
}
