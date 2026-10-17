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

struct Transport;

impl OpenFileControlTransport for Transport {
    fn call(&self, request: OpenFileControl) -> Result<OpenFileControlReply, SharedOpenFileError> {
        let callback = CALLBACK.with(Cell::get).ok_or_else(|| {
            SharedOpenFileError("shared OFD access outside a DBT callback".into())
        })?;
        let pid = unsafe { libc::syscall(libc::SYS_getpid) as i32 };
        let tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
        if (pid, tid) != (callback.physical_pid, callback.physical_tid) {
            return Err(SharedOpenFileError(
                "inherited or foreign OFD callback scope".into(),
            ));
        }
        if !reverie_dbt::sync_rpc::is_active() {
            return Err(SharedOpenFileError(
                "shared OFDs require the external coordinator".into(),
            ));
        }
        let runtime = super::current_runtime();
        let response = reverie_dbt::sync_rpc::send_rpc_from_guest::<GlobalState>(
            callback.context,
            callback.invoke,
            callback.tid,
            &runtime.config,
            detcore::shared_open_file_request(request),
        );
        detcore::shared_open_file_reply(response)
    }
}
