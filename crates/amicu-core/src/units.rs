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
///
/// # Formatting guarantee
///
/// [`Display`](fmt::Display) always emits an exact, lossless representation: it
/// picks the unit with the *largest factor that divides the byte count
/// exactly*, comparing binary and decimal factors on equal footing, so
/// `parse(display(x)) == x` always holds. A value with no coarse divisor falls
/// back to raw bytes (`"…B"`).
///
/// Family preservation is a *tendency*, not a guarantee: for realistic
/// magnitudes a decimal-authored value renders back in the decimal family - `"16GB"`
/// displays as `"16GB"`, not the equal-but-surprising `"15625000KiB"` - but a value
/// that is an exact multiple of both a decimal unit and a larger binary unit
/// crosses families (still losslessly).
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

/// Ordered by descending factor, mixing families, so `Display`'s first exact
/// divisor is the coarsest unit overall.
const UNITS: &[(&str, u64)] = &[
    ("TiB", TIB),
    ("TB", TB),
    ("GiB", GIB),
    ("GB", GB),
    ("MiB", MIB),
    ("MB", MB),
    ("KiB", KIB),
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

    /// Rounded binary rendering for user-facing text, e.g. `11.5 GiB`
    /// (one decimal; `MiB`/`KiB`/`B` below a GiB). Not lossless - use
    /// [`Display`](fmt::Display) for anything that must round-trip.
    #[must_use]
    pub fn human(self) -> String {
        // One-decimal rounding done in integer tenths to avoid float casts.
        let tenths = |factor: u64| {
            let t = (u128::from(self.0) * 10 + u128::from(factor) / 2) / u128::from(factor);
            format!("{}.{}", t / 10, t % 10)
        };
        if self.0 >= GIB {
            format!("{} GiB", tenths(GIB))
        } else if self.0 >= MIB {
            format!("{} MiB", tenths(MIB))
        } else if self.0 >= KIB {
            format!("{} KiB", tenths(KIB))
        } else {
            format!("{} B", self.0)
        }
    }

    /// [`human`](Self::human) prefixed with `~`, for estimates.
    #[must_use]
    pub fn approx(self) -> String {
        format!("~{}", self.human())
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
        // UNITS ends with ("B", 1), which divides every value, so this find
        // always yields a unit.
        let (name, factor) = UNITS
            .iter()
            .find(|(_, factor)| self.0.is_multiple_of(*factor))
            .copied()
            .expect("UNITS always contains B (factor 1)");
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
    fn display_is_family_stable() {
        // For realistic magnitudes a value authored in a decimal unit tends to
        // render back in that family rather than crossing into an
        // equal-but-surprising binary unit. This is a tendency, not a theorem.
        assert_eq!(ByteSize::from_bytes(16 * GB).to_string(), "16GB");
        assert_eq!(ByteSize::from_bytes(48 * GIB).to_string(), "48GiB");
        assert_eq!(ByteSize::from_bytes(1024).to_string(), "1KiB");
        assert_eq!(ByteSize::from_bytes(0).to_string(), "0B");
        // A non-round value has no coarse divisor and falls back to bytes.
        assert_eq!(ByteSize::from_bytes(7 * MIB + 3).to_string(), "7340035B");
    }

    #[test]
    fn human_rounds_for_display() {
        assert_eq!(ByteSize::from_bytes(12_385_752_662).human(), "11.5 GiB");
        assert_eq!(ByteSize::from_bytes(12_385_752_662).approx(), "~11.5 GiB");
        assert_eq!(ByteSize::from_bytes(48 * GIB).human(), "48.0 GiB");
        assert_eq!(ByteSize::from_bytes(512 * MIB).human(), "512.0 MiB");
        assert_eq!(ByteSize::from_bytes(1536).human(), "1.5 KiB");
        assert_eq!(ByteSize::from_bytes(0).human(), "0 B");
        assert!(ByteSize::from_bytes(u64::MAX).human().ends_with("GiB"));
    }

    #[test]
    fn decimal_authored_strings_round_trip_verbatim() {
        for s in ["16GB", "48GiB", "1024", "0B", "512MiB"] {
            let size: ByteSize = s.parse().unwrap();
            let reparsed: ByteSize = size.to_string().parse().unwrap();
            assert_eq!(size, reparsed);
        }
        // Authored decimal unit stays decimal on display.
        assert_eq!("16GB".parse::<ByteSize>().unwrap().to_string(), "16GB");
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
