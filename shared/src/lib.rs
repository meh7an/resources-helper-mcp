//! Types shared by every crate in the resources-helper workspace.

pub mod api;
pub mod config;
mod error;
pub mod mutate;
pub mod paths;
#[cfg(test)]
mod test_support;
pub mod types;

pub use error::{Error, Result};
