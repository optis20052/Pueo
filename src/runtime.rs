//! Bridges the Slint event loop and a Tokio runtime that runs all database I/O.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Runtime;

pub fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .thread_name("pueo-db")
            .build()
            .expect("failed to start Tokio runtime")
    })
}

/// Run `fut` on Tokio and deliver its output on the Slint UI thread.
pub fn spawn<F, T>(fut: F, done: impl FnOnce(T) + 'static)
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handle = runtime().spawn(fut);
    slint::spawn_local(async move {
        match handle.await {
            Ok(value) => done(value),
            Err(e) => eprintln!("background task failed: {e}"),
        }
    })
    .expect("Slint event loop is not running");
}
