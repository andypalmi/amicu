//! Secret Manager (`secretmanager.googleapis.com/v1`) for a deployment's
//! secrets: create (owner-labelled), add a version, read the latest version
//! (tests and debugging only), delete, and grant one service account read
//! access on one secret.
//!
//! Coexistence: a secret is created with amicu's owner label, and everything
//! that changes or deletes one (a new version, its IAM policy, the delete) first
//! reads it and checks the label ([`ensure_owned`]). A `409` on a name that is
//! not ours is an error, never adopted. Access is granted on the secret, never
//! on the project: amicu does not touch project-level IAM.
//!
//! Replication: [`Replication::Region`] is the default choice for callers, a
//! `userManaged` policy with one replica in the deployment's own region, so the
//! payload is stored only there (EU residency). `automatic` lets Google store it
//! anywhere. Replication cannot be changed after creation.
//!
//! Secret values are never logged and never put in an error.

use std::collections::BTreeMap;

use amicu_core::secret::SecretValue;
use amicu_providers::HttpMethod;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::GcpClient;
use crate::error::GcpError;
use crate::labels::ensure_owned;
use crate::validate::{validate_project_id, validate_region, validate_resource_name};

/// Secret Manager API root.
pub const SECRETMANAGER_BASE_URL: &str = "https://secretmanager.googleapis.com/v1";
/// The role the VM's service account gets, on each secret only.
pub const ACCESSOR_ROLE: &str = "roles/secretmanager.secretAccessor";

/// Tries of a read-modify-write of a secret's IAM policy when the etag is stale.
const IAM_ATTEMPTS: u32 = 4;

/// Where a secret's payload is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replication {
    /// Google chooses the locations.
    Automatic,
    /// Only this region (a `userManaged` policy with a single replica).
    Region(String),
}

/// The secrets of one deployment, named after its instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    ApiKey,
    HfToken,
    TlsKey,
}

impl SecretKind {
    /// Every kind, in a stable order.
    pub const ALL: [Self; 3] = [Self::ApiKey, Self::HfToken, Self::TlsKey];

    fn suffix(self) -> &'static str {
        match self {
            Self::ApiKey => "api-key",
            Self::HfToken => "hf-token",
            Self::TlsKey => "tls-key",
        }
    }
}

/// The secret id for `kind` of the deployment whose VM is `instance`:
/// `<instance>-api-key`, `<instance>-hf-token` or `<instance>-tls-key`.
///
/// # Errors
/// [`GcpError::Invalid`] for a bad instance name.
pub fn secret_id(instance: &str, kind: SecretKind) -> Result<String, GcpError> {
    validate_resource_name(instance)?;
    Ok(format!("{instance}-{}", kind.suffix()))
}

/// The fields of a secret amicu reads.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SecretView {
    labels: Option<BTreeMap<String, String>>,
    replication: Option<Value>,
}

impl SecretView {
    /// The replication policy as the API reports it, reduced to what
    /// [`Replication`] can express: `automatic`, or `userManaged` with exactly
    /// one replica. `None` for anything else (several replicas, no policy).
    fn replication(&self) -> Option<Replication> {
        let r = self.replication.as_ref()?;
        if r.get("automatic").is_some() {
            return Some(Replication::Automatic);
        }
        match r["userManaged"]["replicas"].as_array()?.as_slice() {
            [one] => Some(Replication::Region(one["location"].as_str()?.to_owned())),
            _ => None,
        }
    }
}

/// What [`delete_secret`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretDelete {
    AlreadyGone,
    Deleted,
}

fn secrets_url(project: &str) -> Result<String, GcpError> {
    validate_project_id(project)?;
    Ok(format!(
        "{SECRETMANAGER_BASE_URL}/projects/{project}/secrets"
    ))
}

fn secret_url(project: &str, id: &str) -> Result<String, GcpError> {
    validate_secret_id(id)?;
    Ok(format!("{}/{id}", secrets_url(project)?))
}

/// Ids amicu derives are lowercase letters, digits and hyphens: a safe single
/// URL path segment.
fn validate_secret_id(id: &str) -> Result<(), GcpError> {
    let ok = !id.is_empty()
        && id.len() <= 255
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(GcpError::Invalid {
            kind: "secret id",
            value: id.to_owned(),
            reason: "only lowercase letters, digits and hyphens, up to 255 characters",
        })
    }
}

fn replication_body(r: &Replication) -> Result<Value, GcpError> {
    Ok(match r {
        Replication::Automatic => json!({ "automatic": {} }),
        Replication::Region(region) => {
            validate_region(region)?;
            json!({ "userManaged": { "replicas": [{ "location": region }] } })
        }
    })
}

async fn get_owned(
    client: &GcpClient,
    project: &str,
    id: &str,
    owner: &str,
) -> Result<SecretView, GcpError> {
    let s: SecretView = client
        .request_as(HttpMethod::Get, &secret_url(project, id)?, None)
        .await?;
    ensure_owned(&format!("secret {id}"), s.labels.as_ref(), owner)?;
    Ok(s)
}

/// Creates the secret with the owner label (`labels` must carry it) and the
/// replication policy. A secret that already exists and is ours is left as it
/// is (a retry after a lost response), provided its replication policy is the
/// requested one: replication cannot be changed, so a secret stored somewhere
/// else would silently break the residency the caller asked for.
///
/// # Errors
/// [`GcpError::Conflict`] when a secret of that name exists without the owner
/// label (it is left alone) or with a different replication policy, or any API
/// error.
pub async fn create_secret(
    client: &GcpClient,
    project: &str,
    id: &str,
    labels: &BTreeMap<String, String>,
    owner: &str,
    replication: &Replication,
) -> Result<(), GcpError> {
    let url = format!("{}?secretId={id}", secrets_url(project)?);
    validate_secret_id(id)?;
    let body = json!({ "labels": labels, "replication": replication_body(replication)? });
    match client.request(HttpMethod::Post, &url, Some(body)).await {
        Ok(_) => Ok(()),
        Err(GcpError::Conflict { message }) => match get_owned(client, project, id, owner).await {
            Ok(existing) => match existing.replication() {
                Some(found) if found == *replication => Ok(()),
                found => Err(GcpError::Conflict {
                    message: format!(
                        "secret `{id}` already exists with replication {} but {} was \
                         requested; replication cannot be changed, so delete the secret \
                         or pick another name",
                        describe(found.as_ref()),
                        describe(Some(replication)),
                    ),
                }),
            },
            Err(GcpError::NotOwned { .. }) => Err(GcpError::Conflict {
                message: format!(
                    "a secret named `{id}` already exists without amicu's owner label; \
                         it is left alone"
                ),
            }),
            Err(GcpError::NotFound { .. }) => Err(GcpError::Conflict { message }),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    }
}

fn describe(r: Option<&Replication>) -> String {
    match r {
        Some(Replication::Automatic) => "automatic".to_owned(),
        Some(Replication::Region(region)) => format!("region {region}"),
        None => "an unrecognised policy".to_owned(),
    }
}

/// Adds a version holding `value` to a secret amicu owns.
///
/// # Errors
/// [`GcpError::NotOwned`], [`GcpError::NotFound`] or any API error.
pub async fn add_version(
    client: &GcpClient,
    project: &str,
    id: &str,
    owner: &str,
    value: &SecretValue,
) -> Result<(), GcpError> {
    get_owned(client, project, id, owner).await?;
    let url = format!("{}:addVersion", secret_url(project, id)?);
    let body = json!({ "payload": { "data": STANDARD.encode(value.expose_secret()) } });
    client.request(HttpMethod::Post, &url, Some(body)).await?;
    Ok(())
}

/// The latest version's payload. For tests and debugging: the VM reads its
/// secrets itself, and amicu has no other reason to.
///
/// # Errors
/// [`GcpError::NotFound`], [`GcpError::Decode`] or any API error.
pub async fn read_latest(
    client: &GcpClient,
    project: &str,
    id: &str,
) -> Result<SecretValue, GcpError> {
    let url = format!("{}/versions/latest:access", secret_url(project, id)?);
    let v = client.request(HttpMethod::Get, &url, None).await?;
    let data = v["payload"]["data"]
        .as_str()
        .ok_or_else(|| GcpError::Decode("secret version has no payload".into()))?;
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| GcpError::Decode("secret payload is not base64".into()))?;
    Ok(SecretValue::new(bytes))
}

/// Deletes a secret amicu owns, with all its versions.
///
/// # Errors
/// [`GcpError::NotOwned`] (nothing is deleted) or any API error.
pub async fn delete_secret(
    client: &GcpClient,
    project: &str,
    id: &str,
    owner: &str,
) -> Result<SecretDelete, GcpError> {
    match get_owned(client, project, id, owner).await {
        Ok(_) => {}
        Err(GcpError::NotFound { .. }) => return Ok(SecretDelete::AlreadyGone),
        Err(e) => return Err(e),
    }
    match client
        .request(HttpMethod::Delete, &secret_url(project, id)?, None)
        .await
    {
        Ok(_) => Ok(SecretDelete::Deleted),
        Err(e) if e.is_not_found() => Ok(SecretDelete::AlreadyGone),
        Err(e) => Err(e),
    }
}

/// Grants [`ACCESSOR_ROLE`] on this one secret to the service account, by
/// read-modify-write of the secret's IAM policy: `getIamPolicy`, add the member
/// to the role's unconditional binding, `setIamPolicy` with the etag from the
/// read. A stale etag (`409`) re-reads and retries. Nothing is sent when the
/// member already has the role. Never touches project-level IAM.
///
/// # Errors
/// [`GcpError::NotOwned`], [`GcpError::Invalid`] for a bad email,
/// [`GcpError::Conflict`] when the policy keeps changing, or any API error.
pub async fn grant_accessor(
    client: &GcpClient,
    project: &str,
    id: &str,
    owner: &str,
    service_account: &str,
) -> Result<(), GcpError> {
    if !service_account.contains('@') || service_account.contains(char::is_whitespace) {
        return Err(GcpError::Invalid {
            kind: "service account",
            value: service_account.to_owned(),
            reason: "expected an email address",
        });
    }
    get_owned(client, project, id, owner).await?;
    let base = secret_url(project, id)?;
    let member = format!("serviceAccount:{service_account}");
    let mut last = None;
    for _ in 0..IAM_ATTEMPTS {
        let mut policy = client
            .request(
                HttpMethod::Get,
                &format!("{base}:getIamPolicy?options.requestedPolicyVersion=3"),
                None,
            )
            .await?;
        if !add_member(&mut policy, ACCESSOR_ROLE, &member) {
            return Ok(());
        }
        match client
            .request(
                HttpMethod::Post,
                &format!("{base}:setIamPolicy"),
                Some(json!({ "policy": policy })),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(e @ GcpError::Conflict { .. }) => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or(GcpError::Conflict {
        message: format!("the IAM policy of secret `{id}` kept changing"),
    }))
}

/// Adds `member` to `role`'s unconditional binding, keeping every other field
/// (the etag, version, conditional bindings, audit configs) as read. Returns
/// whether the policy changed.
fn add_member(policy: &mut Value, role: &str, member: &str) -> bool {
    if !policy["bindings"].is_array() {
        policy["bindings"] = json!([]);
    }
    let bindings = policy["bindings"].as_array_mut().expect("set above");
    if let Some(b) = bindings
        .iter_mut()
        .find(|b| b["role"] == role && b.get("condition").is_none_or(Value::is_null))
    {
        let members = b["members"].as_array_mut();
        match members {
            Some(m) if m.iter().any(|x| x == member) => false,
            Some(m) => {
                m.push(json!(member));
                true
            }
            None => {
                b["members"] = json!([member]);
                true
            }
        }
    } else {
        bindings.push(json!({ "role": role, "members": [member] }));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::tests::{Mock, TOKEN, client};
    use crate::labels::owner_labels;

    const P: &str = "my-proj";
    const SECRET: &str = "projects/my-proj/secrets/vm-1-api-key";
    const SA: &str = "vm@my-proj.iam.gserviceaccount.com";

    fn labels() -> BTreeMap<String, String> {
        owner_labels(None, "demo", "chat")
    }

    fn owner() -> String {
        labels()[crate::labels::OWNER_LABEL].clone()
    }

    fn ours() -> String {
        ours_with(&json!({ "automatic": {} }))
    }

    fn ours_with(replication: &Value) -> String {
        json!({ "name": SECRET, "labels": labels(), "replication": replication }).to_string()
    }

    const FOREIGN: &str = r#"{"name":"x","labels":{"team":"other"}}"#;
    const ONE: &str = "/projects/my-proj/secrets/vm-1-api-key";

    #[test]
    fn ids_follow_the_instance_name() {
        assert_eq!(
            secret_id("vm-1", SecretKind::TlsKey).unwrap(),
            "vm-1-tls-key"
        );
        assert_eq!(
            secret_id("vm-1", SecretKind::HfToken).unwrap(),
            "vm-1-hf-token"
        );
        assert!(secret_id("Bad/Name", SecretKind::ApiKey).is_err());
    }

    #[tokio::test]
    async fn create_sends_labels_and_the_regional_policy() {
        let (c, m) = client(Mock::default().on(
            HttpMethod::Post,
            "/projects/my-proj/secrets?secretId=vm-1-api-key",
            200,
            "{}",
        ));
        let r = Replication::Region("europe-west4".into());
        create_secret(&c, P, "vm-1-api-key", &labels(), &owner(), &r)
            .await
            .unwrap();
        let seen = m.requests();
        assert_eq!(seen[0].bearer.as_deref(), Some(TOKEN));
        let body = seen[0].body.clone().unwrap();
        assert_eq!(body["labels"], json!(labels()));
        assert_eq!(
            body["replication"],
            json!({"userManaged": {"replicas": [{"location": "europe-west4"}]}})
        );
        let (c, m) =
            client(Mock::default().on(HttpMethod::Post, "secretId=vm-1-api-key", 200, "{}"));
        create_secret(
            &c,
            P,
            "vm-1-api-key",
            &labels(),
            &owner(),
            &Replication::Automatic,
        )
        .await
        .unwrap();
        assert_eq!(
            m.requests()[0].body.clone().unwrap()["replication"],
            json!({"automatic": {}})
        );
    }

    #[tokio::test]
    async fn a_409_on_our_secret_is_adopted_and_on_a_foreign_one_an_error() {
        let conflict = r#"{"error":{"code":409,"status":"ALREADY_EXISTS","message":"exists"}}"#;
        let (c, _) = client(
            Mock::default()
                .on(HttpMethod::Post, "secretId=vm-1-api-key", 409, conflict)
                .on(HttpMethod::Get, ONE, 200, &ours()),
        );
        create_secret(
            &c,
            P,
            "vm-1-api-key",
            &labels(),
            &owner(),
            &Replication::Automatic,
        )
        .await
        .unwrap();
        let (c, _) = client(
            Mock::default()
                .on(HttpMethod::Post, "secretId=vm-1-api-key", 409, conflict)
                .on(HttpMethod::Get, ONE, 200, FOREIGN),
        );
        let e = create_secret(
            &c,
            P,
            "vm-1-api-key",
            &labels(),
            &owner(),
            &Replication::Automatic,
        )
        .await
        .unwrap_err();
        assert!(matches!(e, GcpError::Conflict { .. }), "{e:?}");
    }

    #[tokio::test]
    async fn a_409_on_a_secret_with_other_replication_is_an_error() {
        let conflict = r#"{"error":{"code":409,"status":"ALREADY_EXISTS","message":"exists"}}"#;
        let regional =
            ours_with(&json!({"userManaged": {"replicas": [{"location": "europe-west4"}]}}));
        let adopt = |existing: String, want: Replication| async move {
            let (c, _) = client(
                Mock::default()
                    .on(HttpMethod::Post, "secretId=vm-1-api-key", 409, conflict)
                    .on(HttpMethod::Get, ONE, 200, &existing),
            );
            create_secret(&c, P, "vm-1-api-key", &labels(), &owner(), &want).await
        };
        // Same policy: adopted.
        let eu = Replication::Region("europe-west4".into());
        adopt(regional.clone(), eu.clone()).await.unwrap();
        // Automatic where a region was asked for, the reverse, another region,
        // and a payload without any policy are all refused.
        for (existing, want) in [
            (ours(), eu.clone()),
            (regional.clone(), Replication::Automatic),
            (regional, Replication::Region("us-central1".into())),
            (
                json!({ "name": SECRET, "labels": labels() }).to_string(),
                eu,
            ),
        ] {
            let e = adopt(existing, want).await.unwrap_err();
            assert!(
                matches!(&e, GcpError::Conflict { message } if message.contains("replication")),
                "{e:?}"
            );
        }
    }

    #[tokio::test]
    async fn add_version_base64_encodes_and_refuses_foreign_secrets() {
        let (c, m) = client(Mock::default().on(HttpMethod::Get, ONE, 200, &ours()).on(
            HttpMethod::Post,
            ":addVersion",
            200,
            "{}",
        ));
        add_version(
            &c,
            P,
            "vm-1-api-key",
            &owner(),
            &SecretValue::new("sk-secret"),
        )
        .await
        .unwrap();
        let body = m.requests()[1].body.clone().unwrap();
        assert_eq!(body["payload"]["data"], STANDARD.encode("sk-secret"));

        let (c, m) = client(Mock::default().on(HttpMethod::Get, ONE, 200, FOREIGN));
        let e = add_version(&c, P, "vm-1-api-key", &owner(), &SecretValue::new("x"))
            .await
            .unwrap_err();
        assert!(matches!(e, GcpError::NotOwned { .. }));
        assert_eq!(m.requests().len(), 1);
    }

    #[tokio::test]
    async fn read_latest_decodes_the_payload() {
        let (c, m) = client(Mock::default().on(
            HttpMethod::Get,
            "/versions/latest:access",
            200,
            &json!({"name": "v", "payload": {"data": STANDARD.encode("sk-secret")}}).to_string(),
        ));
        let v = read_latest(&c, P, "vm-1-api-key").await.unwrap();
        assert_eq!(v.expose_secret(), b"sk-secret");
        assert!(
            m.requests()[0]
                .url
                .ends_with("/vm-1-api-key/versions/latest:access")
        );
    }

    #[tokio::test]
    async fn delete_checks_ownership_first() {
        let (c, m) = client(Mock::default().on(HttpMethod::Get, ONE, 200, FOREIGN));
        let e = delete_secret(&c, P, "vm-1-api-key", &owner())
            .await
            .unwrap_err();
        assert!(matches!(e, GcpError::NotOwned { .. }));
        assert!(m.requests().iter().all(|r| r.method != HttpMethod::Delete));

        let (c, m) = client(Mock::default().on(HttpMethod::Get, ONE, 200, &ours()).on(
            HttpMethod::Delete,
            ONE,
            200,
            "{}",
        ));
        assert_eq!(
            delete_secret(&c, P, "vm-1-api-key", &owner())
                .await
                .unwrap(),
            SecretDelete::Deleted
        );
        assert_eq!(m.requests()[1].method, HttpMethod::Delete);

        let (c, _) = client(Mock::default().on(HttpMethod::Get, ONE, 404, "{}"));
        assert_eq!(
            delete_secret(&c, P, "vm-1-api-key", &owner())
                .await
                .unwrap(),
            SecretDelete::AlreadyGone
        );
    }

    #[tokio::test]
    async fn grant_is_a_read_modify_write_with_the_etag_on_the_secret_only() {
        let policy = r#"{"version":3,"etag":"BwXabc=","bindings":[
            {"role":"roles/secretmanager.admin","members":["user:a@b.c"]}]}"#;
        let (c, m) = client(
            Mock::default()
                .on(HttpMethod::Get, ONE, 200, &ours())
                .on(
                    HttpMethod::Get,
                    ":getIamPolicy?options.requestedPolicyVersion=3",
                    200,
                    policy,
                )
                .on(HttpMethod::Post, ":setIamPolicy", 200, "{}"),
        );
        grant_accessor(&c, P, "vm-1-api-key", &owner(), SA)
            .await
            .unwrap();
        let seen = m.requests();
        assert!(
            seen.iter().all(|r| r.url.contains(SECRET_PATH)),
            "{:?}",
            seen.iter().map(|r| &r.url).collect::<Vec<_>>()
        );
        let set = seen.last().unwrap();
        assert!(set.url.ends_with(":setIamPolicy"));
        let p = &set.body.clone().unwrap()["policy"];
        assert_eq!(p["etag"], "BwXabc=");
        assert_eq!(p["bindings"][0]["members"], json!(["user:a@b.c"]));
        assert_eq!(p["bindings"][1]["role"], ACCESSOR_ROLE);
        assert_eq!(
            p["bindings"][1]["members"],
            json!([format!("serviceAccount:{SA}")])
        );
    }

    const SECRET_PATH: &str = "/projects/my-proj/secrets/vm-1-api-key";

    #[tokio::test]
    async fn grant_retries_a_stale_etag_and_skips_an_existing_grant() {
        let stale = r#"{"error":{"code":409,"status":"ABORTED","message":"etag"}}"#;
        let (c, m) = client(
            Mock::default()
                .on(HttpMethod::Get, ONE, 200, &ours())
                .on(
                    HttpMethod::Get,
                    ":getIamPolicy?options.requestedPolicyVersion=3",
                    200,
                    r#"{"etag":"e1"}"#,
                )
                .on(HttpMethod::Post, ":setIamPolicy", 409, stale)
                .on(HttpMethod::Post, ":setIamPolicy", 200, "{}"),
        );
        grant_accessor(&c, P, "vm-1-api-key", &owner(), SA)
            .await
            .unwrap();
        let sets = m
            .requests()
            .iter()
            .filter(|r| r.url.ends_with(":setIamPolicy"))
            .count();
        assert_eq!(sets, 2);

        let has = json!({"etag":"e","bindings":[{"role":ACCESSOR_ROLE,"members":[format!("serviceAccount:{SA}")]}]}).to_string();
        let (c, m) = client(Mock::default().on(HttpMethod::Get, ONE, 200, &ours()).on(
            HttpMethod::Get,
            ":getIamPolicy?options.requestedPolicyVersion=3",
            200,
            &has,
        ));
        grant_accessor(&c, P, "vm-1-api-key", &owner(), SA)
            .await
            .unwrap();
        assert!(
            m.requests()
                .iter()
                .all(|r| !r.url.ends_with(":setIamPolicy"))
        );
    }

    #[tokio::test]
    async fn grant_refuses_a_foreign_secret() {
        let (c, m) = client(Mock::default().on(HttpMethod::Get, ONE, 200, FOREIGN));
        let e = grant_accessor(&c, P, "vm-1-api-key", &owner(), SA)
            .await
            .unwrap_err();
        assert!(matches!(e, GcpError::NotOwned { .. }));
        assert_eq!(m.requests().len(), 1);
    }

    #[test]
    fn conditional_bindings_are_left_alone() {
        let mut p = json!({"bindings":[{"role":ACCESSOR_ROLE,"members":["user:x"],
            "condition":{"title":"t","expression":"true"}}]});
        assert!(add_member(&mut p, ACCESSOR_ROLE, "serviceAccount:a@b"));
        assert_eq!(p["bindings"][0]["members"], json!(["user:x"]));
        assert_eq!(p["bindings"][1]["members"], json!(["serviceAccount:a@b"]));
    }
}
