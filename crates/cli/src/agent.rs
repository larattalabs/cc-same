#![cfg_attr(windows, windows_subsystem = "windows")]
//! Background agent: `cc-same-agent [--user-data <dir>] [--state-dir <dir>] [watch]`.
//! Same as `cc-same watch`, built without a console window on Windows.

fn main() {
    cc_same_core::watch::run_agent(std::env::args_os().skip(1));
}
