use thiserror::Error;

#[derive(Debug, Error)]
pub enum AlmError {
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("infeasible: {0}")]
    Infeasible(String),
    #[error("spectral analysis failed: {0}")]
    Spectral(String),
    #[error("io/json: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, AlmError>;
