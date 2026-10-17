//! A small Prometheus text-exposition (v0.0.4) parser, shared by
//! `amicu modal metrics` and the telemetry agent.
//!
//! Only what the vLLM and llama.cpp `/metrics` endpoints emit is needed:
//! sample lines with optional labels and an optional trailing timestamp.
//! Comments, blank lines and malformed lines are skipped, never fatal.

/// One exposition-format sample line.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Sample {
    /// The value of label `key`, if present.
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Parses Prometheus text exposition (v0.0.4). Comments, blank lines and
/// malformed lines are skipped; a trailing timestamp is ignored.
#[must_use]
pub fn parse_prometheus(text: &str) -> Vec<Sample> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Sample> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let name_end = line.find(['{', ' ', '\t']).unwrap_or(line.len());
    let name = &line[..name_end];
    if name.is_empty() {
        return None;
    }
    let mut rest = &line[name_end..];
    let mut labels = Vec::new();
    if rest.starts_with('{') {
        let (parsed, after) = parse_labels(&rest[1..])?;
        labels = parsed;
        rest = after;
    }
    let value = rest.split_whitespace().next()?.parse::<f64>().ok()?;
    Some(Sample {
        name: name.to_owned(),
        labels,
        value,
    })
}

/// Parses `k="v",k2="v2"}` (input starts after the `{`); returns the labels
/// and the text after the closing brace.
fn parse_labels(mut s: &str) -> Option<(Vec<(String, String)>, &str)> {
    let mut labels = Vec::new();
    loop {
        s = s.trim_start_matches([' ', ',']);
        if let Some(after) = s.strip_prefix('}') {
            return Some((labels, after));
        }
        let eq = s.find('=')?;
        let key = s[..eq].trim().to_owned();
        s = s[eq + 1..].strip_prefix('"')?;
        let mut value = String::new();
        let mut chars = s.char_indices();
        let end = loop {
            let (i, c) = chars.next()?;
            match c {
                '"' => break i,
                '\\' => match chars.next()?.1 {
                    'n' => value.push('\n'),
                    other => value.push(other),
                },
                c => value.push(c),
            }
        };
        labels.push((key, value));
        s = &s[end + 1..];
    }
}

/// Sum of every finite sample called `name` (across label sets); `None` when
/// there is none, or the total is not finite.
#[must_use]
pub fn sum(samples: &[Sample], name: &str) -> Option<f64> {
    let mut found = false;
    let total: f64 = samples
        .iter()
        .filter(|s| s.name == name)
        .inspect(|_| found = true)
        .map(|s| s.value)
        .sum();
    found.then_some(total).filter(|t| t.is_finite())
}

/// The first of `names` that is present, summed.
#[must_use]
pub fn first_sum(samples: &[Sample], names: &[&str]) -> Option<f64> {
    names.iter().find_map(|n| sum(samples, n))
}

/// The largest finite sample called `name`.
#[must_use]
pub fn max(samples: &[Sample], name: &str) -> Option<f64> {
    samples
        .iter()
        .filter(|s| s.name == name && s.value.is_finite())
        .map(|s| s.value)
        .reduce(f64::max)
}

/// The cumulative buckets of histogram `base` as `(le, count)` pairs, sorted
/// by bound and merged across label sets sharing a bound (several engines).
/// The `+Inf` bucket is `f64::INFINITY`. Empty when the histogram is absent.
#[must_use]
pub fn cumulative_buckets(samples: &[Sample], base: &str) -> Vec<(f64, f64)> {
    let bucket_name = format!("{base}_bucket");
    let mut buckets: Vec<(f64, f64)> = samples
        .iter()
        .filter(|s| s.name == bucket_name && s.value.is_finite())
        .filter_map(|s| {
            let le = match s.label("le")? {
                "+Inf" | "Inf" => f64::INFINITY,
                other => other.parse::<f64>().ok()?,
            };
            Some((le, s.value))
        })
        .collect();
    buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
    buckets.dedup_by(|dup, kept| {
        let same = dup.0.total_cmp(&kept.0).is_eq();
        if same {
            kept.1 += dup.1;
        }
        same
    });
    buckets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_labels_escapes_values_and_skips_noise() {
        let text = "# c\n\nfoo{a=\"x y\",b=\"q\\\"z\\\\\",} 1.5 1700000000\nbar 2\nbroken{\nbaz NaN\nnovalue\n";
        let s = parse_prometheus(text);
        assert_eq!(s[0].name, "foo");
        assert_eq!(s[0].label("a"), Some("x y"));
        assert_eq!(s[0].label("b"), Some("q\"z\\"));
        assert!((s[0].value - 1.5).abs() < f64::EPSILON);
        assert_eq!(s[1].name, "bar");
        assert!(s[2].value.is_nan());
        assert_eq!(s.len(), 3, "malformed lines are skipped: {s:?}");
    }

    #[test]
    fn buckets_merge_engines_and_sort() {
        let t = "h_bucket{engine=\"1\",le=\"+Inf\"} 4\nh_bucket{engine=\"0\",le=\"0.5\"} 1\n\
                 h_bucket{engine=\"1\",le=\"0.5\"} 2\nh_bucket{engine=\"0\",le=\"+Inf\"} 3\n";
        let b = cumulative_buckets(&parse_prometheus(t), "h");
        assert_eq!(b, vec![(0.5, 3.0), (f64::INFINITY, 7.0)]);
        assert_eq!(cumulative_buckets(&parse_prometheus(t), "other"), vec![]);
    }

    #[test]
    fn sums_and_maxes_skip_non_finite() {
        let s = parse_prometheus("a 1\na 2\nb NaN\nc 3\nc 7\n");
        assert_eq!(sum(&s, "a"), Some(3.0));
        assert_eq!(sum(&s, "b"), None);
        assert_eq!(max(&s, "c"), Some(7.0));
        assert_eq!(first_sum(&s, &["zz", "c"]), Some(10.0));
    }
}
