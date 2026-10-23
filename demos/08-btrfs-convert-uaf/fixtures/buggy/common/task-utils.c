/*
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public
 * License v2 as published by the Free Software Foundation.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * General Public License for more details.
 *
 * You should have received a copy of the GNU General Public
 * License along with this program; if not, write to the
 * Free Software Foundation, Inc., 59 Temple Place - Suite 330,
 * Boston, MA 021110-1307, USA.
 */

#include <sys/timerfd.h>
#include <pthread.h>
#include <sched.h>
#include <stdlib.h>
#include <unistd.h>
#include <time.h>
#include "common/task-utils.h"
#include "common/compat.h"

struct task_info *task_init(void *(*threadfn)(void *), int (*postfn)(void *),
			    void *thread_private)
{
	struct task_info *info = calloc(1, sizeof(struct task_info));

	if (!info)
		return NULL;

	info->private_data = thread_private;
	info->threadfn = threadfn;
	info->postfn = postfn;

	return info;
}

int task_start(struct task_info *info, time_t *start_time, u64 *item_count)
{
	int ret;

	if (!info)
		return -1;

	if (!info->threadfn)
		return -1;

	if (start_time)
		*start_time = time(NULL);
	if (item_count)
		*item_count = 0;

	ret = pthread_create(&info->id, NULL, info->threadfn,
			     info->private_data);

	/* DEMO08 BUG: historical pre-73e211a7 behavior reintroduced.
	 * Detaching the progress subthread means task_stop() cannot join it. */
	if (ret == 0)
		pthread_detach(info->id);
	else
		info->id = -1;

	return ret;
}

void task_stop(struct task_info *info)
{
	if (!info)
		return;

	/* DEMO08 BUG: pre-73e211a7 behavior, replicated in spirit.
	 * Signal teardown and wake the detached progress subthread with a single
	 * byte (its final "tick"). The subthread runs exactly one more loop
	 * iteration -- reading info->periodic.stop and bumping
	 * info->periodic.wakeups_missed -- then, if it has not yet observed stop,
	 * re-blocks on the empty pipe (so it never becomes perpetually runnable).
	 * Crucially we never pthread_join() the detached thread, so task_deinit()
	 * below can free(info) concurrently with that final iteration: whether the
	 * subthread's load of *info happens before or after the free is decided by
	 * the thread interleaving -- the use-after-free. */
	info->periodic.stop = 1;
	if (info->periodic.wait_write_fd > 0) {
		char tick = 0;
		ssize_t w = write(info->periodic.wait_write_fd, &tick, 1);
		(void)w;
	}

	if (info->postfn)
		info->postfn(info->private_data);
}

void task_deinit(struct task_info *info)
{
	if (!info)
		return;

	free(info);
}

int task_period_start(struct task_info *info, unsigned int period_ms)
{
	int fds[2];

	if (!info)
		return -1;

	/* DEMO08 observability adaptation (see demos/08-btrfs-convert-uaf/README.md): replace the
	 * wall-clock CLOCK_MONOTONIC timerfd -- which never fires under hermit's
	 * logical clock -- with a pipe. The subthread blocks on the read end in
	 * task_period_wait() (parking cheaply during copy_inodes()); task_stop()
	 * writes one byte to wake it. period_ms is now irrelevant. */
	(void)period_ms;

	if (pipe(fds) == -1) {
		info->periodic.timer_fd = -1;
		info->periodic.wait_write_fd = -1;
		return -1;
	}

	info->periodic.timer_fd = fds[0];
	info->periodic.wait_write_fd = fds[1];
	info->periodic.stop = 0;
	info->periodic.wakeups_missed = 0;

	return 0;
};

void task_period_wait(struct task_info *info)
{
	char c;
	ssize_t r;

	if (!info)
		return;

	/* DEMO08 observability adaptation (see demos/08-btrfs-convert-uaf/README.md).
	 * The historical code blocks here on read(info->periodic.timer_fd),
	 * waiting for a wall-clock CLOCK_MONOTONIC timerfd tick. Hermit virtualizes
	 * CLOCK_MONOTONIC to logical time, which does not advance during this
	 * I/O-bound conversion, so the timer never fires and the subthread parks at
	 * that read() forever, leaving the latent use-after-free dormant. We instead
	 * block on a pipe read end so the subthread parks cheaply during
	 * copy_inodes() and is woken by task_stop()'s single "final tick" byte,
	 * letting hermit's deterministic chaos scheduler drive the teardown
	 * interleaving. The store to info->periodic.wakeups_missed below -- and the
	 * threadfn's subsequent load of info->periodic.stop -- are the
	 * use-after-free site once task_stop() has freed info without joining. */
	r = read(info->periodic.timer_fd, &c, 1);
	(void)r;

	info->periodic.wakeups_missed++;
}

void task_period_stop(struct task_info *info)
{
	if (!info)
		return;

	if (info->periodic.wait_write_fd > 0) {
		close(info->periodic.wait_write_fd);
		info->periodic.wait_write_fd = -1;
	}
	if (info->periodic.timer_fd > 0) {
		close(info->periodic.timer_fd);
		info->periodic.timer_fd = -1;
	}
}
