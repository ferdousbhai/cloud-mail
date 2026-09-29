//! Shared API client, types, config and text helpers for the cloudmail CLI and GTK app.

pub mod client;
pub mod config;
pub mod error;
pub mod text;
pub mod types;

pub use client::{Client, ThreadQuery};
pub use config::Config;
pub use error::{Error, ErrorKind, Result};
pub use types::*;
