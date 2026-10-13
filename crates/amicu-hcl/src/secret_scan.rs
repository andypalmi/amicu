//! Suppressible, high-confidence warning heuristic for secrets pasted into
//! genuinely free-text HCL content (amicu-planning#119).
//!
//! This is the residual case the structural `secret()` enforcement in
//! [`crate::de`] cannot cover: a field that legitimately holds arbitrary
//! prose (`description`, `display_name`) or a comment has nothing to anchor
//! a "reject literal" rule to. There is no schema to be directed by, so
//! instead of a hard reject this scans for a small set of high-confidence
//! secret *shapes* - an AWS access key, an OpenAI-style API key, a PEM
//! private-key header, a JWT, or a long high-entropy token - and emits a
//! `Warning` diagnostic, never an error. A trailing `# amicu:allow` (or
//! `// amicu:allow`) comment on the same line suppresses it, mirroring
//! `gitleaks:allow`.
//!
//! This operates on the raw source text rather than the parsed [`Graph`],
//! because HCL comments (including the suppression marker) do not survive
//! parsing, and because keeping it independent of `parse_graph` means a
//! malformed document can still be scanned for leaks.
//!
//! [`Graph`]: amicu_core::graph::Graph

use std::collections::HashMap;

use amicu_core::diagnostic::{Diagnostic, Severity, SourceLocation, catalog};
use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;
use amicu_core::resource::Runtime;

/// Attribute keys whose string value is genuinely free text (as opposed to
/// an id, enum token, or reference) and is therefore scanned. Kept
/// intentionally small: any field that legitimately holds a secret is
/// already structurally enforced (`Fields::secret_ref` / `secret_map` in
/// [`crate::de`]), so scanning more attributes than this would only add
/// noise, not safety.
const FREE_TEXT_KEYS: [&str; 2] = ["description", "display_name"];

/// The suppression marker, matched anywhere inside a same-line comment.
const ALLOW_MARKER: &str = "amicu:allow";

/// Scan raw HCL source for high-confidence secret patterns in free-text
/// attribute values or comments, returning one `Warning`-severity
/// [`Diagnostic`] (`AMICU-POLICY-005`) per unsuppressed hit.
///
/// # Limitations
///
/// This is a line-oriented scan: a multi-line (heredoc) `description` value
/// is not inspected. Given the narrow, high-confidence pattern set, that is
/// an acceptable v1 gap rather than a soundness issue - it is a warning
/// heuristic on an irreducible residual, not a completeness guarantee.
#[must_use]
pub fn scan_secret_leaks(src: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (idx, line) in src.lines().enumerate() {
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let parts = split_comment(line);

        if parts.comment.is_some_and(|c| c.contains(ALLOW_MARKER)) {
            continue;
        }

        if let Some((key, value)) = free_text_attr_value(parts.code) {
            scan_text(value, line_no, key, &mut out);
        }
        if let Some(comment) = parts.comment {
            scan_text(comment, line_no, "comment", &mut out);
        }
    }
    out
}

/// Scan a runtime settings block's `env` map values (the llama.cpp / vLLM
/// `extra_args`/`env` escape hatch, amicu-planning#150) for the same
/// high-confidence secret shapes [`scan_secret_leaks`] looks for in free
/// text, returning one `Warning`-severity [`Diagnostic`] (`AMICU-POLICY-006`)
/// per match.
///
/// Unlike [`scan_secret_leaks`], this runs on the parsed [`Graph`] rather
/// than raw source text: a runtime `env` value is a structured map, not
/// free-form prose, so there is no comment to anchor a suppression marker to
/// and no line number to report. The same structural-vs-heuristic split
/// `credential.env` secret-designation draws (amicu-planning#119) applies
/// here too, just one notch softer: unlike a `credential`, a runtime `env`
/// map is not schema-directed to be secret-designated (it legitimately
/// carries non-secret tuning knobs like `GGML_CUDA_DISABLE_GRAPHS=1`
/// alongside the occasional pasted secret), so this is a nudge, not a
/// rejection.
#[must_use]
pub fn scan_runtime_env_secrets(graph: &Graph) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for deployment in &graph.deployments {
        let (block, env, extra_args) = match &deployment.runtime {
            Runtime::LlamaCpp(s) => ("llama_cpp", &s.env, &s.extra_args),
            Runtime::Vllm(s) => ("vllm", &s.env, &s.extra_args),
        };
        for (key, value) in env {
            if looks_like_secret_value(value)
                || (is_secret_name(key) && is_literal_secret_candidate(value))
            {
                out.push(env_secret_finding(&deployment.id, block, key, value));
            }
        }
        // `extra_args` land in argv (and in state / the service plist), so
        // check both `--flag=value` and `--flag value` forms (#225).
        for (idx, arg) in extra_args.iter().enumerate() {
            let path = format!("{block}.extra_args[{idx}]");
            let hit = if let Some((flag, value)) = arg.split_once('=') {
                looks_like_secret_value(value)
                    || (is_secret_name(flag) && is_literal_secret_candidate(value))
            } else if arg.starts_with('-') {
                extra_args.get(idx + 1).is_some_and(|next| {
                    !next.starts_with('-')
                        && (looks_like_secret_value(next)
                            || (is_secret_name(arg) && is_literal_secret_candidate(next)))
                })
            } else {
                looks_like_secret_value(arg)
            };
            if hit {
                out.push(secret_finding_at(&deployment.id, &path, arg));
            }
        }
    }
    out
}

/// Whether a flag or env-var name designates a secret: any `_`/`-`
/// separated word is `key`, `token`, `secret`, `password`, `passwd` or
/// `apikey` (case-insensitive), unless it names a file/path/dir holding one.
fn is_secret_name(name: &str) -> bool {
    let upper = name.trim_start_matches('-').to_ascii_uppercase();
    let words: Vec<&str> = upper.split(['_', '-']).collect();
    if words.iter().any(|w| matches!(*w, "FILE" | "PATH" | "DIR")) {
        return false;
    }
    words.iter().any(|w| {
        matches!(
            *w,
            "KEY" | "TOKEN" | "SECRET" | "PASSWORD" | "PASSWD" | "APIKEY"
        )
    })
}

/// A value under a secret-named key that is plausibly a real literal: long
/// enough, not a path, and not a plain number or boolean.
fn is_literal_secret_candidate(value: &str) -> bool {
    value.len() >= 8
        && !value.starts_with('/')
        && !value.starts_with("./")
        && !value.starts_with("~/")
        && !value.chars().all(|c| c.is_ascii_digit() || c == '.')
        && !matches!(value.to_ascii_lowercase().as_str(), "true" | "false")
}

/// Build the `AMICU-POLICY-006` finding for one suspicious `env` value.
fn env_secret_finding(
    deployment_id: &DeploymentId,
    block: &str,
    key: &str,
    value: &str,
) -> Diagnostic {
    secret_finding_at(deployment_id, &format!("{block}.env.{key}"), value)
}

/// Build the `AMICU-POLICY-006` finding for the runtime setting at `path`.
fn secret_finding_at(deployment_id: &DeploymentId, path: &str, value: &str) -> Diagnostic {
    let mut d = Diagnostic::new(
        catalog::POLICY_RUNTIME_ENV_SECRET_SUSPECTED,
        Severity::Warning,
        format!(
            "deployment `{deployment_id}` {path} looks like it contains a literal secret ({}). \
             Literals here are stored in state and visible in the process arguments or service \
             definition. Reference the secret instead: put it in a `credential` resource's \
             `env` as `secret(\"NAME\")`, and set `api_key = secret(\"NAME\")` on the \
             deployment for the endpoint key.",
            redact(value)
        ),
    );
    d.resource = Some(format!("deployment.{deployment_id}"));
    d.path = Some(path.to_string());
    d
}

/// A source line split into its code and (if any) trailing comment.
struct LineParts<'a> {
    code: &'a str,
    comment: Option<&'a str>,
}

/// Split a line at the first `#` or `//` that is outside a double-quoted
/// string, so a fragment identifier inside a `secret("vault://...#frag")`
/// call is never mistaken for a comment start.
fn split_comment(line: &str) -> LineParts<'_> {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' if i == 0 || bytes[i - 1] != b'\\' => in_string = !in_string,
            b'#' if !in_string => {
                return LineParts {
                    code: &line[..i],
                    comment: Some(line[i + 1..].trim()),
                };
            }
            b'/' if !in_string && bytes.get(i + 1) == Some(&b'/') => {
                return LineParts {
                    code: &line[..i],
                    comment: Some(line[i + 2..].trim()),
                };
            }
            _ => {}
        }
        i += 1;
    }
    LineParts {
        code: line,
        comment: None,
    }
}

/// If `code` is a single-line `<key> = "<value>"` assignment to one of
/// [`FREE_TEXT_KEYS`], return the key and the unquoted value.
fn free_text_attr_value(code: &str) -> Option<(&str, &str)> {
    let (key, rest) = code.trim_start().split_once('=')?;
    let key = key.trim();
    if !FREE_TEXT_KEYS.contains(&key) {
        return None;
    }
    let rest = rest.trim();
    let value = rest.strip_prefix('"')?.strip_suffix('"')?;
    Some((key, value))
}

/// Run every high-confidence pattern over `text`, pushing at most one
/// finding (the first match) so a single line cannot flood the report.
fn scan_text(text: &str, line: u32, path: &str, out: &mut Vec<Diagnostic>) {
    if let Some(kind) = pem_header(text) {
        out.push(finding(line, path, kind, text));
        return;
    }
    for token in tokenize(text) {
        if let Some(kind) = classify_token(token) {
            out.push(finding(line, path, kind, token));
            return;
        }
    }
}

/// Whether `text` itself has a high-confidence secret *shape* (AWS key,
/// OpenAI-style key, JWT, PEM header, or high-entropy token) - the same
/// detectors [`scan_secret_leaks`] uses on free text.
///
/// Used by [`crate::de::secret_ref_from_expr`] to catch a literal secret
/// smuggled inside a `secret(...)` call argument: the structural check alone
/// only verifies the *syntax* is `secret("...")`, not that the string inside
/// is actually a reference rather than the raw secret value itself
/// (amicu-planning#119 review hardening). This still cannot be zero-false-
/// positive in the abstract - any bare reference name could coincidentally
/// match - but reusing the same narrow, tuned detectors that gate the
/// free-text *warning* keeps genuine references (`vault://kv/...`,
/// `HF_TOKEN`, `hetzner_token`, ...) well clear of the bar while catching the
/// obvious cases: wrapping a real key/token/PEM block in `secret(...)`
/// verbatim.
#[must_use]
pub(crate) fn looks_like_secret_value(text: &str) -> bool {
    pem_header(text).is_some() || tokenize(text).any(|token| classify_token(token).is_some())
}

/// Split text into maximal runs of characters a secret token would plausibly
/// use (alphanumerics plus `+/=_.-`), treating everything else (whitespace,
/// quotes, other punctuation) as a separator.
fn tokenize(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || "+/=_.-".contains(c)))
        .filter(|s| !s.is_empty())
}

fn classify_token(token: &str) -> Option<&'static str> {
    if is_aws_access_key(token) {
        return Some("an AWS access key");
    }
    if is_openai_style_key(token) {
        return Some("an OpenAI-style API key");
    }
    if is_jwt(token) {
        return Some("a JWT");
    }
    if is_vendor_prefixed_token(token) {
        return Some("an access token");
    }
    if is_high_entropy(token) {
        return Some("a high-entropy token");
    }
    None
}

/// `AKIA` followed by 16 uppercase-alphanumeric characters (the standard AWS
/// access key id shape, e.g. the AWS-documentation example
/// `AKIAIOSFODNN7EXAMPLE`).
fn is_aws_access_key(token: &str) -> bool {
    token.len() == 20
        && token.starts_with("AKIA")
        && token[4..]
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// `sk-` followed by at least 20 alphanumeric/`_`/`-` characters, the shape
/// OpenAI-style API keys share.
fn is_openai_style_key(token: &str) -> bool {
    let Some(rest) = token.strip_prefix("sk-") else {
        return false;
    };
    rest.len() >= 20
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Hugging Face (`hf_`) and GitHub (`ghp_`, `gho_`, `ghs_`, `ghu_`,
/// `github_pat_`) tokens: a well-known prefix then a long alphanumeric body.
fn is_vendor_prefixed_token(token: &str) -> bool {
    ["hf_", "ghp_", "gho_", "ghs_", "ghu_", "github_pat_"]
        .iter()
        .any(|prefix| {
            token.strip_prefix(prefix).is_some_and(|rest| {
                rest.len() >= 30 && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        })
}

/// Three dot-separated base64url segments, the first starting with `eyJ`
/// (the base64url encoding of `{"`, i.e. a JSON header) - the JWT shape.
fn is_jwt(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() == 3
        && parts[0].starts_with("eyJ")
        && parts.iter().all(|p| {
            p.len() >= 10
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

/// A long token with real Shannon-entropy density and a mix of upper, lower,
/// and digit characters - random-looking, unlike prose or a dotted
/// identifier. The dot exclusion and length/mix gates keep this from firing
/// on ordinary free text.
fn is_high_entropy(token: &str) -> bool {
    const MIN_LEN: usize = 24;
    const MIN_ENTROPY_BITS_PER_CHAR: f64 = 4.0;

    if token.chars().count() < MIN_LEN || token.contains('.') {
        return false;
    }
    let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    if !(has_upper && has_lower && has_digit) {
        return false;
    }
    shannon_entropy(token) >= MIN_ENTROPY_BITS_PER_CHAR
}

/// Shannon entropy in bits per character.
fn shannon_entropy(s: &str) -> f64 {
    let mut counts: HashMap<char, u32> = HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
    }
    let len = f64::from(u32::try_from(s.chars().count()).unwrap_or(u32::MAX));
    counts.values().fold(0.0, |acc, &count| {
        let p = f64::from(count) / len;
        acc - p * p.log2()
    })
}

/// A PEM private-key header, e.g. `-----BEGIN RSA PRIVATE KEY-----` or
/// `-----BEGIN OPENSSH PRIVATE KEY-----`.
fn pem_header(text: &str) -> Option<&'static str> {
    if text.contains("-----BEGIN") && text.contains("KEY-----") {
        Some("a PEM private key header")
    } else {
        None
    }
}

fn finding(line: u32, path: &str, kind: &str, matched: &str) -> Diagnostic {
    let mut d = Diagnostic::new(
        catalog::POLICY_SECRET_PATTERN_SUSPECTED,
        Severity::Warning,
        format!(
            "{path} on line {line} looks like it contains {kind} ({}). If this is a real \
             secret, move it to a `secret(...)` reference instead. If it is not, suppress this \
             warning with a trailing `# amicu:allow` comment on the same line.",
            redact(matched)
        ),
    );
    d.path = Some(path.to_string());
    d.source = Some(SourceLocation {
        file: String::new(),
        line,
        column: 1,
    });
    d
}

/// Redact matched text before it goes into a diagnostic message: never echo
/// a possible real secret verbatim into logs or CLI output.
fn redact(s: &str) -> String {
    let n = s.chars().count();
    if n <= 6 {
        return "*".repeat(n.max(1));
    }
    let head: String = s.chars().take(3).collect();
    format!("{head}{}", "*".repeat((n - 3).min(8)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_access_key_is_detected() {
        assert!(is_aws_access_key("AKIAIOSFODNN7EXAMPLE"));
        assert!(!is_aws_access_key("AKIA_TOO_SHORT"));
        assert!(!is_aws_access_key("not-an-access-key-at-all"));
    }

    #[test]
    fn openai_style_key_is_detected() {
        assert!(is_openai_style_key("sk-abcdefghijklmnopqrstuvwxyz123456"));
        assert!(!is_openai_style_key("sk-tooshort"));
    }

    #[test]
    fn jwt_is_detected() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        assert!(is_jwt(jwt));
        assert!(!is_jwt("not.a.jwt"));
    }

    #[test]
    fn pem_header_is_detected() {
        assert_eq!(
            pem_header("-----BEGIN RSA PRIVATE KEY-----"),
            Some("a PEM private key header")
        );
        assert_eq!(pem_header("just a normal sentence"), None);
    }

    #[test]
    fn ordinary_prose_is_not_high_entropy() {
        assert!(!is_high_entropy(
            "Customer PII must stay on EU infrastructure"
        ));
    }

    #[test]
    fn split_comment_ignores_hash_inside_string() {
        let parts = split_comment(r#"description = "vault://kv/openai#api_key""#);
        assert!(parts.comment.is_none());
    }

    #[test]
    fn split_comment_finds_trailing_hash_comment() {
        let parts = split_comment("description = \"hello\" # amicu:allow");
        assert_eq!(parts.comment, Some("amicu:allow"));
    }
}
