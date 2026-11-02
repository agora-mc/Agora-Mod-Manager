//! Modrinth as a metadata source for the offline library.
//!
//! The batched lookups here are the same Modrinth API the Browse pages use, behind
//! [`agora_core::library::MetadataSource`]: core decides which items are missing what, this
//! module only asks Modrinth. Requests go through the checked HTTP path, so Lockdown Mode and
//! the Privacy switch apply before any DNS lookup.
//!
//! - Projects (one request for up to [`agora_core::library::METADATA_BATCH`] ids):
//!   <https://docs.modrinth.com/api/operations/getprojects/>
//! - Versions from file hashes (`sha512`, in batches):
//!   <https://docs.modrinth.com/api/operations/versionsfromhashes/>

use std::collections::HashMap;

use agora_core::ctx::Ctx;
use agora_core::error::{LauncherError, LauncherResult};
use agora_core::http_client::{self, ClientCategory};
use agora_core::library::{MetadataSource, ProjectDetails, ProjectVersion};
use serde::Deserialize;

const PROJECTS_URL: &str = "https://api.modrinth.com/v2/projects";
const VERSIONS_FROM_HASHES_URL: &str = "https://api.modrinth.com/v2/version_files";

/// A Modrinth project's fields that fill a store item.
#[derive(Debug, Deserialize)]
struct RawProject {
    id: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    project_type: String,
}

#[derive(Debug, Deserialize)]
struct RawVersion {
    #[serde(default)]
    project_id: String,
    #[serde(default)]
    version_number: String,
}

/// The production source: Modrinth's v2 API through the checked HTTP clients.
pub struct ModrinthMetadataSource {
    ctx: Ctx,
}

impl ModrinthMetadataSource {
    pub fn new(ctx: Ctx) -> Self {
        Self { ctx }
    }
}

impl MetadataSource for ModrinthMetadataSource {
    fn available(&self) -> LauncherResult<()> {
        // The Privacy switch first (a plain settings read, no network), then the process gate,
        // which is where Lockdown Mode is enforced.
        crate::modrinth::ModrinthService::new(self.ctx.clone()).check_network_enabled()?;
        agora_core::network_gate::authorize(ClientCategory::Modrinth)
    }

    fn projects(&self, project_ids: &[String]) -> LauncherResult<Vec<ProjectDetails>> {
        if project_ids.is_empty() {
            return Ok(Vec::new());
        }
        let encoded =
            serde_json::to_string(project_ids).map_err(|error| LauncherError::Generic {
                code: "ERR_MODRINTH_METADATA".into(),
                message: error.to_string(),
            })?;
        let url = format!("{PROJECTS_URL}?ids={}", urlencoding::encode(&encoded));
        let bytes = http_client::blocking_checked_get_bytes(
            &self.ctx.http_clients,
            ClientCategory::Modrinth,
            &url,
        )?;
        let raw: Vec<RawProject> =
            serde_json::from_slice(&bytes).map_err(|error| LauncherError::Generic {
                code: "ERR_MODRINTH_METADATA".into(),
                message: format!("Modrinth returned project details Agora could not read: {error}"),
            })?;
        Ok(raw
            .into_iter()
            .map(|project| {
                let page_url = project
                    .slug
                    .as_deref()
                    .filter(|slug| !slug.trim().is_empty())
                    .map(|slug| {
                        let kind = if project.project_type.trim().is_empty() {
                            "mod"
                        } else {
                            project.project_type.as_str()
                        };
                        format!("https://modrinth.com/{kind}/{slug}")
                    });
                ProjectDetails {
                    project_id: project.id,
                    title: Some(project.title).filter(|title| !title.trim().is_empty()),
                    description: Some(project.description)
                        .map(|description| description.trim().to_string())
                        .filter(|description| !description.is_empty()),
                    page_url,
                    icon_url: project.icon_url.filter(|url| !url.trim().is_empty()),
                }
            })
            .collect())
    }

    fn versions_from_hashes(&self, hashes: &[String]) -> LauncherResult<Vec<ProjectVersion>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({
            "hashes": hashes,
            // Modrinth's hash endpoints accept SHA-1 and SHA-512; the store only keeps SHA-256,
            // so core computes SHA-512 from the object before asking.
            "algorithm": "sha512",
        });
        let bytes = http_client::blocking_checked_post_json(
            &self.ctx.http_clients,
            ClientCategory::Modrinth,
            VERSIONS_FROM_HASHES_URL,
            &body,
        )?;
        let raw: HashMap<String, RawVersion> =
            serde_json::from_slice(&bytes).map_err(|error| LauncherError::Generic {
                code: "ERR_MODRINTH_METADATA".into(),
                message: format!("Modrinth returned version details Agora could not read: {error}"),
            })?;
        Ok(raw
            .into_iter()
            .filter(|(_, version)| !version.project_id.trim().is_empty())
            .map(|(hash, version)| ProjectVersion {
                hash,
                project_id: version.project_id,
                version_number: Some(version.version_number)
                    .filter(|number| !number.trim().is_empty()),
            })
            .collect())
    }

    fn icon_bytes(&self, url: &str) -> LauncherResult<Option<Vec<u8>>> {
        // Same rule as the Browse/Library icon path: fetch bytes, then keep them only if their
        // signature says they are an image.
        let bytes = http_client::blocking_checked_get_bytes_with_policy(
            &self.ctx.http_clients,
            ClientCategory::CommunityImage,
            url,
            http_client::HostPolicy::AnyPublicHost,
        )?;
        if agora_core::community_image::sniff_image(&bytes).is_none() {
            return Err(LauncherError::Generic {
                code: "ERR_NOT_AN_IMAGE".into(),
                message: "The project icon was not an image.".into(),
            });
        }
        Ok(Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct PanicGate;

    impl agora_core::network_gate::NetworkGate for PanicGate {
        fn check(&self, _category: ClientCategory) -> LauncherResult<()> {
            panic!("the metadata source reached the network gate while the switch was off");
        }
    }

    #[test]
    fn the_privacy_switch_refuses_before_any_request_or_gate_check() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::testing_context(tmp.path().join("data"));
        ctx.paths.create_required_dirs().unwrap();
        crate::content_bridge::init_local_state(&ctx);
        let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        agora_core::db::set_setting(&conn, "network_modrinth_enabled", &serde_json::json!(false))
            .unwrap();
        drop(conn);

        // A local state with the Modrinth network switch off must answer first, without the
        // process gate or an HTTP client ever being consulted.
        agora_core::network_gate::install(Arc::new(PanicGate));
        let source = ModrinthMetadataSource::new(ctx);
        let error = source.available().unwrap_err();
        assert_eq!(error.code(), "ERR_NETWORK_DISABLED");
    }
}
