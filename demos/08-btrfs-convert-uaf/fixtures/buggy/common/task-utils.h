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

#ifndef __TASK_UTILS_H__
#define __TASK_UTILS_H__

#include "kerncompat.h"
#include <time.h>
#include <pthread.h>

struct periodic_info {
	int timer_fd;		/* DEMO08: pipe read end (see README) */
	int wait_write_fd;	/* DEMO08: pipe write end, written by task_stop */
	int stop;		/* DEMO08: teardown flag; racy read is the UAF site */
	unsigned long long wakeups_missed;
};

struct task_info {
	struct periodic_info periodic;
	pthread_t id;
	void *private_data;
	void *(*threadfn)(void *);
	int (*postfn)(void *);
};

/* task life cycle */
struct task_info *task_init(void *(*threadfn)(void *), int (*postfn)(void *),
			    void *thread_private);
int task_start(struct task_info *info, time_t *start_time, u64 *item_count);
void task_stop(struct task_info *info);
void task_deinit(struct task_info *info);

/* periodic life cycle */
int task_period_start(struct task_info *info, unsigned int period_ms);
void task_period_wait(struct task_info *info);
void task_period_stop(struct task_info *info);

#endif
