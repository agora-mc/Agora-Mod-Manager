//! The one registry of machine-readable error codes.
//!
//! MASTER_SPEC 32.10: every code is declared once, with its area and the CLI's
//! exit code, and everything else derives from that declaration —
//! [`LauncherError::code`](crate::error::LauncherError::code) names the entry a
//! variant reports, the CLI's exit code is its [`exit_code`](ErrorCode::exit_code),
//! and a string is a code only if [`from_code`](ErrorCode::from_code) knows it.
//!
//! Nothing else may invent a code. `crates/agora-core/tests/error_codes.rs`
//! fails when a `LauncherError::Generic` construction in non-test code, the
//! desktop or the CLI mentions a code this table does not hold.
//!
//! Adding a code is one row, in its area's block. Changing a row's string
//! changes what reaches the user and the exit code the CLI returns, so it is a
//! change to the contract in [`docs/CLI.md`](../../../docs/CLI.md).
//!
//! The table is generated from the `LauncherError` codes, the codes a
//! `LauncherError::Generic` is built with, and the CLI's exit-code table as they
//! stood before this registry existed; from here on it is edited by hand like
//! any other source.

/// Which part of the system a code comes from. Grouping only: callers decide by
/// [`ErrorCode`], not by area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ErrorArea {
    /// Getting content onto disk: downloads, imports, archives, staging.
    Install,
    /// An instance's own records and files: creation, snapshots, templates.
    Instance,
    /// Two writers, one resource: locks, leases, running games.
    GuardAndLocks,
    /// Reaching a remote service: HTTP, policy gates, sign-in.
    Network,
    /// Who vouched for the bytes and where they came from: the registry, the
    /// catalog, hashes, trust, governance.
    RegistryAndTrust,
    /// Plugins, content providers and the MCP server.
    Plugins,
    /// What a game package knows: versions, loaders, Java, assets, launching.
    Games,
    /// Journalled operations that need deciding what to do next.
    Recovery,
    /// Nothing above: the app shell, local storage, arguments.
    Other,
}

macro_rules! error_codes {
    ($(
        $variant:ident = $code:literal, $area:ident, $exit:literal;
    )*) => {
        /// Every machine-readable code Agora emits, one variant each.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum ErrorCode {
            $( $variant, )*
        }

        impl ErrorCode {
            /// The stable string that reaches error payloads, the UI and users.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( ErrorCode::$variant => $code, )*
                }
            }

            /// The area this code belongs to.
            pub const fn area(self) -> ErrorArea {
                match self {
                    $( ErrorCode::$variant => ErrorArea::$area, )*
                }
            }

            /// The exit code the CLI returns for this code. 1 means
            /// unclassified; no code exits 0, which is success.
            pub const fn exit_code(self) -> i32 {
                match self {
                    $( ErrorCode::$variant => $exit, )*
                }
            }

            /// The code this string names, or `None` for a string no
            /// declaration holds. A caller that must not guess treats `None` as
            /// unclassified rather than as permission.
            pub fn from_code(code: &str) -> Option<Self> {
                match code {
                    $( $code => Some(ErrorCode::$variant), )*
                    _ => None,
                }
            }
        }

        /// Every declared code, in table order.
        pub const ALL: &[ErrorCode] = &[ $( ErrorCode::$variant, )* ];
    };
}

error_codes! {
    // ---- Install ----
    ArchiveCacheCreate = "ERR_ARCHIVE_CACHE_CREATE", Install, 1;
    ArchiveCacheRead = "ERR_ARCHIVE_CACHE_READ", Install, 1;
    ArchiveCacheStat = "ERR_ARCHIVE_CACHE_STAT", Install, 1;
    ArchiveEntry = "ERR_ARCHIVE_ENTRY", Install, 1;
    ArchiveExtract = "ERR_ARCHIVE_EXTRACT", Install, 1;
    ArchiveFlush = "ERR_ARCHIVE_FLUSH", Install, 1;
    ArchiveForbiddenEntry = "ERR_ARCHIVE_FORBIDDEN_ENTRY", Install, 1;
    ArchiveInvalidName = "ERR_ARCHIVE_INVALID_NAME", Install, 1;
    ArchiveOpen = "ERR_ARCHIVE_OPEN", Install, 1;
    ArchiveParse = "ERR_ARCHIVE_PARSE", Install, 1;
    ArchiveRename = "ERR_ARCHIVE_RENAME", Install, 1;
    ArchiveSizeMismatch = "ERR_ARCHIVE_SIZE_MISMATCH", Install, 1;
    ArchiveSync = "ERR_ARCHIVE_SYNC", Install, 1;
    ArchiveWrite = "ERR_ARCHIVE_WRITE", Install, 1;
    AtomicWrite = "ERR_ATOMIC_WRITE", Install, 1;
    BatchEmpty = "ERR_BATCH_EMPTY", Install, 1;
    Copy = "ERR_COPY", Install, 1;
    DependencyCheckDownload = "ERR_DEPENDENCY_CHECK_DOWNLOAD", Install, 1;
    DependencyCheckHash = "ERR_DEPENDENCY_CHECK_HASH", Install, 1;
    DependencyCheckSource = "ERR_DEPENDENCY_CHECK_SOURCE", Install, 1;
    DependencyMissing = "ERR_DEPENDENCY_MISSING", Install, 70;
    DirCreate = "ERR_DIR_CREATE", Install, 1;
    Downloads = "ERR_DOWNLOADS", Install, 1;
    DownloadsLock = "ERR_DOWNLOADS_LOCK", Install, 1;
    DownloadsTask = "ERR_DOWNLOADS_TASK", Install, 1;
    DownloadHttp = "ERR_DOWNLOAD_HTTP", Install, 1;
    DownloadSizeExceeded = "ERR_DOWNLOAD_SIZE_EXCEEDED", Install, 1;
    DownloadSizeMismatch = "ERR_DOWNLOAD_SIZE_MISMATCH", Install, 1;
    Export = "ERR_EXPORT", Install, 1;
    ExportCopy = "ERR_EXPORT_COPY", Install, 1;
    ExportEntry = "ERR_EXPORT_ENTRY", Install, 1;
    ExportFilename = "ERR_EXPORT_FILENAME", Install, 1;
    ExportInstallerHash = "ERR_EXPORT_INSTALLER_HASH", Install, 1;
    ExportInstallerMetadata = "ERR_EXPORT_INSTALLER_METADATA", Install, 1;
    ExportMkdir = "ERR_EXPORT_MKDIR", Install, 1;
    ExportModNotBundled = "ERR_EXPORT_MOD_NOT_BUNDLED", Install, 1;
    ExportNoPinned = "ERR_EXPORT_NO_PINNED", Install, 1;
    ExportNoServerJar = "ERR_EXPORT_NO_SERVER_JAR", Install, 1;
    ExportReaddir = "ERR_EXPORT_READDIR", Install, 1;
    ExportUnsupportedLoader = "ERR_EXPORT_UNSUPPORTED_LOADER", Install, 1;
    ExportUnverifiedLoader = "ERR_EXPORT_UNVERIFIED_LOADER", Install, 1;
    ExportWorldName = "ERR_EXPORT_WORLD_NAME", Install, 1;
    ExportWorldNotFound = "ERR_EXPORT_WORLD_NOT_FOUND", Install, 1;
    ExportWrite = "ERR_EXPORT_WRITE", Install, 1;
    FileChanged = "ERR_FILE_CHANGED", Install, 1;
    FileExists = "ERR_FILE_EXISTS", Install, 1;
    FileWrite = "ERR_FILE_WRITE", Install, 1;
    ImportCleanup = "ERR_IMPORT_CLEANUP", Install, 1;
    ImportCopy = "ERR_IMPORT_COPY", Install, 1;
    ImportDiscovery = "ERR_IMPORT_DISCOVERY", Install, 1;
    ImportFinalize = "ERR_IMPORT_FINALIZE", Install, 1;
    ImportHash = "ERR_IMPORT_HASH", Install, 1;
    ImportHttpClient = "ERR_IMPORT_HTTP_CLIENT", Install, 1;
    ImportMissingCfg = "ERR_IMPORT_MISSING_CFG", Install, 1;
    ImportMissingIndex = "ERR_IMPORT_MISSING_INDEX", Install, 1;
    ImportMkdir = "ERR_IMPORT_MKDIR", Install, 1;
    ImportNotDir = "ERR_IMPORT_NOT_DIR", Install, 1;
    ImportNoExisting = "ERR_IMPORT_NO_EXISTING", Install, 1;
    ImportOpen = "ERR_IMPORT_OPEN", Install, 1;
    ImportOpenZip = "ERR_IMPORT_OPEN_ZIP", Install, 1;
    ImportParseIndex = "ERR_IMPORT_PARSE_INDEX", Install, 1;
    ImportParsePack = "ERR_IMPORT_PARSE_PACK", Install, 1;
    ImportPlan = "ERR_IMPORT_PLAN", Install, 1;
    ImportQuarantine = "ERR_IMPORT_QUARANTINE", Install, 1;
    ImportRead = "ERR_IMPORT_READ", Install, 1;
    ImportReadCfg = "ERR_IMPORT_READ_CFG", Install, 1;
    ImportReadPack = "ERR_IMPORT_READ_PACK", Install, 1;
    ImportReadZip = "ERR_IMPORT_READ_ZIP", Install, 1;
    ImportRecursiveSource = "ERR_IMPORT_RECURSIVE_SOURCE", Install, 1;
    ImportSource = "ERR_IMPORT_SOURCE", Install, 1;
    ImportSourceChanged = "ERR_IMPORT_SOURCE_CHANGED", Install, 1;
    ImportSourceMissing = "ERR_IMPORT_SOURCE_MISSING", Install, 1;
    ImportTask = "ERR_IMPORT_TASK", Install, 1;
    ImportUnsafePath = "ERR_IMPORT_UNSAFE_PATH", Install, 1;
    ImportWrite = "ERR_IMPORT_WRITE", Install, 1;
    ImportZip = "ERR_IMPORT_ZIP", Install, 1;
    InstallerFailed = "ERR_INSTALLER_FAILED", Install, 1;
    InstallerTimeout = "ERR_INSTALLER_TIMEOUT", Install, 1;
    InstallActive = "ERR_INSTALL_ACTIVE", Install, 1;
    InstallFailed = "ERR_INSTALL_FAILED", Install, 1;
    InvalidFilename = "ERR_INVALID_FILENAME", Install, 1;
    InvalidPathComponent = "ERR_INVALID_PATH_COMPONENT", Install, 1;
    OverrideSecurityViolation = "ERR_OVERRIDE_SECURITY_VIOLATION", Install, 30;
    Pack = "ERR_PACK", Install, 1;
    PackChanged = "ERR_PACK_CHANGED", Install, 1;
    PackEmpty = "ERR_PACK_EMPTY", Install, 1;
    PackNotReviewed = "ERR_PACK_NOT_REVIEWED", Install, 1;
    PackParse = "ERR_PACK_PARSE", Install, 1;
    PackRead = "ERR_PACK_READ", Install, 1;
    PackUpdate = "ERR_PACK_UPDATE", Install, 1;
    PackVersionNotFound = "ERR_PACK_VERSION_NOT_FOUND", Install, 1;
    Path = "ERR_PATH", Install, 1;
    PruneEmpty = "ERR_PRUNE_EMPTY", Install, 1;
    PruneTask = "ERR_PRUNE_TASK", Install, 1;
    RemoveFailed = "ERR_REMOVE_FAILED", Install, 1;
    SizeMismatch = "ERR_SIZE_MISMATCH", Install, 1;
    StagingCreate = "ERR_STAGING_CREATE", Install, 1;
    StagingParent = "ERR_STAGING_PARENT", Install, 1;
    StagingRemove = "ERR_STAGING_REMOVE", Install, 1;
    TarEntries = "ERR_TAR_ENTRIES", Install, 1;
    TarEntry = "ERR_TAR_ENTRY", Install, 1;
    TarHeader = "ERR_TAR_HEADER", Install, 1;
    TooManyFiles = "ERR_TOO_MANY_FILES", Install, 1;
    UnsupportedArchiveType = "ERR_UNSUPPORTED_ARCHIVE_TYPE", Install, 1;
    UsePackImport = "ERR_USE_PACK_IMPORT", Install, 1;
    UsePackInstall = "ERR_USE_PACK_INSTALL", Install, 1;
    ZipBomb = "ERR_ZIP_BOMB", Install, 30;
    ZipSlip = "ERR_ZIP_SLIP", Install, 1;
    ZipTooLarge = "ERR_ZIP_TOO_LARGE", Install, 1;
    // ---- Instance ----
    Backup = "ERR_BACKUP", Instance, 1;
    Clone = "ERR_CLONE", Instance, 1;
    CloneSameId = "ERR_CLONE_SAME_ID", Instance, 1;
    ContentConvertTask = "ERR_CONTENT_CONVERT_TASK", Instance, 1;
    ContentItemNotMinecraft = "ERR_CONTENT_ITEM_NOT_MINECRAFT", Instance, 1;
    ContentItemNotSingle = "ERR_CONTENT_ITEM_NOT_SINGLE", Instance, 1;
    ContentItemOtherGame = "ERR_CONTENT_ITEM_OTHER_GAME", Instance, 1;
    ContentPath = "ERR_CONTENT_PATH", Instance, 1;
    ContentPreviewTask = "ERR_CONTENT_PREVIEW_TASK", Instance, 1;
    ContentRead = "ERR_CONTENT_READ", Instance, 1;
    ContentSettingTask = "ERR_CONTENT_SETTING_TASK", Instance, 1;
    ContentStore = "ERR_CONTENT_STORE", Instance, 1;
    DatapackNotFound = "ERR_DATAPACK_NOT_FOUND", Instance, 1;
    GroupName = "ERR_GROUP_NAME", Instance, 1;
    Icon = "ERR_ICON", Instance, 1;
    IconUrl = "ERR_ICON_URL", Instance, 1;
    InstanceBusy = "ERR_INSTANCE_BUSY", Instance, 1;
    InstanceCreateFailed = "ERR_INSTANCE_CREATE_FAILED", Instance, 12;
    InstanceDelegatedOnly = "ERR_INSTANCE_DELEGATED_ONLY", Instance, 1;
    InstanceDelete = "ERR_INSTANCE_DELETE", Instance, 1;
    InstanceExists = "ERR_INSTANCE_EXISTS", Instance, 1;
    InstanceFailed = "ERR_INSTANCE_FAILED", Instance, 1;
    InstanceGuard = "ERR_INSTANCE_GUARD", Instance, 1;
    InstanceLaunchIndeterminate = "ERR_INSTANCE_LAUNCH_INDETERMINATE", Instance, 1;
    InstanceManifestMissing = "ERR_INSTANCE_MANIFEST_MISSING", Instance, 1;
    InstanceNotFound = "ERR_INSTANCE_NOT_FOUND", Instance, 1;
    InstancePath = "ERR_INSTANCE_PATH", Instance, 1;
    InstancePromote = "ERR_INSTANCE_PROMOTE", Instance, 1;
    InstanceRestore = "ERR_INSTANCE_RESTORE", Instance, 1;
    InstanceRunning = "ERR_INSTANCE_RUNNING", Instance, 1;
    InstanceStaging = "ERR_INSTANCE_STAGING", Instance, 1;
    InvalidInstance = "ERR_INVALID_INSTANCE", Instance, 1;
    InvalidInstanceName = "ERR_INVALID_INSTANCE_NAME", Instance, 1;
    InvalidVersionId = "ERR_INVALID_VERSION_ID", Instance, 1;
    ItemNotFound = "ERR_ITEM_NOT_FOUND", Instance, 1;
    LkgRead = "ERR_LKG_READ", Instance, 1;
    Loadout = "ERR_LOADOUT", Instance, 1;
    ModDisable = "ERR_MOD_DISABLE", Instance, 1;
    ModDisabledCopyExists = "ERR_MOD_DISABLED_COPY_EXISTS", Instance, 1;
    ModEnable = "ERR_MOD_ENABLE", Instance, 1;
    ModFileNotFound = "ERR_MOD_FILE_NOT_FOUND", Instance, 1;
    ModGroup = "ERR_MOD_GROUP", Instance, 1;
    ModNotFound = "ERR_MOD_NOT_FOUND", Instance, 1;
    NotesTooLong = "ERR_NOTES_TOO_LONG", Instance, 1;
    NotAnImage = "ERR_NOT_AN_IMAGE", Instance, 1;
    RestoreRowSync = "ERR_RESTORE_ROW_SYNC", Instance, 1;
    RestoreTask = "ERR_RESTORE_TASK", Instance, 1;
    SettingInvalid = "ERR_SETTING_INVALID", Instance, 1;
    SettingUnknown = "ERR_SETTING_UNKNOWN", Instance, 1;
    Share = "ERR_SHARE", Instance, 1;
    ShareTask = "ERR_SHARE_TASK", Instance, 1;
    Snapshot = "ERR_SNAPSHOT", Instance, 1;
    SnapshotCreate = "ERR_SNAPSHOT_CREATE", Instance, 1;
    SnapshotFailed = "ERR_SNAPSHOT_FAILED", Instance, 1;
    SnapshotPending = "ERR_SNAPSHOT_PENDING", Instance, 1;
    SnapshotScope = "ERR_SNAPSHOT_SCOPE", Instance, 1;
    SnapshotTask = "ERR_SNAPSHOT_TASK", Instance, 1;
    Template = "ERR_TEMPLATE", Instance, 1;
    TemplateApply = "ERR_TEMPLATE_APPLY", Instance, 1;
    TemplateNotFound = "ERR_TEMPLATE_NOT_FOUND", Instance, 1;
    VersionCacheMiss = "ERR_VERSION_CACHE_MISS", Instance, 1;
    VersionDir = "ERR_VERSION_DIR", Instance, 1;
    // ---- GuardAndLocks ----
    AlreadyRunning = "ERR_ALREADY_RUNNING", GuardAndLocks, 1;
    CoreContextLock = "ERR_CORE_CONTEXT_LOCK", GuardAndLocks, 1;
    Drift = "ERR_DRIFT", GuardAndLocks, 1;
    DriftTask = "ERR_DRIFT_TASK", GuardAndLocks, 1;
    GameIdentityUnknown = "ERR_GAME_IDENTITY_UNKNOWN", GuardAndLocks, 1;
    GameNotRunning = "ERR_GAME_NOT_RUNNING", GuardAndLocks, 1;
    InstanceLocked = "ERR_INSTANCE_LOCKED", GuardAndLocks, 11;
    InvalidLock = "ERR_INVALID_LOCK", GuardAndLocks, 1;
    InvalidLockName = "ERR_INVALID_LOCK_NAME", GuardAndLocks, 1;
    KillFailed = "ERR_KILL_FAILED", GuardAndLocks, 1;
    LockfileCommand = "ERR_LOCKFILE_COMMAND", GuardAndLocks, 1;
    LockAcquire = "ERR_LOCK_ACQUIRE", GuardAndLocks, 1;
    LockCancelled = "ERR_LOCK_CANCELLED", GuardAndLocks, 1;
    LockContested = "ERR_LOCK_CONTESTED", GuardAndLocks, 1;
    LockIo = "ERR_LOCK_IO", GuardAndLocks, 1;
    LockSnapshot = "ERR_LOCK_SNAPSHOT", GuardAndLocks, 1;
    LockTimeout = "ERR_LOCK_TIMEOUT", GuardAndLocks, 1;
    MojangLauncherRunning = "ERR_MOJANG_LAUNCHER_RUNNING", GuardAndLocks, 1;
    NoLockSnapshot = "ERR_NO_LOCK_SNAPSHOT", GuardAndLocks, 1;
    NoPid = "ERR_NO_PID", GuardAndLocks, 1;
    OperationCancelled = "ERR_OPERATION_CANCELLED", GuardAndLocks, 1;
    PidMismatch = "ERR_PID_MISMATCH", GuardAndLocks, 1;
    PluginsLock = "ERR_PLUGINS_LOCK", GuardAndLocks, 1;
    ProcessCaptureFailed = "ERR_PROCESS_CAPTURE_FAILED", GuardAndLocks, 100;
    ProcessStale = "ERR_PROCESS_STALE", GuardAndLocks, 101;
    RuntimeLease = "ERR_RUNTIME_LEASE", GuardAndLocks, 1;
    RuntimeLeaseUnreadable = "ERR_RUNTIME_LEASE_UNREADABLE", GuardAndLocks, 1;
    SessionAlreadyExists = "ERR_SESSION_ALREADY_EXISTS", GuardAndLocks, 1;
    SessionLedger = "ERR_SESSION_LEDGER", GuardAndLocks, 1;
    SessionManagerLock = "ERR_SESSION_MANAGER_LOCK", GuardAndLocks, 1;
    SessionNotFound = "ERR_SESSION_NOT_FOUND", GuardAndLocks, 1;
    Wait = "ERR_WAIT", GuardAndLocks, 1;
    // ---- Network ----
    AuthDeviceCode = "ERR_AUTH_DEVICE_CODE", Network, 1;
    AuthDeviceKeyRead = "ERR_AUTH_DEVICE_KEY_READ", Network, 1;
    AuthDeviceKeyWrite = "ERR_AUTH_DEVICE_KEY_WRITE", Network, 1;
    AuthDpapiWrite = "ERR_AUTH_DPAPI_WRITE", Network, 1;
    AuthEncrypt = "ERR_AUTH_ENCRYPT", Network, 1;
    AuthExpired = "ERR_AUTH_EXPIRED", Network, 40;
    AuthFallbackDelete = "ERR_AUTH_FALLBACK_DELETE", Network, 1;
    AuthFallbackPath = "ERR_AUTH_FALLBACK_PATH", Network, 1;
    AuthFallbackRead = "ERR_AUTH_FALLBACK_READ", Network, 1;
    AuthFallbackWrite = "ERR_AUTH_FALLBACK_WRITE", Network, 1;
    AuthKeyringDelete = "ERR_AUTH_KEYRING_DELETE", Network, 1;
    AuthKeyringRead = "ERR_AUTH_KEYRING_READ", Network, 1;
    AuthLogout = "ERR_AUTH_LOGOUT", Network, 1;
    AuthNotConfigured = "ERR_AUTH_NOT_CONFIGURED", Network, 1;
    AuthProfile = "ERR_AUTH_PROFILE", Network, 1;
    AuthRequired = "ERR_AUTH_REQUIRED", Network, 40;
    AuthSerialize = "ERR_AUTH_SERIALIZE", Network, 1;
    ClientDownloadMissing = "ERR_CLIENT_DOWNLOAD_MISSING", Network, 1;
    HttpClientBuild = "ERR_HTTP_CLIENT_BUILD", Network, 1;
    HttpClientInit = "ERR_HTTP_CLIENT_INIT", Network, 1;
    HttpHostNotAllowed = "ERR_HTTP_HOST_NOT_ALLOWED", Network, 1;
    HttpIpLiteral = "ERR_HTTP_IP_LITERAL", Network, 1;
    HttpPort = "ERR_HTTP_PORT", Network, 1;
    HttpScheme = "ERR_HTTP_SCHEME", Network, 1;
    HttpStatus = "ERR_HTTP_STATUS", Network, 1;
    HttpUserinfo = "ERR_HTTP_USERINFO", Network, 1;
    InvalidQuery = "ERR_INVALID_QUERY", Network, 1;
    InvalidRedirect = "ERR_INVALID_REDIRECT", Network, 1;
    InvalidUrl = "ERR_INVALID_URL", Network, 1;
    ModrinthDisabled = "ERR_MODRINTH_DISABLED", Network, 50;
    ModrinthMetadata = "ERR_MODRINTH_METADATA", Network, 1;
    MsaAuthRequired = "ERR_MSA_AUTH_REQUIRED", Network, 40;
    MsaNotAuthenticated = "ERR_MSA_NOT_AUTHENTICATED", Network, 1;
    MsaNoPendingLogin = "ERR_MSA_NO_PENDING_LOGIN", Network, 1;
    MsaOpenBrowser = "ERR_MSA_OPEN_BROWSER", Network, 1;
    MsaTestCredentialsParse = "ERR_MSA_TEST_CREDENTIALS_PARSE", Network, 1;
    MsaVerificationUrl = "ERR_MSA_VERIFICATION_URL", Network, 1;
    Network = "ERR_NETWORK", Network, 1;
    NetworkDisabled = "ERR_NETWORK_DISABLED", Network, 1;
    NetworkEndpointDisabled = "ERR_NETWORK_ENDPOINT_DISABLED", Network, 1;
    NetworkGateMissing = "ERR_NETWORK_GATE_MISSING", Network, 1;
    NetworkJavaDisabled = "ERR_NETWORK_JAVA_DISABLED", Network, 94;
    NetworkLoaderDisabled = "ERR_NETWORK_LOADER_DISABLED", Network, 92;
    NetworkLockdown = "ERR_NETWORK_LOCKDOWN", Network, 1;
    NetworkMojangContentDisabled = "ERR_NETWORK_MOJANG_CONTENT_DISABLED", Network, 91;
    NetworkMojangMetadataDisabled = "ERR_NETWORK_MOJANG_METADATA_DISABLED", Network, 90;
    NetworkMsaDisabled = "ERR_NETWORK_MSA_DISABLED", Network, 93;
    NetworkOffline = "ERR_NETWORK_OFFLINE", Network, 20;
    NetworkSettingsMalformed = "ERR_NETWORK_SETTINGS_MALFORMED", Network, 1;
    NetworkSettingsUnreadable = "ERR_NETWORK_SETTINGS_UNREADABLE", Network, 1;
    NoEnabledSource = "ERR_NO_ENABLED_SOURCE", Network, 1;
    PackOffline = "ERR_PACK_OFFLINE", Network, 1;
    ProxySave = "ERR_PROXY_SAVE", Network, 1;
    ProxyUrl = "ERR_PROXY_URL", Network, 1;
    RateLimited = "ERR_RATE_LIMITED", Network, 1;
    RedirectNoLocation = "ERR_REDIRECT_NO_LOCATION", Network, 1;
    ResponseEncoding = "ERR_RESPONSE_ENCODING", Network, 1;
    ResponseTooLarge = "ERR_RESPONSE_TOO_LARGE", Network, 1;
    SourceDisabled = "ERR_SOURCE_DISABLED", Network, 1;
    SourceNotAllowed = "ERR_SOURCE_NOT_ALLOWED", Network, 1;
    SsrfBlocked = "ERR_SSRF_BLOCKED", Network, 1;
    TooManyRedirects = "ERR_TOO_MANY_REDIRECTS", Network, 1;
    UnexpectedRedirect = "ERR_UNEXPECTED_REDIRECT", Network, 1;
    // ---- RegistryAndTrust ----
    ArtifactHashInvalid = "ERR_ARTIFACT_HASH_INVALID", RegistryAndTrust, 1;
    ArtifactHashMissing = "ERR_ARTIFACT_HASH_MISSING", RegistryAndTrust, 1;
    AssertedHashBlank = "ERR_ASSERTED_HASH_BLANK", RegistryAndTrust, 1;
    BrowseStale = "ERR_BROWSE_STALE", RegistryAndTrust, 1;
    CatalogClassify = "ERR_CATALOG_CLASSIFY", RegistryAndTrust, 1;
    CatalogUnplaceable = "ERR_CATALOG_UNPLACEABLE", RegistryAndTrust, 1;
    CatalogWrongGame = "ERR_CATALOG_WRONG_GAME", RegistryAndTrust, 1;
    CuratorReviewsParse = "ERR_CURATOR_REVIEWS_PARSE", RegistryAndTrust, 1;
    DirectHashManifest = "ERR_DIRECT_HASH_MANIFEST", RegistryAndTrust, 1;
    FrameworkCheck = "ERR_FRAMEWORK_CHECK", RegistryAndTrust, 1;
    GovernanceConfig = "ERR_GOVERNANCE_CONFIG", RegistryAndTrust, 1;
    GovernanceDiagnostics = "ERR_GOVERNANCE_DIAGNOSTICS", RegistryAndTrust, 1;
    GovernanceItemNotFound = "ERR_GOVERNANCE_ITEM_NOT_FOUND", RegistryAndTrust, 1;
    GovernanceNoVoteIssue = "ERR_GOVERNANCE_NO_VOTE_ISSUE", RegistryAndTrust, 1;
    GovernanceQuery = "ERR_GOVERNANCE_QUERY", RegistryAndTrust, 1;
    GovernanceVote = "ERR_GOVERNANCE_VOTE", RegistryAndTrust, 1;
    GovernanceVoteForbidden = "ERR_GOVERNANCE_VOTE_FORBIDDEN", RegistryAndTrust, 1;
    GovernanceVoteImmune = "ERR_GOVERNANCE_VOTE_IMMUNE", RegistryAndTrust, 1;
    GovernanceVoteIssueGone = "ERR_GOVERNANCE_VOTE_ISSUE_GONE", RegistryAndTrust, 1;
    GovJsonParse = "ERR_GOV_JSON_PARSE", RegistryAndTrust, 1;
    GovJsonRead = "ERR_GOV_JSON_READ", RegistryAndTrust, 1;
    HashConfirmationRequired = "ERR_HASH_CONFIRMATION_REQUIRED", RegistryAndTrust, 30;
    HashMalformed = "ERR_HASH_MALFORMED", RegistryAndTrust, 1;
    HashMismatch = "ERR_HASH_MISMATCH", RegistryAndTrust, 30;
    HashUnavailable = "ERR_HASH_UNAVAILABLE", RegistryAndTrust, 1;
    HealthBlocked = "ERR_HEALTH_BLOCKED", RegistryAndTrust, 1;
    HealthTask = "ERR_HEALTH_TASK", RegistryAndTrust, 1;
    ManifestMissing = "ERR_MANIFEST_MISSING", RegistryAndTrust, 1;
    ManifestParse = "ERR_MANIFEST_PARSE", RegistryAndTrust, 1;
    ManifestReadOnly = "ERR_MANIFEST_READ_ONLY", RegistryAndTrust, 1;
    ManifestSerialize = "ERR_MANIFEST_SERIALIZE", RegistryAndTrust, 1;
    ManifestWrite = "ERR_MANIFEST_WRITE", RegistryAndTrust, 1;
    NotApplicable = "ERR_NOT_APPLICABLE", RegistryAndTrust, 1;
    NotOwned = "ERR_NOT_OWNED", RegistryAndTrust, 1;
    OverrideFailed = "ERR_OVERRIDE_FAILED", RegistryAndTrust, 1;
    PinnedManifest = "ERR_PINNED_MANIFEST", RegistryAndTrust, 1;
    ReceiptParse = "ERR_RECEIPT_PARSE", RegistryAndTrust, 1;
    ReceiptRead = "ERR_RECEIPT_READ", RegistryAndTrust, 1;
    ReceiptSchema = "ERR_RECEIPT_SCHEMA", RegistryAndTrust, 1;
    ReceiptSerialize = "ERR_RECEIPT_SERIALIZE", RegistryAndTrust, 1;
    Registry = "ERR_REGISTRY", RegistryAndTrust, 1;
    RegistryBundleInvalid = "ERR_REGISTRY_BUNDLE_INVALID", RegistryAndTrust, 1;
    RegistryBundleRead = "ERR_REGISTRY_BUNDLE_READ", RegistryAndTrust, 1;
    RegistryCachedStampUnreadable = "ERR_REGISTRY_CACHED_STAMP_UNREADABLE", RegistryAndTrust, 1;
    RegistryCheck = "ERR_REGISTRY_CHECK", RegistryAndTrust, 1;
    RegistryDownloadFailed = "ERR_REGISTRY_DOWNLOAD_FAILED", RegistryAndTrust, 21;
    RegistryExpired = "ERR_REGISTRY_EXPIRED", RegistryAndTrust, 1;
    RegistryFloor = "ERR_REGISTRY_FLOOR", RegistryAndTrust, 1;
    RegistryGameCatalog = "ERR_REGISTRY_GAME_CATALOG", RegistryAndTrust, 1;
    RegistryMissing = "ERR_REGISTRY_MISSING", RegistryAndTrust, 13;
    RegistryNewerSchema = "ERR_REGISTRY_NEWER_SCHEMA", RegistryAndTrust, 1;
    RegistryNotFound = "ERR_REGISTRY_NOT_FOUND", RegistryAndTrust, 1;
    RegistryNoBundle = "ERR_REGISTRY_NO_BUNDLE", RegistryAndTrust, 1;
    RegistryQuery = "ERR_REGISTRY_QUERY", RegistryAndTrust, 1;
    RegistryRateLimited = "ERR_REGISTRY_RATE_LIMITED", RegistryAndTrust, 1;
    RegistryRollback = "ERR_REGISTRY_ROLLBACK", RegistryAndTrust, 1;
    RegistrySignatureInvalid = "ERR_REGISTRY_SIGNATURE_INVALID", RegistryAndTrust, 13;
    RegistryStale = "ERR_REGISTRY_STALE", RegistryAndTrust, 1;
    RegistryStampInvalid = "ERR_REGISTRY_STAMP_INVALID", RegistryAndTrust, 1;
    RegistryTrustState = "ERR_REGISTRY_TRUST_STATE", RegistryAndTrust, 1;
    RegistryUnreadable = "ERR_REGISTRY_UNREADABLE", RegistryAndTrust, 1;
    RegistryWrite = "ERR_REGISTRY_WRITE", RegistryAndTrust, 1;
    SchemaTooNew = "ERR_SCHEMA_TOO_NEW", RegistryAndTrust, 13;
    SecurityViolation = "ERR_SECURITY_VIOLATION", RegistryAndTrust, 1;
    SkinFile = "ERR_SKIN_FILE", RegistryAndTrust, 1;
    UntrustedSource = "ERR_UNTRUSTED_SOURCE", RegistryAndTrust, 31;
    UnverifiedLoaderLibrary = "ERR_UNVERIFIED_LOADER_LIBRARY", RegistryAndTrust, 1;
    UnverifiedPacksDisabled = "ERR_UNVERIFIED_PACKS_DISABLED", RegistryAndTrust, 1;
    UpdateTargetMissing = "ERR_UPDATE_TARGET_MISSING", RegistryAndTrust, 1;
    // ---- Plugins ----
    Bisect = "ERR_BISECT", Plugins, 1;
    CrashAnalyzeNoFiles = "ERR_CRASH_ANALYZE_NO_FILES", Plugins, 1;
    CrashAnswer = "ERR_CRASH_ANSWER", Plugins, 1;
    CrashEvidence = "ERR_CRASH_EVIDENCE", Plugins, 1;
    CrashInstanceRead = "ERR_CRASH_INSTANCE_READ", Plugins, 1;
    CrashInstanceReads = "ERR_CRASH_INSTANCE_READS", Plugins, 1;
    CrashInstanceUnknown = "ERR_CRASH_INSTANCE_UNKNOWN", Plugins, 1;
    CrashInstanceView = "ERR_CRASH_INSTANCE_VIEW", Plugins, 1;
    CrashSuspectNotItem = "ERR_CRASH_SUSPECT_NOT_ITEM", Plugins, 1;
    CrashSuspectNoFile = "ERR_CRASH_SUSPECT_NO_FILE", Plugins, 1;
    McpDenied = "ERR_MCP_DENIED", Plugins, 81;
    McpGrantInvalidIdentifier = "ERR_MCP_GRANT_INVALID_IDENTIFIER", Plugins, 1;
    McpGrantInvalidState = "ERR_MCP_GRANT_INVALID_STATE", Plugins, 1;
    McpStartFailed = "ERR_MCP_START_FAILED", Plugins, 1;
    McpTooManyRequests = "ERR_MCP_TOO_MANY_REQUESTS", Plugins, 80;
    McpUnauthorized = "ERR_MCP_UNAUTHORIZED", Plugins, 82;
    OfficialPluginFileRead = "ERR_OFFICIAL_PLUGIN_FILE_READ", Plugins, 1;
    PluginsUnavailable = "ERR_PLUGINS_UNAVAILABLE", Plugins, 1;
    PluginCapabilityDenied = "ERR_PLUGIN_CAPABILITY_DENIED", Plugins, 1;
    PluginConsentRequired = "ERR_PLUGIN_CONSENT_REQUIRED", Plugins, 1;
    PluginDeactivate = "ERR_PLUGIN_DEACTIVATE", Plugins, 1;
    PluginDisabled = "ERR_PLUGIN_DISABLED", Plugins, 1;
    PluginIdInvalid = "ERR_PLUGIN_ID_INVALID", Plugins, 1;
    PluginIdTaken = "ERR_PLUGIN_ID_TAKEN", Plugins, 1;
    PluginIncompatible = "ERR_PLUGIN_INCOMPATIBLE", Plugins, 1;
    PluginIo = "ERR_PLUGIN_IO", Plugins, 1;
    PluginNeeded = "ERR_PLUGIN_NEEDED", Plugins, 1;
    PluginNotFound = "ERR_PLUGIN_NOT_FOUND", Plugins, 1;
    PluginNoCheckpoint = "ERR_PLUGIN_NO_CHECKPOINT", Plugins, 1;
    PluginNoCode = "ERR_PLUGIN_NO_CODE", Plugins, 1;
    PluginPackageInvalid = "ERR_PLUGIN_PACKAGE_INVALID", Plugins, 1;
    PluginPreviewChanged = "ERR_PLUGIN_PREVIEW_CHANGED", Plugins, 1;
    PluginPublisherChanged = "ERR_PLUGIN_PUBLISHER_CHANGED", Plugins, 1;
    PluginRepairConflict = "ERR_PLUGIN_REPAIR_CONFLICT", Plugins, 1;
    PluginRepairInvalid = "ERR_PLUGIN_REPAIR_INVALID", Plugins, 1;
    PluginRepairStale = "ERR_PLUGIN_REPAIR_STALE", Plugins, 1;
    PluginSettingInvalid = "ERR_PLUGIN_SETTING_INVALID", Plugins, 1;
    PluginSettingUnknown = "ERR_PLUGIN_SETTING_UNKNOWN", Plugins, 1;
    PluginStorageIo = "ERR_PLUGIN_STORAGE_IO", Plugins, 1;
    PluginStorageKey = "ERR_PLUGIN_STORAGE_KEY", Plugins, 1;
    PluginStorageQuota = "ERR_PLUGIN_STORAGE_QUOTA", Plugins, 1;
    PluginSurfaceUnknown = "ERR_PLUGIN_SURFACE_UNKNOWN", Plugins, 1;
    PluginUpdateNone = "ERR_PLUGIN_UPDATE_NONE", Plugins, 1;
    PluginUpdateRefused = "ERR_PLUGIN_UPDATE_REFUSED", Plugins, 1;
    ProvidersUnavailable = "ERR_PROVIDERS_UNAVAILABLE", Plugins, 1;
    ProviderDisabled = "ERR_PROVIDER_DISABLED", Plugins, 1;
    ProviderItem = "ERR_PROVIDER_ITEM", Plugins, 1;
    ProviderNoDetail = "ERR_PROVIDER_NO_DETAIL", Plugins, 1;
    ProviderPackChanged = "ERR_PROVIDER_PACK_CHANGED", Plugins, 1;
    ProviderPackIdentifier = "ERR_PROVIDER_PACK_IDENTIFIER", Plugins, 1;
    ProviderPlanInvalid = "ERR_PROVIDER_PLAN_INVALID", Plugins, 1;
    ProviderResponse = "ERR_PROVIDER_RESPONSE", Plugins, 1;
    ProviderTask = "ERR_PROVIDER_TASK", Plugins, 1;
    ProviderUnavailable = "ERR_PROVIDER_UNAVAILABLE", Plugins, 1;
    ProviderUnknown = "ERR_PROVIDER_UNKNOWN", Plugins, 1;
    // ---- Games ----
    AdoptedGeneratedLibMissing = "ERR_ADOPTED_GENERATED_LIB_MISSING", Games, 1;
    ArgumentQuotes = "ERR_ARGUMENT_QUOTES", Games, 1;
    ArgumentRules = "ERR_ARGUMENT_RULES", Games, 1;
    AssetCorrupt = "ERR_ASSET_CORRUPT", Games, 1;
    AssetIndexMissing = "ERR_ASSET_INDEX_MISSING", Games, 1;
    AssetIndexParse = "ERR_ASSET_INDEX_PARSE", Games, 1;
    AssetIndexRead = "ERR_ASSET_INDEX_READ", Games, 1;
    AssetMaterialize = "ERR_ASSET_MATERIALIZE", Games, 1;
    AssetMissing = "ERR_ASSET_MISSING", Games, 1;
    AssetObjectInvalid = "ERR_ASSET_OBJECT_INVALID", Games, 1;
    AssetTask = "ERR_ASSET_TASK", Games, 1;
    AssetVirtualCopy = "ERR_ASSET_VIRTUAL_COPY", Games, 1;
    AssetVirtualDir = "ERR_ASSET_VIRTUAL_DIR", Games, 1;
    DelegatedLaunchAdapterRequired = "ERR_DELEGATED_LAUNCH_ADAPTER_REQUIRED", Games, 1;
    DelegatedProfileInvalid = "ERR_DELEGATED_PROFILE_INVALID", Games, 1;
    DelegatedProfileMissing = "ERR_DELEGATED_PROFILE_MISSING", Games, 1;
    EnableFailed = "ERR_ENABLE_FAILED", Games, 1;
    GameCrash = "ERR_GAME_CRASH", Games, 7;
    GameDefinition = "ERR_GAME_DEFINITION", Games, 1;
    GameVersionNotFound = "ERR_GAME_VERSION_NOT_FOUND", Games, 61;
    InvalidJvmGc = "ERR_INVALID_JVM_GC", Games, 1;
    InvalidJvmMemory = "ERR_INVALID_JVM_MEMORY", Games, 1;
    JavaBinaryNotFound = "ERR_JAVA_BINARY_NOT_FOUND", Games, 1;
    JavaDiscovery = "ERR_JAVA_DISCOVERY", Games, 1;
    JavaHash = "ERR_JAVA_HASH", Games, 1;
    JavaIncompatible = "ERR_JAVA_INCOMPATIBLE", Games, 62;
    JavaInspect = "ERR_JAVA_INSPECT", Games, 1;
    JavaInspectFailed = "ERR_JAVA_INSPECT_FAILED", Games, 1;
    JavaInvalid = "ERR_JAVA_INVALID", Games, 1;
    JavaProvision = "ERR_JAVA_PROVISION", Games, 1;
    JavaRuntimeCancelled = "ERR_JAVA_RUNTIME_CANCELLED", Games, 62;
    JavaRuntimeCatalogMissing = "ERR_JAVA_RUNTIME_CATALOG_MISSING", Games, 62;
    JavaRuntimeDownloadDisabled = "ERR_JAVA_RUNTIME_DOWNLOAD_DISABLED", Games, 94;
    JavaRuntimeMissing = "ERR_JAVA_RUNTIME_MISSING", Games, 62;
    JavaRuntimeMode = "ERR_JAVA_RUNTIME_MODE", Games, 1;
    JavaVersionMismatch = "ERR_JAVA_VERSION_MISMATCH", Games, 1;
    LaunchArtifactMissing = "ERR_LAUNCH_ARTIFACT_MISSING", Games, 1;
    LaunchCacheCreate = "ERR_LAUNCH_CACHE_CREATE", Games, 1;
    LaunchCachePath = "ERR_LAUNCH_CACHE_PATH", Games, 1;
    LaunchCacheRename = "ERR_LAUNCH_CACHE_RENAME", Games, 1;
    LaunchCacheWrite = "ERR_LAUNCH_CACHE_WRITE", Games, 1;
    LaunchCommand = "ERR_LAUNCH_COMMAND", Games, 1;
    LaunchFailed = "ERR_LAUNCH_FAILED", Games, 61;
    LaunchInstallActive = "ERR_LAUNCH_INSTALL_ACTIVE", Games, 1;
    LaunchMetadataHttp = "ERR_LAUNCH_METADATA_HTTP", Games, 1;
    LaunchMetadataParse = "ERR_LAUNCH_METADATA_PARSE", Games, 1;
    LaunchSpawn = "ERR_LAUNCH_SPAWN", Games, 1;
    LaunchStartSignal = "ERR_LAUNCH_START_SIGNAL", Games, 1;
    LaunchTask = "ERR_LAUNCH_TASK", Games, 1;
    LibraryArtifactMissing = "ERR_LIBRARY_ARTIFACT_MISSING", Games, 1;
    LoaderCatalogConflict = "ERR_LOADER_CATALOG_CONFLICT", Games, 1;
    LoaderCatalogLock = "ERR_LOADER_CATALOG_LOCK", Games, 1;
    LoaderCatalogParse = "ERR_LOADER_CATALOG_PARSE", Games, 1;
    LoaderCatalogQuery = "ERR_LOADER_CATALOG_QUERY", Games, 1;
    LoaderChangeConflict = "ERR_LOADER_CHANGE_CONFLICT", Games, 1;
    LoaderChangeInvalidTarget = "ERR_LOADER_CHANGE_INVALID_TARGET", Games, 1;
    LoaderChangeManifest = "ERR_LOADER_CHANGE_MANIFEST", Games, 1;
    LoaderChangeNoop = "ERR_LOADER_CHANGE_NOOP", Games, 1;
    LoaderChangeNoRequirements = "ERR_LOADER_CHANGE_NO_REQUIREMENTS", Games, 1;
    LoaderChangeRejected = "ERR_LOADER_CHANGE_REJECTED", Games, 1;
    LoaderChangeRollbackFailed = "ERR_LOADER_CHANGE_ROLLBACK_FAILED", Games, 1;
    LoaderEmptyDownload = "ERR_LOADER_EMPTY_DOWNLOAD", Games, 1;
    LoaderLibraryChecksum = "ERR_LOADER_LIBRARY_CHECKSUM", Games, 1;
    LoaderProfileNotFound = "ERR_LOADER_PROFILE_NOT_FOUND", Games, 61;
    LoaderProfileStructure = "ERR_LOADER_PROFILE_STRUCTURE", Games, 1;
    LoaderTuple = "ERR_LOADER_TUPLE", Games, 1;
    LoaderTupleMismatch = "ERR_LOADER_TUPLE_MISMATCH", Games, 1;
    LogEmpty = "ERR_LOG_EMPTY", Games, 1;
    LogNotShareable = "ERR_LOG_NOT_SHAREABLE", Games, 1;
    LogRead = "ERR_LOG_READ", Games, 1;
    LogUpload = "ERR_LOG_UPLOAD", Games, 1;
    MainClassMissing = "ERR_MAIN_CLASS_MISSING", Games, 1;
    MavenCoordinate = "ERR_MAVEN_COORDINATE", Games, 1;
    MavenDescriptor = "ERR_MAVEN_DESCRIPTOR", Games, 30;
    MojangNotFound = "ERR_MOJANG_NOT_FOUND", Games, 60;
    NativeArchive = "ERR_NATIVE_ARCHIVE", Games, 1;
    NativeArchiveEntry = "ERR_NATIVE_ARCHIVE_ENTRY", Games, 1;
    NativeArchiveEntryInvalid = "ERR_NATIVE_ARCHIVE_ENTRY_INVALID", Games, 1;
    NativeClassifierMissing = "ERR_NATIVE_CLASSIFIER_MISSING", Games, 1;
    NativeDuplicatePath = "ERR_NATIVE_DUPLICATE_PATH", Games, 1;
    NativeExtraction = "ERR_NATIVE_EXTRACTION", Games, 1;
    NativeExtractionFailed = "ERR_NATIVE_EXTRACTION_FAILED", Games, 1;
    NativeRules = "ERR_NATIVE_RULES", Games, 1;
    ProfileCorrupt = "ERR_PROFILE_CORRUPT", Games, 11;
    ProfileMissing = "ERR_PROFILE_MISSING", Games, 11;
    ProfilePromotion = "ERR_PROFILE_PROMOTION", Games, 1;
    ProfileUnsupportedMetadata = "ERR_PROFILE_UNSUPPORTED_METADATA", Games, 11;
    ProfileWriteFailed = "ERR_PROFILE_WRITE_FAILED", Games, 60;
    QuickPlay = "ERR_QUICK_PLAY", Games, 1;
    ReservedJvmArgument = "ERR_RESERVED_JVM_ARGUMENT", Games, 1;
    Resolve = "ERR_RESOLVE", Games, 1;
    RuntimeBackup = "ERR_RUNTIME_BACKUP", Games, 1;
    RuntimeCorrupt = "ERR_RUNTIME_CORRUPT", Games, 1;
    RuntimeJavaPath = "ERR_RUNTIME_JAVA_PATH", Games, 1;
    RuntimeLayout = "ERR_RUNTIME_LAYOUT", Games, 1;
    RuntimeList = "ERR_RUNTIME_LIST", Games, 1;
    RuntimePromote = "ERR_RUNTIME_PROMOTE", Games, 1;
    RuntimeRemove = "ERR_RUNTIME_REMOVE", Games, 1;
    RuntimeRemoveInvalid = "ERR_RUNTIME_REMOVE_INVALID", Games, 1;
    RuntimeRemoveNoReceipt = "ERR_RUNTIME_REMOVE_NO_RECEIPT", Games, 1;
    RuntimeRemoveOutside = "ERR_RUNTIME_REMOVE_OUTSIDE", Games, 1;
    RuntimeRemoveSymlink = "ERR_RUNTIME_REMOVE_SYMLINK", Games, 1;
    TechnicBuild = "ERR_TECHNIC_BUILD", Games, 1;
    TechnicDecode = "ERR_TECHNIC_DECODE", Games, 1;
    TechnicDisabled = "ERR_TECHNIC_DISABLED", Games, 1;
    TechnicFilename = "ERR_TECHNIC_FILENAME", Games, 1;
    TechnicNotFound = "ERR_TECHNIC_NOT_FOUND", Games, 1;
    TechnicSlug = "ERR_TECHNIC_SLUG", Games, 1;
    TechnicSolder = "ERR_TECHNIC_SOLDER", Games, 1;
    TechnicZipPack = "ERR_TECHNIC_ZIP_PACK", Games, 1;
    UnresolvedPlaceholder = "ERR_UNRESOLVED_PLACEHOLDER", Games, 70;
    UnsupportedLoader = "ERR_UNSUPPORTED_LOADER", Games, 61;
    UnsupportedStrategy = "ERR_UNSUPPORTED_STRATEGY", Games, 1;
    VersionNotFound = "ERR_VERSION_NOT_FOUND", Games, 61;
    // ---- Recovery ----
    ImportJournal = "ERR_IMPORT_JOURNAL", Recovery, 1;
    MigrationConflict = "ERR_MIGRATION_CONFLICT", Recovery, 71;
    MigrationFailed = "ERR_MIGRATION_FAILED", Recovery, 72;
    MigrationRejected = "ERR_MIGRATION_REJECTED", Recovery, 1;
    // ---- Other ----
    AppDataDir = "ERR_APP_DATA_DIR", Other, 1;
    AppPaths = "ERR_APP_PATHS", Other, 1;
    CacheDir = "ERR_CACHE_DIR", Other, 1;
    CachePath = "ERR_CACHE_PATH", Other, 1;
    CacheRead = "ERR_CACHE_READ", Other, 1;
    CacheRename = "ERR_CACHE_RENAME", Other, 1;
    CacheWrite = "ERR_CACHE_WRITE", Other, 1;
    Db = "ERR_DB", Other, 1;
    DevRegistryOpen = "ERR_DEV_REGISTRY_OPEN", Other, 1;
    DevRegistryPath = "ERR_DEV_REGISTRY_PATH", Other, 1;
    DevRegistrySchema = "ERR_DEV_REGISTRY_SCHEMA", Other, 1;
    DevRegistrySchemaFuture = "ERR_DEV_REGISTRY_SCHEMA_FUTURE", Other, 1;
    Diskfull = "ERR_DISKFULL", Other, 34;
    EnvVarName = "ERR_ENV_VAR_NAME", Other, 1;
    InvalidFormat = "ERR_INVALID_FORMAT", Other, 1;
    InvalidSource = "ERR_INVALID_SOURCE", Other, 1;
    JsonDecode = "ERR_JSON_DECODE", Other, 1;
    JsonEncode = "ERR_JSON_ENCODE", Other, 1;
    LocalStateFailed = "ERR_LOCAL_STATE_FAILED", Other, 10;
    LocalStatePath = "ERR_LOCAL_STATE_PATH", Other, 1;
    ManualFile = "ERR_MANUAL_FILE", Other, 1;
    ManualPath = "ERR_MANUAL_PATH", Other, 1;
    ManualRead = "ERR_MANUAL_READ", Other, 1;
    NotAPack = "ERR_NOT_A_PACK", Other, 1;
    NoCompatibleVersion = "ERR_NO_COMPATIBLE_VERSION", Other, 1;
    NoFile = "ERR_NO_FILE", Other, 1;
    NoGamePackage = "ERR_NO_GAME_PACKAGE", Other, 1;
    PlanNotFound = "ERR_PLAN_NOT_FOUND", Other, 1;
    PreLaunchCommand = "ERR_PRE_LAUNCH_COMMAND", Other, 1;
    ReducedSecurityRequired = "ERR_REDUCED_SECURITY_REQUIRED", Other, 1;
    SandboxUnavailable = "ERR_SANDBOX_UNAVAILABLE", Other, 50;
    Shortcut = "ERR_SHORTCUT", Other, 1;
    StderrPipe = "ERR_STDERR_PIPE", Other, 1;
    StdoutPipe = "ERR_STDOUT_PIPE", Other, 1;
    TaskFailed = "ERR_TASK_FAILED", Other, 1;
    TestBatch = "ERR_TEST_BATCH", Other, 1;
    TestHttp = "ERR_TEST_HTTP", Other, 1;
    TestNetworkOff = "ERR_TEST_NETWORK_OFF", Other, 1;
    UserDecisionRequired = "ERR_USER_DECISION_REQUIRED", Other, 71;
    WindowSize = "ERR_WINDOW_SIZE", Other, 1;
}
