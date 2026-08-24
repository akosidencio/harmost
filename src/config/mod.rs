//! Configuration loading: parse, then refuse anything unsafe.

pub mod schema;
pub mod units;
pub mod validation;

pub use schema::*;
pub use units::{Bytes, Dur};
pub use validation::ValidationError;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("reading {path}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path}")]
    Parse {
        path: String,
        #[source]
        source: serde_norway::Error,
    },
    #[error("invalid configuration in {path}")]
    Invalid {
        path: String,
        #[source]
        source: ValidationError,
    },
}

/// Read, parse and validate a config file. All three failures name the file.
pub fn load(path: &str) -> Result<Config, LoadError> {
    let text = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.to_string(),
        source,
    })?;
    let cfg: Config = serde_norway::from_str(&text).map_err(|source| LoadError::Parse {
        path: path.to_string(),
        source,
    })?;
    validation::validate(&cfg).map_err(|source| LoadError::Invalid {
        path: path.to_string(),
        source,
    })?;
    Ok(cfg)
}
