#[cfg(not(target_family = "wasm"))]
pub mod cli;

pub mod zip;

#[cfg(target_family = "wasm")]
pub mod wasm;
