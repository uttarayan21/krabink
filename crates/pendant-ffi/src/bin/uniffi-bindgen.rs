//! Bindgen entry point for library-mode generation:
//! `cargo run -p pendant-ffi --bin uniffi-bindgen -- generate --library <dylib> --language swift ...`

fn main() {
    uniffi::uniffi_bindgen_main()
}
