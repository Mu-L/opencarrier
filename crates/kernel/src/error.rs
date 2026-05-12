//! Kernel-specific error types.

use types::error::CarrierError;
use thiserror::Error;

/// Kernel error type wrapping CarrierError with kernel-specific context.
#[derive(Error, Debug)]
pub enum KernelError {
    /// A wrapped CarrierError.
    #[error(transparent)]
    Carrier(#[from] CarrierError),

    /// The kernel failed to boot.
    #[error("Boot failed: {0}")]
    BootFailed(String),
}

/// Alias for kernel results.
pub type KernelResult<T> = Result<T, KernelError>;
