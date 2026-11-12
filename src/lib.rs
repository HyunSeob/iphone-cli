// A failed command carries its whole report (code, hint, candidates, screen
// summary) in the error value. It is built once per failure and never copied
// in a loop, so its size does not matter.
#![allow(clippy::result_large_err)]

pub mod actions;
pub mod ax;
pub mod client;
pub mod clock;
pub mod config;
pub mod daemon;
pub mod ddi;
pub mod device;
pub mod devices;
pub mod dnd;
pub mod doctor;
pub mod human;
pub mod i18n;
pub mod ipc;
pub mod keys;
pub mod mcp;
pub mod paths;
pub mod script;
pub mod steps;
pub mod tcp;
pub mod reply;
pub mod selector;
pub mod setup;
