use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// On-disk data failed an integrity check or could not be decoded.
    Corrupt(String),
    /// Referenced entity does not exist (or is not visible / not live).
    NotFound(u64),
    /// Invalid argument or operation.
    Invalid(String),
    /// Query syntax error with byte offset.
    Parse { msg: String, pos: usize },
    /// Query runtime error.
    Query(String),
    /// Feature not compiled in or not supported.
    Unsupported(String),
    /// The storage backend (PostgreSQL) failed or refused the operation.
    Backend(String),
    /// Authentication failed (bad credentials, expired or revoked token, locked account).
    Auth(String),
    /// The caller is authenticated but not allowed to do this.
    Denied(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Corrupt(m) => write!(f, "corrupt data: {m}"),
            Error::NotFound(id) => write!(f, "entity {id} not found"),
            Error::Invalid(m) => write!(f, "invalid: {m}"),
            Error::Parse { msg, pos } => write!(f, "syntax error at offset {pos}: {msg}"),
            Error::Query(m) => write!(f, "query error: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::Backend(m) => write!(f, "storage backend error: {m}"),
            Error::Auth(m) => write!(f, "authentication failed: {m}"),
            Error::Denied(m) => write!(f, "permission denied: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

#[cfg(feature = "postgres")]
impl From<postgres::Error> for Error {
    fn from(e: postgres::Error) -> Self {
        // `Display` for db errors is just "db error"; keep the server message.
        match e.as_db_error() {
            Some(d) => Error::Backend(format!("{} ({}): {}", d.severity(), d.code().code(), d.message())),
            None => {
                // `Display` shows only the outermost message; the cause (TLS alert, bad
                // configuration, socket error) is what tells the operator what is wrong.
                let mut m = e.to_string();
                let mut src = std::error::Error::source(&e);
                while let Some(c) = src {
                    m.push_str(": ");
                    m.push_str(&c.to_string());
                    src = c.source();
                }
                Error::Backend(m)
            }
        }
    }
}

pub(crate) fn invalid<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(m.into()))
}
