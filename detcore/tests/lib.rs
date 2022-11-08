// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

#![feature(get_mut_unchecked)]
#![feature(thread_id_value)]
#![feature(atomic_mut_ptr)]

mod fds;
mod filesystem;
mod misc;
mod parallelism;
mod stats;
mod time;
