pub mod changes;
pub mod dependencies;
pub mod doctor;
pub mod indexes;
pub mod interactive;
pub mod locked;
pub mod locking;
pub mod output;
pub mod policy;
pub mod selection;
pub mod verification;
pub mod workspace;

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug)]
pub struct Interrupted;

impl Display for Interrupted {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("Interrupted.")
    }
}

impl Error for Interrupted {}

pub fn check_interrupt() -> anyhow::Result<()> {
    if INTERRUPTED.load(Ordering::Relaxed) {
        return Err(Interrupted.into());
    }
    Ok(())
}
