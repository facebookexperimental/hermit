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

	/* DEMO08 FIX (73e211a7): do NOT detach the progress subthread, so
	 * task_stop() can pthread_join() it before the memory is freed. */
	if (ret)
		info->id = -1;

	return ret;
}

void task_stop(struct task_info *info)
{
	if (!info)
		return;

	/* DEMO08 FIX (73e211a7): signal teardown and wake the progress subthread
	 * with the same single "final tick" byte as the buggy variant, so the only
	 * behavioral difference is the join below. On observing stop the threadfn
	 * breaks its loop and returns. */
	info->periodic.stop = 1;
	if (info->periodic.wait_write_fd > 0) {
		char tick = 0;
		ssize_t w = write(info->periodic.wait_write_fd, &tick, 1);
		(void)w;
	}

	/* DEMO08 FIX (73e211a7): pthread_join() guarantees the subthread has fully
	 * stopped touching *info before task_deinit() frees it, closing the
	 * use-after-free window regardless of interleaving. */
	if (info->id > 0)
		pthread_join(info->id, NULL);

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

	/* Identical to the buggy variant: only task_start()/task_stop() differ.
	 * DEMO08 observability adaptation (see demos/08-btrfs-convert-uaf/README.md): replace the
	 * wall-clock CLOCK_MONOTONIC timerfd -- which never fires under hermit's
	 * logical clock -- with a pipe. The subthread blocks on the read end in
	 * task_period_wait(); task_stop() writes one byte to wake it. */
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

	/* Identical to the buggy variant: only task_start()/task_stop() differ.
	 * See demos/08-btrfs-convert-uaf/README.md for why the historical timerfd read() is replaced
	 * with a blocking one-byte-per-tick pipe read() under hermit's logical
	 * clock. */
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
