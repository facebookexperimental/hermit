/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Descriptors a guest receives over `SCM_RIGHTS`.
//!
//! `recvmsg` and `recvmmsg` are the only calls that install descriptors
//! another process sent: Linux's `scm_detach_fds` installs each descriptor
//! that fits the receiver's control buffer, with `O_CLOEXEC` when the call
//! passed `MSG_CMSG_CLOEXEC`, writes one `SCM_RIGHTS` message holding exactly
//! the numbers it installed, releases the rest and sets `MSG_CTRUNC`; a
//! `MSG_PEEK` receive installs them too. The control data the call leaves at
//! the control buffer it was given, read up to the `msg_controllen` Linux
//! writes back, therefore names exactly the descriptors that now exist
//! ([`ReceiveGeometry::delivered`]).
//!
//! Where the tool shares the guest's descriptor table (the in-guest
//! backends), Detcore discovers such a descriptor when a call first names it
//! (`FileMetadata::discover_fd_from_current_process`). Elsewhere (ptrace,
//! KVM) nothing would, and every later call through it that consults
//! Detcore's table failed with `EBADF`
//! (<https://github.com/rrnewton/hermit/issues/4547>). So there Detcore records
//! each received descriptor as the receive returns
//! ([`Detcore::record_received_descriptors`]), from the facts the in-guest
//! discovery takes (`fcntl(F_GETFD)`, `fcntl(F_GETFL)` and `fstat`, here
//! taken through the recording), through the same
//! `FileMetadata::add_discovered_fd`, so the
//! record is the description the in-guest discovery makes: a directory, for
//! one, gets a sorted stream of its own. Unlike that discovery, a received
//! descriptor is never given the container's stdio resource (it is not the
//! container's stdio), nor rebuilt as a fixed procfs file or held for the
//! session's `/proc/cpuinfo` decision (those are an exec image's own
//! descriptors); a rendered procfs placeholder is adopted through the
//! placeholder registry instead, as `Detcore::adopt_received_placeholders`
//! adopts one when it is first named, or, where Detcore runs in each guest
//! process (LiteInst and in-guest-trap record received descriptors too), by
//! the name and seals of a placeholder another process made
//! (`Detcore::received_placeholder`).
//!
//! An anonymous inode has no file type, so `fstat` does not tell its kind.
//! Its descriptor link does: one that reads `anon_inode:inotify` (an injected
//! `readlinkat` of `/proc/thread-self/fd/<fd>`) is recorded as an inotify
//! instance, as the in-guest discovery records it, so its reads renumber
//! rename cookies (`crate::syscalls::inotify`).
//!
//! Not recorded, as before, each named in a warning: any other anonymous
//! inode (eventfd, signalfd, timerfd, epoll, pidfd, userfaultfd, ...), which
//! an ordinary record would mishandle
//! (<https://github.com/rrnewton/hermit/issues/4107>'s signalfd mode), and
//! one whose link cannot be read; every receive on the background path; and
//! a receive whose layout cannot be trusted (its buffers overlap so that the
//! call may have overwritten its own inputs, its vector cannot be read in
//! full, or its inputs changed before the attempt that succeeded), where
//! unsupported operations are allowed or the receive ran on the background
//! path. In the caller's turn, a fail-closed run ends at such a receive
//! that names a control buffer (or whose geometry cannot say), through a
//! socket whose queue does not rule descriptors out (`DetFd::may_deliver_rights`:
//! a datagram or seqpacket `socketpair` whose queue holds every message and
//! none with descriptors rules them out), as a policy refusal: the
//! descriptors it may have installed
//! would be missing from the table, and a later call through one would run
//! on the background path, outside the turn order. Without sequentialized
//! threads no receive is recorded, logged at debug level only
//! (`ReceiptMode`).
//!
//! Under record and replay every fact is taken through the recording, so
//! replay, which serves the receive from the log and holds a stand-in in each
//! received slot, rebuilds the table the recording built. Where the recording
//! joined a received descriptor to an open file the receiving table holds,
//! replay also makes the received slot name the live open file
//! (`Detcore::install_live_alias`), and a received socket whose peer is a
//! guest is polled in the caller's turn (`Detcore::classify_socket_peer`).
//!
//! Linux installs the sender's `struct file` (`receive_fd`): the received
//! descriptor shares its status flags, directory position and lock ownership
//! with every descriptor of that open file, the sender's included, in
//! whichever process holds them. Detcore names that open file without asking
//! the kernel, as <https://github.com/rrnewton/hermit/issues/4670> proposes:
//! at the send, it queues on the receiving socket the open file description
//! its table modeled for each descriptor carried, with the file's
//! `(st_dev, st_ino)` from an injected `fstat`
//! ([`Detcore::queue_sent_rights`]); at the receipt, it takes the oldest
//! queued message whose identities are the received descriptors', in order
//! (all of them, or a prefix where Linux set `MSG_CTRUNC`; a `MSG_PEEK`
//! receive leaves it queued), and each received descriptor joins the
//! description its sender's table modeled, as `dup` would, before the
//! anonymous-inode refusal, so a received eventfd is the sender's eventfd.
//! The decision depends only on Detcore's own model and the facts its
//! injected `fcntl` and `fstat` take, so it is the same on every backend that
//! records received descriptors (ptrace, KVM); before, it asked the kernel
//! through `kcmp(KCMP_FILE)`, which KVM's executor does not answer and which
//! the host's kernel configuration and seccomp profile can refuse.
//!
//! A queued message holds the descriptions it carries
//! ([`crate::fd::InFlightOpenFile`]), as Linux holds the `struct file` of each
//! descriptor a message carries: the send takes a reference on each
//! (`scm_fp_copy`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/core/scm.c?h=v6.17#n69>),
//! which the queued message keeps (`unix_attach_fds`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n1962>)
//! until a receive installs the descriptors that fit and drops the rest
//! (`scm_detach_fds`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/core/scm.c?h=v6.17#n354>),
//! or the message is freed unreceived (`unix_destruct_scm`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n1989>):
//! by the close of the receiving socket (`unix_release_sock`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n665>)
//! or a datagram `connect` (`unix_dgram_disconnected`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n616>).
//! So the receipt names the description even when the sender closed every
//! descriptor of it, or exited, first: a sender's own table is released when
//! the backend runs the end of its exit hook, which KVM does after the parent
//! can already wait for the child, at no particular point of the guest's
//! schedule, and a receipt that depended on that release decided by host
//! timing (<https://github.com/rrnewton/hermit/issues/4930>).
//!
//! A description counts the copies of it in flight
//! (`OpenFileDescription::in_flight`): one more when a send queues one, one
//! fewer when a receipt installs it or a guest call makes Linux release it.
//! Only guest calls change the count, never a backend's teardown, so whether
//! a close drops a description's last reference, which decides its port and,
//! for a socket, whether its queue goes with it, is decided from the
//! descriptors that name it and that count (`DetFd::is_last_reference`).
//!
//! Detcore lets go where Linux does, each time when the guest call that makes
//! Linux let go completes, in its turn ([`Detcore::settle_released_rights`]):
//! - a receive that takes the message: the descriptors it could not install
//!   (`MSG_CTRUNC`) go with it, and so do those recorded as open files of
//!   their own;
//! - a receive that takes a later message, which releases the ones queued
//!   before it, which calls Detcore could not match took;
//! - where the queue holds every message (see below), a call that takes a
//!   message without installing what it carried: a read without a control
//!   buffer, a receive that delivered its message without descriptors
//!   (`MSG_CTRUNC` with no room), and a datagram or seqpacket receive that
//!   failed with `EFAULT` copying the message's bytes, or without room for a
//!   descriptor;
//! - a `connect` of the datagram socket, and a send through it that finds its
//!   connected peer gone (`ECONNREFUSED`), which purge its receive queue
//!   (`unix_dgram_disconnected`, `DetFd::unpair_rights_peer`);
//! - the close, `dup2` or `close_range` that drops the receiving socket's last
//!   descriptor (`unix_release_sock`, `FileMetadata::release_detfd`);
//! - the exit that leaves a descriptor table to no holder (a thread's `exit`,
//!   or an `exit_group`, which ends its whole process), as guest calls alone
//!   count a table's holders (`FileMetadata::holders`): it lets go of every
//!   descriptor of the table in the exiting thread's turn, once the scheduler
//!   has granted the exit (`ResourceID::Exit`) and just before the native
//!   call (`Detcore::release_table_at_exit`), as Linux releases a table with
//!   its last holder's exit (`exit_files`,
//!   <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/exit.c?h=v6.17#n956>);
//! - a `SIGKILL` a process sends itself (a `kill`, `tgkill`, `tkill` or
//!   `pidfd_send_signal` naming its own process or thread), which ends it as
//!   an `exit_group` does: its table goes the same way, after the grant and
//!   just before the call that sends it
//!   (`Detcore::reserve_controlled_self_sigkill_exit`,
//!   `Detcore::send_reserved_sigkill`);
//! - on KVM, the death of a process by the default action of a fatal signal
//!   (its own `SIGTERM`, `abort()`'s `SIGABRT`, a fault's `SIGSEGV`), which
//!   no guest call of the dying process carries: its terminal receipt, which
//!   the scheduler consumes while the control barrier it installs holds every
//!   later turn, lets go of each table the dying threads named
//!   (`Detcore::name_table_for_death`) that no other process holds, before
//!   the backend publishes the death to the parent or retires the process
//!   (`GlobalState::release_tables_at_death`), as Linux's `do_exit` runs
//!   `exit_files`, and the deferred last `fput`s in `exit_task_work`, before
//!   `exit_notify` lets the parent reap it
//!   (<https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/kernel/exit.c?h=v7.1.3#n971>,
//!   `#n976`, `#n988`): the receiving socket's last reference purges its
//!   queue (`unix_release_sock`,
//!   <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v7.1.3#n721>),
//!   and the last reference of a description a purged message held removes
//!   its locks (`__fput`'s `locks_remove_file`,
//!   <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/file_table.c?h=v7.1.3#n502>);
//! - the `exec` that drops the receiving socket's last descriptor by
//!   close-on-exec, where threads of the exec'ing process alone hold its
//!   table, as guest calls count them (`FileMetadata::holders`): Linux kills
//!   the other threads (`de_thread`), copies the table only where another
//!   process holds it (`unshare_files`), and closes the close-on-exec
//!   descriptors of the table the exec keeps (`do_close_on_exec`,
//!   <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/exec.c?h=v6.17#n1194>);
//!   Detcore lets go of the replaced table's descriptors at the new image's
//!   first callback, still in the exec's turn
//!   (`Detcore::release_exec_dropped_descriptors`): the close-on-exec ones,
//!   and the others, which the new table holds too, so their going releases
//!   nothing.
//!
//! A description whose last reference a release drops goes with it, with the
//! messages queued to it in turn, as Linux's last `fput` releases them: a
//! socket's port with it (`GlobalRequest::ReleasePort`), and the lock
//! ledger's count of that copy in flight, which held the description's locks
//! meanwhile (`crate::procfs::locks::LockLedger`). A peek, a read of no bytes
//! and a receive that fails before it reads the socket let go of nothing.
//!
//! Residuals, each named with whether it is deterministic or host-timed:
//! - deterministic: a receiving socket whose last descriptor goes with a
//!   table Detcore did not let go takes its queue with it when that table is
//!   released, without the releases above. Those tables are: the table of a
//!   process another process's `SIGKILL` killed, which Detcore holds only in
//!   the victim's exit hook (KVM refuses a signal to another process); on a
//!   backend that sends no terminal receipt (every backend but KVM), the
//!   table of a process another fatal signal ended, where the delivery's
//!   commit does not tell Detcore whether the process ends (a PID
//!   namespace's init survives it, and an exec in flight changes which
//!   threads die); one a thread made without `CLONE_FILES` held when another
//!   thread's `exit_group` or `exec` ended it, and, outside KVM's terminal
//!   receipt, a fatal signal; and one carried across a snapshot, whose
//!   holders are not known, also at an `exec`. The copies in flight such a
//!   queue held stay counted for the rest of the run, so the lock ledger
//!   keeps counting their locks, and a socket among them keeps its port, as a
//!   socket an exiting process held keeps its port. Where `/proc/locks` is
//!   the kernel's file, filtered by the ledger (ptrace), no row shows for
//!   those locks once the kernel has none, so only the port is kept. Where
//!   Detcore renders `/proc/locks` from the ledger (KVM), a terminal receipt
//!   that cannot decide about a table it ended (a dying thread that named
//!   none, in a process one of whose threads has a table of its own, or a
//!   table whose holders do not name the process) refuses every later
//!   snapshot by name, for the rest of the run, if the ledger counts any copy
//!   in flight at the receipt, rather than list a lock Linux has released.
//!   The count never depends on when that table is released;
//! - host-timed, as before this queue existed: a table Detcore did not let go
//!   at an exit still names its descriptions until the backend tears the
//!   exited thread's state down, at no particular point of the guest's
//!   schedule, so whether another table's later close of a description it
//!   shared is the last reference, and with it that description's port and a
//!   receiving socket's queue, follows that teardown. A table whose last
//!   holder leaves through `exit` or `exit_group` (an ordinary forked
//!   child's, or a threaded process's whose threads all exit or one of which
//!   calls `exit_group`), or through a `SIGKILL` of its own, goes at that
//!   exit, and one an exec replaced where only the exec'ing process held it
//!   goes at the exec, so this never applies to them;
//! - deterministic: a socket queued, directly or through other queued
//!   sockets, on its own queue keeps its description, and every description
//!   queued on it, until the run ends, where Linux's garbage collector
//!   (`__unix_gc`,
//!   <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/garbage.c?h=v6.17#n559>)
//!   frees such a cycle once no descriptor reaches it;
//! - deterministic: a datagram socket's `connect` to the peer it already has
//!   purges its queue here where Linux keeps it (`unix_dgram_connect` purges
//!   only on a change of peer,
//!   <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n1600>):
//!   the receipts of its messages join nothing, and what they held is
//!   released early (a socket among them frees its port);
//! - deterministic: where a socket's queue no longer holds every message
//!   (see below), a read without a control buffer that may have taken a
//!   message's descriptors leaves that message suspect until a later receipt
//!   or the receiving socket's last close releases it: a socket it carried
//!   keeps its port until then. So does a stream receive that failed with
//!   `EFAULT` after it took bytes, whose count Linux does not report, where
//!   Detcore cannot count them: its queue no longer holds every message, the
//!   receive had room to install descriptors, or the memory map does not
//!   show every byte it took writable;
//! - deterministic: a receipt of an anonymous inode that more than one queued
//!   message can be (an eventfd received after such a stream receive, which
//!   may have taken an earlier eventfd's message) joins neither, and the
//!   received descriptor is refused, as any anonymous inode Detcore cannot
//!   classify;
//! - deterministic: a receive that installed descriptors and then failed
//!   writing its header back (`EFAULT`) leaves them in the receiving table
//!   unrecorded, as before;
//! - deterministic: a lock on a description in flight over a socket Detcore
//!   does not pair is not held by the ledger between the sender's last close
//!   and the receipt, which shows it again. Nor is a lock taken through a
//!   description after a copy of it was sent: a copy counts for the ledger
//!   only where the ledger followed the description's locks when the send
//!   queued it (`SentDescriptor::lock_copy`), so from the sender's last close
//!   to the receipt no row shows for that lock, on ptrace and on KVM, while
//!   Linux lists it and refuses a conflicting lock (Codex's notes on K8-h
//!   reb7).
//!
//! The receiving socket is known only where Detcore knows a socket's peer,
//! in any process that holds it and through any of its aliases: the two
//! sockets of a `socketpair`, and an `AF_UNIX` stream or seqpacket connection
//! and the socket whose `connect` made it. Linux queues each connection on
//! the listener's receive queue at the tail (`unix_stream_connect`,
//! `__skb_queue_tail(&other->sk_receive_queue, skb)`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n1811>)
//! and `accept` takes it from the head (`unix_accept`, `skb_recv_datagram`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n1880>),
//! so Detcore queues each guest's connection to a name a guest bound,
//! abstract or an absolute path, where the `connect` completed in the
//! caller's turn, and pairs the oldest with the socket an `accept` on that
//! listener returns, also in its turn ([`Detcore::note_unix_accept`]).
//!
//! That needs every connection a listener gets. So a connect or accept on
//! the name outside the turn leaves its order unknown until the name is
//! bound again, and so does any `AF_UNIX` stream connect Detcore cannot
//! attribute to one bound name, for every name: one through a relative path
//! or another spelling of a path, to a name no guest bound here, or of a
//! socket whose type Detcore did not see made ([`UnixConnections`]). A
//! listener bound before its path was unlinked and bound again pairs
//! nothing: the connections Detcore tracks for the name are the later
//! listener's. A connection made from outside Hermit is not seen at all, and
//! shifts the pairing of those queued after it.
//!
//! A `socketpair` datagram socket reaches its peer only while it stays
//! connected to it and names no other destination. A `connect` gives it
//! another peer, or none, and Linux then purges its receive queue
//! (`unix_dgram_connect`, `unix_dgram_disconnected`), so Detcore unpairs the
//! two and neither queue is in lockstep any more
//! (`DetFd::unpair_rights_peer`). A send that names its destination
//! (`msg_namelen`, `unix_dgram_sendmsg`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n2172>)
//! queues nothing, and its peer's queue is no longer in lockstep, since the
//! named socket may be the peer itself. A seqpacket send ignores the name
//! (`unix_seqpacket_sendmsg`) and a stream send with one fails
//! (`unix_stream_sendmsg`).
//!
//! A descriptor received any other way is a description of its own in
//! Detcore's model, as before: through any other socket (a datagram socket
//! not made by `socketpair`, a connection Detcore did not pair), from a
//! sender outside Hermit or whose send Detcore did not see complete in its
//! turn, one whose sender's table modeled no description for it, or one
//! whose queued message may be a stale entry (see below). The in-guest
//! backends discover received descriptors lazily and keep the separate
//! model they had.
//!
//! The identities confirm what order proposes, except for anonymous inodes:
//! every eventfd, timerfd, signalfd and epoll set has one file identity. So
//! once a message may have reached a socket without being queued, no
//! anonymous inode received through it joins anything
//! (`SocketRights::lockstep_lost`, `Detcore::record_received_facts`): from
//! the first message sent to it that was not queued (unreadable control
//! data, facts that could not be taken, a send outside the turn, a datagram
//! send that named its destination, more than 4096 waiting), a `connect` of
//! a datagram socket of the pair, or a receipt that names no queued message,
//! or a later one than Linux can have delivered.
//!
//! A call that took a message from the socket Detcore could not match keeps
//! the queue: Linux still delivers the messages it did not take. Such a
//! call is a `read`, `readv`, `recvfrom`, `preadv2` at the current offset or
//! `splice` that took one, a receive that released descriptors
//! (`MSG_CTRUNC` with none installed), one outside the turn or with an
//! untrusted layout, one whose descriptors' facts could not be taken, a
//! receive that failed with `EFAULT` after it read the socket (below), or a
//! `recvmmsg` that stopped short with messages queued. Each took at most one
//! message, the oldest Linux held (a stream read detaches the descriptors of
//! one buffer at most, `unix_stream_read_generic`), so after `n` of them only
//! the first `n` queued messages can be gone (`SocketRights::suspects`). A
//! receipt that matches one of those, or the first message after them, joins
//! only where no later one of them, nor that first message after them, names
//! the same files; one that matches a message past that names a message
//! Linux cannot have delivered yet, so it took one never queued here or one
//! a call Detcore did not count took: it joins nothing and takes nothing,
//! and every message up to the match stays queued, suspect, with the queue
//! out of lockstep (`DetFd::take_queued_rights`). So a message Linux
//! released is never joined, whatever the files it names, and no message
//! Linux still holds is released. A peek may reach a later message
//! (`SO_PEEK_OFF`): it joins wherever it is, while the queue is in lockstep
//! and no other queued message names the same files. Other files out of
//! lockstep still join where their identities match: two open files of one
//! file, or the two ends of one pipe, which share its identity, are then told
//! apart only by order where one of them was sent without being queued.
//!
//! Where nothing but its peer can reach a socket, the queue can hold every
//! message, data alone included, in Linux's order (`SocketRights::complete`):
//! the two sockets of a `socketpair` while neither has a name, outside record
//! and replay, where Detcore notes every send between them
//! (`Detcore::note_send_without_control`, `Detcore::queue_sent_rights`). On a
//! datagram or seqpacket socket a call that takes one message without naming
//! its descriptors then took the oldest queued one, which must agree with
//! what the call reports (a read returns as many of its bytes as fit,
//! `DetFd::take_head_message`). On a stream socket a receive takes the bytes
//! it reports from the oldest messages on, and detaches the descriptors of
//! the first message whose bytes it reaches, past which it takes nothing
//! (`DetFd::take_stream_bytes`, and `DetFd::take_queued_rights` for one that
//! installed them). Either way what the call took without installing it is
//! released, as Linux releases it. The queue stops holding every message for
//! good once a message may reach the socket unseen or be taken unseen: a
//! `bind` or `SO_PASSCRED` (which names it, `unix_autobind`), a `connect`, a
//! send to it outside the turn or naming a destination, a datagram `splice`
//! into it, out-of-band data on a stream, a call outside the turn or whose
//! result does not fit the queue, more than 4096 waiting, or either socket of
//! the pair passed over `SCM_RIGHTS` or binder, where a receipt may record it
//! as an open file of its own, through which sends and receives go unseen
//! (`DetFd::give_up_pair_completeness`).
//!
//! Which receives that fail with `EFAULT` took a message is Linux's copy
//! order (v6.17): one that fails copying its inputs in, before it reads the
//! socket, takes nothing: a buffer outside user space (`import_ubuf`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/lib/iov_iter.c?h=v6.17#n1528>),
//! an iovec array it cannot read or an iovec outside user space
//! (`__import_iovec`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/lib/iov_iter.c?h=v6.17#n1447>),
//! a `msghdr` or a `recvmmsg` timeout it cannot read (`recvmsg_copy_msghdr`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/socket.c?h=v6.17#n2864>).
//! Past those, a datagram or seqpacket receive dequeues its message before it
//! copies the bytes, and frees it when that copy fails (`__unix_dgram_recvmsg`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n2645>),
//! so it took one. A stream receive fails only where it copied nothing, and
//! then keeps the bytes, descriptors and all (`unix_stream_read_generic`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n3059>),
//! unless it succeeded and a copy out after it failed: the sender's address
//! (`move_addr_to_user`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/socket.c?h=v6.17#n2281>)
//! or the header's lengths and flags (`____sys_recvmsg`,
//! <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/socket.c?h=v6.17#n2808>),
//! possible only where the first byte it receives into is writable
//! (`Guest::memory_map`).
//!
//! Under record and replay the send's `fstat` goes through the recording too,
//! and a received descriptor joins its sender's description only where the
//! receiving table holds a descriptor of it: replay holds a stand-in in the
//! received slot and can put the live open file there only by duplicating a
//! descriptor of the same table. One whose description only another
//! process's table, or only its queued message, holds is recorded from the
//! kernel's facts, in both runs alike. The queue there keeps every decision
//! existing recordings were made with (`Detcore::keeps_recorded_decisions`),
//! so a recording made before replays after: a queued message keeps the
//! ownership they were made with, which keeps nothing alive
//! (`DetFd::in_flight_open_file`), so every decision that reaches a
//! description through it (a receipt's join, a pending connection's pairing,
//! [`UnixConnections`]) decides as before; a `read`, `readv`, `recvfrom` or
//! `splice` that returns puts a queue holding messages out of lockstep, and
//! a `preadv2` does not, as before; a receipt joins the message it matches,
//! wherever it is, forgetting those before it; no queue holds messages of
//! data alone, an `EFAULT` receive, a short batch and a send to a gone peer
//! change nothing, and a table goes when the backend tears it down. The lock
//! ledger feeds nothing the guest reads there
//! (`Detcore::refresh_received_locks`), and counts no copy in flight for
//! these messages.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::sync::Arc;

use nix::fcntl::OFlag;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;
use reverie::syscalls::linux_abi::Mmsghdr;
use reverie::syscalls::linux_abi::Msghdr;
use tracing::debug;
use tracing::warn;

use crate::fd::FileIdentity;
use crate::fd::InFlightOpenFile;
use crate::fd::OpenFileHandle;
use crate::fd::SentDescriptor;
use crate::fd::TakenRights;
use crate::record_or_replay::RecordOrReplay;
use crate::syscalls::files::SocketRole;
use crate::tool_local::Detcore;

/// The bytes `received_descriptor_link` gives the path
/// `/proc/thread-self/fd/<fd>`: 21, up to 10 digits and the terminating NUL.
const LINK_PATH: usize = 32;
/// The bytes it gives the link read there. A link that fills them is longer
/// than `anon_inode:inotify` (18 bytes), so it is not read whole.
const LINK_TARGET: usize = 64;

/// The bytes of one received control buffer Detcore reads. One receive
/// delivers at most one `SCM_RIGHTS` message, of at most `SCM_MAX_FD` (253)
/// descriptors, beside a few fixed-size messages, far below this.
const MAX_CONTROL_BYTES: usize = 64 * 1024;

/// What releasing queued descriptors that no receive will deliver releases
/// in turn ([`settle`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Settled {
    /// The copies in flight `procfs::locks::LockLedger` counted, which it
    /// uncounts (`PlaceholderCopies::Unsent`).
    pub(crate) lock_copies: Vec<crate::types::OpenFileId>,
    /// The sockets among the descriptions that went with their last copy,
    /// whose ports go with them (`GlobalRequest::ReleasePort`).
    pub(crate) ports: Vec<crate::types::OpenFileId>,
}

/// Release `released`, queued descriptors Linux released without a receive:
/// each uncounts its copy in flight, and a description whose last reference
/// such a copy was goes with it, with the messages queued to it in turn
/// ([`InFlightOpenFile::release`]), as Linux's last `fput` releases them. What
/// that releases in the global state comes back, for the caller to apply
/// (`Detcore::settle_released_rights` in the releasing call, and the global
/// state at the terminal receipt of a death by a fatal signal,
/// `GlobalState::release_tables_at_death`).
pub(crate) fn settle(released: Vec<SentDescriptor>) -> Settled {
    let mut pending = released;
    let mut settled = Settled::default();
    while let Some(sent) = pending.pop() {
        settled.lock_copies.extend(sent.lock_copy);
        if let Some((open_file, ty, queued)) = sent.open_file.and_then(InFlightOpenFile::release) {
            if ty == crate::fd::FdType::Socket {
                settled.ports.push(open_file);
            }
            pending.extend(queued);
        }
    }
    settled
}

/// Whether a run with `cfg` follows the queues of `AF_UNIX` sockets and lets
/// a table go at its last holder's exit (`Detcore::release_table_at_exit`,
/// and the global state at the terminal receipt of a death by a fatal
/// signal, `GlobalState::release_tables_at_death`): where Detcore records
/// received descriptors (`Detcore::receipt_mode`), and not under record and
/// replay, whose recordings were made with a table going at the teardown.
pub(crate) fn releases_tables_at_exits(cfg: &crate::config::Config) -> bool {
    !cfg.recordreplay_modes
        && !cfg.backend.tool_shares_guest_descriptor_table
        && cfg.sequentialize_threads
}

/// Whether a run with `cfg` has the lock ledger follow copies in flight
/// (`procfs::locks::LockLedger::placeholder_copies`) of the descriptions
/// whose locks it follows: where it feeds the `/proc/locks` the guest reads
/// (`Detcore::refresh_received_locks`).
pub(crate) fn follows_lock_copies(cfg: &crate::config::Config) -> bool {
    cfg.virtualize_time && !cfg.recordreplay_modes
}

/// The descriptors carried by the `SCM_RIGHTS` messages in a sent or
/// received control buffer.
pub(crate) fn received_descriptors(control: &[u8]) -> Vec<RawFd> {
    const HEADER: usize = size_of::<libc::cmsghdr>();
    let align = |length: usize| length.next_multiple_of(size_of::<usize>());
    let mut descriptors = Vec::new();
    let mut offset = 0;
    while let Some(header) = control.get(offset..offset + HEADER) {
        let length = usize::from_ne_bytes(header[..8].try_into().unwrap());
        let level = i32::from_ne_bytes(header[8..12].try_into().unwrap());
        let kind = i32::from_ne_bytes(header[12..16].try_into().unwrap());
        let Some(data) = control.get(offset + HEADER..offset.saturating_add(length)) else {
            break;
        };
        if level == libc::SOL_SOCKET && kind == libc::SCM_RIGHTS {
            descriptors.extend(
                data.as_chunks::<{ size_of::<RawFd>() }>()
                    .0
                    .iter()
                    .map(|fd| RawFd::from_ne_bytes(*fd)),
            );
        }
        if length < HEADER {
            break;
        }
        offset += align(length);
    }
    descriptors
}

/// The descriptors a `struct msghdr`, read after a receive, says Linux
/// installed through `SCM_RIGHTS`: Linux updates `msg_controllen` to the
/// control bytes it wrote, so bytes past it (a reused buffer's earlier
/// messages) are not read.
fn delivered_through<M: MemoryAccess>(memory: &M, header: &Msghdr) -> Vec<RawFd> {
    if header.msg_control.is_null() || header.msg_controllen == 0 {
        return Vec::new();
    }
    let Some(address) = Addr::<u8>::from_raw(header.msg_control as usize) else {
        return Vec::new();
    };
    let mut control = vec![0; header.msg_controllen.min(MAX_CONTROL_BYTES)];
    if memory.read_exact(address, &mut control).is_err() {
        return Vec::new();
    }
    received_descriptors(&control)
}

/// The descriptors `call`, which returned `ret`, installed through
/// `SCM_RIGHTS`, in the order Linux reports them: those of a `recvmsg`, or of
/// the `ret` messages a `recvmmsg` received. `None` for any other call and
/// for a failed one. Read from guest memory after the call; a header that
/// cannot be read names none.
pub(crate) fn delivered_descriptors<M: MemoryAccess>(
    memory: &M,
    call: &Syscall,
    ret: i64,
) -> Option<Vec<RawFd>> {
    if ret < 0 {
        return None;
    }
    match call {
        Syscall::Recvmsg(call) => {
            let header: Option<Msghdr> = call
                .msg()
                .and_then(|address| memory.read_value(address).ok());
            Some(header.map_or_else(Vec::new, |header| delivered_through(memory, &header)))
        }
        Syscall::Recvmmsg(call) => {
            let Some(address) = call.mmsg() else {
                return Some(Vec::new());
            };
            let count = (ret as usize).min(libc::UIO_MAXIOV as usize);
            // SAFETY: `mmsghdr` is a plain C record; all-zero values are
            // valid staging values that `read_values` overwrites.
            let mut messages: Vec<Mmsghdr> =
                (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
            if memory.read_values(address.into(), &mut messages).is_err() {
                return Some(Vec::new());
            }
            Some(
                messages
                    .iter()
                    .flat_map(|message| delivered_through(memory, &message.msg_hdr))
                    .collect(),
            )
        }
        _ => None,
    }
}

/// Whether a `struct stat` can be a rendered procfs placeholder: an empty
/// regular file with no link, as `discover_fd_from_current_process` and
/// `Detcore::adopt_received_placeholders` require too.
fn may_be_placeholder(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFREG && stat.st_nlink == 0 && stat.st_size == 0
}

/// A guest address range, `(start, length)`.
type Range = (usize, usize);

/// The most `recvmmsg` entries whose geometry Detcore reads before a call.
/// Linux processes up to `vlen` entries (`do_recvmmsg` has no `UIO_MAXIOV`
/// clamp, unlike `sendmmsg`), as many as there are datagrams; a larger
/// vector is not snapshotted, and its descriptors are not recorded.
const MAX_SNAPSHOT_ENTRIES: usize = 64 * 1024;

/// The input-only fields of one message: Linux reads them and never writes
/// them back, so their values after the call are the ones the receive used.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MessageFields {
    name: usize,
    iov: usize,
    iovlen: usize,
    control: usize,
    /// The iovec array's segments, `(base, length)`.
    segments: Vec<Range>,
}

/// One message of a receive, as its inputs were before the call.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MessageInput {
    /// The header Linux reads and writes back into: the `msghdr`, or for
    /// `recvmmsg` the whole `mmsghdr` entry (its `msg_len` too).
    header: Range,
    /// The control buffer and its capacity (`msg_controllen` on input).
    control: Range,
    /// The name buffer Linux may write (`msg_name`, `msg_namelen` on input).
    name: Range,
    /// The iovec array Linux reads.
    iov_array: Range,
    fields: MessageFields,
}

/// Reads the input-only fields of `message`; `None` if its iovec array
/// cannot be read (Linux then fails that message with EFAULT).
fn message_fields<M: MemoryAccess>(memory: &M, message: &Msghdr) -> Option<MessageFields> {
    let mut segments = Vec::new();
    if !message.msg_iov.is_null() && message.msg_iovlen > 0 {
        let count = message.msg_iovlen.min(libc::UIO_MAXIOV as usize);
        // SAFETY: `iovec` is a plain C record; all-zero values are valid
        // staging values that `read_values` overwrites.
        let mut iovecs: Vec<libc::iovec> =
            (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
        memory
            .read_values(
                Addr::<libc::iovec>::from_raw(message.msg_iov as usize)?,
                &mut iovecs,
            )
            .ok()?;
        segments.extend(
            iovecs
                .iter()
                .map(|iov| (iov.iov_base as usize, iov.iov_len)),
        );
    }
    Some(MessageFields {
        name: message.msg_name as usize,
        iov: message.msg_iov as usize,
        iovlen: message.msg_iovlen,
        control: message.msg_control as usize,
        segments,
    })
}

/// Whether the `count` iovecs at `iov` ask for no bytes: there are none, or
/// each is of length 0. An array that cannot be read counts as one that asks
/// for some.
pub(crate) fn iovecs_are_empty<M: MemoryAccess>(
    memory: &M,
    iov: Option<Addr<libc::iovec>>,
    count: usize,
) -> bool {
    if count == 0 {
        return true;
    }
    let Some(iov) = iov.filter(|_| count <= libc::UIO_MAXIOV as usize) else {
        return false;
    };
    // SAFETY: `iovec` is a plain C record; all-zero values are valid staging
    // values that `read_values` overwrites.
    let mut iovecs: Vec<libc::iovec> = (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
    memory.read_values(iov, &mut iovecs).is_ok() && iovecs.iter().all(|iov| iov.iov_len == 0)
}

/// Whether any output range overlaps any input range other than its own
/// counterpart: `outputs` pairs each range with whether an identical input
/// range is its own (a header written back into itself, a control buffer
/// filled with its own control data). Counted with sorted interval ends:
/// the inputs overlapping `[start, end)` are those starting before `end`
/// less those ending at or before `start`.
fn outputs_overlap_inputs(inputs: &[Range], outputs: &[(Range, bool)]) -> bool {
    let live = |range: &&Range| range.1 > 0;
    let mut starts: Vec<usize> = inputs.iter().filter(live).map(|range| range.0).collect();
    let mut ends: Vec<usize> = inputs
        .iter()
        .filter(live)
        .map(|range| range.0.saturating_add(range.1))
        .collect();
    starts.sort_unstable();
    ends.sort_unstable();
    outputs.iter().any(|&((start, length), own)| {
        if length == 0 {
            return false;
        }
        let end = start.saturating_add(length);
        let overlapping =
            starts.partition_point(|&s| s < end) - ends.partition_point(|&e| e <= start);
        overlapping > usize::from(own)
    })
}

/// The input geometry of a `recvmsg` or `recvmmsg`, read before the call:
/// each message's header, control buffer, name buffer and iovecs, and a
/// `recvmmsg`'s timeout. Read before because Linux overwrites some inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReceiveGeometry {
    messages: Vec<MessageInput>,
    /// A `recvmmsg`, whose result counts messages.
    batch: bool,
    /// A `recvmmsg` timeout Linux writes the remaining time back into.
    timeout: Option<Range>,
    /// Why the geometry cannot be trusted, if it cannot.
    unusable: Option<&'static str>,
}

/// What a completed receive delivered over `SCM_RIGHTS`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Delivered {
    /// The descriptors Linux reports installing, in order.
    pub(crate) fds: Vec<RawFd>,
    /// Each message the receive took, in order: its share of `fds`, which
    /// follow one another. One that delivered none has a count of 0, and is
    /// truncated where Linux may have released descriptors it carried.
    pub(crate) messages: Vec<DeliveredMessage>,
    /// Why the receive's descriptors cannot be named, if they cannot: then
    /// `fds` is empty, and none of the call's descriptors is recorded.
    pub(crate) refused: Option<&'static str>,
    /// With [`Self::refused`], for a receive that completed in the caller's
    /// turn: whether it may have installed descriptors. Linux installs one
    /// only into a control buffer the message names (`scm_detach_fds`), so
    /// this is false where no message of the geometry names one and the
    /// geometry says which header each message used.
    pub(crate) may_have_installed: bool,
}

/// The descriptors one received message delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeliveredMessage {
    /// How many of [`Delivered::fds`] it delivered.
    pub(crate) count: usize,
    /// Whether Linux set `MSG_CTRUNC` in its `msg_flags`: the control buffer
    /// had room for fewer descriptors than the message carried, which Linux
    /// installed in order, releasing the rest.
    pub(crate) truncated: bool,
    /// The bytes Linux reports receiving with it (a `recvmsg`'s result, a
    /// `recvmmsg` entry's `msg_len`), where it reports them.
    pub(crate) bytes: Option<usize>,
}

/// What a send completed in the caller's turn passed over `SCM_RIGHTS`
/// (`Detcore::queue_sent_rights`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SentRights {
    /// For each message Linux reports sending: the descriptors its
    /// `SCM_RIGHTS` carried, in order (none for a message of data alone), the
    /// bytes it sent, and whether it named its destination (a nonzero
    /// `msg_namelen`), which a datagram socket's send reaches instead of its
    /// peer (`unix_dgram_sendmsg`).
    pub(crate) messages: Vec<(Vec<RawFd>, usize, bool)>,
    /// Whether a message Linux reports sending could not be read in full, so
    /// that what it carried is unknown.
    pub(crate) unread: bool,
}

/// The socket a receive took its messages from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReceivedFrom {
    /// The receiving socket's descriptor.
    pub(crate) socket: RawFd,
    /// A `MSG_PEEK` receive, which installs the descriptors and leaves the
    /// message queued.
    pub(crate) peek: bool,
    /// How many messages the call took: one for `recvmsg`, as many as it
    /// reports for `recvmmsg`.
    pub(crate) messages: usize,
    /// The call that took them: `recvmsg`, `recvmmsg`, or the `ioctl` of a
    /// binder transaction.
    pub(crate) call: Sysno,
}

/// A receive through a call that passes no control buffer
/// (`Detcore::note_receive_without_control`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReceiveWithoutControl {
    /// With `MSG_PEEK`, which takes nothing.
    pub(crate) peek: bool,
    /// Where its bytes go, and how many it asked for.
    pub(crate) request: Request,
    /// A `read`, `readv`, `preadv2` or `splice`, which returns 0 at once when
    /// it asks for no bytes; `recvfrom` passes such a call on to the socket.
    pub(crate) returns_at_once_when_empty: bool,
    /// A `recvfrom` with an address to fill, which Linux copies out after the
    /// receive (`move_addr_to_user`): an `EFAULT` there comes after the call
    /// took its message.
    pub(crate) address_out: bool,
    /// How many messages the call takes.
    pub(crate) takes: Takes,
    /// Whether the call completes in the caller's turn, decided before it
    /// (`Detcore::completes_in_turn`): only then is the message it took the
    /// oldest one of the queue as the queue stands when it returns.
    pub(crate) in_turn: bool,
    /// Whether the rule existing recordings were made with notes this call:
    /// `read`, `readv`, `recvfrom` and `splice`, not `preadv2`
    /// (`Detcore::keeps_recorded_decisions`).
    pub(crate) noted_by_recordings: bool,
}

/// How a receive without a control buffer takes from a socket
/// ([`ReceiveWithoutControl`]). Each takes one message of a datagram or
/// seqpacket socket, and from a stream socket the bytes its result counts,
/// detaching the descriptors of one message at most, as it stops after a
/// buffer that carried some (`unix_stream_read_generic`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Takes {
    /// A `read`, `readv`, `recvfrom` or `preadv2`, whose result counts the
    /// bytes of the message it took.
    Counted,
    /// A `splice`, which reads a datagram or seqpacket socket once
    /// (`copy_splice_read`): its result counts the bytes it moved, which need
    /// not be the message's, and from a stream socket those it took. (Detcore
    /// refuses a `sendfile` through a socket, `Detcore::handle_sendfile`, so
    /// none takes anything.)
    One,
}

/// A send through a call that passes no control buffer, to note after it
/// (`Detcore::send_without_control`, `Detcore::note_send_without_control`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SendWithoutControl {
    /// The sending socket's descriptor.
    socket: RawFd,
    /// Whether the send completes in the caller's turn, decided before it.
    in_turn: bool,
    /// A datagram send that names its destination, which may not be the
    /// peer, or a stream send of out-of-band data (`MSG_OOB`), which a
    /// stream's reads do not count among its bytes: either leaves the peer's
    /// queue without a place for it.
    unplaced: bool,
    /// Whether the pair is `SOCK_STREAM`, where a send of no bytes queues
    /// nothing.
    stream: bool,
}

/// Where a receive's bytes go ([`ReceiveWithoutControl`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// A buffer at this guest address, of this many bytes.
    Bytes(usize, usize),
    /// The iovecs at this guest address (0 for none), this many of them,
    /// read from guest memory only where they decide what the call took
    /// ([`iovecs_are_empty`], [`receive_buffers`]).
    Iovecs(usize, usize),
    /// A pipe or file, this many bytes (`splice`): no guest buffer.
    Spliced(usize),
}

/// The highest user-space address, plus one, on x86-64 with four-level page
/// tables (`TASK_SIZE_MAX`): `access_ok` refuses a range past it.
const USER_SPACE_END: usize = 0x7fff_ffff_f000;

/// Linux's cap on one read's bytes (`MAX_RW_COUNT`), which `import_ubuf`
/// applies before `access_ok`.
const MAX_RW_COUNT: usize = (i32::MAX as usize) & !0xfff;

/// Whether `[at, at + length)` is in user space, as `access_ok` asks of every
/// buffer before a receive reads the socket.
fn in_user_space(at: usize, length: usize) -> bool {
    at.checked_add(length.min(MAX_RW_COUNT))
        .is_some_and(|end| end <= USER_SPACE_END)
}

/// The buffers a receive's bytes go to, as Linux checks them before it asks
/// the socket (Linux v6.17): `import_ubuf` and `vfs_read` require a buffer in
/// user space
/// (<https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/lib/iov_iter.c?h=v6.17#n1528>),
/// and `__import_iovec` reads the iovec array and requires each iovec in user
/// space
/// (<https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/lib/iov_iter.c?h=v6.17#n1447>).
/// `None` where that check fails with `EFAULT`, so that the call took
/// nothing from the socket.
pub(crate) fn receive_buffers<M: MemoryAccess>(
    memory: &M,
    request: Request,
) -> Option<Vec<(usize, usize)>> {
    match request {
        Request::Bytes(at, length) => in_user_space(at, length).then(|| vec![(at, length)]),
        Request::Spliced(_) => Some(Vec::new()),
        Request::Iovecs(_, 0) => Some(Vec::new()),
        Request::Iovecs(at, count) => {
            if count > libc::UIO_MAXIOV as usize {
                return None;
            }
            let iov = Addr::<libc::iovec>::from_raw(at)?;
            // SAFETY: `iovec` is a plain C record; all-zero values are valid
            // staging values that `read_values` overwrites.
            let mut iovecs: Vec<libc::iovec> =
                (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
            memory.read_values(iov, &mut iovecs).ok()?;
            iovecs
                .iter()
                .map(|iov| (iov.iov_base as usize, iov.iov_len))
                .map(|(at, length)| in_user_space(at, length).then_some((at, length)))
                .collect()
        }
    }
}

/// The buffers the first message of a `recvmsg` or `recvmmsg` (`call`)
/// receives into, as Linux checks them before it asks the socket
/// (`recvmsg_copy_msghdr`, `___sys_recvmsg`,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/socket.c?h=v6.17#n2864>):
/// its header must be readable, a `recvmmsg` timeout too, and its iovecs as
/// [`receive_buffers`] says; and whether its control buffer has room for
/// one descriptor, without which Linux releases every descriptor the message
/// carried (`scm_detach_fds`). `None` where that check fails with `EFAULT`.
pub(crate) fn message_receive_buffers<M: MemoryAccess>(
    memory: &M,
    call: &Syscall,
) -> Option<(Vec<(usize, usize)>, bool)> {
    let header: Msghdr = match call {
        Syscall::Recvmsg(call) => memory.read_value(call.msg()?).ok()?,
        Syscall::Recvmmsg(call) => {
            if let Some(timeout) = call.timeout() {
                let _: libc::timespec = memory.read_value(timeout.cast()).ok()?;
            }
            let entry: Mmsghdr = memory.read_value(call.mmsg()?).ok()?;
            entry.msg_hdr
        }
        _ => return Some((Vec::new(), false)),
    };
    // SAFETY: CMSG_LEN computes a length; it reads nothing.
    let one = unsafe { libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) } as usize;
    let room = !header.msg_control.is_null() && header.msg_controllen >= one;
    receive_buffers(
        memory,
        Request::Iovecs(header.msg_iov as usize, header.msg_iovlen),
    )
    .map(|buffers| (buffers, room))
}

/// Whether a send's `result` is Linux's answer to a datagram send whose
/// connected peer has gone (`ECONNREFUSED`, `Detcore::note_peer_gone`).
pub(crate) fn found_its_peer_gone(result: &Result<i64, Error>) -> bool {
    matches!(result, Err(Error::Errno(Errno::ECONNREFUSED)))
}

/// What an address a guest passed names, for `AF_UNIX` pairing
/// ([`unix_address`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UnixAddress {
    /// Not an `AF_UNIX` address (its family is another, or it is too short
    /// to have one).
    Other,
    /// An `AF_UNIX` name Detcore can compare: an abstract name's bytes after
    /// `sun_family`, its leading NUL included, which Linux compares whole
    /// (`unix_find_abstract`), or an absolute path up to its first NUL.
    Named(Box<[u8]>),
    /// An `AF_UNIX` address whose socket Detcore cannot name: a relative
    /// path, which names a different file from each working directory, an
    /// unnamed address, or one it could not read (a `connect` or `bind`
    /// through an unreadable address fails with `EFAULT`, so only a call
    /// that succeeded is ever attributed).
    Unnamed,
}

/// What the address at `address` in guest memory (`length` bytes) names
/// ([`UnixAddress`]).
pub(crate) fn unix_address<M: MemoryAccess>(
    memory: &M,
    address: usize,
    length: usize,
) -> UnixAddress {
    const PATH: usize = std::mem::offset_of!(libc::sockaddr_un, sun_path);
    let length = length.min(size_of::<libc::sockaddr_un>());
    if length < size_of::<libc::sa_family_t>() {
        return UnixAddress::Other;
    }
    let mut bytes = vec![0u8; length];
    let read = Addr::<u8>::from_raw(address)
        .is_some_and(|address| memory.read_exact(address, &mut bytes).is_ok());
    if !read {
        return UnixAddress::Unnamed;
    }
    if u16::from_ne_bytes([bytes[0], bytes[1]]) != libc::AF_UNIX as u16 {
        return UnixAddress::Other;
    }
    let path = bytes.get(PATH..).unwrap_or_default();
    match path.first() {
        Some(0) => UnixAddress::Named(path.into()),
        Some(b'/') => path
            .split(|&byte| byte == 0)
            .next()
            .map_or(UnixAddress::Unnamed, |path| UnixAddress::Named(path.into())),
        _ => UnixAddress::Unnamed,
    }
}

/// The name an `AF_UNIX` address names a socket by ([`UnixAddress::Named`]),
/// read from guest memory at `address` (`length` bytes). `None` for another
/// family, an unnamed or unreadable address, or a relative path.
pub(crate) fn unix_socket_name<M: MemoryAccess>(
    memory: &M,
    address: usize,
    length: usize,
) -> Option<Box<[u8]>> {
    match unix_address(memory, address, length) {
        UnixAddress::Named(name) => Some(name),
        UnixAddress::Other | UnixAddress::Unnamed => None,
    }
}

/// The most connections kept waiting for an `accept` on one name
/// ([`UnixConnections`]); beyond it the name's order is unknown.
const MAX_PENDING_CONNECTIONS: usize = 4096;

/// The connections guests made to one `AF_UNIX` name a guest bound
/// ([`UnixConnections`]).
#[derive(Default)]
struct PendingConnections {
    /// Which bind of the name this is: a listener bound before the name was
    /// unlinked and bound again keeps the earlier one, and its accepts take
    /// connections Detcore no longer tracks.
    generation: u64,
    /// The connecting sockets' descriptions, oldest first, as Linux queues
    /// the connections on the listener for `accept` to take: at the tail
    /// (`unix_stream_connect`), from the head (`unix_accept`).
    sockets: VecDeque<OpenFileHandle>,
    /// The socket a guest bound to the name, its listener once it listens.
    listener: Option<OpenFileHandle>,
    /// A `connect` or `accept` on the name completed outside its caller's
    /// turn, a connect Detcore could not attribute may have reached it, or
    /// too many connections waited: which connection an `accept` takes is
    /// unknown until a guest binds the name again.
    unknown: bool,
}

impl PendingConnections {
    fn lose_order(&mut self) {
        self.sockets.clear();
        self.unknown = true;
    }
}

/// The connections waiting for an `accept` on each `AF_UNIX` name a guest
/// bound, so that an accepted connection and the socket whose `connect` made
/// it are paired (`Detcore::note_unix_accept`). One per run: every task of
/// the process tree shares it (`ThreadState::unix_connections`), as it shares
/// the guest clock, so a bind, a connect and an accept in different threads
/// or processes meet.
///
/// A connect Detcore cannot attribute to one of these names may have
/// reached any of them: one through a relative path or another spelling of
/// a path (a doubled slash, a `..`, a symlink or hard link, a renamed
/// socket file), to a name no guest bound here, through an absolute path
/// from a task whose root may not be the container's (`chroot`, where one
/// path names another file), or of a socket whose type Detcore did not see
/// made. Every name's order is then unknown until it is bound again
/// ([`UnixConnections::lost`]), so no accept pairs with a socket whose
/// connection it may not have taken. A bind through an absolute path from
/// such a task is not tracked. A connection made from outside Hermit is not
/// seen at all.
///
/// That costs precision, never correctness: one stream connect to a socket
/// no guest bound here, as a glibc user or group lookup makes through NSS
/// (nscd, systemd-userdb), stops pairing for every name until it is bound
/// again, and descriptors received over those connections are then handled
/// as on main.
#[derive(Default)]
pub(crate) struct UnixConnections {
    names: BTreeMap<Box<[u8]>, PendingConnections>,
    /// The last bind's generation ([`PendingConnections::generation`]).
    binds: u64,
}

impl UnixConnections {
    /// A guest bound `listener` to `name`: its listener's queue starts
    /// empty. Which bind of the name it was, for the listener to keep.
    pub(crate) fn bound(&mut self, name: &[u8], listener: OpenFileHandle) -> u64 {
        self.binds += 1;
        self.names.insert(
            name.into(),
            PendingConnections {
                generation: self.binds,
                listener: Some(listener),
                ..Default::default()
            },
        );
        self.binds
    }

    /// The socket the last guest bind of `name` bound, if a guest bound it.
    fn listener(&self, name: &[u8]) -> Option<&OpenFileHandle> {
        self.names.get(name)?.listener.as_ref()
    }

    /// How many connections wait for an `accept` on `name`, or `None` where
    /// their order is unknown or no guest bound it.
    #[cfg(test)]
    pub(crate) fn waiting(&self, name: &[u8]) -> Option<usize> {
        self.names
            .get(name)
            .filter(|pending| !pending.unknown)
            .map(|pending| pending.sockets.len())
    }

    /// A guest's `connect` to `name` completed: `socket` is the connecting
    /// socket where it completed in the caller's turn, `None` where it did
    /// not. A name no guest bound here may be another spelling of one that
    /// was ([`Self::lost`]).
    fn connected(&mut self, name: &[u8], socket: Option<OpenFileHandle>) {
        let Some(pending) = self.names.get_mut(name) else {
            self.lost();
            return;
        };
        match socket {
            _ if pending.unknown => {}
            Some(socket) if pending.sockets.len() < MAX_PENDING_CONNECTIONS => {
                pending.sockets.push_back(socket)
            }
            _ => pending.lose_order(),
        }
    }

    /// A guest's stream or seqpacket `connect` completed that Detcore cannot
    /// attribute to one name ([`UnixConnections`]): every name's order is
    /// unknown.
    fn lost(&mut self) {
        self.names
            .values_mut()
            .for_each(PendingConnections::lose_order);
    }

    /// A guest's `accept` on the listener bound to `name` by bind
    /// `generation` completed, in the caller's turn or not (`in_turn`): the
    /// socket whose connection it took, if that is known. A listener whose
    /// name was bound again since has none: the connections Detcore tracks
    /// are the later listener's.
    fn accepted(&mut self, name: &[u8], generation: u64, in_turn: bool) -> Option<OpenFileHandle> {
        let pending = self
            .names
            .get_mut(name)
            .filter(|pending| pending.generation == generation)?;
        if !in_turn {
            pending.lose_order();
        }
        if pending.unknown {
            return None;
        }
        pending.sockets.pop_front()
    }
}

/// Whether guests alone hold both ends of a connection that a `connect` or
/// `accept` completed, in the caller's turn or not (`in_turn`): the
/// connecting socket and the listener, where Detcore knows them, are both
/// sockets a guest made in this run and never passed out of the guest
/// (`DetFd::is_guest_created`). Only guests then hold the connecting socket,
/// and only guests can accept from the listener (`Detcore::classify_guest_held_peer`).
pub(crate) fn both_ends_guest_made(
    in_turn: bool,
    connecting: Option<&OpenFileHandle>,
    listener: Option<&OpenFileHandle>,
) -> bool {
    in_turn
        && connecting.is_some_and(OpenFileHandle::is_guest_created)
        && listener.is_some_and(OpenFileHandle::is_guest_created)
}

/// Runs `f` on the run's [`UnixConnections`].
fn with_connections<T: RecordOrReplay, G: Guest<Detcore<T>>, R>(
    guest: &G,
    f: impl FnOnce(&mut UnixConnections) -> R,
) -> R {
    let connections = Arc::clone(&guest.thread_state().unix_connections);
    let mut connections = connections
        .lock()
        .expect("AF_UNIX connection registry poisoned");
    f(&mut connections)
}

impl ReceiveGeometry {
    /// The geometry of `call`, a `recvmsg` or `recvmmsg`, read from guest
    /// memory before it runs; `None` for any other call or an unreadable
    /// `recvmsg` header. A `recvmmsg` vector is read in full, up to the
    /// `vlen` Linux may process (at most [`MAX_SNAPSHOT_ENTRIES`] entries);
    /// an entry or iovec array that cannot be read makes the geometry
    /// unusable, and the call's descriptors are not recorded.
    pub(crate) fn before<M: MemoryAccess>(memory: &M, call: &Syscall) -> Option<Self> {
        let message_input = |header: Range, message: &Msghdr| -> Option<MessageInput> {
            Some(MessageInput {
                header,
                control: (message.msg_control as usize, message.msg_controllen),
                name: (message.msg_name as usize, message.msg_namelen as usize),
                iov_array: (
                    message.msg_iov as usize,
                    message.msg_iovlen.min(libc::UIO_MAXIOV as usize) * size_of::<libc::iovec>(),
                ),
                fields: message_fields(memory, message)?,
            })
        };
        match call {
            Syscall::Recvmsg(call) => {
                let address = call.msg()?;
                let message: Msghdr = memory.read_value(address).ok()?;
                let header = (address.as_raw(), size_of::<Msghdr>());
                let (messages, unusable) = match message_input(header, &message) {
                    Some(input) => (vec![input], None),
                    None => (vec![], Some("its iovec array cannot be read")),
                };
                Some(ReceiveGeometry {
                    messages,
                    batch: false,
                    timeout: None,
                    unusable,
                })
            }
            Syscall::Recvmmsg(call) => {
                let base = call.mmsg().map_or(0, |vector| vector.as_raw());
                let timeout = call
                    .timeout()
                    .map(|timeout| (timeout.as_raw(), size_of::<libc::timespec>()));
                if call.vlen() as usize > MAX_SNAPSHOT_ENTRIES {
                    return Some(ReceiveGeometry {
                        messages: Vec::new(),
                        batch: true,
                        timeout,
                        unusable: Some("its vector is too long to read before the call"),
                    });
                }
                let mut messages = Vec::new();
                let mut unusable = None;
                for index in 0..call.vlen() as usize {
                    let at = base.saturating_add(index * size_of::<Mmsghdr>());
                    let Some(Ok(entry)) =
                        Addr::<Mmsghdr>::from_raw(at).map(|entry| memory.read_value(entry))
                    else {
                        unusable = Some("an entry of its vector cannot be read");
                        break;
                    };
                    let entry: Mmsghdr = entry;
                    match message_input((at, size_of::<Mmsghdr>()), &entry.msg_hdr) {
                        Some(input) => messages.push(input),
                        None => {
                            unusable = Some("an iovec array cannot be read");
                            break;
                        }
                    }
                }
                Some(ReceiveGeometry {
                    messages,
                    batch: true,
                    timeout,
                    unusable,
                })
            }
            _ => None,
        }
    }

    /// Whether an output range of the call overlaps an input range other
    /// than its own: every range Linux may write for any entry it may
    /// process (a header's written-back fields and `msg_len`, a name
    /// buffer, an iovec segment, a control buffer, the timeout) against
    /// every range it reads (headers, iovec arrays, control buffers).
    fn overlapping(&self) -> bool {
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        for message in &self.messages {
            inputs.extend([message.header, message.iov_array, message.control]);
            outputs.extend([
                (message.header, true),
                (message.control, true),
                (message.name, false),
            ]);
            outputs.extend(
                message
                    .fields
                    .segments
                    .iter()
                    .map(|&segment| (segment, false)),
            );
        }
        outputs.extend(self.timeout.map(|timeout| (timeout, false)));
        outputs_overlap_inputs(&inputs, &outputs)
    }

    /// Whether the input-only fields read now, after the call, are the
    /// ones read before it. After a successful attempt that ran in the
    /// caller's turn no other guest thread ran, so these are the values that
    /// attempt used: a sibling that changed them during an earlier attempt's
    /// wait (`EAGAIN`, then polling) is detected here.
    fn still_named<M: MemoryAccess>(&self, memory: &M) -> bool {
        self.messages.iter().all(|message| {
            Addr::<Msghdr>::from_raw(message.header.0)
                .and_then(|at| memory.read_value(at).ok())
                .and_then(|now: Msghdr| message_fields(memory, &now))
                .is_some_and(|now| now == message.fields)
        })
    }

    /// The descriptors a receive with this geometry, which returned `ret`,
    /// installed: for each delivered message (one for `recvmsg`, the first
    /// `ret` for `recvmmsg`), the control bytes Linux reports writing (the
    /// `msg_controllen` it writes back) at the control buffer named before
    /// the call. None are named if an output of the call overlaps one of its
    /// inputs, if the inputs changed before the attempt that succeeded, or if
    /// Linux reports more control bytes than the buffer named before the call
    /// held.
    pub(crate) fn delivered<M: MemoryAccess>(&self, memory: &M, ret: i64) -> Delivered {
        let refused = |reason, may_have_installed| Delivered {
            fds: Vec::new(),
            messages: Vec::new(),
            refused: Some(reason),
            may_have_installed,
        };
        if ret < 0 {
            return Delivered::default();
        }
        // Whether a refused receive may have installed descriptors
        // (`Delivered::may_have_installed`). The geometry is that of the
        // attempt that completed, read right before it
        // (`ThreadState::receive_geometry`), and Linux copies a header in
        // before it writes anything for its message (`___sys_recvmsg`), so
        // the geometry names the control buffers Linux used, except where
        // it is incomplete or a batch's output overlaps a header: Linux
        // reads each later entry's header only when it reaches it.
        let names_control = self
            .messages
            .iter()
            .any(|message| message.control.0 != 0 && message.control.1 != 0);
        if let Some(reason) = self.unusable {
            return refused(reason, true);
        }
        if self.overlapping() {
            return refused(
                "a buffer it writes overlaps a buffer or header it reads",
                names_control || self.batch,
            );
        }
        if !self.still_named(memory) {
            return refused(
                "its buffers changed before the attempt that succeeded",
                names_control,
            );
        }
        let count = if self.batch {
            (ret as usize).min(self.messages.len())
        } else {
            self.messages.len().min(1)
        };
        let mut fds = Vec::new();
        let mut messages = Vec::new();
        for message in &self.messages[..count] {
            // Linux writes the message's flags back too.
            let flags = message.header.0 + std::mem::offset_of!(Msghdr, msg_flags);
            let flags: Option<libc::c_int> =
                Addr::<libc::c_int>::from_raw(flags).and_then(|at| memory.read_value(at).ok());
            // And a batch entry's length (`mmsghdr::msg_len`, after its
            // header); a single receive's result is its length.
            let bytes = if self.batch {
                Addr::<u32>::from_raw(message.header.0 + std::mem::size_of::<Msghdr>())
                    .and_then(|at| memory.read_value(at).ok())
                    .map(|length: u32| length as usize)
            } else {
                usize::try_from(ret).ok()
            };
            let received = if message.control.0 == 0 || message.control.1 == 0 {
                Vec::new()
            } else {
                let written = message.header.0 + std::mem::offset_of!(Msghdr, msg_controllen);
                let Some(length) =
                    Addr::<usize>::from_raw(written).and_then(|at| memory.read_value(at).ok())
                else {
                    return refused("its control length cannot be read back", true);
                };
                if length > message.control.1 {
                    return refused(
                        "its control buffer grew before the attempt that succeeded",
                        true,
                    );
                }
                let length = length.min(MAX_CONTROL_BYTES);
                let mut control = vec![0; length];
                match Addr::<u8>::from_raw(message.control.0)
                    .map(|at| memory.read_exact(at, &mut control))
                {
                    _ if length == 0 => Vec::new(),
                    Some(Ok(())) => received_descriptors(&control),
                    _ => return refused("its control data cannot be read back", true),
                }
            };
            if received.is_empty() {
                // With no room for even one descriptor (no control buffer,
                // or too small a one), Linux releases them all and sets
                // `MSG_CTRUNC` (`scm_detach_fds`, `scm_recv_unix`): the
                // message may have carried some. So may one whose flags
                // cannot be read back. Any other carried none.
                messages.push(DeliveredMessage {
                    count: 0,
                    truncated: flags.is_none_or(|flags| flags & libc::MSG_CTRUNC != 0),
                    bytes,
                });
                continue;
            }
            // Flags that cannot be read back count as complete, so that only
            // all of a queued message can match.
            let truncated = flags.is_some_and(|flags| flags & libc::MSG_CTRUNC != 0);
            messages.push(DeliveredMessage {
                count: received.len(),
                truncated,
                bytes,
            });
            fds.extend(received);
        }
        Delivered {
            fds,
            messages,
            refused: None,
            may_have_installed: false,
        }
    }
}

/// The kernel's facts about one received descriptor, taken as the receive
/// returns ([`Detcore::received_facts`]).
struct ReceivedFacts {
    /// `F_GETFD`.
    descriptor_flags: i32,
    /// `F_GETFL`.
    status_flags: i32,
    stat: libc::stat,
}

impl ReceivedFacts {
    fn file(&self) -> FileIdentity {
        (self.stat.st_dev, self.stat.st_ino)
    }
}

/// What a receive took for one descriptor it installed
/// ([`Detcore::record_received_facts`]).
#[derive(Clone, Copy)]
struct TakenDescriptor<'a> {
    /// The descriptor the send queued in its place, where the receipt may
    /// join it.
    sent: Option<&'a SentDescriptor>,
    /// Whether the queue it came from was in lockstep with the socket's
    /// messages.
    lockstep: bool,
    /// Whether the receive was a `MSG_PEEK`, which leaves the message queued.
    peek: bool,
}

/// What became of one received descriptor ([`Detcore::record_received_facts`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Receipt {
    /// Recorded as an open file new to this table.
    New,
    /// Joined an open file this table already models, as an alias.
    Joined,
    /// Left out of the table.
    Unrecorded,
}

/// How a receive's descriptors reached the guest, as warnings name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Channel {
    /// A `recvmsg` or `recvmmsg` control message.
    ScmRights,
    /// A binder transaction read with `BINDER_WRITE_READ`
    /// (`crate::syscalls::binder`).
    Binder,
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ScmRights => "over SCM_RIGHTS",
            Self::Binder => "in a binder transaction",
        })
    }
}

/// How this run handles descriptors received over `SCM_RIGHTS` or in a
/// binder transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReceiptMode {
    /// The tool shares the guest's descriptor table and discovers a received
    /// descriptor when a call first names it.
    Discovered,
    /// Detcore records each one as the receive returns.
    Recorded,
    /// Detcore keeps no record of them, for the reason given.
    Unrecorded(&'static str),
}

impl<T: RecordOrReplay> Detcore<T> {
    /// How this run handles received descriptors. Recorded only where the
    /// tool does not share the guest's descriptor table (it discovers them
    /// there), and with sequentialized threads (otherwise another thread could
    /// close a number, or `dup2` another file over it, between the receive
    /// and the facts taken here; a received rendered procfs descriptor is
    /// refused by name there when first named,
    /// `Detcore::adopt_received_placeholders`). Under record and replay the
    /// facts come from the recording ([`Self::probe_received`]), so replay,
    /// which stands in a placeholder for each received descriptor, takes the
    /// recording's.
    fn receipt_mode<G: Guest<Self>>(&self, guest: &G) -> ReceiptMode {
        if self.cfg.backend.tool_shares_guest_descriptor_table {
            ReceiptMode::Discovered
        } else if !guest.config().sequentialize_threads {
            ReceiptMode::Unrecorded("without sequentialized threads")
        } else {
            ReceiptMode::Recorded
        }
    }

    /// Whether a send or receive through `call` completes in the caller's
    /// turn, decided before the call: the descriptor is one Detcore polls in
    /// turn (`IOAction::NonblockizeRetry`) or one already nonblocking
    /// (`IOAction::PassThru`). Any other call runs on the background path
    /// Whether a receive through `call` completes in the caller's turn,
    /// decided before the call: the descriptor is one Detcore polls in turn
    /// (`IOAction::NonblockizeRetry`), or one already nonblocking or a call
    /// whose own flags keep it from waiting (`MSG_DONTWAIT`;
    /// `IOAction::PassThru`). Any other receive runs on the background path
    /// (`BlockingExternalIO`: a physically blocking socket, or one Detcore
    /// keeps no model of), where other tasks run between the kernel's
    /// completion and the caller's continuation and could close or replace a
    /// delivered number, or rewrite the control data; the descriptors such a
    /// receive delivers are never recorded, nor those such a send carries
    /// queued.
    pub(crate) fn completes_in_turn<G, C>(&self, guest: &mut G, call: C) -> bool
    where
        G: Guest<Self>,
        C: crate::syscalls::helpers::NonblockableSyscall + Into<Syscall>,
    {
        matches!(
            crate::syscalls::helpers::ioaction_based_on_fd_status(guest, call),
            Ok(crate::syscalls::helpers::IOAction::NonblockizeRetry
                | crate::syscalls::helpers::IOAction::PassThru)
        )
    }

    /// Before a receive through `fd` without a control buffer (`call`):
    /// whether it completes in the caller's turn ([`Self::completes_in_turn`]).
    /// Asked only where it matters, of an `AF_UNIX` socket whose type
    /// Detcore saw made, outside record and replay
    /// (`Detcore::note_receive_without_control`); false elsewhere.
    pub(crate) fn receives_in_turn<G, C>(&self, guest: &mut G, fd: RawFd, call: C) -> bool
    where
        G: Guest<Self>,
        C: crate::syscalls::helpers::NonblockableSyscall + Into<Syscall>,
    {
        !self.keeps_recorded_decisions()
            && self.pairs_unix_sockets(guest)
            && guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.unix_type().is_some())
                .unwrap_or(false)
            && self.completes_in_turn(guest, call)
    }

    /// Whether the tool discovers received descriptors itself (it shares the
    /// guest's descriptor table), so that a receive records none.
    pub(crate) fn discovers_received_descriptors<G: Guest<Self>>(&self, guest: &G) -> bool {
        self.receipt_mode(guest) == ReceiptMode::Discovered
    }

    /// Whether a receive marks the inotify instances it delivers on a backend
    /// that discovers them (`mark_discovered_inotify_receipts`), so it must
    /// know what it delivered (`ThreadState::receive_geometry`).
    pub(crate) fn marks_discovered_inotify_receipts<G: Guest<Self>>(&self, guest: &G) -> bool {
        self.discovers_received_descriptors(guest)
            && self.cfg.use_nonblocking_sockets()
            && !self.cfg.recordreplay_modes
    }

    /// Where the tool discovers received descriptors (DBT, whose Tool shares
    /// the guest's descriptor table), the receiver's description is made from
    /// the host's status flags (`ThreadState::with_detfd`), and an inotify
    /// instance's host O_NONBLOCK may be the one Detcore set when its sender
    /// made it (`Detcore::handle_inotify_init1`) rather than the sender's own.
    /// So each inotify instance a receive delivered (`delivered`) whose host
    /// description is nonblocking has a guest blocking mode that is unknown
    /// until the guest sets it (`DetFd::blocking_mode_unknown`), as an
    /// unmatched receipt's is where receipts are recorded: an `F_GETFL` of
    /// it, and a read of it that finds nothing queued, are refused by name
    /// rather than answered with a mode that may be wrong (a blocking read
    /// answered with EAGAIN, which Linux never gives it).
    fn mark_discovered_inotify_receipts<G: Guest<Self>>(&self, guest: &G, delivered: &Delivered) {
        if !self.marks_discovered_inotify_receipts(guest) {
            return;
        }
        for &fd in &delivered.fds {
            // A number another thread has closed meanwhile is not discovered.
            let _ = guest.thread_state().with_detfd(fd, |detfd| {
                if detfd.ty() == crate::fd::FdType::Inotify
                    && detfd.status_flags() & libc::O_NONBLOCK != 0
                {
                    detfd.mark_blocking_mode_unknown();
                }
            });
        }
    }

    /// Record in this thread's descriptor table each descriptor a receive
    /// from `from` installed (`delivered`), where this run records received
    /// descriptors and the receive completed in the caller's turn (`in_turn`,
    /// [`Self::completes_in_turn`]), and return those it recorded as open
    /// files new to the table. One that joined the open file its sender's
    /// table modeled (see the module documentation) is not returned: it brings
    /// the lock ledger no new identity, and `refresh_description_locks` would
    /// mark that shared open file, the sender's too, as received. The guest's
    /// result is never changed. An `SCM_RIGHTS` receive in the caller's turn
    /// whose layout cannot be trusted, and which may have installed
    /// descriptors (its geometry names a control buffer,
    /// [`Delivered::may_have_installed`], and its socket's queue does not rule
    /// them out, `DetFd::may_deliver_rights`), ends a fail-closed run as a
    /// policy refusal
    /// ([`Self::refuse_unserviceable_operation_because`]): the
    /// descriptors it may have installed would be left out of the table, and
    /// a later call through one would run on the background path
    /// (`BlockingExternalIO`), outside the turn order. Otherwise a descriptor
    /// left out of the table, as every received descriptor was before, is
    /// named in a warning: for each receive whose layout cannot be trusted
    /// (where it can have installed none, where unsupported operations are
    /// allowed, on the background path, or in a binder transaction,
    /// https://github.com/rrnewton/hermit/issues/5046), each
    /// receive on the background path, each
    /// number that already has a record, each anonymous inode other than an
    /// inotify instance, and each descriptor whose facts cannot be taken.
    /// Without sequentialized threads none is recorded, logged at debug level
    /// only (a warning per call, or per tool process, would differ between
    /// runs that share a process). Where the tool discovers them, none is
    /// recorded either, and the inotify instances among them are marked
    /// (`Self::mark_discovered_inotify_receipts`). `cloexec` is whether the receive asked for
    /// its descriptors close-on-exec (`MSG_CMSG_CLOEXEC`), which a descriptor
    /// listed without its facts keeps. A tool error, after which record and
    /// replay could no longer follow each other, ends the run.
    pub(crate) async fn record_received_descriptors<G: Guest<Self>>(
        &self,
        guest: &mut G,
        delivered: &Delivered,
        channel: Channel,
        in_turn: bool,
        cloexec: bool,
        from: ReceivedFrom,
    ) -> Result<Vec<RawFd>, Error> {
        match self.receipt_mode(guest) {
            ReceiptMode::Discovered => {
                self.mark_discovered_inotify_receipts(guest, delivered);
                return Ok(Vec::new());
            }
            ReceiptMode::Unrecorded(why) => {
                if !delivered.fds.is_empty() || delivered.refused.is_some() {
                    // A listing of this table cannot come from the model now
                    // (`syscalls::files::descriptor_directory`).
                    guest.thread_state().note_unaccounted_receipt();
                }
                if !delivered.fds.is_empty() {
                    debug!(
                        "[detcore] descriptors {:?}, received {channel}, are not recorded ({why})",
                        delivered.fds
                    );
                }
                return Ok(Vec::new());
            }
            ReceiptMode::Recorded => {}
        }
        if let Some(reason) = delivered.refused {
            guest.thread_state().note_unaccounted_receipt();
            if channel == Channel::ScmRights
                && in_turn
                && delivered.may_have_installed
                && self.cfg.panic_on_unsupported_syscalls
                && guest
                    .thread_state()
                    .with_detfd(from.socket, |socket| socket.may_deliver_rights())
                    .unwrap_or(true)
            {
                let refusal = format!(
                    "the descriptors it may have received over SCM_RIGHTS cannot be \
                     recorded ({reason}), and a call through one would leave the turn order"
                );
                // Named on stderr before the policy ends the run, as a
                // backend's refusal is (`Detcore::report_backend_refusal`):
                // on ptrace and KVM the policy's shutdown ends the process
                // before the reason it files is printed
                // (https://github.com/rrnewton/hermit/issues/5047).
                let dettid = guest.thread_state().dettid;
                crate::syscalls::helpers::report_refusal_reason(
                    true,
                    &format!("[dtid {dettid}] {}: {refusal}", from.call),
                );
                // Only a fail-closed run asks: the receive has already run,
                // so the errno a permissive run would get is never given.
                self.refuse_unserviceable_operation_because(
                    guest,
                    from.call,
                    Errno::EOPNOTSUPP,
                    &refusal,
                )
                .await?;
            }
            warn!(
                "[detcore] the descriptors a receive got {channel}, if any, are not recorded: \
                 {reason}"
            );
            self.lose_lockstep_if_queued(guest, from, from.messages);
            return Ok(Vec::new());
        }
        if !in_turn {
            self.lose_lockstep_if_queued(guest, from, from.messages);
            if !delivered.fds.is_empty() {
                guest.thread_state().note_unaccounted_receipt();
                warn!(
                    "[detcore] descriptors {:?}, received {channel} on the background path, are \
                     not recorded: another task may have closed or replaced their numbers, or \
                     rewritten the control data, before this one resumed",
                    delivered.fds
                );
            }
            return Ok(Vec::new());
        }
        let mut recorded = Vec::new();
        let mut rest = delivered.fds.as_slice();
        for message in &delivered.messages {
            if message.count == 0 {
                self.note_message_without_descriptors(
                    guest,
                    from,
                    message.truncated,
                    message.bytes,
                );
                continue;
            }
            let (fds, after) = rest.split_at(message.count.min(rest.len()));
            rest = after;
            let mut facts = Vec::with_capacity(fds.len());
            for &fd in fds {
                match self.received_facts(guest, fd, channel).await {
                    Err(Error::Tool(error)) => return Err(Error::Tool(error)),
                    taken => facts.push(taken),
                }
            }
            let taken = Self::queued_rights(
                guest,
                from,
                &facts,
                message.truncated,
                self.keeps_recorded_decisions(),
                message.bytes,
            );
            let lockstep = taken.as_ref().is_some_and(|taken| taken.lockstep);
            let joinable = taken
                .as_ref()
                .is_some_and(|taken| taken.joinable || self.keeps_recorded_decisions());
            let (mut sent, forgotten) = taken
                .map(|taken| (taken.sent, taken.forgotten))
                .unwrap_or_default();
            // The messages before it, which calls Detcore could not match took,
            // are released with this receipt.
            guest.thread_state().defer_released_rights(forgotten);
            let mut joined = vec![false; sent.len()];
            for (index, (&fd, facts)) in fds.iter().zip(facts).enumerate() {
                let facts = match facts {
                    Ok(Some(facts)) => facts,
                    Ok(None) => continue,
                    Err(error) => {
                        // Open, but without its facts: listed, never modelled.
                        guest.thread_state().note_unmodelled_descriptor(fd, cloexec);
                        warn!(
                            "[detcore] descriptor {fd}, received {channel}, is not \
                             recorded: {error}"
                        );
                        continue;
                    }
                };
                let joins = sent.get(index).filter(|_| joinable);
                match self
                    .record_received_facts(
                        guest,
                        fd,
                        &facts,
                        TakenDescriptor {
                            sent: joins,
                            lockstep,
                            peek: from.peek,
                        },
                        channel,
                    )
                    .await
                {
                    Ok(Receipt::New) => recorded.push(fd),
                    Ok(Receipt::Joined) => joined[index] = true,
                    Ok(Receipt::Unrecorded) => {}
                    Err(Error::Tool(error)) => return Err(Error::Tool(error)),
                    Err(error) => {
                        // Open, but without its facts: listed, never modelled.
                        guest.thread_state().note_unmodelled_descriptor(fd, cloexec);
                        warn!(
                            "[detcore] descriptor {fd}, received {channel}, is not \
                             recorded: {error}"
                        )
                    }
                }
            }
            // What the message held and did not hand to this table goes with
            // it: the descriptors Linux could not install (`MSG_CTRUNC`) and
            // those this receipt recorded as open files of their own. A peek
            // took a copy of a message that stays queued: releasing the copy's
            // references releases nothing else.
            if !from.peek {
                guest.thread_state().defer_released_rights(
                    sent.drain(..)
                        .zip(joined)
                        .filter(|(_, joined)| !joined)
                        .map(|(sent, _)| sent),
                );
            }
        }
        Ok(recorded)
    }

    /// The kernel's facts about descriptor `fd`, which Linux has just
    /// installed through `SCM_RIGHTS`: `None` for a number that already has a
    /// record, which is kept as it is.
    async fn received_facts<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        channel: Channel,
    ) -> Result<Option<ReceivedFacts>, Error> {
        if fd < 0 {
            return Ok(None);
        }
        // Linux gave the receiver free numbers. A record of one already
        // describes something Detcore does not know to be gone: kept as it is.
        if guest.thread_state().with_detfd(fd, |_| ()).is_ok() {
            warn!(
                "[detcore] descriptor {fd}, received {channel}, already has a record, which is \
                 kept"
            );
            return Ok(None);
        }
        // Passive probes through the recording, retried when a signal stopped
        // them before they ran.
        let descriptor_flags = self
            .probe_received(
                guest,
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(syscalls::FcntlCmd::F_GETFD),
            )
            .await? as i32;
        let status_flags = self
            .probe_received(
                guest,
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(syscalls::FcntlCmd::F_GETFL),
            )
            .await? as i32;
        let stat = self.received_descriptor_fstat(guest, fd).await?;
        Ok(Some(ReceivedFacts {
            descriptor_flags,
            status_flags,
            stat,
        }))
    }

    /// The descriptors a send queued on the socket `from` names (see the
    /// module documentation) for the message whose received descriptors'
    /// facts are `facts` (`truncated` when Linux set `MSG_CTRUNC` for it), in
    /// the order sent, whether that queue is still in lockstep with the
    /// socket's messages, and whether the receipt may join them
    /// (`DetFd::take_queued_rights`, which takes a stream receive's `bytes`
    /// from a queue that holds every message): `None` when no queued message
    /// has their identities, or when one of them has no facts, which leaves
    /// the message, if queued, unmatched.
    fn queued_rights<G: Guest<Self>>(
        guest: &mut G,
        from: ReceivedFrom,
        facts: &[Result<Option<ReceivedFacts>, Error>],
        truncated: bool,
        legacy: bool,
        bytes: Option<usize>,
    ) -> Option<TakenRights> {
        let received = facts
            .iter()
            .map(|facts| match facts {
                Ok(Some(facts)) => Some(facts.file()),
                _ => None,
            })
            .collect::<Option<Vec<FileIdentity>>>();
        let Some(received) = received else {
            if !from.peek {
                let _ = guest.thread_state().with_detfd(from.socket, |socket| {
                    socket.lose_lockstep_if_queued(1, legacy)
                });
            }
            return None;
        };
        guest
            .thread_state()
            .with_detfd(from.socket, |socket| {
                socket.take_queued_rights(&received, truncated, from.peek, legacy, bytes)
            })
            .ok()
            .flatten()
    }

    /// A receive from `from` took `taken` messages Detcore could not match
    /// to the socket's queue (`DetFd::lose_lockstep_if_queued`); not a
    /// `MSG_PEEK` receive, which takes nothing.
    fn lose_lockstep_if_queued<G: Guest<Self>>(
        &self,
        guest: &mut G,
        from: ReceivedFrom,
        taken: usize,
    ) {
        if !from.peek {
            let legacy = self.keeps_recorded_decisions();
            let _ = guest.thread_state().with_detfd(from.socket, |socket| {
                socket.lose_lockstep_if_queued(taken, legacy)
            });
        }
    }

    /// A `read`, `readv`, `recvfrom`, `preadv2` at the current offset or
    /// `splice` through `fd` (`read`) returned `result`. On a socket with
    /// descriptors queued to it, it may have taken a message that carried
    /// some, which Linux then releases unseen ([`Self::take_or_suspect`]).
    ///
    /// Where the socket's queue holds every message, the call took the
    /// oldest queued message on a datagram or seqpacket socket, and on a
    /// stream socket the bytes it reports from the oldest messages on
    /// (`DetFd::take_stream_bytes`), and what it took is released.
    ///
    /// What takes a message is Linux's to say (Linux v6.17): a call with
    /// `MSG_PEEK` takes nothing; `read`, `readv` and `preadv2` of no bytes
    /// return at once (`sock_read_iter`, "Match SYS5 behaviour",
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/socket.c?h=v6.17#n1154>);
    /// a stream socket's read that returns 0 bytes found the stream ended,
    /// unless it asked for none, which only `recvfrom` passes on, and which
    /// takes the descriptors of the buffer at the head
    /// (`unix_stream_read_generic` detaches them before it looks at the
    /// length); a datagram or seqpacket read takes a message whatever the
    /// bytes it returns. One that fails with `EFAULT` took a message as
    /// [`Self::efault_may_have_taken`] says. Under record and replay the
    /// rule existing recordings were made with holds: the calls it noted
    /// (`ReceiveWithoutControl::noted_by_recordings`) lose lockstep whenever
    /// they return.
    pub(crate) fn note_receive_without_control<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        result: &Result<i64, Error>,
        read: ReceiveWithoutControl,
    ) {
        if !self.pairs_unix_sockets(guest) {
            return;
        }
        if self.keeps_recorded_decisions() {
            if read.noted_by_recordings && matches!(result, Ok(taken) if *taken >= 0) {
                let _ = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.lose_lockstep_if_queued(1, true));
            }
            return;
        }
        if read.peek {
            return;
        }
        // Only a socket whose type Detcore saw made is queued to.
        let Some(kind) = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.unix_type())
            .ok()
            .flatten()
        else {
            return;
        };
        let stream = kind == libc::SOCK_STREAM;
        let empty = || match read.request {
            Request::Bytes(_, length) | Request::Spliced(length) => length == 0,
            Request::Iovecs(at, count) => {
                iovecs_are_empty(&guest.memory(), Addr::from_raw(at), count)
            }
        };
        let took = match result {
            // Other tasks ran before this one resumed, and may have changed
            // the memory the call named: any return may have taken one.
            Ok(_) | Err(Error::Errno(Errno::EFAULT)) if !read.in_turn => true,
            Ok(0) if empty() => !read.returns_at_once_when_empty,
            Ok(0) => !stream,
            Ok(_) => true,
            Err(Error::Errno(Errno::EFAULT)) => {
                self.efault_may_have_taken(guest, stream, read.request, read.address_out)
            }
            Err(_) => false,
        };
        if !took {
            return;
        }
        if stream {
            // The bytes a stream read reports come from the oldest messages
            // on, whatever call took them (`DetFd::take_stream_bytes`). One
            // that failed copying the sender's address out after it took
            // them reports none, and had no control buffer to install
            // descriptors into ([`Self::take_faulted_stream_read`]).
            let released = match result {
                Ok(taken) if read.in_turn => guest
                    .thread_state()
                    .with_detfd(fd, |detfd| {
                        detfd.take_stream_bytes(*taken as usize, |_| true)
                    })
                    .ok()
                    .flatten(),
                Err(_) if read.in_turn => {
                    let buffers = receive_buffers(&guest.memory(), read.request);
                    buffers.and_then(|buffers| self.take_faulted_stream_read(guest, fd, &buffers))
                }
                _ => None,
            };
            match released {
                Some(released) => guest.thread_state().defer_released_rights(released),
                None => self.take_or_suspect(guest, fd, false, |_| true),
            }
            return;
        }
        // A datagram or seqpacket read takes one message, the oldest Linux
        // holds, and returns as many of its bytes as fit, or all of them
        // (`MSG_TRUNC`).
        let fits = match (read.request, result) {
            (Request::Bytes(_, asked), Ok(read)) => Some((asked, *read as usize)),
            (Request::Iovecs(at, count), Ok(read)) => {
                receive_buffers(&guest.memory(), Request::Iovecs(at, count)).map(|buffers| {
                    (
                        buffers.iter().map(|buffer| buffer.1).sum::<usize>(),
                        *read as usize,
                    )
                })
            }
            _ => None,
        };
        let one = read.takes == Takes::Counted && read.in_turn;
        self.take_or_suspect(guest, fd, one, move |head| match fits {
            Some((asked, read)) => read == asked.min(head.bytes) || read == head.bytes,
            None => true,
        });
    }

    /// A call through the socket `fd` took a message from it without naming
    /// its descriptors. Where Detcore knows which (`one`: the oldest Linux
    /// held, for a call completed in the caller's turn) and the socket's
    /// queue holds every message, that is the oldest queued one, which `fits`
    /// must agree with (`DetFd::take_head_message`): it goes, and what it
    /// carried is released when the call completes, as Linux releases it.
    /// Otherwise the call may have taken a message unseen
    /// (`DetFd::lose_lockstep_if_queued`), suspect until a later receipt.
    fn take_or_suspect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        one: bool,
        fits: impl FnOnce(&crate::fd::QueuedMessage) -> bool,
    ) {
        let mut fits = Some(fits);
        let taken = guest
            .thread_state()
            .with_detfd(fd, |detfd| {
                if one
                    && let Some(fits) = fits.take()
                    && let Some(taken) = detfd.take_head_message(fits)
                {
                    return Some(taken);
                }
                detfd.lose_lockstep_if_queued(1, false);
                None
            })
            .ok()
            .flatten();
        if let Some(taken) = taken {
            guest.thread_state().defer_released_rights(taken);
        }
    }

    /// Whether a receive through a socket (`stream` or not) that failed with
    /// `EFAULT` may have taken a message from it, by which copy failed
    /// (Linux v6.17). One that fails copying its inputs in, before it reads
    /// the socket, takes nothing ([`receive_buffers`]). A datagram or
    /// seqpacket receive dequeues its message before it copies the bytes and
    /// frees it when that copy fails (`__unix_dgram_recvmsg`,
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n2645>),
    /// so it took one either way. A stream receive fails only when it copied
    /// nothing, and then keeps the bytes, descriptors and all
    /// (`unix_stream_read_generic` breaks before `unix_detach_fds`,
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n3059>),
    /// unless the receive succeeded and only a copy out after it failed: the
    /// sender's address (`address_out`; `move_addr_to_user`,
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/socket.c?h=v6.17#n2281>).
    /// That one may have taken a message where the first byte it receives
    /// into is writable, so that its first copy can have succeeded (the
    /// guest's memory map says, [`Self::first_byte_writable`]).
    fn efault_may_have_taken<G: Guest<Self>>(
        &self,
        guest: &mut G,
        stream: bool,
        request: Request,
        address_out: bool,
    ) -> bool {
        let Some(buffers) = receive_buffers(&guest.memory(), request) else {
            return false;
        };
        !stream || (address_out && Self::first_byte_writable(guest, &buffers))
    }

    /// The descriptors a stream receive through `fd` that asked for the bytes
    /// `buffers` hold detached, where it took its bytes, then failed copying
    /// an output out (`EFAULT`), and had no room to install descriptors, so
    /// that Linux released them: no control buffer (`__scm_recv_common`,
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/core/scm.c?h=v6.17#n501>),
    /// or one too short for a descriptor (`scm_detach_fds`,
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/core/scm.c?h=v6.17#n354>).
    /// Linux does not report how many bytes it took. Where the queue holds
    /// every message and the receive could copy every byte it took (each lies
    /// in a writable mapping, [`Self::bytes_writable`]), that is the count
    /// `DetFd::stream_read_count` gives, the bytes go
    /// (`DetFd::take_stream_bytes`), and the descriptors they detached are
    /// returned for release. `None` otherwise, for the caller to count the
    /// receive as one that may have taken a message unseen: one whose copy of
    /// its bytes may have failed partway took fewer.
    fn take_faulted_stream_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        buffers: &[(usize, usize)],
    ) -> Option<Vec<SentDescriptor>> {
        let asked = buffers.iter().map(|buffer| buffer.1).sum::<usize>();
        let read = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.stream_read_count(asked))
            .ok()
            .flatten()?;
        if !Self::bytes_writable(guest, buffers, read) {
            return None;
        }
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.take_stream_bytes(read, |_| true))
            .ok()
            .flatten()
    }

    /// Whether the first `count` bytes `buffers` receive into all lie in
    /// writable mappings of the calling thread's address space, as its memory
    /// map reports (`Guest::memory_map`). A byte the map does not list, or a
    /// map that cannot be read, is not known to be writable.
    fn bytes_writable<G: Guest<Self>>(
        guest: &mut G,
        buffers: &[(usize, usize)],
        count: usize,
    ) -> bool {
        let mut left = count;
        let mut ranges = Vec::new();
        for &(at, length) in buffers {
            if left == 0 {
                break;
            }
            let part = length.min(left);
            left -= part;
            if part > 0 {
                ranges.push((at as u64, at as u64 + part as u64));
            }
        }
        if ranges.is_empty() {
            return true;
        }
        let Ok(map) = guest.memory_map(guest.tid()) else {
            return false;
        };
        ranges.into_iter().all(|(start, end)| {
            let mut at = start;
            while at < end {
                match map
                    .mappings
                    .iter()
                    .find(|mapping| mapping.start <= at && at < mapping.end)
                {
                    Some(mapping) if mapping.prot & libc::PROT_WRITE as u32 != 0 => {
                        at = mapping.end;
                    }
                    _ => return false,
                }
            }
            true
        })
    }

    /// Whether the first byte `buffers` receive into lies in a writable
    /// mapping of the calling thread's address space, as its memory map
    /// reports (`Guest::memory_map`); true for buffers of no bytes, which a
    /// receive copies nothing into. Where the map does not list the byte, a
    /// map that may omit mappings or cannot be read leaves the byte writable
    /// if it can be read, which counts a receive that may have taken a
    /// message as one that did.
    fn first_byte_writable<G: Guest<Self>>(guest: &mut G, buffers: &[(usize, usize)]) -> bool {
        let Some(&(at, _)) = buffers.iter().find(|buffer| buffer.1 > 0) else {
            return true;
        };
        let byte = at as u64;
        let map = guest.memory_map(guest.tid()).ok();
        if let Some(map) = &map {
            if let Some(mapping) = map
                .mappings
                .iter()
                .find(|mapping| mapping.start <= byte && byte < mapping.end)
            {
                return mapping.prot & libc::PROT_WRITE as u32 != 0;
            }
            if map.complete {
                return false;
            }
        }
        let mut value = [0u8];
        Addr::<u8>::from_raw(at).is_some_and(|at| guest.memory().read_exact(at, &mut value).is_ok())
    }

    /// A message a `recvmsg` or `recvmmsg` from `from` delivered without
    /// installing descriptors: one Linux truncated (`MSG_CTRUNC` with no room
    /// for one descriptor), which carried descriptors Linux released, or one
    /// of data alone. Where the queue holds every message, that is the oldest
    /// queued one on a datagram or seqpacket socket
    /// ([`Self::take_or_suspect`], `DetFd::take_data_only_message`), and on a
    /// stream socket the bytes it reports (`bytes`) come from the oldest
    /// messages on, the descriptors they detach, if any, released
    /// (`DetFd::take_stream_bytes`); otherwise a truncated message may have
    /// been any of them, and one of data alone took none.
    fn note_message_without_descriptors<G: Guest<Self>>(
        &self,
        guest: &mut G,
        from: ReceivedFrom,
        truncated: bool,
        bytes: Option<usize>,
    ) {
        if from.peek {
            return;
        }
        if self.keeps_recorded_decisions() {
            // The rule existing recordings were made with.
            if truncated {
                self.lose_lockstep_if_queued(guest, from, 1);
            }
            return;
        }
        let datagram = self.takes_whole_messages(guest, from.socket);
        if !datagram {
            let released = bytes.and_then(|read| {
                guest
                    .thread_state()
                    .with_detfd(from.socket, |detfd| {
                        detfd.take_stream_bytes(read, |detached| detached.is_some() == truncated)
                    })
                    .ok()
                    .flatten()
            });
            match released {
                Some(released) => guest.thread_state().defer_released_rights(released),
                None if truncated => self.lose_lockstep_if_queued(guest, from, 1),
                None => {}
            }
        } else if truncated {
            self.take_or_suspect(guest, from.socket, true, |head| {
                !head.descriptors.is_empty()
            });
        } else {
            let _ = guest
                .thread_state()
                .with_detfd(from.socket, |detfd| detfd.take_data_only_message());
        }
    }

    /// A `recvmsg` or `recvmmsg` from `from` (`call`) failed with `errno`.
    /// An `EFAULT` may have taken a message as
    /// [`Self::efault_may_have_taken`] says: none where Linux failed copying
    /// the header, the iovec array or a timeout in
    /// ([`message_receive_buffers`]); one from a datagram or seqpacket socket
    /// otherwise; and from a stream socket, one where the first byte it
    /// receives into is writable, so that the receive can have succeeded and
    /// failed only writing the header, name, lengths or flags back after it
    /// (`____sys_recvmsg`,
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/socket.c?h=v6.17#n2808>).
    /// A datagram or seqpacket message whose bytes could not be copied (its
    /// first byte is not writable, and it has some) was freed, descriptors and
    /// all, and so was every descriptor of one taken by a receive without room
    /// for one in its control buffer (`scm_detach_fds`): where the queue holds
    /// every message the oldest goes, released ([`Self::take_or_suspect`]).
    /// A stream receive without that room took the bytes and released the
    /// descriptors [`Self::take_faulted_stream_read`] finds, where it can.
    /// Any other is suspect (`SocketRights::suspects`): a receive that failed
    /// only writing its header back installed the descriptors it delivered,
    /// which no release may then let go. A receive that completed outside the
    /// caller's turn (`in_turn`, decided before it) is not classified: other
    /// tasks ran before it resumed and may have changed the memory it named,
    /// so it may have taken one.
    pub(crate) fn note_receive_error<G: Guest<Self>>(
        &self,
        guest: &mut G,
        from: ReceivedFrom,
        errno: Errno,
        call: &Syscall,
        in_turn: bool,
    ) {
        if errno != Errno::EFAULT
            || from.peek
            || self.keeps_recorded_decisions()
            || !self.pairs_unix_sockets(guest)
        {
            return;
        }
        let Some(kind) = guest
            .thread_state()
            .with_detfd(from.socket, |detfd| detfd.unix_type())
            .ok()
            .flatten()
        else {
            return;
        };
        let mut freed = false;
        let mut room = true;
        if in_turn {
            let Some((buffers, has_room)) = message_receive_buffers(&guest.memory(), call) else {
                return;
            };
            let writable = Self::first_byte_writable(guest, &buffers);
            if kind == libc::SOCK_STREAM && !writable {
                return;
            }
            if kind == libc::SOCK_STREAM
                && !has_room
                && let Some(released) = self.take_faulted_stream_read(guest, from.socket, &buffers)
            {
                guest.thread_state().defer_released_rights(released);
                return;
            }
            room = has_room;
            freed = kind != libc::SOCK_STREAM && (!writable || !room);
        }
        // With room for a descriptor, only a message with bytes to copy
        // faulted before its descriptors were installed.
        self.take_or_suspect(guest, from.socket, freed, |head| !room || head.bytes > 0);
    }

    /// A `recvmmsg` from `from` that asked for `asked` messages reported
    /// fewer. Linux stops a batch at the first message that fails, and when
    /// it already has some to report it keeps that error for the socket's
    /// next call (`do_recvmmsg`): a datagram or seqpacket message whose bytes
    /// could not be copied was taken and freed unseen, as for
    /// [`Self::note_receive_error`]. A batch also stops where Linux's queue
    /// ran dry, or at its timeout: so one message may be gone, suspect where
    /// some are queued. Where none is queued, nothing is lost, and a queue
    /// that holds every message still does.
    pub(crate) fn note_short_batch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        from: ReceivedFrom,
        asked: usize,
    ) {
        if from.messages < asked
            && !self.keeps_recorded_decisions()
            && self.takes_whole_messages(guest, from.socket)
        {
            let _ = guest.thread_state().with_detfd(from.socket, |detfd| {
                if detfd.has_queued_rights() {
                    detfd.lose_lockstep_if_queued(1, false);
                }
            });
        }
    }

    /// Whether the queue keeps the decisions existing recordings were made
    /// with: under record and replay (see the module documentation). There a
    /// queued message holds nothing (`DetFd::in_flight_open_file`); any read
    /// without a control buffer that returns loses lockstep, a peek and a
    /// read of no bytes too; a receipt joins the message it matches, a
    /// suspect one included (`SocketRights::suspects`); and an `EFAULT`
    /// receive, a short batch and a send to a gone peer change nothing,
    /// exactly as before.
    fn keeps_recorded_decisions(&self) -> bool {
        self.cfg.recordreplay_modes
    }

    /// Whether `socket` is a datagram or seqpacket socket this run pairs:
    /// one whose receive takes a whole message before it copies the bytes.
    fn takes_whole_messages<G: Guest<Self>>(&self, guest: &mut G, socket: RawFd) -> bool {
        self.pairs_unix_sockets(guest)
            && matches!(
                guest
                    .thread_state()
                    .with_detfd(socket, |detfd| detfd.unix_type())
                    .ok()
                    .flatten(),
                Some(libc::SOCK_DGRAM | libc::SOCK_SEQPACKET)
            )
    }

    /// A send, `call` (a `write`, `writev`, `sendto`, a `pwritev2` at the
    /// current offset, which writes as `writev` does, or a `splice` into the
    /// socket; `sendmsg` and `sendmmsg` note it themselves; Detcore refuses a
    /// `sendfile` through a socket, `Detcore::handle_sendfile`), failed with
    /// `ECONNREFUSED`
    /// ([`found_its_peer_gone`]). A datagram socket whose connected peer has
    /// gone purges its own receive queue then (`unix_dgram_sendmsg` calls
    /// `unix_dgram_disconnected` and fails with `ECONNREFUSED`,
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n2232>):
    /// so does its queue here, and what it held is released when the call
    /// completes; the pair is unpaired (`DetFd::unpair_rights_peer`). Only a
    /// send that names no destination: one to a name finds that socket gone
    /// and purges nothing.
    pub(crate) fn note_send_refused<G: Guest<Self>>(&self, guest: &mut G, call: &Syscall) {
        let (socket, addressed) = match call {
            Syscall::Write(write) => (write.fd(), false),
            Syscall::Writev(writev) => (writev.fd(), false),
            Syscall::Pwritev2(pwritev2)
                if super::files::vectored_offset(pwritev2.pos_l(), pwritev2.pos_h()) == -1 =>
            {
                (pwritev2.fd(), false)
            }
            Syscall::Sendto(sendto) => (
                sendto.fd(),
                sendto.addr().is_some() && sendto.addr_len() != 0,
            ),
            Syscall::Splice(splice) => (splice.fd_out(), false),
            _ => return,
        };
        self.note_peer_gone(guest, socket, addressed);
    }

    /// [`Self::note_send_refused`] for the send through `socket`, naming its
    /// destination (`addressed`) or not.
    pub(crate) fn note_peer_gone<G: Guest<Self>>(
        &self,
        guest: &mut G,
        socket: RawFd,
        addressed: bool,
    ) {
        if addressed || self.keeps_recorded_decisions() || !self.pairs_unix_sockets(guest) {
            return;
        }
        let purged = guest
            .thread_state()
            .with_detfd(socket, |detfd| {
                (detfd.unix_type() == Some(libc::SOCK_DGRAM)
                    && detfd.rights_peer_is_stream().is_some())
                .then(|| detfd.unpair_rights_peer())
            })
            .ok()
            .flatten();
        if let Some(purged) = purged {
            guest.thread_state().defer_released_rights(purged);
        }
    }

    /// Before a send through `socket` that passes no control buffer (a
    /// `write`, `writev`, `sendto`, or `pwritev2` at the current offset): the
    /// send to note after it ([`Self::note_send_without_control`]), where the
    /// peer's queue holds every message (`SocketRights::complete`), which
    /// must then hold the message of data alone the send adds. `in_turn`
    /// decides whether the send completes in the caller's turn
    /// ([`Self::completes_in_turn`]); `addressed` is whether it names a
    /// destination, which a datagram send reaches instead of its peer, and
    /// `out_of_band` whether it passes `MSG_OOB`.
    pub(crate) fn send_without_control<G: Guest<Self>>(
        &self,
        guest: &mut G,
        socket: RawFd,
        in_turn: impl FnOnce(&mut G) -> bool,
        addressed: bool,
        out_of_band: bool,
    ) -> Option<SendWithoutControl> {
        if self.keeps_recorded_decisions() || !self.pairs_unix_sockets(guest) {
            return None;
        }
        let (complete, kind) = guest
            .thread_state()
            .with_detfd(socket, |detfd| {
                (detfd.peer_queue_is_complete(), detfd.unix_type())
            })
            .ok()?;
        let stream = kind == Some(libc::SOCK_STREAM);
        complete.then(|| SendWithoutControl {
            socket,
            in_turn: in_turn(guest),
            unplaced: (addressed && kind == Some(libc::SOCK_DGRAM)) || (out_of_band && stream),
            stream,
        })
    }

    /// After the send `send` ([`Self::send_without_control`]) returned
    /// `result`: a send that succeeded queued a message, of the bytes it
    /// reports, on the peer, which the peer's queue holds in its place
    /// (`DetFd::queue_rights_to_peer`); a stream send of no bytes queued
    /// nothing. One whose place among the peer's other messages is unknown
    /// (it ran outside the caller's turn), that may have reached another
    /// socket (a datagram send that named its destination), or whose bytes a
    /// stream's reads do not count (`MSG_OOB`) leaves the peer's queue
    /// without every message (`DetFd::lose_peer_lockstep`).
    pub(crate) fn note_send_without_control<G: Guest<Self>>(
        &self,
        guest: &mut G,
        send: Option<SendWithoutControl>,
        result: &Result<i64, Error>,
    ) {
        let (Some(send), Ok(bytes)) = (send, result) else {
            return;
        };
        let Ok(bytes) = usize::try_from(*bytes) else {
            return;
        };
        let _ = guest.thread_state().with_detfd(send.socket, |detfd| {
            if !send.in_turn || send.unplaced {
                detfd.lose_peer_lockstep();
            } else if !(send.stream && bytes == 0) {
                let _ = detfd.queue_rights_to_peer(Vec::new(), bytes);
            }
        });
    }

    /// A `splice` into `socket` returned `result`: one that moved bytes sent
    /// them as messages whose number Detcore does not know (`splice_to_socket`
    /// sends a message per batch of the pipe's buffers). A stream's reads
    /// count only bytes, so its peer's queue holds them as one message of
    /// data alone; a datagram or seqpacket peer's queue no longer holds every
    /// message (`DetFd::lose_peer_lockstep`).
    pub(crate) fn note_spliced_send<G: Guest<Self>>(
        &self,
        guest: &mut G,
        socket: RawFd,
        result: &Result<i64, Error>,
    ) {
        if self.keeps_recorded_decisions() || !self.pairs_unix_sockets(guest) {
            return;
        }
        let Ok(moved) = result else {
            return;
        };
        let Ok(moved @ 1..) = usize::try_from(*moved) else {
            return;
        };
        let _ = guest.thread_state().with_detfd(socket, |detfd| {
            if !detfd.peer_queue_is_complete() {
                return;
            }
            if detfd.unix_type() == Some(libc::SOCK_STREAM) {
                let _ = detfd.queue_rights_to_peer(Vec::new(), moved);
            } else {
                detfd.lose_peer_lockstep();
            }
        });
    }

    /// Before a `sendmsg` or `sendmmsg` whose messages' headers are at the
    /// guest addresses `headers`, whatever it returns: a socket of a pair
    /// whose queues hold every message that the messages pass over
    /// `SCM_RIGHTS` may reach a table that holds it as an open file of its
    /// own, through which sends and receives go unseen, so neither queue of
    /// that pair holds every message any more
    /// (`DetFd::give_up_pair_completeness`). Where a header or control buffer
    /// cannot be read in full, every socket of this table may be among them.
    pub(crate) fn note_sockets_passed_out<G: Guest<Self>>(&self, guest: &mut G, headers: &[usize]) {
        if self.keeps_recorded_decisions() || !self.pairs_unix_sockets(guest) {
            return;
        }
        let (sent, unread) = super::io::sent_descriptors(guest, headers);
        if unread {
            guest.thread_state().give_up_every_pair_completeness();
            return;
        }
        for fd in sent {
            let _ = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.give_up_pair_completeness());
        }
    }

    /// Before a binder transaction, which can pass any descriptor of this
    /// table to a table that holds it as an open file of its own
    /// (`Channel::Binder`): no pair's queues hold every message any more
    /// (`DetFd::give_up_pair_completeness`).
    pub(crate) fn note_binder_transaction<G: Guest<Self>>(&self, guest: &mut G) {
        if !self.keeps_recorded_decisions() && self.pairs_unix_sockets(guest) {
            guest.thread_state().give_up_every_pair_completeness();
        }
    }

    /// A `bind` gave the `AF_UNIX` socket `fd` a name, of any form (a path,
    /// an abstract name, or one Linux chose, `unix_autobind`), or a
    /// `setsockopt` set `SO_PASSCRED` or `SO_PASSPIDFD` on it, which names it
    /// at its next send or connect (`unix_dgram_sendmsg`,
    /// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/net/unix/af_unix.c?h=v6.17#n2133>):
    /// other sockets can now send to it, unseen by its queue, which no
    /// longer holds every message (`DetFd::give_up_completeness`).
    pub(crate) fn note_unix_socket_reachable<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) {
        if self.pairs_unix_sockets(guest) {
            let _ = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.give_up_completeness());
        }
    }

    /// Whether the lock ledger follows copies in flight
    /// (`procfs::locks::LockLedger::placeholder_copies`) of the descriptions
    /// whose locks it follows: where it feeds the `/proc/locks` the guest
    /// reads (`Detcore::refresh_received_locks`).
    fn follows_lock_copies(&self) -> bool {
        follows_lock_copies(&self.cfg)
    }

    /// Release the queued descriptors that Linux released in the current
    /// call, which no receive will deliver (`ThreadState::take_released_rights`):
    /// the copies the lock ledger counted in flight are uncounted, and a
    /// description whose last reference such a descriptor held goes with it,
    /// with the messages queued to it in turn and, for a socket, the port
    /// Detcore reserved for it (`GlobalRequest::ReleasePort`), as Linux's
    /// last `fput` releases them. Each comes from a guest call, which holds
    /// them until it completes (`ThreadState::defer_released_rights`), and
    /// is released then, in its turn, before any observer, posthook or turn
    /// hand-off of that call (`Detcore::handle_syscall_event`): a receive
    /// that took this message without installing what it carried (a read
    /// with no control buffer, a truncation, a receipt recorded as an open
    /// file of its own) or took a later one after calls Detcore could not
    /// match, a purge, or the close of the receiving socket's last descriptor
    /// (`FileMetadata::release_detfd`). Each uncounts its copy in flight, and
    /// a description is released only when it counts none
    /// ([`InFlightOpenFile::release`]).
    pub(crate) async fn settle_released_rights<G: Guest<Self>>(&self, guest: &mut G) {
        let Settled { lock_copies, ports } = settle(guest.thread_state().take_released_rights());
        if !lock_copies.is_empty() && self.follows_lock_copies() {
            let _ = crate::tool_global::placeholder_copies(
                guest,
                crate::procfs::locks::PlaceholderCopies::Unsent(lock_copies),
            )
            .await;
        }
        for open_file in ports {
            self.release_port_for_open_file(guest, open_file).await;
        }
    }

    /// Name this thread's descriptor table to the global state
    /// (`GlobalRequest::HoldTable`), at the thread's start and after an exec
    /// gave it a new table, for the terminal receipt of a death by a fatal
    /// signal to let go of (`GlobalState::release_tables_at_death`): the death
    /// of a process by a fatal signal's default action (its own `SIGTERM`, an
    /// `abort()`, a `SIGSEGV`) is no guest call of its own, so no handler of
    /// the dying process runs at the point of the schedule where it ends.
    /// Only where those receipts exist, a run whose backend's process signal
    /// control is `ToolControlled` (KVM), whose tool runs in Detcore's own
    /// process, so that the request carries the table itself
    /// (`tool_global::HeldTable`), and only where tables go at their last
    /// holder's exit ([`releases_tables_at_exits`]).
    pub(crate) async fn name_table_for_death<G: Guest<Self>>(&self, guest: &mut G) {
        if !releases_tables_at_exits(&self.cfg)
            || guest.signal_control_mode() != reverie::BackendSignalControlMode::ToolControlled
        {
            return;
        }
        let table = Arc::clone(&guest.thread_state().file_metadata);
        crate::tool_global::hold_table(guest, table).await;
    }

    /// The calling thread is about to exit, with its whole process where
    /// `whole_process` (an `exit_group`, or a `SIGKILL` it sends its own
    /// process): the scheduler has granted the exit (`ResourceID::Exit`), and
    /// the native call that carries it out comes next, so this runs in the
    /// exiting thread's own turn, before any other task's. Where that exit
    /// leaves its descriptor table to no holder, as guest calls alone count
    /// them (`FileMetadata::holders`), the table goes now, as Linux releases
    /// it with its last holder's exit (`ThreadState::release_table_at_exit`),
    /// and what the queues of its sockets held is released with them
    /// (`Self::settle_released_rights`). A later close in another table is
    /// then a description's last reference whether or not the backend has
    /// torn any thread state down yet. Not before the grant: until then the
    /// process's other threads still run, and Linux still holds every
    /// descriptor of the table, so a call through one must not fail with
    /// `EBADF`, and another process must not see a lock gone that the host
    /// still holds (Claude's and Codex's narrow checks of K8-h reb7). The
    /// grant of a group exit logically ends the other threads, and no turn
    /// runs until the group has ended (`Scheduler::hold_turns_until_the_group_ends`).
    /// Not under record and replay, whose recordings were made with the table
    /// going at the teardown, and only where this run pairs `AF_UNIX`
    /// sockets.
    pub(crate) async fn release_table_at_exit<G: Guest<Self>>(
        &self,
        guest: &mut G,
        whole_process: bool,
    ) {
        if self.keeps_recorded_decisions() || !self.pairs_unix_sockets(guest) {
            return;
        }
        if guest.thread_state().release_table_at_exit(whole_process)
            && guest.thread_state().holds_released_rights()
        {
            Box::pin(self.settle_released_rights(guest)).await;
        }
    }

    /// An exec of `process` is about to replace its table, `replaced`, with
    /// `new`, which keeps `replaced` for the new image's first callback
    /// (`Self::release_exec_dropped_descriptors`): there `process` leaves
    /// `replaced`, as guest calls count its holders (`FileMetadata::holders`),
    /// and where threads of `process` alone held it, the exec drops its
    /// close-on-exec descriptors from it, and its descriptors go. A failed
    /// exec drops `new`, and with it the table it kept, so nothing changes.
    /// Not under record and replay, and only where this run pairs `AF_UNIX`
    /// sockets, as for the release of a table at an exit
    /// (`Self::release_table_at_exit`).
    pub(crate) fn keep_table_for_exec<G: Guest<Self>>(
        &self,
        guest: &G,
        replaced: &Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
        new: &mut crate::tool_local::FileMetadata,
        process: crate::types::DetPid,
    ) {
        if self.keeps_recorded_decisions() || !self.pairs_unix_sockets(guest) {
            return;
        }
        new.keep_replaced_table(Arc::clone(replaced), process);
    }

    /// The new image's first callback after an exec Detcore ran
    /// (`Self::handle_execveat`), still in the exec's turn: the descriptors of
    /// the table the exec replaced go now
    /// (`ThreadState::release_exec_dropped_descriptors`), the close-on-exec
    /// ones as Linux closes them at the exec (`do_close_on_exec`), and what
    /// the queues of their sockets held is released with them
    /// (`Self::settle_released_rights`): a receiving socket whose last
    /// descriptor was close-on-exec takes the copies in flight it held, and
    /// the lock ledger stops counting their locks.
    pub(crate) async fn release_exec_dropped_descriptors<G: Guest<Self>>(&self, guest: &mut G) {
        if guest.thread_state().release_exec_dropped_descriptors() {
            Box::pin(self.settle_released_rights(guest)).await;
        }
    }

    /// Record descriptor `fd`, which Linux has just installed through
    /// `SCM_RIGHTS` or a binder transaction (`channel`), from the kernel's
    /// facts about it (`facts`), and say how: as an alias of the open file
    /// description its sender's table modeled (`taken`), or as an open file
    /// of its own.
    async fn record_received_facts<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        facts: &ReceivedFacts,
        taken: TakenDescriptor<'_>,
        channel: Channel,
    ) -> Result<Receipt, Error> {
        let TakenDescriptor {
            sent,
            lockstep,
            peek,
        } = taken;
        // Exited senders' exit hooks may run meanwhile (docs/RACE_PROBES.md).
        #[cfg(debug_assertions)]
        crate::race_probe::delay_on_any_backend(
            crate::race_probe::RECEIPT_DECISION,
            self.cfg.backend.tool_shares_guest_descriptor_table,
        )
        .await;
        let cloexec = facts.descriptor_flags & libc::FD_CLOEXEC != 0;
        // Every anonymous inode has one file identity, so out of lockstep
        // only order could say which queued one this is: none is joined.
        let anonymous = facts.stat.st_mode & libc::S_IFMT == 0;
        // Linux installed the sender's `struct file`, so the received
        // descriptor shares its status flags, lock state, directory position
        // and scheduler resource, as `dup` would make it: an `O_NONBLOCK` set
        // through either shows through both, and a blocking read through the
        // received end of a pipe Detcore keeps physically nonblocking still
        // waits for the writer. Checked before the anonymous-inode refusal, so
        // a received eventfd is the sender's eventfd.
        if let Some(open_file) = sent
            .filter(|sent| sent.file == facts.file() && (lockstep || !anonymous))
            .and_then(|sent| sent.open_file.as_ref())
            && self
                .join_sent_open_file(guest, fd, cloexec, open_file)
                .await?
        {
            // This table holds the description now: the copy left flight.
            if !peek {
                open_file.land();
            }
            // The lock ledger follows this description's locks: this table
            // holds it now, as Linux's receiver does, and is noted with the
            // locks its fdinfo shows before the copy in flight that kept them
            // is uncounted, so they never lapse.
            if self.follows_lock_copies()
                && guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.is_lock_tracked())
                    .unwrap_or(false)
            {
                Box::pin(self.refresh_description_locks(
                    guest,
                    fd,
                    super::files::LockCall::Joined,
                    None,
                ))
                .await?;
            }
            // The copy is no longer in flight: it was received, so its
            // description may take a placeholder lock again
            // (`procfs::locks::LockLedger::placeholder_copies`). A peek leaves
            // a copy whose locks the ledger follows in flight, to be
            // uncounted by the receive that takes it, or its release.
            let peeked_lock_copy = peek && sent.is_some_and(|sent| sent.lock_copy.is_some());
            if !peeked_lock_copy
                && let Ok(open_file_id) = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.open_file_id())
            {
                let _ = crate::tool_global::placeholder_copies(
                    guest,
                    crate::procfs::locks::PlaceholderCopies::Received(open_file_id),
                )
                .await;
            }
            return Ok(Receipt::Joined);
        }
        let stat = &facts.stat;
        // An anonymous inode (an eventfd, signalfd, timerfd, epoll set,
        // inotify instance, pidfd, userfaultfd, ...) has no file type, and
        // `fstat` cannot tell which it is. Recorded as an ordinary file, its
        // reads and flag changes would take paths that are wrong for it (a
        // blocking signalfd read held in the caller's turn, an
        // `EFD_SEMAPHORE` eventfd read twice). Its link names an inotify
        // instance, which is recorded as one, so its reads renumber rename
        // cookies (`crate::syscalls::inotify`); any other is left unrecorded,
        // as before. Under record and replay the link is read through the
        // recording (`Self::received_descriptor_link`), so replay, whose slot
        // holds a stand-in eventfd, classifies the descriptor as the
        // recording did.
        let link = if stat.st_mode & libc::S_IFMT == 0 {
            self.received_descriptor_link(guest, fd).await?
        } else {
            None
        };
        if stat.st_mode & libc::S_IFMT == 0
            && link.as_deref() != Some(crate::syscalls::inotify::INOTIFY_LINK)
        {
            // Open, and listed in the table's descriptor directory, but not
            // modelled.
            guest.thread_state().note_unmodelled_descriptor(fd, cloexec);
            match &link {
                Some(link) => warn!(
                    "[detcore] descriptor {fd}, received {channel}, is an anonymous inode ({}) \
                     that an ordinary record would mishandle: not recorded",
                    String::from_utf8_lossy(link)
                ),
                None => warn!(
                    "[detcore] descriptor {fd}, received {channel}, is an anonymous inode whose \
                     link cannot be read, so its kind is unknown: not recorded"
                ),
            }
            return Ok(Receipt::Unrecorded);
        }
        // A rendered procfs file's placeholder, by the identity this tool
        // process registered or, where Detcore runs in each guest process, by
        // its name and seals (`Detcore::received_placeholder`): the file again,
        // never the empty memfd behind it.
        if may_be_placeholder(stat)
            && self.renders_procfs(guest)
            && let Some((path, named)) = self.received_placeholder(guest, fd, stat).await?
        {
            self.adopt_received_placeholder(guest, fd, path, (stat.st_dev, stat.st_ino), named)
                .await?;
            return Ok(Receipt::New);
        }
        // Any other received descriptor is a description of its own in the
        // model (see the module documentation), made exactly as the in-guest
        // discovery makes it: a directory gets a sorted stream of its own.
        guest.thread_state().add_discovered_fd(
            fd,
            facts.descriptor_flags,
            facts.status_flags,
            stat,
            link.as_deref(),
        )?;
        // A per-process file that follows its record's sequence which the
        // kernel opened, received without joining the open file its sender's
        // table modeled (above): where the tool runs in each guest process
        // (LiteInst, in-guest-trap), every such receipt; where one tool
        // process serves every guest, one Detcore could not match. That
        // sequence belongs to the open file, which every holder shares and
        // the sender's Detcore keeps, and which this table cannot know, so
        // every read and seek through it is refused by name
        // (`Detcore::refuse_unknown_record_sequence`), as the in-guest
        // discovery of DBT and SaBRe refuses one
        // (`FileMetadata::discover_fd_from_current_process`). Before, it read
        // the kernel's file, where the sender's captures had left it (end of
        // file), unsanitized. Its link is read through the recording, as
        // every received fact is.
        if stat.st_mode & libc::S_IFMT == libc::S_IFREG
            && stat.st_size == 0
            && stat.st_nlink > 0
            && let Some(link) = self.received_descriptor_link(guest, fd).await?
            && let Some(path) = crate::tool_local::kernel_record_file_link(stat, &link)
        {
            guest.thread_state().with_detfd(fd, |detfd| {
                detfd.set_path(path.clone());
                detfd.mark_record_sequence_unknown();
            })?;
        }
        // An inotify instance whose host description is nonblocking, in a run
        // where Detcore makes every inotify instance it creates physically
        // nonblocking (`Detcore::handle_inotify_init1`): received without the
        // open file description its sender modelled, its O_NONBLOCK may be
        // Detcore's own rather than the sender's, so the guest's blocking mode
        // is unknown until the guest sets it (`DetFd::blocking_mode_unknown`).
        if link.as_deref() == Some(crate::syscalls::inotify::INOTIFY_LINK)
            && facts.status_flags & libc::O_NONBLOCK != 0
            && self.cfg.use_nonblocking_sockets()
            && !self.cfg.recordreplay_modes
        {
            guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.mark_blocking_mode_unknown())?;
        }
        // A placeholder descriptor, which its identity and its tag tell apart
        // (`descriptor_directory`).
        self.recognize_received_placeholder(guest, fd, stat, facts.status_flags)
            .await;
        if stat.st_mode & libc::S_IFMT == libc::S_IFSOCK {
            self.classify_socket_peer(guest, fd, SocketRole::Received)
                .await;
        }
        Ok(Receipt::New)
    }

    /// Make received descriptor `fd` an alias of `open_file`, the description
    /// its sender's table modeled, as `dup` would, with `FD_CLOEXEC` as
    /// `cloexec` says; whether it did. Outside record and replay the queued
    /// message kept the description alive ([`InFlightOpenFile`]), so
    /// this does not depend on whether the sender still holds it. Under
    /// record and replay (legacy ownership) only where this
    /// table holds a descriptor of it (the lowest numbered, `other`): replay
    /// holds a stand-in in `fd` and first duplicates `other` over it, so that
    /// the slot names the live open file (`Detcore::install_live_alias`).
    async fn join_sent_open_file<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        cloexec: bool,
        open_file: &InFlightOpenFile,
    ) -> Result<bool, Error> {
        let Some(alias) = open_file.descriptor(fd, cloexec) else {
            return Ok(false);
        };
        if self.cfg.recordreplay_modes {
            let Some(other) = guest
                .thread_state()
                .lowest_slot_naming(alias.open_file_id())
            else {
                return Ok(false);
            };
            if self.cfg.replaying {
                let flags = if cloexec {
                    OFlag::O_CLOEXEC
                } else {
                    OFlag::empty()
                };
                self.install_live_alias(guest, other, fd, flags)
                    .await
                    .map_err(|errno| {
                        Error::Tool(anyhow::anyhow!(
                            "replay cannot make descriptor {fd} the alias of {other} the \
                             recording found: {errno}"
                        ))
                    })?;
            }
        }
        guest.thread_state().install_alias(alias);
        Ok(true)
    }

    /// `call`, a passive probe of a received descriptor, through the
    /// recording: recorded under record, served from the recording under
    /// replay (which holds a stand-in where the recording held the
    /// descriptor), injected otherwise. Retried while a signal stops it
    /// before it runs; every attempt is an event, so replay retries as the
    /// recording did.
    async fn probe_received<G, S>(&self, guest: &mut G, call: S) -> Result<i64, Errno>
    where
        G: Guest<Self>,
        S: Into<Syscall> + Copy,
    {
        const ATTEMPTS: usize = 64;
        let mut last = Errno::ERESTARTSYS;
        for _ in 0..ATTEMPTS {
            match self.record_or_replay(guest, call).await {
                Err(errno @ (Errno::EINTR | Errno::ERESTARTSYS)) => last = errno,
                answer => return answer,
            }
        }
        Err(last)
    }

    /// Whether a send through `socket` queues the open files its
    /// `SCM_RIGHTS` messages carry ([`Self::queue_sent_rights`]): only where
    /// this run records received descriptors, and only for a socket whose
    /// peer Detcore knows (see the module documentation). If it does,
    /// whether the pair is
    /// `SOCK_STREAM`.
    pub(crate) fn rights_peer_is_stream<G: Guest<Self>>(
        &self,
        guest: &mut G,
        socket: RawFd,
    ) -> Option<bool> {
        if self.receipt_mode(guest) != ReceiptMode::Recorded {
            return None;
        }
        guest
            .thread_state()
            .with_detfd(socket, |socket| socket.rights_peer_is_stream())
            .ok()
            .flatten()
    }

    /// After a send through `socket` that completed in the caller's turn
    /// ([`Self::completes_in_turn`]) and queues the open files it carries
    /// ([`Self::rights_peer_is_stream`] says whether the pair is a stream,
    /// `stream`), queue on the socket's peer, for each message Linux reports
    /// sending (`sent`: the descriptors its `SCM_RIGHTS` messages carried,
    /// and the bytes it sent), the open file description this table modeled
    /// for each descriptor and the file's identity from an injected `fstat`,
    /// so that the receipt can name the open file each received descriptor
    /// is (see the module documentation).
    ///
    /// A message of data alone is queued only where the peer's queue holds
    /// every message (`SocketRights::complete`), never under record and
    /// replay, whose recordings saw none. A stream send of out-of-band data
    /// (`out_of_band`, `MSG_OOB`) queues nothing and leaves the peer's queue
    /// out of lockstep. A stream socket's message of no
    /// bytes carried nothing: Linux attaches the descriptors to the first
    /// buffer it queues, and queues none. A message whose descriptors' facts
    /// cannot all be taken queues nothing, so its receipt names no open file,
    /// as before. A send that failed queued nothing in Linux and queues
    /// nothing here. A tool error (an `fstat` under record and replay that
    /// could not be recorded) ends the run.
    pub(crate) async fn queue_sent_rights<G: Guest<Self>>(
        &self,
        guest: &mut G,
        socket: RawFd,
        stream: bool,
        sent: SentRights,
        out_of_band: bool,
    ) -> Result<(), Error> {
        if sent.unread {
            self.lose_peer_lockstep(guest, socket);
        }
        // Out-of-band data is not among the bytes a stream's reads count, and
        // descriptors may ride its byte (`queue_oob`): the queue keeps no
        // place for such a send. Recordings made before queued as usual.
        if out_of_band && stream && !self.keeps_recorded_decisions() {
            self.lose_peer_lockstep(guest, socket);
            return Ok(());
        }
        // A datagram send that names its destination reaches the socket it
        // names, not the peer (`unix_dgram_sendmsg`); a seqpacket send
        // ignores a name (`unix_seqpacket_sendmsg`) and a stream send with
        // one fails (`unix_stream_sendmsg`).
        let datagram = guest
            .thread_state()
            .with_detfd(socket, |socket| socket.unix_type())
            .ok()
            .flatten()
            == Some(libc::SOCK_DGRAM);
        // Held as Linux holds them, except under record and replay, where the
        // ownership existing recordings were made with stays
        // (`DetFd::in_flight_open_file`).
        let held = !self.cfg.recordreplay_modes;
        let follows_locks = self.follows_lock_copies();
        for (fds, bytes, addressed) in sent.messages {
            if fds.is_empty() && self.keeps_recorded_decisions() {
                continue;
            }
            if stream && bytes == 0 {
                continue;
            }
            if datagram && addressed {
                // The named socket may be the peer itself, which then holds a
                // message its queue does not.
                self.lose_peer_lockstep(guest, socket);
                continue;
            }
            if fds.is_empty() {
                let _ = guest.thread_state().with_detfd(socket, |socket| {
                    let _ = socket.queue_rights_to_peer(Vec::new(), bytes);
                });
                continue;
            }
            let mut descriptors = Vec::with_capacity(fds.len());
            for &fd in &fds {
                // Through the recording, as the receipt's facts are, so that
                // replay matches what the recording matched.
                let stat = match self.received_descriptor_fstat(guest, fd).await {
                    Ok(stat) => stat,
                    Err(Error::Tool(error)) => return Err(Error::Tool(error)),
                    Err(_) => break,
                };
                let (open_file, lock_copy) = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| {
                        // A placeholder's copies are counted at the send
                        // (`Detcore::inspect_placeholder_send`).
                        let lock_copy = (follows_locks
                            && detfd.is_lock_tracked()
                            && detfd.placeholder().is_none())
                        .then(|| detfd.open_file_id());
                        (detfd.in_flight_open_file(held), lock_copy)
                    })
                    .ok()
                    .unzip();
                descriptors.push(SentDescriptor {
                    file: (stat.st_dev, stat.st_ino),
                    open_file,
                    lock_copy: lock_copy.flatten(),
                });
            }
            if descriptors.len() != fds.len() {
                self.lose_peer_lockstep(guest, socket);
                continue;
            }
            let copies: Vec<_> = descriptors
                .iter()
                .filter_map(|sent| sent.lock_copy)
                .collect();
            let mut descriptors = Some(descriptors);
            let queued = guest
                .thread_state()
                .with_detfd(socket, |socket| {
                    socket.queue_rights_to_peer(descriptors.take().unwrap_or_default(), bytes)
                })
                .ok();
            // Not queued, nothing was counted, and the sender's descriptors
            // still hold what the message names.
            if matches!(queued, Some(Ok(()))) && !copies.is_empty() {
                // The sender's descriptors still hold the descriptions, so
                // their locks were never in danger before this count.
                let _ = Box::pin(crate::tool_global::placeholder_copies(
                    guest,
                    crate::procfs::locks::PlaceholderCopies::Sending(copies),
                ))
                .await;
            }
        }
        Ok(())
    }

    /// A send through `socket` may have passed descriptors its peer's queue
    /// does not hold: a message whose control data could not be read, one
    /// whose descriptors' facts could not be taken, or a send outside the
    /// caller's turn (`DetFd::lose_peer_lockstep_with_rights`).
    pub(crate) fn lose_peer_lockstep<G: Guest<Self>>(&self, guest: &mut G, socket: RawFd) {
        let _ = guest
            .thread_state()
            .with_detfd(socket, |socket| socket.lose_peer_lockstep_with_rights());
    }

    /// Whether this run pairs `AF_UNIX` connections, so that their sends
    /// queue the open files they carry: where it records received
    /// descriptors.
    fn pairs_unix_sockets<G: Guest<Self>>(&self, guest: &G) -> bool {
        self.receipt_mode(guest) == ReceiptMode::Recorded
    }

    /// `socketpair` made `fds`, of `family` and `kind` (its type, flags
    /// included): each socket's messages reach the other, so a receive
    /// through one can name the open files a send through the other carried
    /// (see the module documentation).
    pub(crate) fn note_socketpair<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fds: [RawFd; 2],
        family: i32,
        kind: i32,
    ) {
        // Linux's SOCK_TYPE_MASK: the type without SOCK_NONBLOCK and SOCK_CLOEXEC.
        const SOCK_TYPE_MASK: i32 = 0xf;
        if family != libc::AF_UNIX || !self.pairs_unix_sockets(guest) {
            return;
        }
        let kind = kind & SOCK_TYPE_MASK;
        // Outside record and replay, Detcore notes every send of data alone,
        // so the queues of a pair, which nothing else reaches, hold every
        // message (`SocketRights::complete`).
        let complete = !self.keeps_recorded_decisions();
        if let Ok(first) = guest
            .thread_state()
            .with_detfd(fds[0], |first| first.clone())
        {
            let _ = guest.thread_state().with_detfd(fds[1], |second| {
                first.set_unix_type(kind);
                second.set_unix_type(kind);
                first.pair_sockets(second, kind == libc::SOCK_STREAM);
                if complete {
                    first.mark_complete_pair(second);
                }
            });
        }
    }

    /// `socket` made `fd`, of `family` and `kind` (its type, flags
    /// included): record an `AF_UNIX` socket's type.
    pub(crate) fn note_unix_socket<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        family: i32,
        kind: i32,
    ) {
        // Linux's SOCK_TYPE_MASK: the type without SOCK_NONBLOCK and SOCK_CLOEXEC.
        const SOCK_TYPE_MASK: i32 = 0xf;
        if family == libc::AF_UNIX && self.pairs_unix_sockets(guest) {
            let _ = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.set_unix_type(kind & SOCK_TYPE_MASK));
        }
    }

    /// A `bind` gave socket `fd` the `AF_UNIX` name `name`
    /// ([`unix_socket_name`]): a listener there starts with no connection
    /// waiting.
    pub(crate) fn note_unix_bind<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd, name: &[u8]) {
        // An absolute path names another file from a task whose root may not
        // be the container's: such a listener is not tracked.
        if self.pairs_unix_sockets(guest) && !Self::path_names_another_file(guest, name) {
            let Ok(listener) = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.open_file_handle())
            else {
                return;
            };
            let generation =
                with_connections(guest, |connections| connections.bound(name, listener));
            let _ = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.set_unix_name(name.into(), generation));
        }
    }

    /// The socket the last guest bind of `address`, the address a `connect`
    /// just used, bound: its listener. `None` for any address Detcore cannot
    /// name, one no guest bound here, or an absolute path the caller may
    /// resolve against another root.
    pub(crate) fn listener_named<G: Guest<Self>>(
        &self,
        guest: &G,
        address: &UnixAddress,
    ) -> Option<OpenFileHandle> {
        let UnixAddress::Named(name) = address else {
            return None;
        };
        if !self.pairs_unix_sockets(guest) || Self::path_names_another_file(guest, name) {
            return None;
        }
        with_connections(guest, |connections| connections.listener(name).cloned())
    }

    /// A `connect` of socket `fd` to `address` succeeded, in the caller's
    /// turn or not (`in_turn`, decided before the call,
    /// [`Self::completes_in_turn`]). `address` is read after the call: in the
    /// turn, it is what the attempt that connected read.
    ///
    /// A datagram socket now sends to the socket it connected to, or to
    /// none (`AF_UNSPEC`), not to the peer Detcore paired it with
    /// (`DetFd::unpair_rights_peer`). A stream or seqpacket connection to a
    /// name a guest bound queues for that listener's `accept`; only one
    /// completed in the turn has a known place among the others. One
    /// completed outside the turn read its address at a host-chosen moment,
    /// after other threads may have rewritten it, so it may have reached any
    /// listener, and every name's order is unknown (Codex review of
    /// https://github.com/rrnewton/hermit/pull/4862). Any other `AF_UNIX`
    /// connect may have reached any listener: through a relative path or
    /// another spelling of a path, or from a socket whose type Detcore did
    /// not see made ([`UnixConnections`]).
    pub(crate) fn note_unix_connect<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        address: &UnixAddress,
        in_turn: bool,
    ) {
        if !self.pairs_unix_sockets(guest) {
            return;
        }
        let kind = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.unix_type())
            .ok()
            .flatten();
        if kind == Some(libc::SOCK_DGRAM) {
            let purged = guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.unpair_rights_peer())
                .unwrap_or_default();
            guest.thread_state().defer_released_rights(purged);
            return;
        }
        if !in_turn {
            with_connections(guest, UnixConnections::lost);
            return;
        }
        let address = match address {
            UnixAddress::Named(name) if Self::path_names_another_file(guest, name) => {
                &UnixAddress::Unnamed
            }
            address => address,
        };
        match (address, kind) {
            (UnixAddress::Other, _) => {}
            (UnixAddress::Named(name), Some(libc::SOCK_STREAM | libc::SOCK_SEQPACKET)) => {
                let socket = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.open_file_handle())
                    .ok();
                with_connections(guest, |connections| connections.connected(name, socket));
            }
            // A named connect of a socket whose type Detcore did not see made
            // may be a stream connection: that name's order, or every name's
            // where no guest bound it here, is unknown.
            (UnixAddress::Named(name), None) => {
                with_connections(guest, |connections| connections.connected(name, None));
            }
            (UnixAddress::Named(_), Some(_)) => {}
            (UnixAddress::Unnamed, _) => with_connections(guest, UnixConnections::lost),
        }
    }

    /// Whether `name`, an `AF_UNIX` name ([`UnixAddress::Named`]), is an
    /// absolute path the calling task may resolve against another root
    /// than the container's: one it, or a task it shares or inherited its
    /// root from, changed with `chroot` (`ThreadState::changed_root`). An
    /// abstract name names the same socket from any root.
    fn path_names_another_file<G: Guest<Self>>(guest: &G, name: &[u8]) -> bool {
        name.first() == Some(&b'/')
            && guest
                .thread_state()
                .changed_root
                .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// An `accept` on `listener` made the connection `fd`, in the caller's
    /// turn or not (`in_turn`): it is a socket of the listener's type, and
    /// the peer of the socket whose `connect` it took, where Detcore knows
    /// which ([`UnixConnections`]). Returns that socket, if known.
    pub(crate) fn note_unix_accept<G: Guest<Self>>(
        &self,
        guest: &mut G,
        listener: RawFd,
        fd: RawFd,
        in_turn: bool,
    ) -> Option<OpenFileHandle> {
        if !self.pairs_unix_sockets(guest) {
            return None;
        }
        let Ok(Some((name, generation, kind))) = guest
            .thread_state()
            .with_detfd(listener, |detfd| detfd.unix_name_and_type())
        else {
            return None;
        };
        let peer = with_connections(guest, |connections| {
            connections.accepted(&name, generation, in_turn)
        });
        let _ = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_unix_type(kind);
            if let Some(peer) = &peer {
                detfd.pair_with(peer, kind == libc::SOCK_STREAM);
            }
        });
        peer
    }

    /// What descriptor `fd` of the calling thread links to, or `None` where
    /// that read fails or the link is longer than any anonymous inode's.
    ///
    /// Under record and replay the link is read through the recording
    /// (`Detcore::recorded_descriptor_link`): replay's slot holds a stand-in
    /// eventfd until the recording's answers say otherwise, so a live read
    /// there would name the stand-in. A buffer that cannot be prepared ends
    /// the run there, as for the received descriptor's `fstat`: no readlink
    /// ran, so the other phase could consume an event this one did not.
    ///
    /// Otherwise it is read in the guest (`readlinkat` of
    /// `/proc/thread-self/fd/<fd>`, injected). The path and the result share
    /// one buffer, staged where `inject_fstat` stages its own
    /// ([`crate::syscalls::scratch::inject_with_scratch`]).
    async fn received_descriptor_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<Vec<u8>>, Error> {
        if self.cfg.recordreplay_modes {
            return self
                .recorded_descriptor_link(guest, fd)
                .await
                .map_err(|errno| {
                    Error::Tool(anyhow::anyhow!(
                        "cannot record descriptor {fd}, received over SCM_RIGHTS, under record \
                         and replay: no buffer for its link ({errno})"
                    ))
                });
        }
        let mut buffer = [0_u8; LINK_PATH + LINK_TARGET];
        let spelled = format!("/proc/thread-self/fd/{fd}");
        let Some(slot) = buffer.get_mut(..spelled.len()) else {
            return Ok(None);
        };
        slot.copy_from_slice(spelled.as_bytes());
        let Ok((length, bytes)) =
            crate::syscalls::scratch::inject_with_scratch(guest, buffer, |base| {
                // SAFETY: the buffer holds LINK_PATH + LINK_TARGET bytes.
                let target = unsafe { base.add(LINK_PATH) };
                syscalls::Readlinkat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(base.as_raw() as *const libc::c_char))
                    .with_buf(Some(target.cast::<libc::c_char>()))
                    .with_buf_len(LINK_TARGET)
                    .into()
            })
            .await
        else {
            return Ok(None);
        };
        Ok(usize::try_from(length)
            .ok()
            .filter(|length| *length < LINK_TARGET)
            .map(|length| bytes[LINK_PATH..LINK_PATH + length].to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use reverie::syscalls::AddrMut;
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::Timespec;

    use super::*;

    /// One control message's bytes, padded as `CMSG_SPACE` pads it.
    fn message(level: i32, kind: i32, fds: &[RawFd]) -> Vec<u8> {
        let length = size_of::<libc::cmsghdr>() + size_of_val(fds);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&length.to_ne_bytes());
        bytes.extend_from_slice(&level.to_ne_bytes());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        fds.iter()
            .for_each(|fd| bytes.extend_from_slice(&fd.to_ne_bytes()));
        bytes.resize(length.next_multiple_of(size_of::<usize>()), 0);
        bytes
    }

    #[test]
    fn rights_are_found_in_every_control_message() {
        let mut control = message(libc::SOL_SOCKET, libc::SCM_CREDENTIALS, &[1, 2, 3]);
        control.extend(message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[5]));
        control.extend(message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[6, 7]));
        assert_eq!(received_descriptors(&control), vec![5, 6, 7]);
        // A truncated message carries nothing.
        assert_eq!(received_descriptors(&control[..control.len() - 8]), vec![5]);
        let mut zero_length = control.clone();
        zero_length[..8].copy_from_slice(&0usize.to_ne_bytes());
        assert_eq!(received_descriptors(&zero_length), Vec::<RawFd>::new());
        assert_eq!(received_descriptors(&[]), Vec::<RawFd>::new());
    }

    #[test]
    fn rights_control_length_is_not_truncated_to_wrapper_width() {
        let mut control = [0u8; 24];
        control[..8].copy_from_slice(&((1usize << 32) + 20).to_ne_bytes());
        control[8..12].copy_from_slice(&libc::SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&libc::SCM_RIGHTS.to_ne_bytes());
        control[16..20].copy_from_slice(&42i32.to_ne_bytes());
        assert!(received_descriptors(&control).is_empty());
        // The identical low-width valid message must still yield its FD.
        control[..8].copy_from_slice(&20usize.to_ne_bytes());
        assert_eq!(received_descriptors(&control), vec![42]);
        control[..8].copy_from_slice(&usize::MAX.to_ne_bytes());
        assert!(received_descriptors(&control).is_empty());
    }

    /// A `msghdr` whose control buffer is `control`, with `msg_controllen`
    /// set to `delivered`, as Linux leaves it after a receive.
    fn header(control: &mut [u8], delivered: usize) -> Msghdr {
        // SAFETY: all-zero is a valid `msghdr`.
        let mut header: Msghdr = unsafe { std::mem::zeroed() };
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = delivered;
        header
    }

    /// Only the bytes Linux reports writing are read: a reused buffer's
    /// earlier `SCM_RIGHTS` message past `msg_controllen` names nothing, as
    /// on a receive that delivered no descriptor.
    #[test]
    fn only_the_delivered_control_bytes_name_descriptors() {
        let mut control = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[9, 10]);
        let length = control.len();
        let memory = LocalMemory::new();
        assert_eq!(
            delivered_through(&memory, &header(&mut control, length)),
            vec![9, 10]
        );
        assert!(delivered_through(&memory, &header(&mut control, 0)).is_empty());
        // A null control buffer names nothing whatever its length says.
        let mut empty = header(&mut control, length);
        empty.msg_control = std::ptr::null_mut();
        assert!(delivered_through(&memory, &empty).is_empty());
    }

    /// MSG_CTRUNC: Linux writes one message holding only the descriptors it
    /// installed (`cmsg_len` is `CMSG_LEN` of those) and reports the bytes
    /// it consumed, at most the buffer's length, so a buffer with room for
    /// one of three descriptors names exactly that one.
    #[test]
    fn a_truncated_receive_names_only_the_installed_descriptor() {
        // CMSG_LEN(sizeof(int)): room for one descriptor, no padding.
        let room = size_of::<libc::cmsghdr>() + size_of::<RawFd>();
        let mut control = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[12]);
        control.truncate(room);
        let memory = LocalMemory::new();
        assert_eq!(
            delivered_through(&memory, &header(&mut control, room)),
            vec![12]
        );
        // Room for none: Linux writes no message and reports 0 bytes.
        let mut nothing = vec![0u8; size_of::<libc::cmsghdr>()];
        assert!(delivered_through(&memory, &header(&mut nothing, 0)).is_empty());
    }

    /// `recvmsg` reads the one header after the call; `recvmmsg` the headers
    /// of the messages it reports, not the vector's later entries; other
    /// calls and failures name none.
    #[test]
    fn receives_name_the_descriptors_of_the_messages_they_delivered() {
        let memory = LocalMemory::new();
        let mut first = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[3]);
        let mut second = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[4, 5]);
        let mut header_one = header(&mut first, 0);
        header_one.msg_controllen = first.len();
        let recvmsg = Syscall::Recvmsg(
            syscalls::Recvmsg::new()
                .with_sockfd(7)
                .with_msg(AddrMut::from_ptr(&raw mut header_one)),
        );
        assert_eq!(delivered_descriptors(&memory, &recvmsg, 1), Some(vec![3]));
        assert_eq!(delivered_descriptors(&memory, &recvmsg, -1), None);

        // SAFETY: all-zero is a valid `mmsghdr`.
        let mut vector: [Mmsghdr; 2] = unsafe { std::mem::zeroed() };
        vector[0].msg_hdr = header(&mut first, 0);
        vector[0].msg_hdr.msg_controllen = first.len();
        vector[1].msg_hdr = header(&mut second, 0);
        vector[1].msg_hdr.msg_controllen = second.len();
        let recvmmsg = Syscall::Recvmmsg(
            syscalls::Recvmmsg::new()
                .with_fd(7)
                .with_mmsg(AddrMut::from_ptr(vector.as_mut_ptr().cast()))
                .with_vlen(2),
        );
        assert_eq!(
            delivered_descriptors(&memory, &recvmmsg, 2),
            Some(vec![3, 4, 5])
        );
        assert_eq!(delivered_descriptors(&memory, &recvmmsg, 1), Some(vec![3]));
        assert_eq!(delivered_descriptors(&memory, &recvmmsg, 0), Some(vec![]));
        let read = Syscall::Read(syscalls::Read::new().with_fd(7));
        assert_eq!(delivered_descriptors(&memory, &read, 1), None);
    }

    /// A `recvmsg` whose header names `control`.
    fn recvmsg_of(header: &mut Msghdr) -> Syscall {
        Syscall::Recvmsg(
            syscalls::Recvmsg::new()
                .with_sockfd(7)
                .with_msg(AddrMut::from_ptr(header as *mut Msghdr)),
        )
    }

    /// The control buffer is the one named before the call: a header whose
    /// `msg_control` the receive overwrote still names the descriptors at
    /// the buffer Linux wrote them to, and only the bytes it reports.
    #[test]
    fn the_control_buffer_named_before_the_call_is_read() {
        let memory = LocalMemory::new();
        let mut control = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[8, 9]);
        let length = control.len();
        // On the heap, written below only through this pointer, as the
        // kernel and a sibling thread write the guest's header.
        let header = Box::into_raw(Box::new(header(&mut control, length)));
        // SAFETY: `header` is a live allocation this test owns.
        let call = recvmsg_of(unsafe { &mut *header });
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        let delivered = geometry.delivered(&memory, 1);
        assert_eq!(delivered.fds, vec![8, 9]);
        assert_eq!(delivered.refused, None);
        assert_eq!(geometry.delivered(&memory, -1), Delivered::default());
        // Linux reports 0 control bytes and no MSG_CTRUNC: a reused buffer
        // names nothing, and the message carried no descriptor.
        // SAFETY: as above.
        unsafe { (*header).msg_controllen = 0 };
        assert_eq!(
            geometry.delivered(&memory, 1),
            Delivered {
                fds: Vec::new(),
                messages: vec![DeliveredMessage {
                    count: 0,
                    truncated: false,
                    bytes: Some(1),
                }],
                refused: None,
                may_have_installed: false,
            }
        );
        // More bytes than the buffer named before the call held: the
        // capacity changed before the attempt that succeeded.
        // SAFETY: as above.
        unsafe { (*header).msg_controllen = length + 8 };
        assert!(geometry.delivered(&memory, 1).refused.is_some());
        // The pointer changed before the attempt that succeeded (a sibling
        // during an earlier attempt's wait): nothing is named.
        // SAFETY: as above.
        unsafe {
            (*header).msg_controllen = length;
            (*header).msg_control = std::ptr::dangling_mut();
        }
        let delivered = geometry.delivered(&memory, 1);
        assert!(delivered.fds.is_empty() && delivered.refused.is_some());
        // SAFETY: made by `Box::into_raw` above, and no longer read.
        drop(unsafe { Box::from_raw(header) });
    }

    /// What Linux checks before a receive reads the socket (Linux v6.17): a
    /// buffer in user space (`import_ubuf`), an iovec array it can read whose
    /// iovecs are in user space (`__import_iovec`), and a readable header
    /// for `recvmsg`; and whether that header's control buffer has room for
    /// one descriptor. Each check that fails means the call took nothing.
    #[test]
    fn a_receive_takes_nothing_where_its_inputs_cannot_be_copied_in() {
        let memory = LocalMemory::new();
        let mut byte = [0u8; 1];
        let at = byte.as_mut_ptr() as usize;
        assert_eq!(
            receive_buffers(&memory, Request::Bytes(at, 1)),
            Some(vec![(at, 1)])
        );
        assert_eq!(
            receive_buffers(&memory, Request::Bytes(0xffff_8880_0000_0000, 1)),
            None,
            "a kernel-range buffer"
        );
        assert_eq!(
            receive_buffers(&memory, Request::Bytes(USER_SPACE_END - 1, 2)),
            None,
            "a buffer that runs past user space"
        );
        assert_eq!(
            receive_buffers(&memory, Request::Bytes(8, 1)),
            Some(vec![(8, 1)]),
            "an unmapped user buffer passes the check: its copy faults later"
        );
        let iov = [libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        }];
        assert_eq!(
            receive_buffers(&memory, Request::Iovecs(iov.as_ptr() as usize, 1)),
            Some(vec![(at, 1)])
        );
        assert_eq!(
            receive_buffers(&memory, Request::Iovecs(8, 1)),
            None,
            "an iovec array that cannot be read"
        );
        assert_eq!(
            receive_buffers(&memory, Request::Iovecs(8, 0)),
            Some(Vec::new())
        );

        let mut control = [0u8; 32];
        let room = control.len();
        let mut header = header(&mut control, room);
        header.msg_iov = iov.as_ptr().cast_mut();
        header.msg_iovlen = 1;
        assert_eq!(
            message_receive_buffers(&memory, &recvmsg_of(&mut header)),
            Some((vec![(at, 1)], true))
        );
        header.msg_controllen = 4;
        assert_eq!(
            message_receive_buffers(&memory, &recvmsg_of(&mut header)),
            Some((vec![(at, 1)], false)),
            "no room for one descriptor"
        );
        let unreadable = Syscall::Recvmsg(
            syscalls::Recvmsg::new()
                .with_sockfd(7)
                .with_msg(AddrMut::from_raw(8)),
        );
        assert_eq!(message_receive_buffers(&memory, &unreadable), None);
    }

    /// Outputs against inputs: only a range written back into itself is
    /// exempt, counted over sorted interval ends.
    #[test]
    fn an_output_may_overlap_only_its_own_input() {
        let header = (1000, 56);
        let control = (2000, 24);
        assert!(!outputs_overlap_inputs(
            &[header, control],
            &[(header, true), (control, true), ((3000, 16), false)]
        ));
        // A name buffer over the control buffer, or over a header.
        assert!(outputs_overlap_inputs(
            &[header, control],
            &[((2010, 4), false)]
        ));
        assert!(outputs_overlap_inputs(
            &[header, control],
            &[((1040, 8), false)]
        ));
        // A control buffer that is also another message's.
        assert!(outputs_overlap_inputs(
            &[control, control],
            &[(control, true)]
        ));
        // Touching ends do not overlap; empty ranges never do.
        assert!(!outputs_overlap_inputs(
            &[header],
            &[((1056, 8), false), ((1000, 0), false)]
        ));
    }

    /// A refused receive may have installed descriptors only where a
    /// message names a control buffer, or where the geometry cannot say
    /// which header Linux used: a batch whose outputs overlap its inputs,
    /// since Linux reads each later entry's header only when it reaches it.
    /// A payload over the header's own msg_name field, with the control
    /// buffer elsewhere, may have; without a control buffer it cannot have.
    #[test]
    fn a_refused_receive_may_have_installed_descriptors_only_where_it_names_a_control_buffer() {
        let memory = LocalMemory::new();
        let mut control = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[4]);
        let length = control.len();
        for names_control in [true, false] {
            // On the heap, so that the iovec can name its own header's
            // msg_name field.
            let header = Box::into_raw(Box::new(header(
                &mut control,
                if names_control { length } else { 0 },
            )));
            let iov = [libc::iovec {
                // SAFETY: `header` is a live allocation this test owns.
                iov_base: unsafe { (&raw mut (*header).msg_name).cast() },
                iov_len: size_of::<usize>(),
            }];
            // SAFETY: as above.
            unsafe {
                (*header).msg_iov = iov.as_ptr().cast_mut().cast();
                (*header).msg_iovlen = 1;
                if !names_control {
                    (*header).msg_control = std::ptr::null_mut();
                }
            }
            // SAFETY: as above.
            let call = recvmsg_of(unsafe { &mut *header });
            let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
            let delivered = geometry.delivered(&memory, 8);
            assert!(delivered.refused.is_some(), "names_control {names_control}");
            assert_eq!(
                delivered.may_have_installed, names_control,
                "names_control {names_control}"
            );
            // SAFETY: made by `Box::into_raw` above, and no longer read.
            drop(unsafe { Box::from_raw(header) });
        }
        // A batch whose timeout overlaps an entry's header, naming no
        // control buffer: Linux reads that header only when it reaches the
        // entry, so the geometry cannot say what it named.
        // SAFETY: all-zero is a valid `mmsghdr`.
        let mut vector: [Mmsghdr; 1] = unsafe { std::mem::zeroed() };
        let timeout = vector.as_ptr().cast::<Timespec>();
        let call = Syscall::Recvmmsg(
            syscalls::Recvmmsg::new()
                .with_fd(7)
                .with_mmsg(AddrMut::from_ptr(vector.as_mut_ptr()))
                .with_vlen(1)
                .with_timeout(Addr::from_ptr(timeout)),
        );
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        let delivered = geometry.delivered(&memory, 1);
        assert!(delivered.refused.is_some() && delivered.may_have_installed);
    }

    /// Whatever the call may write over what it reads refuses the whole
    /// receive: a control buffer inside its own header, a name buffer over
    /// its control buffer, and a timeout over a control buffer.
    #[test]
    fn a_receive_whose_outputs_overlap_its_inputs_names_nothing() {
        let memory = LocalMemory::new();
        #[repr(C, align(8))]
        struct Both {
            header: Msghdr,
            control: [u8; 24],
        }
        // SAFETY: all-zero is a valid `Both`.
        let mut both: Both = unsafe { std::mem::zeroed() };
        let inside = (&raw mut both.header).cast::<u8>().wrapping_add(16);
        both.header.msg_control = inside.cast();
        both.header.msg_controllen = 20;
        let call = recvmsg_of(&mut both.header);
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        assert!(geometry.delivered(&memory, 1).refused.is_some());

        let mut control = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[4]);
        let mut header = header(&mut control, 0);
        header.msg_controllen = control.len();
        header.msg_name = control.as_mut_ptr().cast();
        header.msg_namelen = 8;
        let call = recvmsg_of(&mut header);
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        assert!(geometry.delivered(&memory, 1).refused.is_some());

        let mut control = message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[5]);
        // SAFETY: all-zero is a valid `mmsghdr`.
        let mut vector: [Mmsghdr; 1] = unsafe { std::mem::zeroed() };
        vector[0].msg_hdr = super::tests::header(&mut control, 0);
        vector[0].msg_hdr.msg_controllen = control.len();
        let timeout = control.as_ptr().cast::<Timespec>();
        let call = Syscall::Recvmmsg(
            syscalls::Recvmmsg::new()
                .with_fd(7)
                .with_mmsg(AddrMut::from_ptr(vector.as_mut_ptr()))
                .with_vlen(1)
                .with_timeout(Addr::from_ptr(timeout)),
        );
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        assert!(geometry.delivered(&memory, 1).refused.is_some());
    }

    /// A batch names the descriptors of the messages it reports, however
    /// many (recvmmsg has no `UIO_MAXIOV` clamp), bounded by the vector it was
    /// given; two messages sharing a control buffer refuse the whole batch.
    #[test]
    fn a_batch_names_every_reported_message_up_to_its_vector() {
        let memory = LocalMemory::new();
        let count = libc::UIO_MAXIOV as usize + 76;
        let mut controls: Vec<Vec<u8>> = (0..count)
            .map(|index| message(libc::SOL_SOCKET, libc::SCM_RIGHTS, &[index as RawFd + 3]))
            .collect();
        // SAFETY: all-zero is a valid `mmsghdr`.
        let mut vector: Vec<Mmsghdr> = (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
        for (entry, control) in vector.iter_mut().zip(controls.iter_mut()) {
            entry.msg_hdr = header(control, 0);
            entry.msg_hdr.msg_controllen = control.len();
        }
        let call = Syscall::Recvmmsg(
            syscalls::Recvmmsg::new()
                .with_fd(7)
                .with_mmsg(AddrMut::from_ptr(vector.as_mut_ptr()))
                .with_vlen(count as u32),
        );
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        let delivered = geometry.delivered(&memory, count as i64);
        assert_eq!(delivered.fds.len(), count);
        assert_eq!(delivered.fds[count - 1], count as RawFd + 2);
        assert_eq!(geometry.delivered(&memory, 2).fds, vec![3, 4]);
        // More reported than given: the vector bounds it.
        assert_eq!(
            geometry.delivered(&memory, count as i64 + 9).fds.len(),
            count
        );
        // Two entries sharing one control buffer.
        vector[1].msg_hdr.msg_control = vector[0].msg_hdr.msg_control;
        let geometry = ReceiveGeometry::before(&memory, &call).unwrap();
        let delivered = geometry.delivered(&memory, 3);
        assert!(delivered.fds.is_empty() && delivered.refused.is_some());
    }

    #[test]
    fn only_an_empty_unlinked_regular_file_may_be_a_placeholder() {
        // SAFETY: all-zero is a valid `stat`.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        stat.st_mode = libc::S_IFREG | 0o600;
        assert!(may_be_placeholder(&stat));
        stat.st_nlink = 1;
        assert!(!may_be_placeholder(&stat));
        stat.st_nlink = 0;
        stat.st_size = 1;
        assert!(!may_be_placeholder(&stat));
        stat.st_size = 0;
        stat.st_mode = libc::S_IFIFO | 0o600;
        assert!(!may_be_placeholder(&stat));
    }

    /// The `AF_UNIX` name an address gives: an abstract name whole, an
    /// absolute path up to its NUL, and none for a relative path, another
    /// family or an unnamed address.
    #[test]
    fn unix_socket_names_are_abstract_names_and_absolute_paths() {
        let offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
        let name_of = |family: i32, path: &[u8], length: usize| {
            let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            address.sun_family = family as libc::sa_family_t;
            for (slot, byte) in address.sun_path.iter_mut().zip(path) {
                *slot = *byte as libc::c_char;
            }
            unix_socket_name(&LocalMemory::new(), (&raw const address) as usize, length)
        };
        assert_eq!(
            name_of(libc::AF_UNIX, b"\0abc", offset + 4).as_deref(),
            Some(&b"\0abc"[..])
        );
        assert_eq!(
            name_of(libc::AF_UNIX, b"/tmp/s\0junk", offset + 11).as_deref(),
            Some(&b"/tmp/s"[..])
        );
        assert_eq!(name_of(libc::AF_UNIX, b"relative", offset + 8), None);
        assert_eq!(name_of(libc::AF_UNIX, b"", offset), None);
        assert_eq!(name_of(libc::AF_INET, b"\0abc", offset + 4), None);
    }

    /// Connections to a bound name are taken by its accepts in order; a
    /// connect or accept outside the turn leaves the order unknown until the
    /// name is bound again.
    #[test]
    fn unix_connections_are_accepted_in_order() {
        use crate::fd::DetFd;
        use crate::fd::FdType;
        use crate::types::DetTid;
        use crate::types::OpenFileId;
        let mut connections = UnixConnections::default();
        let name = b"\0unix-connections-are-accepted-in-order";
        let socket = |sequence| {
            DetFd::new(
                3,
                nix::fcntl::OFlag::O_RDWR,
                FdType::Socket,
                OpenFileId::new(DetTid::from_raw(1), sequence),
            )
        };
        let (first, second) = (socket(0), socket(1));
        let taken = |accepted: Option<OpenFileHandle>| {
            accepted
                .and_then(|handle| handle.descriptor(9, false))
                .map(|detfd| detfd.open_file_id())
        };

        connections.connected(name, Some(first.open_file_handle()));
        let bind = connections.bound(name, OpenFileHandle::dangling());
        assert_eq!(
            taken(connections.accepted(name, bind, true)),
            None,
            "no guest bound the name yet"
        );
        connections.connected(name, Some(first.open_file_handle()));
        connections.connected(name, Some(second.open_file_handle()));
        assert_eq!(
            taken(connections.accepted(name, bind, true)),
            Some(first.open_file_id())
        );
        assert_eq!(
            taken(connections.accepted(name, bind, true)),
            Some(second.open_file_id())
        );
        assert_eq!(taken(connections.accepted(name, bind, true)), None);

        connections.connected(name, None);
        connections.connected(name, Some(first.open_file_handle()));
        assert_eq!(
            taken(connections.accepted(name, bind, true)),
            None,
            "after a connect outside the turn"
        );

        let bind = connections.bound(name, OpenFileHandle::dangling());
        connections.connected(name, Some(second.open_file_handle()));
        assert_eq!(taken(connections.accepted(name, bind, false)), None);
        connections.connected(name, Some(first.open_file_handle()));
        assert_eq!(
            taken(connections.accepted(name, bind, true)),
            None,
            "after an accept outside the turn"
        );
    }

    /// A connect Detcore cannot attribute to one bound name (a relative
    /// path, another spelling of a path, a name no guest bound here) may have
    /// reached any listener: every name's order is unknown until it is bound
    /// again, so a later accept pairs with nothing rather than with the
    /// wrong connecting socket.
    #[test]
    fn an_unattributed_connect_leaves_every_name_unknown() {
        use crate::fd::DetFd;
        use crate::fd::FdType;
        use crate::types::DetTid;
        use crate::types::OpenFileId;
        let socket = |sequence| {
            DetFd::new(
                3,
                nix::fcntl::OFlag::O_RDWR,
                FdType::Socket,
                OpenFileId::new(DetTid::from_raw(1), sequence),
            )
        };
        let (first, second) = (socket(0), socket(1));
        let taken = |accepted: Option<OpenFileHandle>| {
            accepted
                .and_then(|handle| handle.descriptor(9, false))
                .map(|detfd| detfd.open_file_id())
        };
        let (path, other) = (&b"/tmp/listener"[..], &b"\0other-listener"[..]);

        type Unattributed<'a> = Box<dyn Fn(&mut UnixConnections) + 'a>;
        let unattributed: [Unattributed<'_>; 3] = [
            // A relative path, or a socket of a type Detcore did not see made.
            Box::new(|connections| connections.lost()),
            // Another spelling of the bound path, in the turn or not.
            Box::new(|connections| connections.connected(b"/tmp//listener", None)),
            Box::new(|connections| {
                connections.connected(b"/tmp/listener-link", Some(first.open_file_handle()))
            }),
        ];
        for unattributed in unattributed {
            let mut connections = UnixConnections::default();
            let (bind, other_bind) = (
                connections.bound(path, OpenFileHandle::dangling()),
                connections.bound(other, OpenFileHandle::dangling()),
            );
            unattributed(&mut connections);
            connections.connected(path, Some(second.open_file_handle()));
            connections.connected(other, Some(first.open_file_handle()));
            assert_eq!(taken(connections.accepted(path, bind, true)), None);
            assert_eq!(taken(connections.accepted(other, other_bind, true)), None);

            let bind = connections.bound(path, OpenFileHandle::dangling());
            connections.connected(path, Some(first.open_file_handle()));
            assert_eq!(
                taken(connections.accepted(path, bind, true)),
                Some(first.open_file_id()),
                "a new bind starts the name's order again"
            );
        }
    }

    /// A connection is guest-held only when both of its ends are sockets a
    /// guest made and never passed out (`both_ends_guest_made`), and only
    /// where the connect or accept completed in its turn: a connecting socket
    /// or a listener that a guest received, or passed out, or that Detcore
    /// does not know, leaves it unmarked.
    #[test]
    fn a_connection_is_guest_made_only_when_both_of_its_ends_are() {
        use crate::fd::DetFd;
        use crate::fd::FdType;
        use crate::types::DetTid;
        use crate::types::OpenFileId;
        let socket = |sequence| {
            DetFd::new(
                3,
                nix::fcntl::OFlag::O_RDWR,
                FdType::Socket,
                OpenFileId::new(DetTid::from_raw(1), sequence),
            )
        };
        // The descriptors stay open: a handle reaches only a modeled
        // description.
        let open = (socket(0), socket(1), socket(2));
        open.0.set_guest_created();
        open.1.set_guest_created();
        let (connecting, listener, received) = (
            open.0.open_file_handle(),
            open.1.open_file_handle(),
            open.2.open_file_handle(),
        );
        assert!(both_ends_guest_made(
            true,
            Some(&connecting),
            Some(&listener)
        ));
        assert!(!both_ends_guest_made(
            false,
            Some(&connecting),
            Some(&listener)
        ));
        assert!(!both_ends_guest_made(
            true,
            Some(&received),
            Some(&listener)
        ));
        assert!(!both_ends_guest_made(
            true,
            Some(&connecting),
            Some(&received)
        ));
        assert!(!both_ends_guest_made(true, None, Some(&listener)));
        assert!(!both_ends_guest_made(true, Some(&connecting), None));
        listener
            .descriptor(4, false)
            .expect("still modeled")
            .clear_guest_created();
        assert!(
            !both_ends_guest_made(true, Some(&connecting), Some(&listener)),
            "a listener passed out of the guest"
        );
    }

    /// A listener bound before its path was unlinked and bound again takes
    /// connections Detcore no longer tracks: its accepts pair with nothing,
    /// and the later listener's connections stay its own.
    #[test]
    fn a_listener_whose_name_was_bound_again_pairs_nothing() {
        use crate::fd::DetFd;
        use crate::fd::FdType;
        use crate::types::DetTid;
        use crate::types::OpenFileId;
        let socket = DetFd::new(
            3,
            nix::fcntl::OFlag::O_RDWR,
            FdType::Socket,
            OpenFileId::new(DetTid::from_raw(1), 0),
        );
        let path = &b"/tmp/rebound"[..];
        let mut connections = UnixConnections::default();
        let earlier = connections.bound(path, OpenFileHandle::dangling());
        let later = connections.bound(path, OpenFileHandle::dangling());
        connections.connected(path, Some(socket.open_file_handle()));
        assert!(connections.accepted(path, earlier, true).is_none());
        assert!(
            connections
                .accepted(path, later, true)
                .and_then(|handle| handle.descriptor(9, false))
                .is_some_and(|detfd| detfd.open_file_id() == socket.open_file_id()),
            "the later listener takes its own connection"
        );
    }
}
