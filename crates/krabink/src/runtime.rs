//! Where the sync node's async work runs: a tokio runtime natively, the
//! browser's own event loop (wasm-bindgen futures) on the web.

use std::future::Future;

use bevy::prelude::Resource;

/// Lives as a resource so systems can drive the node's async calls.
/// Clones share one runtime, so `main` can still shut the node down once
/// the app (and its world) is gone.
#[derive(Resource, Clone)]
pub struct Runtime {
    #[cfg(not(target_arch = "wasm32"))]
    rt: std::sync::Arc<tokio::runtime::Runtime>,
}

impl Runtime {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(rt: tokio::runtime::Runtime) -> Self {
        Self {
            rt: std::sync::Arc::new(rt),
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Self {
        Self {}
    }

    /// Run `fut` in the background; never blocks the frame.
    pub fn spawn(&self, fut: impl Future<Output = ()> + Send + 'static) {
        #[cfg(not(target_arch = "wasm32"))]
        drop(self.rt.spawn(fut));
        #[cfg(target_arch = "wasm32")]
        drop(n0_future::task::spawn(fut));
    }

    /// Wait for `fut` on the runtime. Native only: the browser's one thread
    /// cannot wait for its own event loop.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn block_on<F: Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }

    /// Enter the runtime so sync code can spawn onto it (the local link's
    /// serving task). A no-op in the browser, where spawning always works.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn enter(&self) -> tokio::runtime::EnterGuard<'_> {
        self.rt.enter()
    }

    #[cfg(target_arch = "wasm32")]
    pub fn enter(&self) {}
}
