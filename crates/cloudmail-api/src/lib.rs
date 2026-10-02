//! Shared API client, types, config and text helpers for the cloudmail CLI and GTK app, plus the
//! provider abstraction that lets linked accounts (HEY, Gmail) sit next to your worker.

pub mod attach;
pub mod client;
pub mod config;
pub mod error;
pub mod gmail;
pub mod hey;
pub mod provider;
pub mod text;
pub mod types;
pub mod unified;

pub use client::{Client, ThreadQuery};
pub use config::Config;
pub use error::{Error, ErrorKind, Result};
pub use provider::{AccountStatus, AccountWarning, ExtraFolder, Provider};
pub use types::*;
pub use unified::{Listing, Mail};
