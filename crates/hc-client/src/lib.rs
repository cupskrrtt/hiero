//! Typed async client for herdr-core (`hc`).
//!
//! ```no_run
//! # async fn demo() -> hc_client::Result<()> {
//! use hc_client::{Client, RunParams};
//! let hc = Client::connect().await?;
//! let t = hc.run(&RunParams::new().argv(["bash"]).name("scratch")).await?;
//! hc.send(&t.address, "echo hi").await?;
//! println!("{}", hc.read(&t.address, 20).await?.text);
//! # Ok(()) }
//! ```
//!
//! For live updates use [`Watcher`] (reconnects on its own) and fold its
//! messages into a [`TerminalSet`].

mod client;
pub mod endpoint;
mod error;
pub mod events;
mod state;
mod types;

pub use client::Client;
pub use error::{Error, ErrorKind, Result, RpcError};
pub use events::{Event, EventData, Filter, Notification, Subscription, WatchMsg, Watcher};
pub use state::TerminalSet;
pub use types::*;
