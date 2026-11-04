//! Cooperative cancellation shared by the collector and terminal UI.
use anyhow::Result;
use std::{
    fmt, io,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug)]
pub struct Cancelled;
impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Interrupted")
    }
}
impl std::error::Error for Cancelled {}

pub fn check(stopped: &AtomicBool) -> Result<()> {
    if stopped.load(Ordering::Relaxed) {
        Err(Cancelled.into())
    } else {
        Ok(())
    }
}

pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<Cancelled>()
            || cause
                .downcast_ref::<io::Error>()
                .and_then(io::Error::get_ref)
                .is_some_and(|inner| inner.is::<Cancelled>())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_survives_io_and_context_wrappers() {
        let error =
            anyhow::Error::from(io::Error::other(Cancelled)).context("Cannot read sessions");
        assert!(is_cancelled(&error));
        assert!(!is_cancelled(&anyhow::anyhow!("Unrelated file error")));
    }
}
