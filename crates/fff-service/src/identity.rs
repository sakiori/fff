use std::hash::{Hash, Hasher};

const BUILD_TAG_LEN: usize = 16;
const ENDPOINT_PREFIX: &str = "endpoint-";
const ENDPOINT_SUFFIX: &str = ".json";
#[cfg(unix)]
const EXECUTABLE_PREFIX: &str = "fff-mcp-";

/// Build identity compiled into the library from its version, commit and features.
/// Clients and services of one identity share a service; others run side by side.
pub struct Build {
    id: String,
}

impl Build {
    pub fn current() -> Self {
        let mut id = env!("FFF_SERVICE_BUILD_ID").to_owned();
        // Tests run another build's service from the same executable.
        if cfg!(debug_assertions)
            && let Some(variant) = std::env::var_os("FFF_MCP_TEST_BUILD_ID")
        {
            let mut hasher = std::hash::DefaultHasher::new();
            (&id, variant).hash(&mut hasher);
            id = format!("{:016x}{}", hasher.finish(), &id[BUILD_TAG_LEN..]);
        }
        Self { id }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn files(&self) -> ServiceFiles {
        ServiceFiles {
            tag: self.id[..BUILD_TAG_LEN].to_owned(),
        }
    }
}

/// Namespace file names of one build's service, derived from a short build tag.
pub struct ServiceFiles {
    tag: String,
}

impl ServiceFiles {
    pub fn from_endpoint(name: &str) -> Option<Self> {
        let tag = name
            .strip_prefix(ENDPOINT_PREFIX)?
            .strip_suffix(ENDPOINT_SUFFIX)?;
        Some(Self {
            tag: tag.to_owned(),
        })
    }

    pub fn endpoint_pattern() -> (&'static str, &'static str) {
        (ENDPOINT_PREFIX, ENDPOINT_SUFFIX)
    }

    pub fn endpoint(&self) -> String {
        format!("{ENDPOINT_PREFIX}{}{ENDPOINT_SUFFIX}", self.tag)
    }

    pub fn startup_lock(&self) -> String {
        format!("startup-{}.lock", self.tag)
    }

    pub fn owner_lock(&self) -> String {
        format!("owner-{}.lock", self.tag)
    }

    pub fn stderr(&self) -> String {
        format!("service-{}.stderr", self.tag)
    }

    /// The private copy of the service executable a client starts instead of one that lives
    /// in a transient mount.
    #[cfg(unix)]
    pub fn executable(&self) -> String {
        format!("{EXECUTABLE_PREFIX}{}", self.tag)
    }

    /// The files of the build whose executable copy is `name`.
    #[cfg(unix)]
    pub fn from_executable(name: &str) -> Option<Self> {
        let tag = name.strip_prefix(EXECUTABLE_PREFIX)?;
        (tag.len() == BUILD_TAG_LEN && tag.bytes().all(|byte| byte.is_ascii_hexdigit())).then(
            || Self {
                tag: tag.to_owned(),
            },
        )
    }

    #[cfg(unix)]
    pub fn executable_pattern() -> (&'static str, &'static str) {
        (EXECUTABLE_PREFIX, "")
    }

    #[cfg(unix)]
    pub fn socket(&self) -> String {
        format!("{}.sock", self.tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_names_round_trip_to_service_files() {
        let files = ServiceFiles {
            tag: "0123456789abcdef".into(),
        };
        let parsed = ServiceFiles::from_endpoint(&files.endpoint()).unwrap();
        assert_eq!(parsed.owner_lock(), files.owner_lock());
        assert!(ServiceFiles::from_endpoint("owner-0123.lock").is_none());
    }
}
