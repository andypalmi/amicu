//! Deterministic, provider-safe resource names derived from a project id and a
//! deployment id.
//!
//! Shared by every cloud backend that names the thing it deploys (Modal apps,
//! RunPod endpoints) so the same deployment gets the same readable, collision
//! resistant name everywhere, and a stop handle can be re-derived from state
//! alone. Pure string functions: no I/O, no platform or toolchain dependence.

/// Deployment-name-safe app name: `amicu-<sanitized deployment id>`.
///
/// Lower-cases `deployment_id`, replaces every byte that is not
/// `[a-z0-9-]` with `-`, collapses repeats, and trims leading/trailing
/// dashes - conservative enough to satisfy any reasonable "deployment name"
/// constraint without needing Modal's exact validation regex (not published
/// as of this writing).
///
/// Lossy (`a_b` and `a-b` collide) and not project-scoped: only for state that
/// predates [`project_app_name`], whose recorded name must keep working.
#[must_use]
pub fn app_name_for(deployment_id: &str) -> String {
    let mut out = String::with_capacity(deployment_id.len() + 6);
    out.push_str("amicu-");
    let mut last_was_dash = true; // swallow a leading separator too
    for byte in deployment_id.to_ascii_lowercase().bytes() {
        let ch = if byte.is_ascii_alphanumeric() {
            byte as char
        } else {
            '-'
        };
        if ch == '-' {
            if !last_was_dash {
                out.push('-');
            }
            last_was_dash = true;
        } else {
            out.push(ch);
            last_was_dash = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The longest app name amicu asks Modal for.
const MAX_APP_NAME: usize = 63;

/// The most of the project id that appears in an app name.
const MAX_PROJECT_SLUG: usize = 16;

/// App name for `deployment_id` in the project `project_id`:
/// `amicu-<project>-<readable slug>-<12 hex digits>`.
///
/// Unlike [`app_name_for`], two deployments never share a name by sanitizing
/// alike (`a_b` and `a-b`), and two projects never share one for the same
/// deployment id: the readable part is only for humans, and the suffix is a
/// stable FNV-1a digest of the exact project and deployment ids. Deterministic
/// (never depends on the platform or the toolchain), so a stop handle can be
/// derived from state alone. Always `[a-z0-9-]`, at most 63 bytes.
#[must_use]
pub fn project_app_name(project_id: &str, deployment_id: &str) -> String {
    // Capped so a long project id cannot push the name past Modal's limit; the
    // digest below still covers the whole id.
    let mut project = slug(project_id);
    project.truncate(MAX_PROJECT_SLUG);
    while project.ends_with('-') {
        project.pop();
    }
    let digest = fnv1a(&[project_id.as_bytes(), &[0], deployment_id.as_bytes()]);
    let hex = format!("{:012x}", digest & 0xffff_ffff_ffff);
    let head = if project.is_empty() {
        "amicu".to_owned()
    } else {
        format!("amicu-{project}")
    };
    // `<head>-<slug>-<hex>`; the slug gives way first when the name is long.
    let room = MAX_APP_NAME.saturating_sub(head.len() + hex.len() + 2);
    let mut readable = slug(deployment_id);
    readable.truncate(room);
    while readable.ends_with('-') {
        readable.pop();
    }
    if readable.is_empty() {
        format!("{head}-{hex}")
    } else {
        format!("{head}-{readable}-{hex}")
    }
}

/// Lower-cased, `[a-z0-9-]` only, repeats collapsed, no edge dashes.
fn slug(s: &str) -> String {
    let full = app_name_for(s);
    full.strip_prefix("amicu-").unwrap_or_default().to_owned()
}

/// 64-bit FNV-1a over the concatenation of `parts`.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in parts.iter().flat_map(|p| p.iter()) {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Endpoint name for `deployment_id` in the project `project_id`. The same
/// scheme as [`project_app_name`] (RunPod endpoints take the same
/// `[a-z0-9-]`, at most 63 byte names), under the name the RunPod backend uses.
#[must_use]
pub fn project_endpoint_name(project_id: &str, deployment_id: &str) -> String {
    project_app_name(project_id, deployment_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_name_sanitizes_and_prefixes() {
        assert_eq!(app_name_for("support-chat"), "amicu-support-chat");
        assert_eq!(app_name_for("Support_Chat.01"), "amicu-support-chat-01");
        assert_eq!(app_name_for("  weird//id--"), "amicu-weird-id");
    }

    #[test]
    fn project_app_names_are_distinct_per_project_and_per_exact_id() {
        let a = project_app_name("aaaa1111", "a_b");
        let b = project_app_name("aaaa1111", "a-b");
        assert_ne!(a, b, "ids that sanitize alike must not collide");
        assert_ne!(a, project_app_name("bbbb2222", "a_b"));
        assert_ne!(project_app_name("p", "Chat"), project_app_name("p", "chat"));
        assert_eq!(a, project_app_name("aaaa1111", "a_b"), "deterministic");
        assert!(a.starts_with("amicu-aaaa1111-a-b-"), "{a}");
    }

    #[test]
    fn a_long_project_id_is_capped_but_still_tells_projects_apart() {
        let (a, b) = ("p".repeat(60) + "a", "p".repeat(60) + "b");
        let (na, nb) = (project_app_name(&a, "chat"), project_app_name(&b, "chat"));
        assert!(na.len() <= 63 && nb.len() <= 63, "{na} {nb}");
        assert_ne!(na, nb, "the digest covers the whole project id");
    }

    #[test]
    fn project_app_names_are_stable_and_modal_safe() {
        // Pinned below: the name is recorded in state, so it must never drift.
        assert_eq!(
            project_app_name("aaaa1111", "support-chat"),
            "amicu-aaaa1111-support-chat-e285b9df4c23"
        );
        for (p, d) in [
            ("", ""),
            ("--", "//"),
            ("P.1", &"x".repeat(200)[..]),
            (&"p".repeat(100)[..], "dep"),
            (&"p".repeat(100)[..], &"d".repeat(100)[..]),
            ("aaaa1111", "Support_Chat.01"),
        ] {
            let n = project_app_name(p, d);
            assert!(n.len() <= 63, "{n}");
            assert!(n.starts_with("amicu-"), "{n}");
            assert!(
                n.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
                "{n}"
            );
            assert!(!n.ends_with('-') && !n.contains("--"), "{n}");
        }
    }

    #[test]
    fn endpoint_name_matches_app_name() {
        assert_eq!(
            project_endpoint_name("aaaa1111", "support-chat"),
            project_app_name("aaaa1111", "support-chat")
        );
    }
}
