// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use hermit::Context;
use hermit::Error;
use hermit::SerializableError;
use rand::thread_rng;
use rand::Rng;
use reverie::process::Container;
use reverie::process::Mount;
use reverie::process::Namespace;

pub fn default_container(pin_threads: bool) -> Container {
    let mut container = Container::new();
    container
        .unshare(Namespace::PID)
        .map_root()
        .hostname("hermetic-container.local")
        .domainname("local")
        .mount(Mount::proc());

    if pin_threads {
        let mut rng = thread_rng();
        let rand_core: usize = rng.gen_range(0..num_cpus::get());
        tracing::info!("Pinning tracer and guest threads to core {}", rand_core);
        container.affinity(rand_core);
    }
    container
}

/// Helper to run a function inside a container, taking care to display any
/// errors and propagate the exit status.
pub fn with_container<F, T>(container: &mut Container, mut f: F) -> Result<T, Error>
where
    F: FnMut() -> Result<T, Error>,
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    Ok(container
        .run(|| f().map_err(SerializableError::from))
        .context("Sandbox container exited unexpectedly")??)
}
