//! Physical units used across resource definitions.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// A byte quantity parsed from and serialized to human strings like
/// `"48GiB"` or `"16GB"`.
///
/// Binary suffixes (`KiB`, `MiB`, `GiB`, `TiB`) use powers of 1024;
/// decimal suffixes (`KB`, `MB`, `GB`, `TB`) use powers of 1000. A bare
/// number, or a `B` suffix, is interpreted as raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteSize(u64);

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

const KB: u64 = 1000;
const MB: u64 = 1000 * KB;
const GB: u64 = 1000 * MB;
const TB: u64 = 1000 * GB;

/// Ordered largest-first so formatting picks the coarsest exact unit.
const UNITS: &[(&str, u64)] = &[
    ("TiB", TIB),
    ("GiB", GIB),
    ("MiB", MIB),
    ("KiB", KIB),
    ("TB", TB),
    ("GB", GB),
    ("MB", MB),
    ("KB", KB),
    ("B", 1),
];

impl ByteSize {
    #[must_use]
    pub const fn from_bytes(bytes: u64) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

impl From<u64> for ByteSize {
    fn from(bytes: u64) -> Self {
        Self(bytes)
    }
}

impl From<ByteSize> for u64 {
    fn from(size: ByteSize) -> Self {
        size.0
    }
}

/// Error returned when a byte-size string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseByteSizeError(String);

impl fmt::Display for ParseByteSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid byte size: {}", self.0)
    }
}

impl std::error::Error for ParseByteSizeError {}

impl FromStr for ByteSize {
    type Err = ParseByteSizeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let split = trimmed
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(trimmed.len());
        let (num, suffix) = trimmed.split_at(split);
        let num = num.trim();
        let suffix = suffix.trim();

        let value: u64 = num.parse().map_err(|_| ParseByteSizeError(s.to_string()))?;

        let factor = if suffix.is_empty() {
            1
        } else {
            UNITS
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(suffix))
                .map(|(_, factor)| *factor)
                .ok_or_else(|| ParseByteSizeError(s.to_string()))?
        };

        value
            .checked_mul(factor)
            .map(Self)
            .ok_or_else(|| ParseByteSizeError(s.to_string()))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return write!(f, "0B");
        }
        let (name, factor) = UNITS
            .iter()
            .find(|(_, factor)| self.0.is_multiple_of(*factor))
            .copied()
            .unwrap_or(("B", 1));
        write!(f, "{}{name}", self.0 / factor)
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_and_decimal_suffixes() {
        assert_eq!("48GiB".parse::<ByteSize>().unwrap().bytes(), 48 * GIB);
        assert_eq!("16GB".parse::<ByteSize>().unwrap().bytes(), 16 * GB);
        assert_eq!("512MiB".parse::<ByteSize>().unwrap().bytes(), 512 * MIB);
        assert_eq!("2048".parse::<ByteSize>().unwrap().bytes(), 2048);
        assert_eq!("4B".parse::<ByteSize>().unwrap().bytes(), 4);
    }

    #[test]
    fn is_case_and_whitespace_insensitive() {
        assert_eq!("  8 gib ".parse::<ByteSize>().unwrap().bytes(), 8 * GIB);
    }

    #[test]
    fn rejects_garbage() {
        assert!("".parse::<ByteSize>().is_err());
        assert!("GiB".parse::<ByteSize>().is_err());
        assert!("12ZB".parse::<ByteSize>().is_err());
    }

    #[test]
    fn display_uses_coarsest_exact_unit() {
        assert_eq!(ByteSize::from_bytes(48 * GIB).to_string(), "48GiB");
        assert_eq!(ByteSize::from_bytes(0).to_string(), "0B");
        assert_eq!(ByteSize::from_bytes(1).to_string(), "1B");
    }

    #[test]
    fn display_round_trips_through_parse() {
        for bytes in [0, 1, 1023, 48 * GIB, 16 * GB, 7 * MIB + 3] {
            let size = ByteSize::from_bytes(bytes);
            let reparsed: ByteSize = size.to_string().parse().unwrap();
            assert_eq!(size, reparsed);
        }
    }

    #[test]
    fn serde_round_trip() {
        let size = ByteSize::from_bytes(48 * GIB);
        let json = serde_json::to_string(&size).unwrap();
        assert_eq!(json, "\"48GiB\"");
        assert_eq!(serde_json::from_str::<ByteSize>(&json).unwrap(), size);
    }
}
