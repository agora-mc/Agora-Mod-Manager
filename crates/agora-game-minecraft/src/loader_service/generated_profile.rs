use crate::installed_profile::{derive_profile_id, LoaderTuple};
use crate::launch::{parse_maven_descriptor, Library, LibraryArtifact};
use crate::{launch_planner, loader_manifests};
use agora_core::error::{LauncherError, LauncherResult};
use serde_json::{json, Value};
use std::collections::BTreeSet;

// Client entries from the upstream installer metadata. The legacy entries
// also appear in the Fabric Meta and archived Quilt Meta API documentation.
// https://github.com/FabricMC/fabric-loader/blob/master/src/main/resources/fabric-installer.json
// https://github.com/QuiltMC/quilt-loader/blob/develop/src/main/resources/quilt_installer.json
// https://github.com/FabricMC/fabric-meta#v2
// https://github.com/QuiltMC/quilt-meta#v3
const FABRIC_MAIN_CLASSES: &[&str] = &[
    "net.fabricmc.loader.impl.launch.knot.KnotClient",
    "net.fabricmc.loader.launch.knot.KnotClient",
];
const QUILT_MAIN_CLASSES: &[&str] = &[
    "org.quiltmc.loader.impl.launch.knot.KnotClient",
    "org.quiltmc.loader.launch.knot.KnotClient",
];
// https://github.com/FabricMC/fabric-meta/blob/master/src/main/java/net/fabricmc/meta/web/ProfileHandler.java
// The captured 0.20.1 / 1.21.1 profile also disables the HotSpot Base64 intrinsic.
const FABRIC_JVM_ARGUMENTS: &[&str] = &[
    "-XX:+UnlockDiagnosticVMOptions",
    "-XX:-UseBASE64Intrinsics",
    "-DFabricMcEmu= net.minecraft.client.main.Main ",
];
// Quilt's 0.30.1 / 1.21.1 profile carried no JVM arguments; its 0.31.0-beta.6 /
// 26.3 profile carries the same two HotSpot flags Fabric's does, and no
// emulation argument. Anything else is still refused.
const QUILT_JVM_ARGUMENTS: &[&str] =
    &["-XX:+UnlockDiagnosticVMOptions", "-XX:-UseBASE64Intrinsics"];
const GAME_ARGUMENTS: &[&str] = &[];

fn invalid(message: impl Into<String>) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_LOADER_PROFILE_STRUCTURE".into(),
        message: message.into(),
    }
}

fn maven_url(raw: &str, name: &str) -> LauncherResult<reqwest::Url> {
    loader_manifests::ensure_allowed_domain(raw).map_err(|_| {
        invalid(format!(
            "Library {name} has a URL outside the loader host allowlist"
        ))
    })?;
    let url = reqwest::Url::parse(raw).map_err(|e| invalid(format!("Library {name} URL: {e}")))?;
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(format!(
            "Library {name} requires a plain HTTPS Maven URL"
        )));
    }
    Ok(url)
}

/// Check all launch-affecting fields before requesting any library. Publish a
/// normalized arguments object and use explicit artifacts after verification.
pub(super) fn validate(
    profile: &mut Value,
    tuple: &LoaderTuple,
) -> LauncherResult<Vec<(String, LibraryArtifact)>> {
    let (main_classes, jvm_args, loader, mappings) = match tuple.loader.as_str() {
        "fabric" => (
            FABRIC_MAIN_CLASSES,
            FABRIC_JVM_ARGUMENTS,
            ("net.fabricmc", "fabric-loader"),
            ("net.fabricmc", "intermediary"),
        ),
        "quilt" => (
            QUILT_MAIN_CLASSES,
            QUILT_JVM_ARGUMENTS,
            ("org.quiltmc", "quilt-loader"),
            ("org.quiltmc", "hashed"),
        ),
        _ => return Err(LauncherError::UnsupportedLoader),
    };
    let object = profile
        .as_object_mut()
        .ok_or_else(|| invalid("Expected a profile JSON object"))?;
    let profile_id = derive_profile_id(tuple);
    if object.get("id").and_then(Value::as_str) != Some(profile_id.as_str()) {
        return Err(invalid(format!("id must be {profile_id}")));
    }
    if object.get("inheritsFrom").and_then(Value::as_str) != Some(tuple.minecraft_version.as_str())
    {
        return Err(invalid(format!(
            "inheritsFrom must be {}",
            tuple.minecraft_version
        )));
    }
    if !object
        .get("mainClass")
        .and_then(Value::as_str)
        .is_some_and(|c| main_classes.contains(&c))
    {
        return Err(invalid(format!(
            "mainClass is outside the {} client allowlist",
            tuple.loader
        )));
    }
    if let Some(field) = object.keys().find(|field| {
        !matches!(
            field.as_str(),
            "id" | "inheritsFrom"
                | "mainClass"
                | "arguments"
                | "libraries"
                | "time"
                | "releaseTime"
                | "type"
                | "minimumLauncherVersion"
        )
    }) {
        return Err(invalid(format!("Unexpected loader profile field {field}")));
    }

    let arguments = object
        .entry("arguments")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid("arguments must be an object"))?;
    if arguments
        .keys()
        .any(|key| !matches!(key.as_str(), "jvm" | "game"))
    {
        return Err(invalid("arguments may contain only jvm and game"));
    }
    for (key, allowed) in [("jvm", jvm_args), ("game", GAME_ARGUMENTS)] {
        let entries = arguments
            .entry(key)
            .or_insert_with(|| json!([]))
            .as_array()
            .ok_or_else(|| invalid(format!("arguments.{key} must be an array")))?;
        if entries
            .iter()
            .any(|entry| !entry.as_str().is_some_and(|s| allowed.contains(&s)))
        {
            return Err(invalid(format!(
                "arguments.{key} contains an entry outside the allowlist"
            )));
        }
    }

    let libraries = object
        .get("libraries")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("libraries must be an array"))?;
    let mut jars = Vec::new();
    let mut identities = BTreeSet::new();
    let mut loader_found = false;
    let mut mappings_found = false;
    for value in libraries {
        let library: Library = serde_json::from_value(value.clone())
            .map_err(|e| invalid(format!("Invalid library entry: {e}")))?;
        let name = &library.name;
        if library.rules.is_some()
            || library.natives.is_some()
            || library.extract.is_some()
            || library
                .downloads
                .as_ref()
                .is_some_and(|downloads| downloads.classifiers.is_some())
        {
            return Err(invalid(format!(
                "Library {name} must be an unconditional jar"
            )));
        }
        let coordinate = parse_maven_descriptor(name)
            .map_err(|_| invalid(format!("Invalid Maven coordinate {name}")))?;
        if coordinate.extension != "jar"
            || coordinate.classifier.is_some()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".:_+-".contains(&b))
            || [&coordinate.group, &coordinate.name, &coordinate.version]
                .iter()
                .any(|part| part.starts_with('.') || part.ends_with('.'))
        {
            return Err(invalid(format!(
                "Library {name} must name a plain Maven jar"
            )));
        }
        let identity = (coordinate.group.as_str(), coordinate.name.as_str());
        if !identities.insert((
            identity.0.to_ascii_lowercase(),
            identity.1.to_ascii_lowercase(),
        )) {
            return Err(invalid(format!("Duplicate library identity for {name}")));
        }
        if identity == loader {
            if coordinate.version != tuple.loader_version {
                return Err(invalid(format!(
                    "The loader artifact must have version {}",
                    tuple.loader_version
                )));
            }
            loader_found = true;
        }
        // Quilt can also include Fabric's intermediary beside its hashed mappings.
        if identity == mappings || identity == ("net.fabricmc", "intermediary") {
            if coordinate.version != tuple.minecraft_version {
                return Err(invalid(format!(
                    "The mappings artifact must have version {}",
                    tuple.minecraft_version
                )));
            }
            mappings_found |= identity == mappings;
        }
        if let Some(repository) = &library.url {
            maven_url(repository, name)?;
        }
        let artifact = launch_planner::resolve_library_artifact(&library)?
            .ok_or_else(|| invalid(format!("Library {name} has no download URL")))?;
        let url = maven_url(&artifact.url, name)?;
        let path = coordinate.to_relative_path();
        if artifact.path != path || !url.path().ends_with(&format!("/{path}")) {
            return Err(invalid(format!(
                "Library {name} download must match its Maven coordinate"
            )));
        }
        jars.push((library.name, artifact));
    }
    if !loader_found {
        return Err(invalid(format!(
            "Missing loader artifact {}:{}:{}",
            loader.0, loader.1, tuple.loader_version
        )));
    }
    // Mojang removed obfuscation from the 26.1 release onward. These releases
    // have no mappings artifact in the profile.
    let unobfuscated = tuple
        .minecraft_version
        .split_once('.')
        .and_then(|(year, _)| year.parse::<u32>().ok())
        .is_some_and(|year| (26..100).contains(&year));
    if !mappings_found && !unobfuscated {
        return Err(invalid(format!(
            "Missing mappings artifact {}:{}:{}",
            mappings.0, mappings.1, tuple.minecraft_version
        )));
    }
    Ok(jars)
}
