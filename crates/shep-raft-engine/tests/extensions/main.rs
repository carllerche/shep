#![cfg_attr(feature = "bt", feature(error_generic_member_access))]
#![allow(clippy::uninlined_format_args)]

extern crate shep_raft_engine as openraft;
#[macro_use]
#[path = "../fixtures/mod.rs"]
mod fixtures;

mod t10_extensions_api;
