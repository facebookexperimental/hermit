// Copyright 2026 The gVisor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Binary counter measures raw syscall exits through a gVisor platform.
package main

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"runtime"
	"time"

	"golang.org/x/sys/unix"
	"gvisor.dev/gvisor/pkg/context"
	"gvisor.dev/gvisor/pkg/cpuid"
	"gvisor.dev/gvisor/pkg/hostarch"
	"gvisor.dev/gvisor/pkg/memutil"
	"gvisor.dev/gvisor/pkg/sentry/arch"
	"gvisor.dev/gvisor/pkg/sentry/limits"
	"gvisor.dev/gvisor/pkg/sentry/memmap"
	"gvisor.dev/gvisor/pkg/sentry/mm"
	"gvisor.dev/gvisor/pkg/sentry/pgalloc"
	"gvisor.dev/gvisor/pkg/sentry/platform"
	_ "gvisor.dev/gvisor/pkg/sentry/platform/kvm"
	_ "gvisor.dev/gvisor/pkg/sentry/platform/systrap"
	"gvisor.dev/gvisor/pkg/usermem"
)

const (
	defaultIterations = 1_000_000
	guestPID          = 1
)

// guestCode executes getpid iterations times, then invokes exit_group.
// R12 is initialized to iterations by runOnce.
var guestCode = []byte{
	0xb8, 0x27, 0x00, 0x00, 0x00, // mov $SYS_getpid, %eax
	0x0f, 0x05, // syscall
	0x49, 0xff, 0xcc, // dec %r12
	0x75, 0xf4, // jnz guestCode[0]
	0xb8, 0xe7, 0x00, 0x00, 0x00, // mov $SYS_exit_group, %eax
	0x0f, 0x05, // syscall
	0x0f, 0x0b, // ud2: exit_group must never return
}

type result struct {
	Backend         string  `json:"backend"`
	Workload        string  `json:"workload"`
	SyscallPatching bool    `json:"syscall_patching"`
	TimingScope     string  `json:"timing_scope"`
	Run             int     `json:"run"`
	GetpidSyscalls  uint64  `json:"getpid_syscalls"`
	TotalSyscalls   uint64  `json:"total_syscalls"`
	ElapsedNanos    int64   `json:"elapsed_ns"`
	SyscallsPerSec  float64 `json:"syscalls_per_second"`
	NanosecondsEach float64 `json:"nanoseconds_per_syscall"`
}

func createMemoryFile() (*pgalloc.MemoryFile, error) {
	const name = "gvisor-platform-counter"
	fd, err := memutil.CreateMemFD(name, 0)
	if err != nil {
		return nil, fmt.Errorf("create memory file: %w", err)
	}
	f := os.NewFile(uintptr(fd), name)
	mf, err := pgalloc.NewMemoryFile(f, pgalloc.MemoryFileOpts{
		DelayedEviction:         pgalloc.DelayedEvictionDisabled,
		DisableIMAWorkAround:    true,
		DisableMemoryAccounting: true,
	})
	if err != nil {
		f.Close()
		return nil, fmt.Errorf("initialize memory file: %w", err)
	}
	return mf, nil
}

func mapAnonymous(ctx context.Context, memory *mm.MemoryManager, length uint64, name string) (hostarch.Addr, error) {
	addr, err := memory.MMap(ctx, memmap.MMapOpts{
		Length:         length,
		Perms:          hostarch.AnyAccess,
		MaxPerms:       hostarch.AnyAccess,
		Private:        true,
		Stack:          name == "[stack]",
		PlatformEffect: memmap.PlatformEffectCommit,
		Name:           name,
	})
	if err != nil {
		return 0, fmt.Errorf("map %s: %w", name, err)
	}
	return addr, nil
}

func runOnce(ctx context.Context, p platform.Platform, mf *pgalloc.MemoryFile, backend string, run int, iterations uint64) (result, error) {
	limitSet := limits.NewLimitSet()
	ctx = context.WithValue(ctx, limits.CtxLimits, limitSet)
	ac := arch.New(arch.AMD64)
	memory, err := mm.NewMemoryManager(p, mf)
	if err != nil {
		return result{}, fmt.Errorf("create address space: %w", err)
	}
	defer memory.DecUsers(ctx)
	if _, err := memory.SetMmapLayout(ac, limitSet); err != nil {
		return result{}, fmt.Errorf("set mmap layout: %w", err)
	}

	codeAddr, err := mapAnonymous(ctx, memory, hostarch.PageSize, "[platform-counter-code]")
	if err != nil {
		return result{}, err
	}
	if n, err := memory.CopyOut(ctx, codeAddr, guestCode, usermem.IOOpts{}); err != nil || n != len(guestCode) {
		return result{}, fmt.Errorf("copy guest code: copied %d/%d bytes: %w", n, len(guestCode), err)
	}
	stackAddr, err := mapAnonymous(ctx, memory, hostarch.PageSize, "[stack]")
	if err != nil {
		return result{}, err
	}

	ac.SetIP(uintptr(codeAddr))
	ac.SetStack(uintptr(stackAddr + hostarch.PageSize - 16))
	ac.StateData().Regs.R12 = iterations

	platformContext := p.NewContext(ctx)
	defer platformContext.Release()
	platformContext.FullStateChanged()

	var getpidCount, total uint64
	started := time.Now()
	for {
		info, access, err := platformContext.Switch(ctx, memory, ac, -1)
		switch err {
		case nil:
			sysno := ac.SyscallNo()
			total++
			switch sysno {
			case unix.SYS_GETPID:
				getpidCount++
				ac.SetReturn(guestPID)
			case unix.SYS_EXIT_GROUP:
				if getpidCount != iterations {
					return result{}, fmt.Errorf("exit after %d getpid syscalls, want %d", getpidCount, iterations)
				}
				elapsed := time.Since(started)
				return result{
					Backend:         backend,
					Workload:        "getpid-loop",
					SyscallPatching: false,
					TimingScope:     "platform-switch-loop",
					Run:             run,
					GetpidSyscalls:  getpidCount,
					TotalSyscalls:   total,
					ElapsedNanos:    elapsed.Nanoseconds(),
					SyscallsPerSec:  float64(total) / elapsed.Seconds(),
					NanosecondsEach: float64(elapsed.Nanoseconds()) / float64(total),
				}, nil
			default:
				return result{}, fmt.Errorf("unexpected guest syscall %d after %d exits", sysno, total)
			}
		case platform.ErrContextInterrupt, platform.ErrContextCPUPreempted:
			continue
		case platform.ErrContextSignal:
			if info != nil && access.Any() {
				if err := memory.HandleUserFault(ctx, hostarch.Addr(info.Addr()), access, hostarch.Addr(ac.Stack())); err != nil {
					return result{}, fmt.Errorf("handle guest fault at %#x: %w", info.Addr(), err)
				}
				continue
			}
			return result{}, fmt.Errorf("guest signal: info=%+v access=%v", info, access)
		default:
			return result{}, fmt.Errorf("platform switch: %w", err)
		}
	}
}

func run() error {
	backend := flag.String("backend", "systrap", "gVisor platform backend: systrap or kvm")
	device := flag.String("device", "", "backend device path (default: platform-specific)")
	iterations := flag.Uint64("syscalls", defaultIterations, "number of guest getpid syscalls per run")
	runs := flag.Int("runs", 1, "number of measured runs")
	flag.Parse()

	if runtime.GOARCH != "amd64" {
		return fmt.Errorf("platform counter supports amd64 only, got %s", runtime.GOARCH)
	}
	if *backend != "systrap" && *backend != "kvm" {
		return fmt.Errorf("unsupported backend %q: expected systrap or kvm", *backend)
	}
	if *iterations == 0 {
		return errors.New("--syscalls must be greater than zero")
	}
	if *runs <= 0 {
		return errors.New("--runs must be greater than zero")
	}

	cpuid.Initialize()
	constructor, err := platform.Lookup(*backend)
	if err != nil {
		return err
	}
	deviceFile, err := constructor.OpenDevice(*device)
	if err != nil {
		return fmt.Errorf("open %s platform device: %w", *backend, err)
	}
	if deviceFile != nil {
		defer deviceFile.Close()
	}
	p, err := constructor.New(platform.Options{
		DeviceFile:             deviceFile,
		DisableSyscallPatching: true,
	})
	if err != nil {
		return fmt.Errorf("initialize %s platform: %w", *backend, err)
	}
	mf, err := createMemoryFile()
	if err != nil {
		return err
	}
	defer mf.Destroy()

	encoder := json.NewEncoder(os.Stdout)
	for i := 1; i <= *runs; i++ {
		result, err := runOnce(context.Background(), p, mf, *backend, i, *iterations)
		if err != nil {
			return fmt.Errorf("run %d: %w", i, err)
		}
		if err := encoder.Encode(result); err != nil {
			return fmt.Errorf("write result: %w", err)
		}
	}
	return nil
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "platform-counter: %v\n", err)
		os.Exit(1)
	}
}
