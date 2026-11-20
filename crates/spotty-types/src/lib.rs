//! Core domain types, error definitions, and LMS host traits for `Spotty-RS`.

pub mod error;
pub mod host;

pub use crate::error::{Result, SpottyError};
pub use crate::host::{ClientId, LmsHostCallbacks, PlaylistCommand};
