#![deny(clippy::await_holding_lock)]

mod http;

pub use http::{router, serve};
