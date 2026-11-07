//! The one error type for the core crate. `Display` text is shown to players,
//! so it says what happened and what to do, not which function failed.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Couldn't read or write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Couldn't reach {host}. Check your connection and try again.")]
    Offline { host: String },
    #[error("{host} answered with an error ({status}). Try again in a little while.")]
    HttpStatus { host: String, status: u16 },
    #[error("The download was cancelled.")]
    Cancelled,
    #[error("The download looks corrupt: {0}. Try again.")]
    Corrupt(String),
    /// A site answered a download with a web page, such as a notice, and a success code.
    #[error(
        "{site} sent a web page instead of the zip. Try again later, and if it keeps happening, use Open page to check the addon is still offered there."
    )]
    WebPage { site: String },
    #[error("This addon's zip isn't safe to install: {0}")]
    UnsafeZip(String),
    #[error("This addon's zip doesn't contain anything WoW can load.")]
    NoAddonInZip,
    #[error("Couldn't verify this update's signature, so it wasn't installed.")]
    BadSignature,
    #[error("Close WoW first: {0}")]
    GameRunning(String),
    #[error(
        "This download would also replace {}, which isn't part of this addon, so nothing was changed.",
        .folders.join(", ")
    )]
    WouldReplace { folders: Vec<String> },
    #[error("Couldn't read {what}: {detail}")]
    Parse { what: String, detail: String },
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Wrap an IO error with the path it happened on.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
