use crate::artifact_hash::HashOrigin;
use crate::error_code::ErrorCode;

/// A download whose bytes differ from an expectation the user has to confirm:
/// the curator's pin for that release file, the hash recorded when it was
/// installed before, or the hash a modpack lists for it. See [`crate::artifact_hash`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HashConfirmation {
    /// The file that did not match, as the instance would name it.
    pub file: String,
    /// The release tag it was published under, when it came from a release.
    pub release: Option<String>,
    /// The SHA-256 the expectation named.
    pub expected: String,
    /// The SHA-256 of the bytes that were downloaded.
    pub actual: String,
    /// Where the expectation came from.
    pub expected_from: HashOrigin,
    /// Further expectations the bytes also failed (a pack hash and a curator pin
    /// can both be unmet). "Install anyway" overrides all of them, so all are shown.
    #[serde(default)]
    pub also_unmet: Box<[crate::artifact_hash::ConfirmableHash]>,
}

impl std::fmt::Display for HashConfirmation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let file = match &self.release {
            Some(release) => format!("{} from release {release}", self.file),
            None => self.file.clone(),
        };
        let origin = match self.expected_from {
            HashOrigin::CuratorPin => "pinned curator",
            HashOrigin::PreviousInstall => "previously installed",
            HashOrigin::PackPin => "modpack",
        };
        write!(
            f,
            "{file} does not match the {origin} hash: expected {}, downloaded {}.",
            self.expected, self.actual
        )?;
        for other in &self.also_unmet {
            let origin = match other.origin {
                HashOrigin::CuratorPin => "pinned curator",
                HashOrigin::PreviousInstall => "previously installed",
                HashOrigin::PackPin => "modpack",
            };
            write!(
                f,
                " It also differs from the {origin} hash {}.",
                other.sha256
            )?;
        }
        Ok(())
    }
}

/// Standardized launcher error codes matching the human-centric taxonomy.
#[derive(Debug, Clone)]
pub enum LauncherError {
    /// ERR_NETWORK_OFFLINE — Agora could not reach the server. Says nothing about
    /// whether cached data was used: callers that fell back to cache must not
    /// return this error.
    NetworkOffline,
    /// ERR_NETWORK_OFFLINE — the same failure as [`LauncherError::NetworkOffline`],
    /// with what was being attempted, e.g. "download the catalog". Build it with
    /// [`LauncherError::while_attempting`] so the message says what failed.
    NetworkUnavailable { action: String },
    /// ERR_REGISTRY_DOWNLOAD_FAILED — Could not download the latest registry.
    RegistryDownloadFailed,
    /// ERR_REGISTRY_SIGNATURE_INVALID — Registry signature check failed.
    RegistrySignatureInvalid,
    /// ERR_SCHEMA_TOO_NEW — This registry requires a newer launcher version.
    SchemaTooNew,
    /// ERR_ZIP_BOMB — Installation aborted: this archive exceeds safety limits.
    ZipBomb,
    /// ERR_OVERRIDE_SECURITY_VIOLATION — Pack override contains forbidden files.
    OverrideSecurityViolation,
    /// ERR_HASH_MISMATCH — Downloaded file does not match its expected hash.
    HashMismatch,
    /// ERR_HASH_CONFIRMATION_REQUIRED — The download does not match a curator
    /// pin or a hash recorded on an earlier install. Install only after the
    /// user confirms, which retries with the install-anyway override.
    HashConfirmationRequired(Box<HashConfirmation>),
    /// ERR_UNTRUSTED_SOURCE — Download rejected: URL is not from an allowed source.
    UntrustedSource,
    /// ERR_DISKFULL — Not enough disk space to complete this operation.
    DiskFull,
    /// ERR_AUTH_EXPIRED — Your GitHub session has expired.
    AuthExpired,
    /// ERR_AUTH_REQUIRED — This feature requires GitHub sign-in.
    AuthRequired,
    /// ERR_MSA_AUTH_REQUIRED — Minecraft launch requires Microsoft sign-in.
    MsaAuthRequired,
    /// ERR_MODRINTH_DISABLED — Modrinth integration is disabled.
    ModrinthDisabled,
    /// ERR_INSTANCE_LOCKED — This instance is locked.
    InstanceLocked,
    /// ERR_SANDBOX_UNAVAILABLE — Dev Mode builds require a sandbox runtime.
    SandboxUnavailable,
    /// ERR_MOJANG_NOT_FOUND — Minecraft Launcher not found.
    MojangNotFound,
    /// ERR_LAUNCH_FAILED — Could not start Minecraft.
    LaunchFailed,
    /// ERR_GAME_CRASH — The game process exited abnormally.
    GameCrash,
    /// ERR_LOCAL_STATE_FAILED — Local state database error.
    LocalStateFailed,
    /// ERR_INSTANCE_CREATE_FAILED — Could not create instance.
    InstanceCreateFailed,
    /// ERR_PROFILE_WRITE_FAILED — Could not update Mojang launcher profiles.
    ProfileWriteFailed,
    /// ERR_REGISTRY_MISSING — Cached registry database is missing.
    RegistryMissing,
    /// ERR_UNSUPPORTED_LOADER — This modloader version is not yet verified.
    UnsupportedLoader,

    /// ERR_VERSION_NOT_FOUND — Requested mod version not found.
    VersionNotFound,
    /// ERR_GAME_VERSION_NOT_FOUND — The Minecraft version is not in the Mojang
    /// manifest. Distinct from `VersionNotFound`, which refers to *mod* versions.
    GameVersionNotFound,
    /// ERR_LOADER_PROFILE_NOT_FOUND — The modloader profile JSON (Fabric/Quilt
    /// partial version, or Forge/NeoForge install profile) could not be
    /// resolved or merged with the base Minecraft version.
    LoaderProfileNotFound,
    /// ERR_PROFILE_MISSING — The loader profile JSON is missing from the
    /// Mojang launcher's version directory.
    ProfileMissing(ProfileIssue),
    /// ERR_PROFILE_UNSUPPORTED_METADATA — The installed profile is structurally
    /// valid but contains metadata or artifacts that this launcher version
    /// does not support (unknown rules, unverifiable generated libraries, etc.).
    ProfileUnsupportedMetadata(ProfileIssue),
    /// ERR_PROFILE_CORRUPT — The installed profile is malformed, corrupted,
    /// or fails security validation.
    ProfileCorrupt(ProfileIssue),
    /// ERR_JAVA_INCOMPATIBLE — The selected Java runtime is older than the
    /// major version required by this Minecraft version's metadata.
    JavaIncompatible,
    /// ERR_JAVA_RUNTIME_MISSING — No Java runtime could be found for the
    /// required major version across all candidate sources (system, Mojang,
    /// managed).
    JavaRuntimeMissing { major: u32, component: String },
    /// ERR_JAVA_RUNTIME_CATALOG_MISSING — The runtime catalog has no entry
    /// for the requested (major, os, arch) tuple. The platform is not
    /// supported by the curated catalog.
    JavaRuntimeCatalogMissing {
        major: u32,
        os: String,
        arch: String,
    },
    /// ERR_UNRESOLVED_PLACEHOLDER — A `${...}` token in the JVM or game
    /// arguments could not be substituted before spawn.
    UnresolvedPlaceholder,
    /// ERR_DEPENDENCY_MISSING — A mod requires a dependency.
    DependencyMissing,
    /// ERR_MCP_TOO_MANY_REQUESTS — AI client sent too many requests.
    McpTooManyRequests,
    /// ERR_MCP_DENIED — AI tool request denied based on approval preferences.
    McpDenied,
    /// ERR_MCP_UNAUTHORIZED — AI client connection rejected.
    McpUnauthorized,
    /// ERR_NETWORK_MOJANG_METADATA_DISABLED — Mojang metadata fetches are disabled in Privacy settings.
    NetworkMojangMetadataDisabled,
    /// ERR_NETWORK_MOJANG_CONTENT_DISABLED — Mojang content downloads are disabled in Privacy settings.
    NetworkMojangContentDisabled,
    /// ERR_NETWORK_LOADER_DISABLED — Modloader metadata/content downloads are disabled in Privacy settings.
    NetworkLoaderDisabled,
    /// ERR_NETWORK_MSA_DISABLED — Microsoft account authentication is disabled in Privacy settings.
    NetworkMsaDisabled,
    /// ERR_NETWORK_JAVA_DISABLED — Java runtime downloads are disabled in Privacy settings.
    NetworkJavaDisabled,
    /// ERR_JAVA_RUNTIME_CANCELLED — The Java runtime provisioning was cancelled by the user.
    JavaRuntimeCancelled { major: u32, component: String },
    /// ERR_JAVA_RUNTIME_DOWNLOAD_DISABLED — Java runtime downloads are disabled,
    /// with structured details for the frontend to show suggested actions.
    JavaRuntimeDownloadDisabled { major: u32, component: String },
    /// ERR_MAVEN_DESCRIPTOR — A Maven descriptor string is malformed or unsafe.
    MavenDescriptor,
    /// ERR_MIGRATION_CONFLICT — Data migration aborted due to conflicting files or databases.
    MigrationConflict { message: String },
    /// ERR_MIGRATION_FAILED — Data migration failed during copy, backup, or promotion.
    MigrationFailed { message: String },
    /// ERR_PROCESS_CAPTURE_FAILED — Could not capture OS identity for a just‑spawned
    /// process. The child was killed and the launch aborted.
    ProcessCaptureFailed { pid: u32, detail: String },
    /// ERR_PROCESS_STALE — The tracked OS process no longer matches its captured
    /// identity (PID was reused, the process died, or the executable changed).
    /// The stale record has been detached and the caller should treat it as idle.
    ProcessStale { pid: u32, detail: String },
    /// ERR_USER_DECISION_REQUIRED — Noninteractive CLI has unresolved choices
    /// requiring user input (conflicts, optional dependencies, etc.).
    UserDecisionRequired,
    /// Catch-all for errors that do not yet have a dedicated code.
    Generic { code: String, message: String },
}

impl LauncherError {
    /// Say what was being attempted when a request failed for lack of a network.
    ///
    /// `NetworkOffline` is only the category of failure; the message a user
    /// needs names the thing that did not download. Any other error passes
    /// through untouched, so this is safe to apply to a whole fallible step.
    pub fn while_attempting(self, action: impl Into<String>) -> Self {
        let action = action.into();
        match self {
            // An empty description would read "Couldn't : no internet
            // connection." The plain offline error is the honest fallback.
            LauncherError::NetworkOffline if !action.trim().is_empty() => {
                LauncherError::NetworkUnavailable { action }
            }
            other => other,
        }
    }

    /// The machine-readable code this error reports: the string the registry
    /// ([`crate::error_code`]) declares for it, so a code cannot be written in
    /// two places and drift.
    pub fn code(&self) -> String {
        let code = match self {
            // `Generic` keeps free text until D1 gives its call sites a
            // typed variant; it is the one code no declaration holds.
            LauncherError::Generic { code, .. } => return code.clone(),
            LauncherError::AuthExpired => ErrorCode::AuthExpired,
            LauncherError::AuthRequired => ErrorCode::AuthRequired,
            LauncherError::DependencyMissing => ErrorCode::DependencyMissing,
            LauncherError::DiskFull => ErrorCode::Diskfull,
            LauncherError::GameCrash => ErrorCode::GameCrash,
            LauncherError::GameVersionNotFound => ErrorCode::GameVersionNotFound,
            LauncherError::HashConfirmationRequired(_) => ErrorCode::HashConfirmationRequired,
            LauncherError::HashMismatch => ErrorCode::HashMismatch,
            LauncherError::InstanceCreateFailed => ErrorCode::InstanceCreateFailed,
            LauncherError::InstanceLocked => ErrorCode::InstanceLocked,
            LauncherError::JavaIncompatible => ErrorCode::JavaIncompatible,
            LauncherError::JavaRuntimeCancelled { .. } => ErrorCode::JavaRuntimeCancelled,
            LauncherError::JavaRuntimeCatalogMissing { .. } => ErrorCode::JavaRuntimeCatalogMissing,
            LauncherError::JavaRuntimeDownloadDisabled { .. } => {
                ErrorCode::JavaRuntimeDownloadDisabled
            }
            LauncherError::JavaRuntimeMissing { .. } => ErrorCode::JavaRuntimeMissing,
            LauncherError::LaunchFailed => ErrorCode::LaunchFailed,
            LauncherError::LoaderProfileNotFound => ErrorCode::LoaderProfileNotFound,
            LauncherError::LocalStateFailed => ErrorCode::LocalStateFailed,
            LauncherError::MavenDescriptor => ErrorCode::MavenDescriptor,
            LauncherError::McpDenied => ErrorCode::McpDenied,
            LauncherError::McpTooManyRequests => ErrorCode::McpTooManyRequests,
            LauncherError::McpUnauthorized => ErrorCode::McpUnauthorized,
            LauncherError::MigrationConflict { .. } => ErrorCode::MigrationConflict,
            LauncherError::MigrationFailed { .. } => ErrorCode::MigrationFailed,
            LauncherError::ModrinthDisabled => ErrorCode::ModrinthDisabled,
            LauncherError::MojangNotFound => ErrorCode::MojangNotFound,
            LauncherError::MsaAuthRequired => ErrorCode::MsaAuthRequired,
            LauncherError::NetworkJavaDisabled => ErrorCode::NetworkJavaDisabled,
            LauncherError::NetworkLoaderDisabled => ErrorCode::NetworkLoaderDisabled,
            LauncherError::NetworkMojangContentDisabled => ErrorCode::NetworkMojangContentDisabled,
            LauncherError::NetworkMojangMetadataDisabled => {
                ErrorCode::NetworkMojangMetadataDisabled
            }
            LauncherError::NetworkMsaDisabled => ErrorCode::NetworkMsaDisabled,
            LauncherError::NetworkOffline | LauncherError::NetworkUnavailable { .. } => {
                ErrorCode::NetworkOffline
            }
            LauncherError::OverrideSecurityViolation => ErrorCode::OverrideSecurityViolation,
            LauncherError::ProcessCaptureFailed { .. } => ErrorCode::ProcessCaptureFailed,
            LauncherError::ProcessStale { .. } => ErrorCode::ProcessStale,
            LauncherError::ProfileCorrupt(..) => ErrorCode::ProfileCorrupt,
            LauncherError::ProfileMissing(..) => ErrorCode::ProfileMissing,
            LauncherError::ProfileUnsupportedMetadata(..) => ErrorCode::ProfileUnsupportedMetadata,
            LauncherError::ProfileWriteFailed => ErrorCode::ProfileWriteFailed,
            LauncherError::RegistryDownloadFailed => ErrorCode::RegistryDownloadFailed,
            LauncherError::RegistryMissing => ErrorCode::RegistryMissing,
            LauncherError::RegistrySignatureInvalid => ErrorCode::RegistrySignatureInvalid,
            LauncherError::SandboxUnavailable => ErrorCode::SandboxUnavailable,
            LauncherError::SchemaTooNew => ErrorCode::SchemaTooNew,
            LauncherError::UnresolvedPlaceholder => ErrorCode::UnresolvedPlaceholder,
            LauncherError::UnsupportedLoader => ErrorCode::UnsupportedLoader,
            LauncherError::UntrustedSource => ErrorCode::UntrustedSource,
            LauncherError::UserDecisionRequired => ErrorCode::UserDecisionRequired,
            LauncherError::VersionNotFound => ErrorCode::VersionNotFound,
            LauncherError::ZipBomb => ErrorCode::ZipBomb,
        };
        code.as_str().to_string()
    }

    /// The registry entry for this error's [`code`](Self::code).
    ///
    /// A [`LauncherError::Generic`] built from a declared code answers with
    /// that entry; one built from a string no declaration holds answers `None`,
    /// which callers that must not guess treat as unclassified rather than as
    /// a category of their own choosing.
    pub fn error_code(&self) -> Option<ErrorCode> {
        ErrorCode::from_code(&self.code())
    }

    /// A [`LauncherError::Generic`] carrying a declared code.
    ///
    /// Build a `Generic` this way from now on: the compiler and the registry
    /// check the code, where a raw string was checked by nothing. It stays until
    /// MASTER_SPEC 32.10's D1 step gives every call site a typed variant.
    pub fn generic(code: ErrorCode, message: impl Into<String>) -> Self {
        LauncherError::Generic {
            code: code.as_str().to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for LauncherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LauncherError::NetworkOffline => {
                write!(
                    f,
                    "No internet connection: Agora couldn't reach the server."
                )
            }
            LauncherError::NetworkUnavailable { action } => {
                write!(f, "Couldn't {action}: no internet connection.")
            }
            LauncherError::RegistryDownloadFailed => {
                write!(
                    f,
                    "Could not download the latest registry. Using cached version."
                )
            }
            LauncherError::RegistrySignatureInvalid => {
                write!(
                    f,
                    "Catalog signature check failed. The catalog may be compromised."
                )
            }
            LauncherError::SchemaTooNew => {
                write!(
                    f,
                    "This registry requires a newer launcher version. Please update the app."
                )
            }
            LauncherError::ZipBomb => {
                write!(
                    f,
                    "Installation aborted: this archive exceeds safety limits."
                )
            }
            LauncherError::OverrideSecurityViolation => {
                write!(
                    f,
                    "Installation aborted: pack override contains forbidden files."
                )
            }
            LauncherError::HashMismatch => {
                write!(f, "Downloaded file does not match its expected hash. It may be corrupted or tampered with.")
            }
            LauncherError::UntrustedSource => {
                write!(f, "Download rejected: URL is not from an allowed source.")
            }
            LauncherError::DiskFull => {
                write!(f, "Not enough disk space to complete this operation.")
            }
            LauncherError::AuthExpired => {
                write!(
                    f,
                    "Your GitHub session has expired. Sign in again to continue."
                )
            }
            LauncherError::AuthRequired => {
                write!(f, "This feature requires GitHub sign-in.")
            }
            LauncherError::MsaAuthRequired => {
                write!(
                    f,
                    "Minecraft launch requires Microsoft sign-in. Run `agora auth login`."
                )
            }
            LauncherError::ModrinthDisabled => {
                write!(f, "Modrinth integration is disabled. Enable it in Settings or install this mod manually.")
            }
            LauncherError::InstanceLocked => {
                write!(
                    f,
                    "This instance is locked. Unlock it to add or remove mods."
                )
            }
            LauncherError::SandboxUnavailable => {
                write!(f, "Dev Mode builds require Docker, Podman, or Firecracker.")
            }
            LauncherError::MojangNotFound => {
                write!(
                    f,
                    "Minecraft Launcher not found. Please install it or set its path in Settings."
                )
            }
            LauncherError::LaunchFailed => {
                write!(f, "Could not start Minecraft. Check the logs for details.")
            }
            LauncherError::GameCrash => {
                write!(
                    f,
                    "The game exited with a crash. Check crash reports for details."
                )
            }
            LauncherError::LocalStateFailed => {
                write!(f, "Local state database error. The app may be misconfigured or out of disk space.")
            }
            LauncherError::InstanceCreateFailed => {
                write!(f, "Could not create the instance.")
            }
            LauncherError::ProfileWriteFailed => {
                write!(f, "Could not update the Mojang launcher profiles. The file may be locked or corrupt.")
            }
            LauncherError::RegistryMissing => {
                write!(
                    f,
                    "The cached catalog is missing. Please connect to the internet and restart."
                )
            }
            LauncherError::UnsupportedLoader => {
                write!(
                    f,
                    "This modloader version is not yet verified by the curation team."
                )
            }

            LauncherError::VersionNotFound => {
                write!(
                    f,
                    "Requested mod version not found. Install the closest compatible version?"
                )
            }
            LauncherError::GameVersionNotFound => {
                write!(
                    f,
                    "This Minecraft version was not found in the official version manifest."
                )
            }
            LauncherError::LoaderProfileNotFound => {
                write!(
                    f,
                    "The modloader profile for this instance could not be resolved or merged with the base Minecraft version."
                )
            }
            LauncherError::ProfileMissing(ref issue) => {
                write!(f, "Profile missing: {}", issue.reasons.join("; "))
            }
            LauncherError::ProfileUnsupportedMetadata(ref issue) => {
                write!(
                    f,
                    "Profile metadata not supported: {}",
                    issue.reasons.join("; ")
                )
            }
            LauncherError::ProfileCorrupt(ref issue) => {
                write!(f, "Profile corrupted: {}", issue.reasons.join("; "))
            }
            LauncherError::JavaIncompatible => {
                write!(
                    f,
                    "The selected Java runtime is older than the version required by this Minecraft version."
                )
            }
            LauncherError::JavaRuntimeMissing { major, component } => {
                write!(
                    f,
                    "No Java {major} runtime found (component: {component}). Install a compatible JDK/JRE."
                )
            }
            LauncherError::JavaRuntimeCatalogMissing { major, os, arch } => {
                write!(
                    f,
                    "No catalog entry for Java {major} on {os}/{arch}. This platform is not supported by the curated catalog."
                )
            }
            LauncherError::UnresolvedPlaceholder => {
                write!(
                    f,
                    "The launch arguments reference a placeholder that could not be substituted."
                )
            }
            LauncherError::DependencyMissing => {
                write!(f, "A mod requires a dependency. Try installing it?")
            }
            LauncherError::McpTooManyRequests => {
                write!(
                    f,
                    "AI client sent too many requests. Approve or deny pending requests first."
                )
            }
            LauncherError::McpDenied => {
                write!(
                    f,
                    "AI tool request denied based on your saved approval preferences."
                )
            }
            LauncherError::McpUnauthorized => {
                write!(
                    f,
                    "AI client connection rejected: invalid or missing token."
                )
            }
            LauncherError::NetworkMojangMetadataDisabled => {
                write!(
                    f,
                    "Mojang metadata fetches are disabled in Privacy settings."
                )
            }
            LauncherError::NetworkMojangContentDisabled => {
                write!(
                    f,
                    "Mojang content downloads are disabled in Privacy settings."
                )
            }
            LauncherError::NetworkLoaderDisabled => {
                write!(
                    f,
                    "Modloader metadata and content downloads are disabled in Privacy settings."
                )
            }
            LauncherError::NetworkMsaDisabled => {
                write!(
                    f,
                    "Microsoft account authentication is disabled in Privacy settings."
                )
            }
            LauncherError::NetworkJavaDisabled => {
                write!(
                    f,
                    "Java runtime downloads are disabled in Privacy settings."
                )
            }
            LauncherError::MavenDescriptor => {
                write!(f, "Invalid Maven descriptor string.")
            }
            LauncherError::MigrationConflict { message } => {
                write!(f, "Migration conflict: {message}")
            }
            LauncherError::MigrationFailed { message } => {
                write!(f, "Migration failed: {message}")
            }
            LauncherError::ProcessCaptureFailed { pid, detail } => {
                write!(f, "Could not capture OS identity for PID {pid}: {detail}")
            }
            LauncherError::ProcessStale { pid, detail } => {
                write!(f, "Process PID {pid} is stale: {detail}")
            }
            LauncherError::UserDecisionRequired => {
                write!(f, "This operation requires user input (conflicts, optional dependencies). Use --yes, --replace-conflicts, or --include-optional to resolve noninteractively.")
            }
            LauncherError::JavaRuntimeCancelled { major, component } => {
                write!(
                    f,
                    "Java {major} runtime provisioning was cancelled (component: {component})."
                )
            }
            LauncherError::JavaRuntimeDownloadDisabled { major, component } => {
                write!(
                    f,
                    "Java {major} runtime download is disabled (component: {component}). \
                     Enable runtime downloads in Privacy settings or choose a local Java installation."
                )
            }
            LauncherError::HashConfirmationRequired(confirmation) => write!(f, "{confirmation}"),
            LauncherError::Generic { message, .. } => write!(f, "{}", message),
        }
    }
}

impl serde::Serialize for LauncherError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let suggested_action = match self {
            LauncherError::MojangNotFound => {
                Some("Install the Minecraft Launcher or set its path in Settings.")
            }
            LauncherError::HashMismatch => {
                Some("The downloaded file may be corrupted or from an untrusted source. Try again.")
            }
            LauncherError::NetworkOffline | LauncherError::NetworkUnavailable { .. } => {
                Some("Check your internet connection or proxy settings, then try again.")
            }
            LauncherError::AuthExpired => Some("Sign in again via Settings to continue."),
            LauncherError::AuthRequired => Some("Sign in via Settings to use this feature."),
            LauncherError::MsaAuthRequired => {
                Some("Run `agora auth login` and complete Microsoft authorization.")
            }
            LauncherError::GameVersionNotFound => {
                Some("Check that the instance targets a released Minecraft version and update the registry.")
            }
            LauncherError::LoaderProfileNotFound => {
                Some("Re-create the instance or select a supported modloader version.")
            }
            LauncherError::JavaIncompatible => {
                Some("Install a newer Java runtime or select a compatible one in Settings.")
            }
            LauncherError::JavaRuntimeMissing { .. } => {
                None // structured suggested_actions used instead
            }
            LauncherError::JavaRuntimeCatalogMissing { .. } => {
                None  // structured suggested_actions used instead
            }
            LauncherError::JavaRuntimeCancelled { .. } => None,
            LauncherError::JavaRuntimeDownloadDisabled { .. } => None,
            LauncherError::UnresolvedPlaceholder => {
                Some("This Minecraft version's arguments are not fully supported yet. Update Agora.")
            }
            _ => None,
        };

        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("code", &self.code())?;
        map.serialize_entry("message", &self.to_string())?;

        // For profile issue variants, serialize structured recoverable_issue details.
        match self {
            LauncherError::ProfileMissing(ref issue)
            | LauncherError::ProfileUnsupportedMetadata(ref issue)
            | LauncherError::ProfileCorrupt(ref issue) => {
                let details = serde_json::json!({
                    "recoverable_issue": {
                        "kind": issue.kind,
                        "profile_path": issue.profile_path,
                        "reasons": issue.reasons,
                    },
                    "suggested_actions": issue.suggested_actions(),
                });
                map.serialize_entry("details", &details)?;
            }
            LauncherError::JavaRuntimeMissing { major, component } => {
                let details = serde_json::json!({
                    "major": major,
                    "component": component,
                    "suggested_actions": [
                        "download_runtime",
                        "choose_java",
                        "cancel",
                    ],
                });
                map.serialize_entry("details", &details)?;
            }
            LauncherError::JavaRuntimeCatalogMissing { major, os, arch } => {
                let details = serde_json::json!({
                    "major": major,
                    "os": os,
                    "arch": arch,
                    "suggested_actions": [
                        "choose_java",
                        "cancel",
                    ],
                });
                map.serialize_entry("details", &details)?;
            }
            LauncherError::JavaRuntimeCancelled { major, component } => {
                let details = serde_json::json!({
                    "major": major,
                    "component": component,
                    "suggested_actions": ["cancel"],
                });
                map.serialize_entry("details", &details)?;
            }
            LauncherError::JavaRuntimeDownloadDisabled { major, component } => {
                let details = serde_json::json!({
                    "major": major,
                    "component": component,
                    "suggested_actions": [
                        "choose_java",
                        "open_privacy",
                        "cancel",
                    ],
                });
                map.serialize_entry("details", &details)?;
            }
            LauncherError::HashConfirmationRequired(confirmation) => {
                let details = serde_json::json!({
                    "file": confirmation.file,
                    "release": confirmation.release,
                    "expected": confirmation.expected,
                    "actual": confirmation.actual,
                    "expected_from": confirmation.expected_from.as_str(),
                    "also_unmet": confirmation
                        .also_unmet
                        .iter()
                        .map(|other| serde_json::json!({
                            "expected_from": other.origin.as_str(),
                            "expected": other.sha256,
                        }))
                        .collect::<Vec<_>>(),
                    "suggested_actions": ["install_anyway", "cancel"],
                });
                map.serialize_entry("details", &details)?;
            }
            _ => {
                map.serialize_entry("details", &None::<String>)?;
            }
        }

        map.serialize_entry("suggested_action", &suggested_action)?;
        map.end()
    }
}

impl std::error::Error for LauncherError {}

pub type LauncherResult<T> = Result<T, LauncherError>;

/// Suggested action strings returned in serialized profile errors.
const SUGGEST_REINSTALL: &str = "reinstall_loader";
const SUGGEST_DELEGATED: &str = "use_delegated_launch";
const SUGGEST_DISMISS: &str = "dismiss";

// The structured payload of the ERR_PROFILE_* errors. It lives with the error
// because the frontend reads it (`details.recoverable_issue`); the Minecraft
// package, which detects these issues, re-exports it.

/// The kind of profile issue encountered during adoption.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProfileIssueKind {
    /// The profile JSON (or the base version JSON) does not exist on disk.
    MissingProfile,
    /// The profile metadata is structurally valid but contains values that
    /// this launcher version does not support (unknown rules, unverifiable
    /// generated artifacts without a receipt, etc.).
    UnsupportedProfileMetadata,
    /// The profile JSON is malformed, corrupted, fails security checks, or
    /// violates structural invariants.
    CorruptProfile,
}

/// A structured error returned during profile adoption.
///
/// Contains enough information to give the user a targeted recovery action
/// (reinstall the loader, switch to delegated launch via Mojang, or dismiss).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProfileIssue {
    pub kind: ProfileIssueKind,
    /// The absolute path to the profile JSON that caused the issue (if known).
    pub profile_path: Option<std::path::PathBuf>,
    /// Human-readable diagnostic reasons.
    pub reasons: Vec<String>,
}

impl ProfileIssue {
    pub fn missing(path: Option<std::path::PathBuf>, reason: impl Into<String>) -> Self {
        Self {
            kind: ProfileIssueKind::MissingProfile,
            profile_path: path,
            reasons: vec![reason.into()],
        }
    }

    pub fn corrupt(path: Option<std::path::PathBuf>, reason: impl Into<String>) -> Self {
        Self {
            kind: ProfileIssueKind::CorruptProfile,
            profile_path: path,
            reasons: vec![reason.into()],
        }
    }

    pub fn unsupported(path: Option<std::path::PathBuf>, reasons: Vec<String>) -> Self {
        Self {
            kind: ProfileIssueKind::UnsupportedProfileMetadata,
            profile_path: path,
            reasons,
        }
    }

    /// Return the standard suggested actions for this kind of issue.
    pub fn suggested_actions(&self) -> Vec<&'static str> {
        match self.kind {
            ProfileIssueKind::MissingProfile => {
                vec![SUGGEST_REINSTALL, SUGGEST_DELEGATED]
            }
            ProfileIssueKind::UnsupportedProfileMetadata => {
                vec![SUGGEST_REINSTALL, SUGGEST_DELEGATED, SUGGEST_DISMISS]
            }
            ProfileIssueKind::CorruptProfile => {
                vec![SUGGEST_REINSTALL, SUGGEST_DELEGATED, SUGGEST_DISMISS]
            }
        }
    }
}

impl From<ProfileIssue> for LauncherError {
    fn from(issue: ProfileIssue) -> Self {
        match issue.kind {
            ProfileIssueKind::MissingProfile => LauncherError::ProfileMissing(issue),
            ProfileIssueKind::UnsupportedProfileMetadata => {
                LauncherError::ProfileUnsupportedMetadata(issue)
            }
            ProfileIssueKind::CorruptProfile => LauncherError::ProfileCorrupt(issue),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_generic_error() {
        let err = LauncherError::Generic {
            code: "ERR_TEST".into(),
            message: "test message".into(),
        };
        let val = serde_json::to_value(err).unwrap();
        assert_eq!(val.get("code").unwrap().as_str().unwrap(), "ERR_TEST");
        assert_eq!(
            val.get("message").unwrap().as_str().unwrap(),
            "test message"
        );
        assert_eq!(val.get("details"), Some(&serde_json::Value::Null));
        assert_eq!(val.get("suggested_action"), Some(&serde_json::Value::Null));
    }

    #[test]
    fn test_serialize_hash_mismatch_has_suggested_action() {
        let err = LauncherError::HashMismatch;
        let val = serde_json::to_value(err).unwrap();
        assert!(val.get("suggested_action").unwrap().as_str().is_some());
    }

    #[test]
    fn test_serialize_mojang_not_found_has_suggested_action() {
        let err = LauncherError::MojangNotFound;
        let val = serde_json::to_value(err).unwrap();
        assert!(val.get("suggested_action").unwrap().as_str().is_some());
    }

    #[test]
    fn test_serialize_network_offline_has_suggested_action() {
        let err = LauncherError::NetworkOffline;
        let val = serde_json::to_value(err).unwrap();
        assert!(val.get("suggested_action").unwrap().as_str().is_some());
    }

    #[test]
    fn test_serialize_auth_required_has_suggested_action() {
        let err = LauncherError::AuthRequired;
        let val = serde_json::to_value(err).unwrap();
        assert!(val.get("suggested_action").unwrap().as_str().is_some());
    }

    #[test]
    fn test_serialize_local_state_failed() {
        let err = LauncherError::LocalStateFailed;
        let val = serde_json::to_value(err).unwrap();
        assert!(val.get("code").unwrap().as_str().is_some());
        assert_eq!(val.get("details"), Some(&serde_json::Value::Null));
        assert_eq!(val.get("suggested_action"), Some(&serde_json::Value::Null));
    }

    #[test]
    fn test_serialize_roundtrip_preserves_code() {
        let err = LauncherError::Generic {
            code: "ERR_RT".into(),
            message: "roundtrip".into(),
        };
        let val = serde_json::to_value(err).unwrap();
        let code: &str = val.get("code").unwrap().as_str().unwrap();
        assert_eq!(code, "ERR_RT");
    }

    /// One sample of every [`LauncherError`] variant, with the code it reported
    /// before the registry existed.
    ///
    /// The macro builds an exhaustive `match` over the same patterns, so adding
    /// a variant stops this module compiling until its sample and code are
    /// written here: the serialization test cannot quietly cover less than the
    /// whole enum.
    macro_rules! every_variant {
        ($($pat:pat => ($sample:expr, $code:literal $(,)?)),* $(,)?) => {{
            fn covers_every_variant(err: &LauncherError) {
                match err {
                    $( $pat => {} ),*
                }
            }
            let samples: Vec<(LauncherError, &'static str)> = vec![$(($sample, $code)),*];
            for (err, _) in &samples {
                covers_every_variant(err);
            }
            samples
        }};
    }

    fn profile_issue(kind: ProfileIssueKind) -> ProfileIssue {
        ProfileIssue {
            kind,
            profile_path: None,
            reasons: vec!["test".into()],
        }
    }

    fn all_variants() -> Vec<(LauncherError, &'static str)> {
        every_variant![
            LauncherError::NetworkOffline => (LauncherError::NetworkOffline, "ERR_NETWORK_OFFLINE"),
            LauncherError::NetworkUnavailable { .. } => (
                LauncherError::NetworkUnavailable {
                    action: "download the catalog".into(),
                },
                "ERR_NETWORK_OFFLINE",
            ),
            LauncherError::RegistryDownloadFailed => (
                LauncherError::RegistryDownloadFailed,
                "ERR_REGISTRY_DOWNLOAD_FAILED",
            ),
            LauncherError::RegistrySignatureInvalid => (
                LauncherError::RegistrySignatureInvalid,
                "ERR_REGISTRY_SIGNATURE_INVALID",
            ),
            LauncherError::SchemaTooNew => (LauncherError::SchemaTooNew, "ERR_SCHEMA_TOO_NEW"),
            LauncherError::ZipBomb => (LauncherError::ZipBomb, "ERR_ZIP_BOMB"),
            LauncherError::OverrideSecurityViolation => (
                LauncherError::OverrideSecurityViolation,
                "ERR_OVERRIDE_SECURITY_VIOLATION",
            ),
            LauncherError::HashMismatch => (LauncherError::HashMismatch, "ERR_HASH_MISMATCH"),
            LauncherError::HashConfirmationRequired(_) => (
                LauncherError::HashConfirmationRequired(Box::new(HashConfirmation {
                    file: "sodium.jar".into(),
                    release: None,
                    expected: "0".repeat(64),
                    actual: "1".repeat(64),
                    expected_from: HashOrigin::CuratorPin,
                    also_unmet: Box::new([]),
                })),
                "ERR_HASH_CONFIRMATION_REQUIRED",
            ),
            LauncherError::UntrustedSource => (LauncherError::UntrustedSource, "ERR_UNTRUSTED_SOURCE"),
            LauncherError::DiskFull => (LauncherError::DiskFull, "ERR_DISKFULL"),
            LauncherError::AuthExpired => (LauncherError::AuthExpired, "ERR_AUTH_EXPIRED"),
            LauncherError::AuthRequired => (LauncherError::AuthRequired, "ERR_AUTH_REQUIRED"),
            LauncherError::MsaAuthRequired => (LauncherError::MsaAuthRequired, "ERR_MSA_AUTH_REQUIRED"),
            LauncherError::ModrinthDisabled => (LauncherError::ModrinthDisabled, "ERR_MODRINTH_DISABLED"),
            LauncherError::InstanceLocked => (LauncherError::InstanceLocked, "ERR_INSTANCE_LOCKED"),
            LauncherError::SandboxUnavailable => (LauncherError::SandboxUnavailable, "ERR_SANDBOX_UNAVAILABLE"),
            LauncherError::MojangNotFound => (LauncherError::MojangNotFound, "ERR_MOJANG_NOT_FOUND"),
            LauncherError::LaunchFailed => (LauncherError::LaunchFailed, "ERR_LAUNCH_FAILED"),
            LauncherError::GameCrash => (LauncherError::GameCrash, "ERR_GAME_CRASH"),
            LauncherError::LocalStateFailed => (LauncherError::LocalStateFailed, "ERR_LOCAL_STATE_FAILED"),
            LauncherError::InstanceCreateFailed => (
                LauncherError::InstanceCreateFailed,
                "ERR_INSTANCE_CREATE_FAILED",
            ),
            LauncherError::ProfileWriteFailed => (
                LauncherError::ProfileWriteFailed,
                "ERR_PROFILE_WRITE_FAILED",
            ),
            LauncherError::RegistryMissing => (LauncherError::RegistryMissing, "ERR_REGISTRY_MISSING"),
            LauncherError::UnsupportedLoader => (
                LauncherError::UnsupportedLoader,
                "ERR_UNSUPPORTED_LOADER",
            ),
            LauncherError::VersionNotFound => (LauncherError::VersionNotFound, "ERR_VERSION_NOT_FOUND"),
            LauncherError::GameVersionNotFound => (
                LauncherError::GameVersionNotFound,
                "ERR_GAME_VERSION_NOT_FOUND",
            ),
            LauncherError::LoaderProfileNotFound => (
                LauncherError::LoaderProfileNotFound,
                "ERR_LOADER_PROFILE_NOT_FOUND",
            ),
            LauncherError::ProfileMissing(..) => (
                LauncherError::ProfileMissing(profile_issue(ProfileIssueKind::MissingProfile)),
                "ERR_PROFILE_MISSING",
            ),
            LauncherError::ProfileUnsupportedMetadata(..) => (
                LauncherError::ProfileUnsupportedMetadata(profile_issue(
                    ProfileIssueKind::UnsupportedProfileMetadata,
                )),
                "ERR_PROFILE_UNSUPPORTED_METADATA",
            ),
            LauncherError::ProfileCorrupt(..) => (
                LauncherError::ProfileCorrupt(profile_issue(ProfileIssueKind::CorruptProfile)),
                "ERR_PROFILE_CORRUPT",
            ),
            LauncherError::JavaIncompatible => (LauncherError::JavaIncompatible, "ERR_JAVA_INCOMPATIBLE"),
            LauncherError::JavaRuntimeMissing { .. } => (
                LauncherError::JavaRuntimeMissing {
                    major: 21,
                    component: "client".into(),
                },
                "ERR_JAVA_RUNTIME_MISSING",
            ),
            LauncherError::JavaRuntimeCatalogMissing { .. } => (
                LauncherError::JavaRuntimeCatalogMissing {
                    major: 21,
                    os: "linux".into(),
                    arch: "x64".into(),
                },
                "ERR_JAVA_RUNTIME_CATALOG_MISSING",
            ),
            LauncherError::UnresolvedPlaceholder => (
                LauncherError::UnresolvedPlaceholder,
                "ERR_UNRESOLVED_PLACEHOLDER",
            ),
            LauncherError::DependencyMissing => (
                LauncherError::DependencyMissing,
                "ERR_DEPENDENCY_MISSING",
            ),
            LauncherError::McpTooManyRequests => (
                LauncherError::McpTooManyRequests,
                "ERR_MCP_TOO_MANY_REQUESTS",
            ),
            LauncherError::McpDenied => (LauncherError::McpDenied, "ERR_MCP_DENIED"),
            LauncherError::McpUnauthorized => (LauncherError::McpUnauthorized, "ERR_MCP_UNAUTHORIZED"),
            LauncherError::NetworkMojangMetadataDisabled => (
                LauncherError::NetworkMojangMetadataDisabled,
                "ERR_NETWORK_MOJANG_METADATA_DISABLED",
            ),
            LauncherError::NetworkMojangContentDisabled => (
                LauncherError::NetworkMojangContentDisabled,
                "ERR_NETWORK_MOJANG_CONTENT_DISABLED",
            ),
            LauncherError::NetworkLoaderDisabled => (
                LauncherError::NetworkLoaderDisabled,
                "ERR_NETWORK_LOADER_DISABLED",
            ),
            LauncherError::NetworkMsaDisabled => (
                LauncherError::NetworkMsaDisabled,
                "ERR_NETWORK_MSA_DISABLED",
            ),
            LauncherError::NetworkJavaDisabled => (
                LauncherError::NetworkJavaDisabled,
                "ERR_NETWORK_JAVA_DISABLED",
            ),
            LauncherError::JavaRuntimeCancelled { .. } => (
                LauncherError::JavaRuntimeCancelled {
                    major: 21,
                    component: "client".into(),
                },
                "ERR_JAVA_RUNTIME_CANCELLED",
            ),
            LauncherError::JavaRuntimeDownloadDisabled { .. } => (
                LauncherError::JavaRuntimeDownloadDisabled {
                    major: 21,
                    component: "client".into(),
                },
                "ERR_JAVA_RUNTIME_DOWNLOAD_DISABLED",
            ),
            LauncherError::MavenDescriptor => (LauncherError::MavenDescriptor, "ERR_MAVEN_DESCRIPTOR"),
            LauncherError::MigrationConflict { .. } => (
                LauncherError::MigrationConflict {
                    message: "conflict".into(),
                },
                "ERR_MIGRATION_CONFLICT",
            ),
            LauncherError::MigrationFailed { .. } => (
                LauncherError::MigrationFailed {
                    message: "failed".into(),
                },
                "ERR_MIGRATION_FAILED",
            ),
            LauncherError::ProcessCaptureFailed { .. } => (
                LauncherError::ProcessCaptureFailed {
                    pid: 42,
                    detail: "test".into(),
                },
                "ERR_PROCESS_CAPTURE_FAILED",
            ),
            LauncherError::ProcessStale { .. } => (
                LauncherError::ProcessStale {
                    pid: 42,
                    detail: "test".into(),
                },
                "ERR_PROCESS_STALE",
            ),
            LauncherError::UserDecisionRequired => (
                LauncherError::UserDecisionRequired,
                "ERR_USER_DECISION_REQUIRED",
            ),
            LauncherError::Generic { .. } => (
                LauncherError::Generic {
                    code: "ERR_X".into(),
                    message: "x".into(),
                },
                "ERR_X",
            ),
        ]
    }

    /// Every variant, including the ones the code registry knows nothing about:
    /// a variant that serialized no code, or a different one, would reach the
    /// UI and the CLI as an unfamiliar error.
    #[test]
    fn test_all_variants_serialize_without_panic() {
        for (err, expected) in all_variants() {
            let value = serde_json::to_value(&err).unwrap();
            assert_eq!(
                value.get("code").and_then(serde_json::Value::as_str),
                Some(expected),
                "{err:?} no longer serializes the code it reported before the registry"
            );
        }
    }

    #[test]
    fn test_serialize_profile_missing_structured_details() {
        use {ProfileIssue, ProfileIssueKind};

        let issue = ProfileIssue {
            kind: ProfileIssueKind::MissingProfile,
            profile_path: None,
            reasons: vec!["File not found".into()],
        };
        let err = LauncherError::ProfileMissing(issue);
        let val = serde_json::to_value(err).unwrap();

        assert_eq!(
            val.get("code").unwrap().as_str().unwrap(),
            "ERR_PROFILE_MISSING"
        );
        let details = val.get("details").unwrap().as_object().unwrap();
        let recoverable = details
            .get("recoverable_issue")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(
            recoverable.get("kind").unwrap().as_str().unwrap(),
            "MissingProfile"
        );
        assert_eq!(
            recoverable.get("reasons").unwrap().as_array().unwrap()[0]
                .as_str()
                .unwrap(),
            "File not found"
        );
        let suggested_actions = details
            .get("suggested_actions")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(!suggested_actions.is_empty());
        assert_eq!(val.get("suggested_action"), Some(&serde_json::Value::Null));
    }

    #[test]
    fn test_serialize_profile_unsupported_structured_details() {
        use {ProfileIssue, ProfileIssueKind};

        let issue = ProfileIssue {
            kind: ProfileIssueKind::UnsupportedProfileMetadata,
            profile_path: Some(std::path::PathBuf::from("/fake/path.json")),
            reasons: vec!["Unknown feature".into()],
        };
        let err = LauncherError::ProfileUnsupportedMetadata(issue);
        let val = serde_json::to_value(err).unwrap();

        assert_eq!(
            val.get("code").unwrap().as_str().unwrap(),
            "ERR_PROFILE_UNSUPPORTED_METADATA"
        );
        let details = val.get("details").unwrap().as_object().unwrap();
        let recoverable = details
            .get("recoverable_issue")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(
            recoverable.get("kind").unwrap().as_str().unwrap(),
            "UnsupportedProfileMetadata"
        );
        assert!(recoverable.get("profile_path").is_some());
    }

    #[test]
    fn test_serialize_profile_corrupt_structured_details() {
        use {ProfileIssue, ProfileIssueKind};

        let issue = ProfileIssue {
            kind: ProfileIssueKind::CorruptProfile,
            profile_path: None,
            reasons: vec!["Truncated JSON".into()],
        };
        let err = LauncherError::ProfileCorrupt(issue);
        let val = serde_json::to_value(err).unwrap();

        assert_eq!(
            val.get("code").unwrap().as_str().unwrap(),
            "ERR_PROFILE_CORRUPT"
        );
        let details = val.get("details").unwrap().as_object().unwrap();
        let recoverable = details
            .get("recoverable_issue")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(
            recoverable.get("kind").unwrap().as_str().unwrap(),
            "CorruptProfile"
        );
    }

    #[test]
    fn test_profile_missing_suggested_actions() {
        use {ProfileIssue, ProfileIssueKind};
        let issue = ProfileIssue {
            kind: ProfileIssueKind::MissingProfile,
            profile_path: None,
            reasons: vec!["gone".into()],
        };
        let err = LauncherError::ProfileMissing(issue);
        let val = serde_json::to_value(err).unwrap();
        let actions = val["details"]["suggested_actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actions, vec!["reinstall_loader", "use_delegated_launch"]);
    }

    #[test]
    fn test_profile_unsupported_suggested_actions() {
        use {ProfileIssue, ProfileIssueKind};
        let issue = ProfileIssue {
            kind: ProfileIssueKind::UnsupportedProfileMetadata,
            profile_path: None,
            reasons: vec!["weird".into()],
        };
        let err = LauncherError::ProfileUnsupportedMetadata(issue);
        let val = serde_json::to_value(err).unwrap();
        let actions = val["details"]["suggested_actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec!["reinstall_loader", "use_delegated_launch", "dismiss"]
        );
    }

    #[test]
    fn test_java_runtime_missing_structured_details() {
        let err = LauncherError::JavaRuntimeMissing {
            major: 21,
            component: "java-runtime-gamma".into(),
        };
        let val = serde_json::to_value(err).unwrap();
        assert_eq!(
            val.get("code").unwrap().as_str().unwrap(),
            "ERR_JAVA_RUNTIME_MISSING"
        );
        let details = val.get("details").unwrap().as_object().unwrap();
        assert_eq!(details.get("major").unwrap().as_u64(), Some(21));
        assert_eq!(
            details.get("component").unwrap().as_str(),
            Some("java-runtime-gamma")
        );
        let actions = details
            .get("suggested_actions")
            .unwrap()
            .as_array()
            .unwrap();
        let action_strs: Vec<&str> = actions.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(
            action_strs,
            vec!["download_runtime", "choose_java", "cancel"]
        );
    }

    #[test]
    fn test_java_runtime_catalog_missing_structured_details() {
        let err = LauncherError::JavaRuntimeCatalogMissing {
            major: 21,
            os: "linux".into(),
            arch: "x64".into(),
        };
        let val = serde_json::to_value(err).unwrap();
        assert_eq!(
            val.get("code").unwrap().as_str().unwrap(),
            "ERR_JAVA_RUNTIME_CATALOG_MISSING"
        );
        let details = val.get("details").unwrap().as_object().unwrap();
        assert_eq!(details.get("major").unwrap().as_u64(), Some(21));
        assert_eq!(details.get("os").unwrap().as_str(), Some("linux"));
        let actions = details
            .get("suggested_actions")
            .unwrap()
            .as_array()
            .unwrap();
        let action_strs: Vec<&str> = actions.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(action_strs, vec!["choose_java", "cancel"]);
    }

    #[test]
    fn test_java_runtime_missing_suggested_action_is_none() {
        let err = LauncherError::JavaRuntimeMissing {
            major: 17,
            component: "client".into(),
        };
        let val = serde_json::to_value(err).unwrap();
        assert_eq!(val.get("suggested_action"), Some(&serde_json::Value::Null));
    }

    #[test]
    fn test_profile_corrupt_suggested_actions_includes_delegated() {
        use {ProfileIssue, ProfileIssueKind};
        let issue = ProfileIssue {
            kind: ProfileIssueKind::CorruptProfile,
            profile_path: None,
            reasons: vec!["bad data".into()],
        };
        let err = LauncherError::ProfileCorrupt(issue);
        let val = serde_json::to_value(err).unwrap();
        let actions = val["details"]["suggested_actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec!["reinstall_loader", "use_delegated_launch", "dismiss"]
        );
    }
}

#[cfg(test)]
mod network_message_tests {
    use super::LauncherError;

    #[test]
    fn offline_message_does_not_claim_cached_data_was_used() {
        let text = LauncherError::NetworkOffline.to_string();
        assert!(!text.contains("cached"), "{text}");
        assert!(!text.contains("You're offline"), "{text}");
    }

    #[test]
    fn while_attempting_names_the_failed_step() {
        let err = LauncherError::NetworkOffline.while_attempting("download the catalog");
        assert_eq!(
            err.to_string(),
            "Couldn't download the catalog: no internet connection."
        );
        assert_eq!(err.code(), "ERR_NETWORK_OFFLINE");
    }

    #[test]
    fn while_attempting_leaves_other_errors_alone() {
        let err = LauncherError::HashMismatch.while_attempting("download the catalog");
        assert!(matches!(err, LauncherError::HashMismatch));
    }

    #[test]
    fn while_attempting_with_blank_action_keeps_the_plain_offline_error() {
        for blank in [
            "", "   ", "	
",
        ] {
            let err = LauncherError::NetworkOffline.while_attempting(blank);
            assert!(matches!(err, LauncherError::NetworkOffline), "{blank:?}");
        }
    }
}
