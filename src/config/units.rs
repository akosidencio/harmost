//! Duration and byte-size scalars for the config file.
//!
//! Hand-rolled rather than pulled from a crate: the surface is tiny, the error
//! messages matter more than the generality (a bad TTL should say which unit it
//! expected), and the config parser sits on the supply-chain critical path.

use serde::{Deserialize, Deserializer, de::Error as _};
use std::time::Duration;

/// A duration written as `500ms`, `2s`, `5m`, `1h`.
///
/// A bare integer is rejected on purpose: `ttl: 2` is ambiguous between two
/// seconds and two milliseconds, and this config has both scales in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dur(pub Duration);

impl Dur {
    pub const ZERO: Dur = Dur(Duration::ZERO);

    pub fn as_duration(self) -> Duration {
        self.0
    }

    pub fn parse(s: &str) -> Result<Dur, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty duration".into());
        }
        let split = s
            .find(|c: char| c.is_ascii_alphabetic())
            .ok_or_else(|| format!("duration `{s}` has no unit; expected one of ms, s, m, h"))?;
        let (num, unit) = s.split_at(split);
        let num: u64 = num
            .trim()
            .parse()
            .map_err(|_| format!("duration `{s}` has a non-integer value"))?;
        let d = match unit {
            "ms" => Duration::from_millis(num),
            "s" => Duration::from_secs(num),
            "m" => Duration::from_secs(
                num.checked_mul(60)
                    .ok_or_else(|| format!("duration `{s}` overflows"))?,
            ),
            "h" => Duration::from_secs(
                num.checked_mul(3600)
                    .ok_or_else(|| format!("duration `{s}` overflows"))?,
            ),
            other => {
                return Err(format!(
                    "duration `{s}` has unknown unit `{other}`; expected ms, s, m, or h"
                ));
            }
        };
        Ok(Dur(d))
    }
}

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Dur::parse(&s).map_err(D::Error::custom)
    }
}

/// A byte size written as `512MiB`, `4MiB`, `1KiB`, or a bare byte count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bytes(pub u64);

impl Bytes {
    pub fn get(self) -> u64 {
        self.0
    }

    /// The size as a `usize`, saturating rather than truncating.
    ///
    /// A budget wider than the address space cannot be honoured anyway, and
    /// clamping says so; an `as usize` would instead turn a 5 GiB cache into a
    /// 1 GiB one on a 32-bit target and report the larger number back.
    pub fn as_usize(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }

    pub fn parse(s: &str) -> Result<Bytes, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty size".into());
        }
        let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
        let (num, unit) = s.split_at(split);
        let num: u64 = num
            .trim()
            .parse()
            .map_err(|_| format!("size `{s}` has a non-integer value"))?;
        let mult: u64 = match unit.trim() {
            "" | "B" => 1,
            "KiB" => 1024,
            "MiB" => 1024 * 1024,
            "GiB" => 1024 * 1024 * 1024,
            other => {
                return Err(format!(
                    "size `{s}` has unknown unit `{other}`; expected B, KiB, MiB, or GiB"
                ));
            }
        };
        num.checked_mul(mult)
            .map(Bytes)
            .ok_or_else(|| format!("size `{s}` overflows"))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Bytes::parse(&s).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_duration_unit() {
        assert_eq!(Dur::parse("500ms").unwrap().0, Duration::from_millis(500));
        assert_eq!(Dur::parse("2s").unwrap().0, Duration::from_secs(2));
        assert_eq!(Dur::parse("5m").unwrap().0, Duration::from_secs(300));
        assert_eq!(Dur::parse("1h").unwrap().0, Duration::from_secs(3600));
    }

    #[test]
    fn rejects_unitless_duration() {
        // `ttl: 2` must not silently mean 2s in a config that also uses ms.
        let err = Dur::parse("2").unwrap_err();
        assert!(err.contains("no unit"), "{err}");
    }

    #[test]
    fn rejects_unknown_duration_unit() {
        assert!(Dur::parse("5y").unwrap_err().contains("unknown unit"));
    }

    #[test]
    fn parses_binary_sizes() {
        assert_eq!(Bytes::parse("4MiB").unwrap().0, 4 * 1024 * 1024);
        assert_eq!(Bytes::parse("512MiB").unwrap().0, 512 * 1024 * 1024);
        assert_eq!(Bytes::parse("1024").unwrap().0, 1024);
    }

    #[test]
    fn rejects_decimal_size_units() {
        // MB vs MiB is a real footgun in a memory budget; refuse rather than guess.
        assert!(Bytes::parse("4MB").unwrap_err().contains("unknown unit"));
    }
}
