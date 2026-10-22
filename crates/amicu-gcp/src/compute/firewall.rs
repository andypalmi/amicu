//! The ingress firewall rule for the serving port.
//!
//! Firewall rules have no labels (the v1 `Firewall` resource has no `labels`
//! field), so ownership is a marker at the start of the rule's `description`
//! plus a name derived from the instance name. Only rules whose description
//! carries the owner marker are updated or deleted; a rule of the same name
//! without it is an error and is left alone. The rule is added to the `default`
//! network, which is never modified.

use amicu_providers::HttpMethod;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{COMPUTE_BASE_URL, GcpClient};
use crate::error::GcpError;
use crate::operations::{PollTiming, wait_compute_global_operation};
use crate::validate::{validate_project_id, validate_resource_name};

/// Rule priority (Google's default).
const PRIORITY: u32 = 1000;

/// An ingress rule to the serving port of the VMs carrying `network_tag`.
#[derive(Debug, Clone)]
pub struct FirewallSpec {
    pub project: String,
    /// See [`firewall_name`].
    pub name: String,
    pub port: u16,
    /// Who may connect, as CIDR ranges (for example the caller's `203.0.113.4/32`).
    /// `0.0.0.0/0` opens the port to the world: every route but `GET /health`
    /// still needs the API key, but prefer a narrow range.
    pub source_ranges: Vec<String>,
    /// The tag the VM carries ([`crate::compute::VmSpec::network_tag`]).
    pub network_tag: String,
    /// The owner marker of the deployment.
    pub owner: String,
}

/// The rule name for an instance: its name with a `-fw` suffix, cut to 63
/// characters.
///
/// # Errors
/// [`GcpError::Invalid`] for a bad instance name.
pub fn firewall_name(instance_name: &str) -> Result<String, GcpError> {
    validate_resource_name(instance_name)?;
    let mut base = instance_name.to_owned();
    base.truncate(60);
    while base.ends_with('-') {
        base.pop();
    }
    Ok(format!("{base}-fw"))
}

impl FirewallSpec {
    fn description(&self) -> String {
        format!(
            "{}; created by amicu for the vLLM serving port; do not edit",
            description_prefix(&self.owner)
        )
    }

    fn validate(&self) -> Result<(), GcpError> {
        validate_project_id(&self.project)?;
        validate_resource_name(&self.name)?;
        validate_resource_name(&self.network_tag)?;
        let bad = |value: &str, reason| GcpError::Invalid {
            kind: "firewall",
            value: value.to_owned(),
            reason,
        };
        if self.owner.is_empty() || !self.owner.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(bad(&self.owner, "bad owner marker"));
        }
        if self.port == 0 {
            return Err(bad("0", "port must not be 0"));
        }
        if self.source_ranges.is_empty() {
            return Err(bad("", "at least one source range is required"));
        }
        for r in &self.source_ranges {
            let ok = r.contains('/')
                && r.bytes()
                    .all(|b| b.is_ascii_hexdigit() || matches!(b, b'.' | b':' | b'/'));
            if !ok {
                return Err(bad(r, "source ranges are CIDR blocks like 203.0.113.4/32"));
            }
        }
        Ok(())
    }

    fn insert_body(&self) -> Value {
        let mut body = json!({
            "name": self.name,
            "description": self.description(),
            "network": "global/networks/default",
            "direction": "INGRESS",
            "priority": PRIORITY,
        });
        body.as_object_mut()
            .expect("an object")
            .extend(self.mutable_fields());
        body
    }

    /// The fields a `PATCH` may change.
    fn mutable_fields(&self) -> serde_json::Map<String, Value> {
        let Value::Object(m) = json!({
            "allowed": [{ "IPProtocol": "tcp", "ports": [self.port.to_string()] }],
            "sourceRanges": self.source_ranges,
            "targetTags": [self.network_tag],
        }) else {
            unreachable!("a JSON object literal");
        };
        m
    }
}

fn description_prefix(owner: &str) -> String {
    format!("amicu-owner={owner}")
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RuleView {
    description: Option<String>,
}

fn owned(rule: &RuleView, owner: &str) -> bool {
    let prefix = description_prefix(owner);
    rule.description
        .as_deref()
        .is_some_and(|d| d == prefix || d.starts_with(&format!("{prefix};")))
}

fn rules_url(project: &str) -> String {
    format!("{COMPUTE_BASE_URL}/projects/{project}/global/firewalls")
}

/// Creates the rule, or brings our own rule of that name up to date (a `409`
/// on a rule with the owner marker patches its ports, ranges and tag).
///
/// # Errors
/// [`GcpError::Conflict`] when a rule of that name exists without the marker
/// (it is not touched), or any API error.
pub async fn create_firewall(
    client: &GcpClient,
    spec: &FirewallSpec,
    timing: PollTiming,
) -> Result<(), GcpError> {
    spec.validate()?;
    let url = rules_url(&spec.project);
    match client
        .request(HttpMethod::Post, &url, Some(spec.insert_body()))
        .await
    {
        Ok(op) => wait(client, &spec.project, &op, timing).await,
        Err(GcpError::Conflict { message }) => {
            let rule: RuleView = client
                .request_as(HttpMethod::Get, &format!("{url}/{}", spec.name), None)
                .await
                .map_err(|e| {
                    if e.is_not_found() {
                        GcpError::Conflict {
                            message: message.clone(),
                        }
                    } else {
                        e
                    }
                })?;
            if !owned(&rule, &spec.owner) {
                return Err(GcpError::Conflict {
                    message: format!(
                        "a firewall rule named `{}` already exists without amicu's marker; \
                         it is left alone",
                        spec.name
                    ),
                });
            }
            let op = client
                .request(
                    HttpMethod::Patch,
                    &format!("{url}/{}", spec.name),
                    Some(Value::Object(spec.mutable_fields())),
                )
                .await?;
            wait(client, &spec.project, &op, timing).await
        }
        Err(e) => Err(e),
    }
}

/// Deletes the rule when its description carries `owner`, then confirms with a
/// `get` that ends in `404`. An absent rule is fine.
///
/// # Errors
/// [`GcpError::NotOwned`] (nothing deleted) for a rule without the marker, or
/// any API error.
pub async fn delete_firewall(
    client: &GcpClient,
    project: &str,
    name: &str,
    owner: &str,
    timing: PollTiming,
) -> Result<(), GcpError> {
    validate_project_id(project)?;
    validate_resource_name(name)?;
    let url = format!("{}/{name}", rules_url(project));
    let rule: RuleView = match client.request_as(HttpMethod::Get, &url, None).await {
        Ok(r) => r,
        Err(e) if e.is_not_found() => return Ok(()),
        Err(e) => return Err(e),
    };
    if !owned(&rule, owner) {
        return Err(GcpError::NotOwned {
            what: format!("firewall rule {name}"),
        });
    }
    match client.request(HttpMethod::Delete, &url, None).await {
        Ok(op) => wait(client, project, &op, timing).await?,
        Err(e) if e.is_not_found() => {}
        Err(e) => return Err(e),
    }
    match client.request(HttpMethod::Get, &url, None).await {
        Err(e) if e.is_not_found() => Ok(()),
        Ok(_) => Err(GcpError::OperationTimeout {
            operation: format!("delete firewall rule {name}"),
        }),
        Err(e) => Err(e),
    }
}

async fn wait(
    client: &GcpClient,
    project: &str,
    op: &Value,
    timing: PollTiming,
) -> Result<(), GcpError> {
    let name = op["name"]
        .as_str()
        .ok_or_else(|| GcpError::Decode("firewall call returned no operation name".into()))?;
    wait_compute_global_operation(client, project, name, timing)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::tests::{Mock, client};
    use crate::compute::vm::tests::fast;

    const RULES: &str = "/projects/my-proj/global/firewalls";
    const RULE: &str = "/projects/my-proj/global/firewalls/amicu-demo-fw";
    const OP_WAIT: &str = "/projects/my-proj/global/operations/op-9/wait";
    const OP: &str = r#"{"name":"op-9","status":"RUNNING"}"#;
    const DONE: &str = r#"{"name":"op-9","status":"DONE"}"#;

    fn spec() -> FirewallSpec {
        FirewallSpec {
            project: "my-proj".into(),
            name: "amicu-demo-fw".into(),
            port: 8000,
            source_ranges: vec!["203.0.113.4/32".into()],
            network_tag: "amicu-vllm".into(),
            owner: "abc123".into(),
        }
    }

    #[test]
    fn name_is_derived_and_bounded() {
        assert_eq!(firewall_name("amicu-demo-n1").unwrap(), "amicu-demo-n1-fw");
        let long = firewall_name(&"a".repeat(63)).unwrap();
        assert!(long.len() <= 63 && long.ends_with("-fw"));
        assert!(firewall_name("Bad_Name").is_err());
    }

    #[tokio::test]
    async fn create_posts_an_owned_ingress_rule_on_the_default_network() {
        let (c, mock) = client(Mock::default().on(HttpMethod::Post, RULES, 200, OP).on(
            HttpMethod::Post,
            OP_WAIT,
            200,
            DONE,
        ));
        create_firewall(&c, &spec(), fast()).await.unwrap();
        let body = mock.requests()[0].body.clone().unwrap();
        assert_eq!(body["network"], "global/networks/default");
        assert_eq!(body["direction"], "INGRESS");
        assert_eq!(
            body["allowed"],
            json!([{"IPProtocol": "tcp", "ports": ["8000"]}])
        );
        assert_eq!(body["sourceRanges"], json!(["203.0.113.4/32"]));
        assert_eq!(body["targetTags"], json!(["amicu-vllm"]));
        assert!(
            body["description"]
                .as_str()
                .unwrap()
                .starts_with("amicu-owner=abc123;")
        );
        assert!(body.get("labels").is_none());
    }

    #[tokio::test]
    async fn conflict_patches_our_rule_and_refuses_a_foreign_one() {
        let conflict = r#"{"error":{"code":409,"message":"exists"}}"#;
        let (c, mock) = client(
            Mock::default()
                .on(HttpMethod::Post, RULES, 409, conflict)
                .on(
                    HttpMethod::Get,
                    RULE,
                    200,
                    r#"{"description":"amicu-owner=abc123; created by amicu"}"#,
                )
                .on(HttpMethod::Patch, RULE, 200, OP)
                .on(HttpMethod::Post, OP_WAIT, 200, DONE),
        );
        create_firewall(&c, &spec(), fast()).await.unwrap();
        assert!(
            mock.requests()
                .iter()
                .any(|r| r.method == HttpMethod::Patch)
        );

        let (c, mock) = client(
            Mock::default()
                .on(HttpMethod::Post, RULES, 409, conflict)
                .on(
                    HttpMethod::Get,
                    RULE,
                    200,
                    r#"{"description":"managed by terraform"}"#,
                ),
        );
        assert!(matches!(
            create_firewall(&c, &spec(), fast()).await,
            Err(GcpError::Conflict { .. })
        ));
        assert!(
            mock.requests()
                .iter()
                .all(|r| !matches!(r.method, HttpMethod::Patch | HttpMethod::Delete))
        );
    }

    #[tokio::test]
    async fn delete_removes_an_owned_rule_and_confirms_absence() {
        let (c, mock) = client(
            Mock::default()
                .on(
                    HttpMethod::Get,
                    RULE,
                    200,
                    r#"{"description":"amicu-owner=abc123; x"}"#,
                )
                .on(HttpMethod::Get, RULE, 404, "{}")
                .on(HttpMethod::Delete, RULE, 200, OP)
                .on(HttpMethod::Post, OP_WAIT, 200, DONE),
        );
        delete_firewall(&c, "my-proj", "amicu-demo-fw", "abc123", fast())
            .await
            .unwrap();
        assert_eq!(
            mock.requests().last().unwrap().method,
            HttpMethod::Get,
            "confirmed with a get"
        );
    }

    #[tokio::test]
    async fn delete_never_touches_other_rules() {
        for desc in [
            r#"{"description":"amicu-owner=other; x"}"#,
            r#"{"description":"amicu-owner=abc1234; x"}"#,
            r"{}",
        ] {
            let (c, mock) = client(Mock::default().on(HttpMethod::Get, RULE, 200, desc));
            assert!(matches!(
                delete_firewall(&c, "my-proj", "amicu-demo-fw", "abc123", fast()).await,
                Err(GcpError::NotOwned { .. })
            ));
            assert!(mock.requests().iter().all(|r| r.method == HttpMethod::Get));
        }
        let (c, _) = client(Mock::default().on(HttpMethod::Get, RULE, 404, "{}"));
        delete_firewall(&c, "my-proj", "amicu-demo-fw", "abc123", fast())
            .await
            .unwrap();
    }

    #[test]
    fn source_ranges_are_required_and_cidr() {
        let mut s = spec();
        s.source_ranges.clear();
        assert!(s.validate().is_err());
        s.source_ranges = vec!["1.2.3.4".into()];
        assert!(s.validate().is_err());
        s.source_ranges = vec!["2001:db8::/32".into()];
        assert!(s.validate().is_ok());
    }
}
