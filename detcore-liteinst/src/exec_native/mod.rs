//! Explicit native exec components; no constructor enables this path.

pub(crate) mod catalog;
pub(crate) mod consumer;
pub(crate) mod driver;
pub(crate) mod entry;
pub(crate) use entry::qualify_config;
pub(crate) mod provider;
use detcore_model::liteinst_native_exec_runtime as runtime_packet;
