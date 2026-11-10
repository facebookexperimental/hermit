#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Go's os/exec Cmd.Output from four goroutines at once. Each spawn makes a
# pipe for the child's output, which the runtime polls with epoll, and one
# through which the child reports a failed exec; the parent reads both to EOF.
# Goroutines run on several threads, which share those write ends
# (CLONE_FILES), so each must go with the close of the thread that made it, or
# a reader never sees EOF. The program exits 1 unless every spawn read
# exactly its child's line from a child that exited 0. --prepare builds the
# program with the go toolchain into the persisted E2E_FIXTURE_DIR, and --run
# execs the prebuilt binary.

set -euo pipefail

case ${1:-} in
    --prepare)
        command -v go >/dev/null
        test -x /bin/echo
        : "${E2E_FIXTURE_DIR:?E2E_FIXTURE_DIR must be set}"
        mkdir -p "$E2E_FIXTURE_DIR/src" "$E2E_FIXTURE_DIR/cache"
        cat >"$E2E_FIXTURE_DIR/src/main.go" <<'GO'
package main

import (
	"fmt"
	"os"
	"os/exec"
	"strings"
	"sync"
)

const goroutines = 4
const spawns = 5

func main() {
	var results [goroutines][spawns]string
	var correct [goroutines][spawns]bool
	var wg sync.WaitGroup
	for g := 0; g < goroutines; g++ {
		wg.Add(1)
		go func(g int) {
			defer wg.Done()
			for s := 0; s < spawns; s++ {
				expected := fmt.Sprintf("child %d.%d", g, s)
				// Output returns an error for a child that exited nonzero.
				out, err := exec.Command("/bin/echo", expected).Output()
				if err != nil {
					results[g][s] = fmt.Sprintf("error: %v (WRONG)", err)
					continue
				}
				correct[g][s] = string(out) == expected+"\n"
				results[g][s] = fmt.Sprintf("%q", strings.TrimSpace(string(out)))
				if !correct[g][s] {
					results[g][s] += " (WRONG)"
				}
			}
		}(g)
	}
	wg.Wait()
	completed := 0
	for g := 0; g < goroutines; g++ {
		for s := 0; s < spawns; s++ {
			fmt.Printf("goroutine %d spawn %d: %s\n", g, s, results[g][s])
			if correct[g][s] {
				completed++
			}
		}
	}
	if completed != goroutines*spawns {
		fmt.Printf("go-os-exec-output: FAILED, %d of %d spawns read their child's line\n", completed, goroutines*spawns)
		os.Exit(1)
	}
	fmt.Printf("go-os-exec-output: %d spawns from %d goroutines, every pipe reached EOF with its child's line\n", completed, goroutines)
}
GO
        printf 'module example.com/goosexecoutput\n\ngo 1.21\n' >"$E2E_FIXTURE_DIR/src/go.mod"
        env -C "$E2E_FIXTURE_DIR/src" GOCACHE="$E2E_FIXTURE_DIR/cache" GOPATH="$E2E_FIXTURE_DIR/cache/gopath" \
            GOTOOLCHAIN=local GOFLAGS=-trimpath CGO_ENABLED=0 GO111MODULE=on \
            go build -o "$E2E_FIXTURE_DIR/program" .
        ;;
    --run)
        : "${E2E_FIXTURE_DIR:?E2E_FIXTURE_DIR must be set}"
        exec "$E2E_FIXTURE_DIR/program"
        ;;
    *)
        echo "usage: $0 --prepare|--run" >&2
        exit 2
        ;;
esac
