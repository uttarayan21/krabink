pub use error_stack::{Report, ResultExt};

#[derive(Debug, thiserror::Error)]
#[error("krabink-server error")]
pub struct Error;

pub type Result<T, E = error_stack::Report<Error>> = core::result::Result<T, E>;
