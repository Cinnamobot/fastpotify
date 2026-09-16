//! Fastpotify compatibility command, including its original --version output.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod entrypoint;

fn main() -> eframe::Result<()> {
    entrypoint::run()
}
