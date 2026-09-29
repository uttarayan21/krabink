// bevy's `AsBindGroup` derive on `InkMaterial` needs more than the default
// 128 for the auto-trait proof of its render-world system params.
#![recursion_limit = "256"]

mod app;
#[cfg(not(target_arch = "wasm32"))]
mod cli;
mod color_wheel;
mod config;
mod docs;
mod errors;
mod import;
mod ink_assets;
mod ink_material;
#[cfg(not(target_arch = "wasm32"))]
mod lab;
#[cfg(not(target_arch = "wasm32"))]
mod native;
mod node;
#[cfg(not(target_arch = "wasm32"))]
mod replay;
mod runtime;
mod settings;
mod sketch;
mod sync;
mod theme;
mod ui;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> errors::Result<()> {
    native::main()
}

#[cfg(target_arch = "wasm32")]
fn main() {
    web::main();
}
