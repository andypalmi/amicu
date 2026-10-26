//! The VM's secrets in Secret Manager: create them (owner-labelled, in the
//! chosen replication), store the values, let the VM's service account read
//! each one, and delete them again with the VM.
//!
//! Do [`provision_secrets`] before [`super::create`]: the startup script reads
//! them at boot. Only [`SecretDelivery::SecretManager`] uses Secret Manager;
//! with the metadata fallback both functions do nothing.

use std::sync::atomic::{AtomicBool, Ordering};

use amicu_core::secret::SecretValue;

use super::spec::VmSpec;
use super::startup::SecretDelivery;
use super::vm::VmRecord;
use crate::client::GcpClient;
use crate::error::GcpError;
use crate::secrets::{
    Replication, SecretKind, add_version, create_secret, delete_secret, grant_accessor, secret_id,
};

/// Creates `<name>-api-key` (and `-hf-token`, `-tls-key` when the spec has
/// them), adds a version with each value, and grants the spec's service account
/// `roles/secretmanager.secretAccessor` on each of these secrets. Safe to repeat
/// after a failure: existing owned secrets are reused, and each run adds a new
/// version (the VM reads the latest).
///
/// # Errors
/// [`GcpError::Invalid`] for a bad spec, [`GcpError::Conflict`] when a secret of
/// that name exists without the owner label, or any API error. Secrets created
/// before a failure stay: [`delete_secrets`] removes them.
///
/// `touched` is set as soon as a secret may have been written: before every
/// attempt except a first create that fails because the API is disabled, which
/// proves nothing was written.
pub async fn provision_secrets(
    client: &GcpClient,
    spec: &VmSpec,
    replication: &Replication,
    touched: &AtomicBool,
) -> Result<(), GcpError> {
    spec.validate()?;
    if spec.startup.delivery != SecretDelivery::SecretManager {
        return Ok(());
    }
    let owner = spec.owner()?;
    let sa = spec.service_account.as_deref().ok_or(GcpError::Invalid {
        kind: "service account",
        value: String::new(),
        reason: "required to read secrets from Secret Manager",
    })?;
    let mut items: Vec<(SecretKind, SecretValue)> =
        vec![(SecretKind::ApiKey, spec.startup.api_key.clone())];
    if let Some(t) = &spec.startup.hf_token {
        items.push((SecretKind::HfToken, t.clone()));
    }
    if let Some(t) = &spec.startup.tls {
        items.push((SecretKind::TlsKey, t.key_pem.clone()));
    }
    for (kind, value) in items {
        let id = secret_id(&spec.name, kind)?;
        let created =
            create_secret(client, &spec.project, &id, &spec.labels, owner, replication).await;
        match created {
            Err(e) if e.is_service_disabled() && !touched.load(Ordering::SeqCst) => {
                return Err(e);
            }
            r => {
                touched.store(true, Ordering::SeqCst);
                r?;
            }
        }
        add_version(client, &spec.project, &id, owner, &value).await?;
        grant_accessor(client, &spec.project, &id, owner, sa).await?;
    }
    Ok(())
}

/// Deletes the deployment's secrets that carry the record's owner marker; ones
/// that are absent are skipped, one without the marker is an error and is left
/// alone. Every secret is attempted even when an earlier one fails, so one bad
/// secret never leaves the others behind.
///
/// # Errors
/// The first error, after all secrets were attempted: [`GcpError::NotOwned`] or
/// any API error.
pub async fn delete_secrets(client: &GcpClient, record: &VmRecord) -> Result<(), GcpError> {
    let mut first = None;
    for kind in SecretKind::ALL {
        let result = match secret_id(&record.name, kind) {
            Ok(id) => delete_secret(client, &record.project, &id, &record.owner)
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use amicu_providers::HttpMethod;

    use super::*;
    use crate::client::tests::{Mock, client};
    use crate::compute::spec::tests::spec;
    use crate::compute::tls::TlsIdentity;
    use crate::compute::vm::prepare;
    use crate::compute::{Provisioning, SecretDelivery};

    fn sm_spec() -> VmSpec {
        let mut s = spec("g2-standard-4", None, Provisioning::Spot);
        s.startup.delivery = SecretDelivery::SecretManager;
        s.service_account = Some("vm@my-proj.iam.gserviceaccount.com".into());
        s.startup.tls = Some(TlsIdentity::generate(&s.name).unwrap());
        s
    }

    fn mock_for(s: &VmSpec) -> Mock {
        let ours = serde_json::json!({"labels": s.labels}).to_string();
        let mut m = Mock::default();
        for kind in SecretKind::ALL {
            let id = secret_id(&s.name, kind).unwrap();
            m = m
                .on(HttpMethod::Get, &format!("/secrets/{id}"), 200, &ours)
                .on(
                    HttpMethod::Get,
                    &format!("/secrets/{id}:getIamPolicy?options.requestedPolicyVersion=3"),
                    200,
                    r#"{"etag":"e"}"#,
                )
                .on(HttpMethod::Delete, &format!("/secrets/{id}"), 200, "{}")
                .on(
                    HttpMethod::Post,
                    &format!("/secrets?secretId={id}"),
                    200,
                    "{}",
                );
        }
        m
    }

    #[tokio::test]
    async fn provisions_every_secret_with_secret_level_iam_only() {
        let s = sm_spec();
        let mut mock = mock_for(&s);
        for kind in SecretKind::ALL {
            let id = secret_id(&s.name, kind).unwrap();
            mock = mock
                .on(
                    HttpMethod::Post,
                    &format!("/secrets/{id}:addVersion"),
                    200,
                    "{}",
                )
                .on(
                    HttpMethod::Post,
                    &format!("/secrets/{id}:setIamPolicy"),
                    200,
                    "{}",
                );
        }
        let (c, m) = client(mock);
        provision_secrets(
            &c,
            &s,
            &Replication::Region("europe-west4".into()),
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
        let seen = m.requests();
        let prefix = "https://secretmanager.googleapis.com/v1/projects/my-proj/secrets";
        assert!(seen.iter().all(|r| r.url.starts_with(prefix)));
        let count = |suffix: &str| seen.iter().filter(|r| r.url.ends_with(suffix)).count();
        assert_eq!(count(":setIamPolicy"), 3);
        assert_eq!(count(":addVersion"), 3);
        assert_eq!(
            seen.iter().filter(|r| r.url.contains("?secretId=")).count(),
            3
        );
    }

    #[tokio::test]
    async fn metadata_delivery_creates_nothing() {
        let s = spec("g2-standard-4", None, Provisioning::Spot);
        let (c, m) = client(Mock::default());
        provision_secrets(&c, &s, &Replication::Automatic, &AtomicBool::new(false))
            .await
            .unwrap();
        assert!(m.requests().is_empty());
    }

    #[tokio::test]
    async fn deletes_the_three_secrets_by_record() {
        let s = sm_spec();
        let rec = prepare(&s).unwrap();
        let (c, m) = client(mock_for(&s));
        delete_secrets(&c, &rec).await.unwrap();
        let dels = m
            .requests()
            .iter()
            .filter(|r| r.method == HttpMethod::Delete)
            .count();
        assert_eq!(dels, 3);
    }

    #[tokio::test]
    async fn delete_attempts_every_secret_then_returns_the_first_error() {
        let s = sm_spec();
        let rec = prepare(&s).unwrap();
        let first = secret_id(&s.name, SecretKind::ALL[0]).unwrap();
        // The first secret carries no owner label; the others are ours.
        let mut mock = Mock::default().on(
            HttpMethod::Get,
            &format!("/secrets/{first}"),
            200,
            r#"{"labels":{"team":"other"}}"#,
        );
        let ours = serde_json::json!({"labels": s.labels}).to_string();
        for kind in &SecretKind::ALL[1..] {
            let id = secret_id(&s.name, *kind).unwrap();
            mock = mock
                .on(HttpMethod::Get, &format!("/secrets/{id}"), 200, &ours)
                .on(HttpMethod::Delete, &format!("/secrets/{id}"), 200, "{}");
        }
        let (c, m) = client(mock);
        let e = delete_secrets(&c, &rec).await.unwrap_err();
        assert!(matches!(e, GcpError::NotOwned { .. }), "{e:?}");
        let dels: Vec<_> = m
            .requests()
            .into_iter()
            .filter(|r| r.method == HttpMethod::Delete)
            .collect();
        assert_eq!(dels.len(), 2);
    }

    #[test]
    fn secret_manager_delivery_needs_a_service_account() {
        let mut s = sm_spec();
        s.service_account = None;
        assert!(s.validate().is_err());
    }
}
