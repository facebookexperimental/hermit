// SPDX-License-Identifier: GPL-2.0-only
/*
 * hermit_cpuid_fault: turn on Linux's own CPUID-faulting support in a KVM
 * guest that emulates the Intel-style interface (MSR_PLATFORM_INFO bit 31,
 * MSR_MISC_FEATURES_ENABLES bit 0) on a non-Intel vCPU model.
 *
 * Linux 6.16 probes that interface only in arch/x86/kernel/cpu/intel.c
 * (init_intel_misc_features -> init_cpuid_fault), so on the AMD-model
 * on-demand VMs X86_FEATURE_CPUID_FAULT stays clear and
 * arch_prctl(ARCH_SET_CPUID, 0) returns -ENODEV, although the hypervisor
 * implements the MSRs.  Everything else is already vendor-neutral in 6.16:
 * set_cpuid_mode() checks boot_cpu_has(X86_FEATURE_CPUID_FAULT) at run time,
 * and set_cpuid_faulting() (called from disable_cpuid/enable_cpuid and from
 * __switch_to_xtra on a TIF_NOCPUID transition) writes the per-CPU
 * msr_misc_features_shadow to MSR_MISC_FEATURES_ENABLES on any vendor.
 * Linux 6.17 changed set_cpuid_faulting() to write MSR_K7_HWCR on AMD, so this
 * module refuses to build for 6.17 or later.
 *
 * This module never writes an MSR.  It only sets the capability bit, after
 * checking on every online CPU that:
 *   - MSR_PLATFORM_INFO reads and has bit 31 (CPUID faulting supported), and
 *   - MSR_MISC_FEATURES_ENABLES reads and is exactly 0.
 * The second check is what makes the unexported per-CPU
 * msr_misc_features_shadow correct without touching it: on a non-Intel CPU
 * nothing in 6.16 writes the shadow except set_cpuid_faulting(), which is
 * unreachable while the capability is clear, so the shadow is still its
 * static zero; the MSR also being 0 means shadow == MSR on every CPU, which is
 * the invariant init_intel_misc_features() establishes on Intel.  Faulting
 * then stays per task: the kernel sets the MSR bit only while a TIF_NOCPUID
 * task runs on that CPU.
 *
 * Hot-added CPUs are checked by a CPU-hotplug online callback; a CPU that
 * fails the check is refused online, because a TIF_NOCPUID task migrating to
 * it would otherwise execute cpuid natively and silently.
 *
 * Unload: the module holds a reference to itself, so rmmod fails with EBUSY
 * until "echo 1 > /sys/module/hermit_cpuid_fault/parameters/release"
 * succeeds.  release clears the boot capability (new ARCH_SET_CPUID calls get
 * -ENODEV again), waits an RCU grace period, and refuses (-EBUSY, capability
 * restored) while any thread has TIF_NOCPUID.  A thread that slipped past
 * set_cpuid_mode()'s capability check concurrently with release would keep
 * faulting until it execs or exits; the context-switch path does not depend on
 * the capability, so that case is still handled correctly by the kernel.
 */
/* Prefix every message so `dmesg | grep hermit_cpuid_fault:` finds them. */
#define pr_fmt(fmt) KBUILD_MODNAME ": " fmt

#include <linux/module.h>
#include <linux/moduleparam.h>
#include <linux/cpu.h>
#include <linux/cpuhotplug.h>
#include <linux/mutex.h>
#include <linux/rcupdate.h>
#include <linux/sched/signal.h>
#include <linux/version.h>
#include <asm/cpufeature.h>
#include <asm/msr.h>
#include <asm/processor.h>

#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 17, 0)
#error "Linux 6.17+ set_cpuid_faulting() uses MSR_K7_HWCR on AMD; this module is for 6.16"
#endif

static DEFINE_MUTEX(hcf_lock);
static int hcf_state = -1;	/* dynamic cpuhp state, or -1 */
static bool hcf_released;

static int hcf_check_cpu(unsigned int cpu)
{
	u64 pinfo, misc;
	int err;

	err = rdmsrq_safe_on_cpu(cpu, MSR_PLATFORM_INFO, &pinfo);
	if (err) {
		pr_err("cpu%u: rdmsr MSR_PLATFORM_INFO failed (%d)\n", cpu, err);
		return -ENODEV;
	}
	if (!(pinfo & MSR_PLATFORM_INFO_CPUID_FAULT)) {
		pr_err("cpu%u: MSR_PLATFORM_INFO=%#llx lacks bit 31 (CPUID faulting)\n",
		       cpu, pinfo);
		return -ENODEV;
	}
	err = rdmsrq_safe_on_cpu(cpu, MSR_MISC_FEATURES_ENABLES, &misc);
	if (err) {
		pr_err("cpu%u: rdmsr MSR_MISC_FEATURES_ENABLES failed (%d)\n", cpu, err);
		return -ENODEV;
	}
	if (misc != 0) {
		pr_err("cpu%u: MSR_MISC_FEATURES_ENABLES=%#llx, expected 0 (kernel shadow is 0)\n",
		       cpu, misc);
		return -EBUSY;
	}
	return 0;
}

/* CPU-hotplug online callback: runs for every online CPU at setup and for
 * every CPU that comes online later.  A non-zero return fails the setup (load
 * is refused) or keeps a hot-added CPU offline. */
static int hcf_cpu_online(unsigned int cpu)
{
	int err = hcf_check_cpu(cpu);

	if (err)
		return err;
	set_cpu_cap(&cpu_data(cpu), X86_FEATURE_CPUID_FAULT);
	return 0;
}

struct hcf_first {
	pid_t pid;
	char comm[TASK_COMM_LEN];
};

static unsigned int hcf_count_nocpuid(struct hcf_first *first)
{
	struct task_struct *p, *t;
	unsigned int n = 0;

	rcu_read_lock();
	for_each_process_thread(p, t) {
		if (test_tsk_thread_flag(t, TIF_NOCPUID)) {
			if (n++ == 0) {
				first->pid = task_pid_nr(t);
				get_task_comm(first->comm, t);
			}
		}
	}
	rcu_read_unlock();
	return n;
}

/* The kernel's clear_cpu_cap() is not exported (and WARNs after alternatives
 * are patched); X86_FEATURE_CPUID_FAULT has no dependent features and no
 * alternative-patched users (its only reader is set_cpuid_mode()'s run-time
 * boot_cpu_has()), so clearing the bit directly is the exact inverse of
 * set_cpu_cap(). */
#define hcf_clear_cap(c) clear_bit(X86_FEATURE_CPUID_FAULT, (unsigned long *)((c)->x86_capability))

static void hcf_clear_caps(void)
{
	unsigned int cpu;

	hcf_clear_cap(&boot_cpu_data);
	for_each_possible_cpu(cpu)
		hcf_clear_cap(&cpu_data(cpu));
}

static int hcf_release(const char *val, const struct kernel_param *kp)
{
	struct hcf_first first = { 0, "" };
	unsigned int n;
	bool yes;
	int ret;

	ret = kstrtobool(val, &yes);
	if (ret)
		return ret;
	if (!yes)
		return 0;

	mutex_lock(&hcf_lock);
	if (hcf_released) {
		mutex_unlock(&hcf_lock);
		return 0;
	}
	hcf_clear_cap(&boot_cpu_data);
	synchronize_rcu();
	n = hcf_count_nocpuid(&first);
	if (n) {
		set_cpu_cap(&boot_cpu_data, X86_FEATURE_CPUID_FAULT);
		mutex_unlock(&hcf_lock);
		pr_warn("release refused: %u thread(s) have TIF_NOCPUID (first: pid %d %s)\n",
			n, first.pid, first.comm);
		return -EBUSY;
	}
	cpuhp_remove_state_nocalls(hcf_state);
	hcf_state = -1;
	hcf_clear_caps();
	hcf_released = true;
	mutex_unlock(&hcf_lock);
	pr_info("released: X86_FEATURE_CPUID_FAULT cleared on boot_cpu_data and every CPU; rmmod is now allowed\n");
	module_put(THIS_MODULE);
	return 0;
}

static int hcf_release_get(char *buf, const struct kernel_param *kp)
{
	return sysfs_emit(buf, "%d\n", hcf_released);
}

static const struct kernel_param_ops hcf_release_ops = {
	.set = hcf_release,
	.get = hcf_release_get,
};
module_param_cb(release, &hcf_release_ops, NULL, 0600);
MODULE_PARM_DESC(release, "write 1 to clear the capability and allow rmmod (refused while any thread has TIF_NOCPUID)");

static int __init hcf_init(void)
{
	int ret;

	if (boot_cpu_has(X86_FEATURE_CPUID_FAULT)) {
		pr_err("X86_FEATURE_CPUID_FAULT is already set by the kernel; nothing to do\n");
		return -EEXIST;
	}
	if (!boot_cpu_has(X86_FEATURE_HYPERVISOR)) {
		pr_err("not running under a hypervisor; refusing\n");
		return -ENODEV;
	}

	ret = cpuhp_setup_state(CPUHP_AP_ONLINE_DYN, "x86/hermit_cpuid_fault:online",
				hcf_cpu_online, NULL);
	if (ret < 0) {
		pr_err("per-CPU check failed (%d); not loading\n", ret);
		hcf_clear_caps();
		return ret;
	}
	hcf_state = ret;

	/* Every online CPU passed; publish to arch_prctl last. */
	set_cpu_cap(&boot_cpu_data, X86_FEATURE_CPUID_FAULT);
	__module_get(THIS_MODULE);
	pr_info("X86_FEATURE_CPUID_FAULT set on boot_cpu_data and %u online CPUs (vendor %u family %#x model %#x); MSR_MISC_FEATURES_ENABLES left 0\n",
		num_online_cpus(), boot_cpu_data.x86_vendor, boot_cpu_data.x86,
		boot_cpu_data.x86_model);
	return 0;
}

static void __exit hcf_exit(void)
{
	/* Reached normally only after a successful release; a forced unload
	 * (rmmod -f) gets the same cleanup without the TIF_NOCPUID check. */
	if (!hcf_released) {
		pr_warn("unloaded without release (forced?): clearing capability anyway\n");
		if (hcf_state >= 0)
			cpuhp_remove_state_nocalls(hcf_state);
		hcf_clear_caps();
	}
	pr_info("unloaded\n");
}

module_init(hcf_init);
module_exit(hcf_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Enable Linux's CPUID faulting on KVM guests that emulate MSR_PLATFORM_INFO bit 31 on non-Intel vCPUs (6.16)");
