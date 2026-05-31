//! The process-global tokio runtime that drives the async engine.
//!
//! The C-API functions (`sqlite3_open`, `sqlite3_step`, …) keep synchronous signatures; they
//! run the async VFS/pager engine to completion via [`block_on`] on this shared multi-thread
//! runtime. Because it uses `Runtime::block_on`, **do not** call the `sqlite3_*` functions
//! from inside another tokio runtime (e.g. a `#[tokio::test]`) — that panics. Engine-internal
//! async fns are tested directly with their own runtime instead.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};

fn shared() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build the rustqlite tokio runtime")
    })
}

/// Run an async engine operation to completion on the shared runtime.
pub fn block_on<F: Future>(future: F) -> F::Output {
    shared().block_on(future)
}
