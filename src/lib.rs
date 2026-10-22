pub mod changes;
pub mod dependencies;
pub mod indexes;
pub mod interactive;
pub mod locking;
pub mod output;
pub mod selection;
pub mod workspace;

use std::sync::atomic::{AtomicBool, Ordering};

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Interrupted.")
    }
}

impl std::error::Error for Interrupted {}

pub fn check_interrupt() -> anyhow::Result<()> {
    if INTERRUPTED.load(Ordering::Relaxed) {
        return Err(Interrupted.into());
    }
    Ok(())
}
