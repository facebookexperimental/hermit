//! Callback-scoped access to Detcore's canonical open-file store.

use std::cell::Cell;
use std::sync::Once;

use detcore::ChunkedOpenFileChannel;
use detcore::GlobalState;
use detcore::OpenFileControl;
use detcore::OpenFileControlReply;
use detcore::OpenFileControlTransport;
use detcore::SharedOpenFileError;
use reverie::Tid;
use reverie_dbt::SyscallInvoker;

#[derive(Clone, Copy)]
struct Callback {
    context: usize,
    invoke: SyscallInvoker,
    tid: Tid,
    physical_pid: i32,
    physical_tid: i32,
}

// Constant, destructor-free TLS: no allocator, thread park handle, socket,
// mutable Tool borrow, or callback pointer survives its explicit guard.
thread_local! {
    static CALLBACK: Cell<Option<Callback>> = const { Cell::new(None) };
}

pub(super) struct Scope(Option<Callback>);

impl Scope {
    pub(super) fn enter(context: usize, invoke: SyscallInvoker, tid: Tid) -> Self {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            assert!(
                detcore::install_shared_open_file_channel(Box::new(ChunkedOpenFileChannel::new(
                    Transport
                ),))
                .is_ok(),
                "DBT open file channel already installed"
            );
        });
        let callback = Callback {
            context,
            invoke,
            tid,
            // These are runtime identity facts, never guest-visible values.
            physical_pid: unsafe { libc::syscall(libc::SYS_getpid) as i32 },
            physical_tid: unsafe { libc::syscall(libc::SYS_gettid) as i32 },
        };
        Self(CALLBACK.with(|slot| slot.replace(Some(callback))))
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        CALLBACK.with(|slot| slot.set(self.0));
    }
}

/// This process's synchronous request to Detcore's global state from inside a
/// DBT callback, through the callback's application context. Shared open file
/// control ([`Transport`]) and Detcore's diagnostics (`detcore::guest_log`)
/// both send through it. Outside a callback there is no context to send
/// through.
pub(super) fn coordinator_call(
    request: <GlobalState as reverie::GlobalTool>::Request,
) -> std::io::Result<<GlobalState as reverie::GlobalTool>::Response> {
    let callback = CALLBACK
        .with(Cell::get)
        .ok_or_else(|| std::io::Error::other("shared OFD access outside a DBT callback"))?;
    let pid = unsafe { libc::syscall(libc::SYS_getpid) as i32 };
    let tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
    if (pid, tid) != (callback.physical_pid, callback.physical_tid) {
        return Err(std::io::Error::other(
            "inherited or foreign OFD callback scope",
        ));
    }
    if !reverie_dbt::sync_rpc::is_active() {
        return Err(std::io::Error::other(
            "shared OFDs require the external coordinator",
        ));
    }
    let runtime = super::current_runtime();
    Ok(reverie_dbt::sync_rpc::send_rpc_from_guest::<GlobalState>(
        callback.context,
        callback.invoke,
        callback.tid,
        &runtime.config,
        request,
    ))
}

struct Transport;

impl OpenFileControlTransport for Transport {
    fn call(&self, request: OpenFileControl) -> Result<OpenFileControlReply, SharedOpenFileError> {
        let response = coordinator_call(detcore::shared_open_file_request(request))
            .map_err(|error| SharedOpenFileError(error.to_string()))?;
        detcore::shared_open_file_reply(response)
    }
}
