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
        // Boxed: serde_saphyr's error carries position and context and is
        // large enough to bloat every Ok value on this Result.
        #[source]
        source: Box<serde_saphyr::Error>,
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
    let cfg: Config = serde_saphyr::from_str(&text).map_err(|source| LoadError::Parse {
        path: path.to_string(),
        source: Box::new(source),
    })?;
    validation::validate(&cfg).map_err(|source| LoadError::Invalid {
        path: path.to_string(),
        source,
    })?;
    Ok(cfg)
}
