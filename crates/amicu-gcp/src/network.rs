//! The VPC network and subnetwork a VM and its firewall rule attach to. Both
//! are given as a bare name or as a partial or full self link; shape checks
//! only, whether they exist is the preflight's call.

use crate::error::GcpError;
use crate::validate::{validate_project_id, validate_region, validate_resource_name};

/// What a VM and firewall rule use when no `network` is configured.
pub const DEFAULT_NETWORK_PATH: &str = "global/networks/default";

const LINK_PREFIXES: [&str; 2] = [
    "https://www.googleapis.com/compute/v1/",
    "https://compute.googleapis.com/compute/v1/",
];

fn invalid(kind: &'static str, value: &str, reason: &'static str) -> GcpError {
    GcpError::Invalid {
        kind,
        value: value.to_owned(),
        reason,
    }
}

fn segments<'a>(kind: &'static str, input: &'a str) -> Result<Vec<&'a str>, GcpError> {
    let s = input.trim();
    let s = LINK_PREFIXES
        .iter()
        .find_map(|p| s.strip_prefix(p))
        .unwrap_or(s);
    if s.is_empty() || s.starts_with('/') || s.ends_with('/') {
        return Err(invalid(kind, input, "must not be empty"));
    }
    Ok(s.split('/').collect())
}

/// A VPC network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkRef {
    /// The project that owns the network (a Shared VPC host project); `None`
    /// means the deployment's own project.
    pub project: Option<String>,
    pub name: String,
}

impl NetworkRef {
    /// Accepts `name`, `global/networks/name`, `projects/p/global/networks/name`
    /// or the full `https://www.googleapis.com/compute/v1/...` self link.
    ///
    /// # Errors
    /// [`GcpError::Invalid`] for any other shape.
    pub fn parse(input: &str) -> Result<Self, GcpError> {
        const KIND: &str = "network";
        let bad = || {
            invalid(
                KIND,
                input,
                "expected a network name or a link like projects/<project>/global/networks/<name>",
            )
        };
        let (project, name) = match segments(KIND, input)?.as_slice() {
            [name] | ["global", "networks", name] => (None, *name),
            ["projects", p, "global", "networks", name] => (Some(*p), *name),
            _ => return Err(bad()),
        };
        validate_resource_name(name).map_err(|_| bad())?;
        if let Some(p) = project {
            validate_project_id(p).map_err(|_| bad())?;
        }
        Ok(Self {
            project: project.map(str::to_owned),
            name: name.to_owned(),
        })
    }

    /// The project that owns the network.
    #[must_use]
    pub fn project_or<'a>(&'a self, default: &'a str) -> &'a str {
        self.project.as_deref().unwrap_or(default)
    }

    /// `projects/<project>/global/networks/<name>`.
    #[must_use]
    pub fn path(&self, default_project: &str) -> String {
        format!(
            "projects/{}/global/networks/{}",
            self.project_or(default_project),
            self.name
        )
    }
}

/// A regional subnetwork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubnetworkRef {
    pub project: Option<String>,
    /// `None` for a bare name: the deployment's region applies.
    pub region: Option<String>,
    pub name: String,
}

impl SubnetworkRef {
    /// Accepts `name`, `regions/r/subnetworks/name`,
    /// `projects/p/regions/r/subnetworks/name` or the full self link.
    ///
    /// # Errors
    /// [`GcpError::Invalid`] for any other shape.
    pub fn parse(input: &str) -> Result<Self, GcpError> {
        const KIND: &str = "subnetwork";
        let bad = || {
            invalid(
                KIND,
                input,
                "expected a subnetwork name or a link like \
                 projects/<project>/regions/<region>/subnetworks/<name>",
            )
        };
        let (project, region, name) = match segments(KIND, input)?.as_slice() {
            [name] => (None, None, *name),
            ["regions", r, "subnetworks", name] => (None, Some(*r), *name),
            ["projects", p, "regions", r, "subnetworks", name] => (Some(*p), Some(*r), *name),
            _ => return Err(bad()),
        };
        validate_resource_name(name).map_err(|_| bad())?;
        if let Some(p) = project {
            validate_project_id(p).map_err(|_| bad())?;
        }
        if let Some(r) = region {
            validate_region(r).map_err(|_| bad())?;
        }
        Ok(Self {
            project: project.map(str::to_owned),
            region: region.map(str::to_owned),
            name: name.to_owned(),
        })
    }

    /// The project that owns the subnetwork.
    #[must_use]
    pub fn project_or<'a>(&'a self, default: &'a str) -> &'a str {
        self.project.as_deref().unwrap_or(default)
    }

    /// The link's region, or else `region`.
    #[must_use]
    pub fn region_or<'a>(&'a self, region: &'a str) -> &'a str {
        self.region.as_deref().unwrap_or(region)
    }

    /// `projects/<project>/regions/<region>/subnetworks/<name>`.
    #[must_use]
    pub fn path(&self, default_project: &str, region: &str) -> String {
        format!(
            "projects/{}/regions/{}/subnetworks/{}",
            self.project_or(default_project),
            self.region_or(region),
            self.name
        )
    }

    /// An error when the link names a region other than `region`.
    ///
    /// # Errors
    /// [`GcpError::Invalid`] on a region mismatch.
    pub fn check_region(&self, region: &str) -> Result<(), GcpError> {
        match &self.region {
            Some(r) if r != region => Err(invalid(
                "subnetwork",
                &self.name,
                "must be in the same region as the VM",
            )),
            _ => Ok(()),
        }
    }
}

/// The `network` and `subnetwork` fields of a network interface (or of a
/// firewall rule: the network). `None` network is the project's `default`.
///
/// # Errors
/// [`GcpError::Invalid`] for a bad link, a subnetwork in another region, or a
/// subnetwork without a network.
pub fn interface_paths(
    project: &str,
    region: &str,
    network: Option<&str>,
    subnetwork: Option<&str>,
) -> Result<(String, Option<String>), GcpError> {
    let sub = subnetwork.map(SubnetworkRef::parse).transpose()?;
    let Some(network) = network else {
        return match subnetwork {
            Some(s) => Err(invalid("subnetwork", s, "needs a network too")),
            None => Ok((DEFAULT_NETWORK_PATH.to_owned(), None)),
        };
    };
    let net = NetworkRef::parse(network)?;
    let net_project = net.project_or(project);
    let sub = sub
        .map(|s| s.check_region(region).map(|()| s.path(net_project, region)))
        .transpose()?;
    Ok((net.path(project), sub))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_forms() {
        let want = "projects/my-proj/global/networks/vpc-main";
        for input in [
            "vpc-main",
            "global/networks/vpc-main",
            "projects/my-proj/global/networks/vpc-main",
            "https://www.googleapis.com/compute/v1/projects/my-proj/global/networks/vpc-main",
        ] {
            assert_eq!(NetworkRef::parse(input).unwrap().path("my-proj"), want);
        }
        assert_eq!(
            NetworkRef::parse("projects/host-proj/global/networks/vpc-main")
                .unwrap()
                .path("my-proj"),
            "projects/host-proj/global/networks/vpc-main"
        );
        for bad in [
            "",
            "Vpc",
            "a/b",
            "global/networks/",
            "projects/x/global/networks/n",
        ] {
            assert!(NetworkRef::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn subnetwork_forms_and_region() {
        let want = "projects/my-proj/regions/europe-west4/subnetworks/gpu";
        for input in [
            "gpu",
            "regions/europe-west4/subnetworks/gpu",
            "https://www.googleapis.com/compute/v1/projects/my-proj/regions/europe-west4/subnetworks/gpu",
        ] {
            let s = SubnetworkRef::parse(input).unwrap();
            assert_eq!(s.path("my-proj", "europe-west4"), want, "{input}");
        }
        let s = SubnetworkRef::parse("regions/us-central1/subnetworks/gpu").unwrap();
        assert!(s.check_region("us-central1").is_ok());
        assert!(s.check_region("europe-west4").is_err());
        assert!(SubnetworkRef::parse("regions/europe-west4/gpu").is_err());
    }

    #[test]
    fn a_bare_subnetwork_follows_the_networks_project() {
        let (_, s) = interface_paths(
            "svc-proj",
            "europe-west4",
            Some("projects/host-proj/global/networks/vpc"),
            Some("gpu"),
        )
        .unwrap();
        assert_eq!(
            s.unwrap(),
            "projects/host-proj/regions/europe-west4/subnetworks/gpu"
        );
    }

    #[test]
    fn interface_defaults_and_pairing() {
        assert_eq!(
            interface_paths("my-proj", "europe-west4", None, None).unwrap(),
            (DEFAULT_NETWORK_PATH.to_owned(), None)
        );
        let (n, s) = interface_paths("my-proj", "europe-west4", Some("vpc"), Some("gpu")).unwrap();
        assert_eq!(n, "projects/my-proj/global/networks/vpc");
        assert_eq!(
            s.unwrap(),
            "projects/my-proj/regions/europe-west4/subnetworks/gpu"
        );
        assert!(interface_paths("my-proj", "europe-west4", None, Some("gpu")).is_err());
        assert!(
            interface_paths(
                "my-proj",
                "europe-west4",
                Some("vpc"),
                Some("regions/us-central1/subnetworks/gpu")
            )
            .is_err()
        );
    }
}
