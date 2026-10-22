//! Format checks for the ids GCP calls take. Shape only: whether a project,
//! region or zone exists (or offers a GPU) is the catalog's and the API's call.

use crate::error::GcpError;

fn invalid(kind: &'static str, value: &str, reason: &'static str) -> GcpError {
    GcpError::Invalid {
        kind,
        value: value.to_owned(),
        reason,
    }
}

/// A project id: 6 to 30 characters of `[a-z0-9-]`, starting with a letter and
/// not ending with a hyphen. (The numeric project number is not accepted.)
pub fn validate_project_id(id: &str) -> Result<(), GcpError> {
    let kind = "project id";
    if !(6..=30).contains(&id.len()) {
        return Err(invalid(kind, id, "must be 6 to 30 characters"));
    }
    if !id.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(invalid(kind, id, "must start with a lowercase letter"));
    }
    if id.ends_with('-') {
        return Err(invalid(kind, id, "must not end with a hyphen"));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(invalid(
            kind,
            id,
            "only lowercase letters, digits and hyphens",
        ));
    }
    Ok(())
}

/// A region such as `us-central1` or `europe-west4`: `<geo>-<area><number>`.
pub fn validate_region(region: &str) -> Result<(), GcpError> {
    let ok = region.split_once('-').is_some_and(|(geo, rest)| {
        let area = rest.trim_end_matches(|c: char| c.is_ascii_digit());
        let lower = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase());
        lower(geo) && lower(area) && area.len() < rest.len()
    });
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "region",
            region,
            "expected a name like `us-central1`",
        ))
    }
}

/// A zone such as `us-central1-a`: a [region](validate_region), a hyphen and one
/// lowercase letter.
pub fn validate_zone(zone: &str) -> Result<(), GcpError> {
    zone_region(zone).map(|_| ())
}

/// The region a zone belongs to (`us-central1-a` is in `us-central1`, the AI
/// zone `europe-west4-ai1a` in `europe-west4`).
pub fn zone_region(zone: &str) -> Result<&str, GcpError> {
    let bad = || invalid("zone", zone, "expected a name like `us-central1-a`");
    let (region, suffix) = zone.rsplit_once('-').ok_or_else(bad)?;
    if !is_zone_suffix(suffix) {
        return Err(bad());
    }
    validate_region(region).map_err(|_| bad())?;
    Ok(region)
}

/// `a`, or an AI zone's `ai<digits><letter>`.
fn is_zone_suffix(suffix: &str) -> bool {
    let letter = |s: &str| s.len() == 1 && s.starts_with(|c: char| c.is_ascii_lowercase());
    match suffix.strip_prefix("ai") {
        Some(rest) => {
            let digits = rest.trim_end_matches(|c: char| c.is_ascii_lowercase());
            !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit())
                && letter(&rest[digits.len()..])
        }
        None => letter(suffix),
    }
}

/// A name for an instance, Cloud Run service or operation: RFC 1035, 1 to 63
/// characters of `[a-z0-9-]`, starting with a letter and not ending with a
/// hyphen. Also guarantees the name is a safe single URL path segment.
pub fn validate_resource_name(name: &str) -> Result<(), GcpError> {
    let kind = "resource name";
    if name.is_empty() || name.len() > 63 {
        return Err(invalid(kind, name, "must be 1 to 63 characters"));
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(invalid(kind, name, "must start with a lowercase letter"));
    }
    if name.ends_with('-') {
        return Err(invalid(kind, name, "must not end with a hyphen"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(invalid(
            kind,
            name,
            "only lowercase letters, digits and hyphens",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_ids() {
        for ok in [
            "my-project",
            "amicu-123456",
            "a23456",
            "a".repeat(30).as_str(),
        ] {
            assert!(validate_project_id(ok).is_ok(), "{ok}");
        }
        for bad in [
            "short",
            "1starts-digit",
            "ends-with-",
            "Upper-case",
            "under_score",
            "has space",
            "a".repeat(31).as_str(),
            "",
        ] {
            assert!(validate_project_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn regions() {
        for ok in [
            "us-central1",
            "europe-west4",
            "me-central2",
            "northamerica-northeast1",
        ] {
            assert!(validate_region(ok).is_ok(), "{ok}");
        }
        for bad in [
            "us",
            "us-central",
            "us-central1-a",
            "US-central1",
            "-east1",
            "us-1",
            "",
            "us-central1/x",
        ] {
            assert!(validate_region(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn zones_and_their_region() {
        assert_eq!(zone_region("us-central1-a").unwrap(), "us-central1");
        assert_eq!(zone_region("europe-west4-c").unwrap(), "europe-west4");
        assert!(validate_zone("us-east5-b").is_ok());
        assert_eq!(zone_region("europe-west4-ai1a").unwrap(), "europe-west4");
        for bad in [
            "europe-west4-ai",
            "europe-west4-aia",
            "europe-west4-ai1",
            "europe-west4-ai1ab",
            "us-central1",
            "us-central1-",
            "us-central1-ab",
            "us-central1-1",
            "central1-a",
            "us-central-a",
            "",
        ] {
            assert!(validate_zone(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn resource_names_are_single_safe_segments() {
        assert!(validate_resource_name("operation-1700000000-abc").is_ok());
        for bad in ["", "Op", "1op", "op-", "a/b", "a..b", "a?b=c", "a b"] {
            assert!(validate_resource_name(bad).is_err(), "{bad}");
        }
        assert!(validate_resource_name(&"a".repeat(64)).is_err());
    }
}
