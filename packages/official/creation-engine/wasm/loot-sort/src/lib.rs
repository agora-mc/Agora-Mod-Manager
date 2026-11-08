//! LOOT's plugin sort as a WASI command (MASTER_SPEC §26.6).
//!
//! The command reads one JSON request from stdin, sorts the plugins with the
//! vendored libloot, and writes one JSON reply to stdout. It is deliberately
//! a thin adapter: the ordering, metadata and message wording are libloot's,
//! exactly as `agora-core`'s `loot_masterlist.rs` uses them today.
//!
//! The request supplies the current order and the active set, so the module
//! never reads a `Plugins.txt`. The masterlist and prelude are read from the
//! paths in the request (or from inline `*_yaml` fields), and are handed to
//! libloot as strings, so libloot itself opens no YAML file.

pub mod json;

use std::path::{Path, PathBuf};

use libloot::metadata::MessageType;
use libloot::{EvalMode, Game, GameType, MergeMode};

use json::Value;

pub struct Request {
    pub game_type: String,
    pub data: String,
    pub load_order: Vec<String>,
    pub active: Vec<String>,
    pub masterlist: String,
    pub prelude: Option<String>,
    pub masterlist_yaml: Option<String>,
    pub prelude_yaml: Option<String>,
}

impl Request {
    fn from_value(value: &Value) -> Result<Self, String> {
        let Value::Object(_) = value else {
            return Err("the request must be a JSON object".to_string());
        };
        Ok(Self {
            game_type: required_string(value, "game_type")?,
            data: required_string(value, "data")?,
            load_order: string_array(value, "load_order")?,
            active: string_array(value, "active")?,
            masterlist: required_string(value, "masterlist")?,
            prelude: optional_string(value, "prelude")?,
            masterlist_yaml: optional_string(value, "masterlist_yaml")?,
            prelude_yaml: optional_string(value, "prelude_yaml")?,
        })
    }
}

fn required_string(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("the request needs a string '{key}'"))
}

fn optional_string(value: &Value, key: &str) -> Result<Option<String>, String> {
    match value.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("'{key}' must be a string or null")),
    }
}

fn string_array(value: &Value, key: &str) -> Result<Vec<String>, String> {
    let array = value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("the request needs an array '{key}'"))?;
    array
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("every entry of '{key}' must be a string"))
        })
        .collect()
}

fn game_type(name: &str) -> Result<GameType, String> {
    match name {
        "Oblivion" => Ok(GameType::Oblivion),
        "Skyrim" => Ok(GameType::Skyrim),
        "Fallout3" => Ok(GameType::Fallout3),
        "FalloutNV" => Ok(GameType::FalloutNV),
        "Fallout4" => Ok(GameType::Fallout4),
        "SkyrimSE" => Ok(GameType::SkyrimSE),
        "Fallout4VR" => Ok(GameType::Fallout4VR),
        "SkyrimVR" => Ok(GameType::SkyrimVR),
        "Morrowind" => Ok(GameType::Morrowind),
        "Starfield" => Ok(GameType::Starfield),
        "OpenMW" => Ok(GameType::OpenMW),
        "OblivionRemastered" => Ok(GameType::OblivionRemastered),
        other => Err(format!("Agora does not know the game type '{other}'")),
    }
}

fn level(message_type: MessageType) -> &'static str {
    match message_type {
        MessageType::Error => "error",
        MessageType::Warn => "warn",
        MessageType::Say => "info",
    }
}

fn english_text(content: &[libloot::metadata::MessageContent]) -> Option<String> {
    libloot::metadata::select_message_content(content, "en")
        .map(|content| content.text().to_string())
}

fn read_metadata(request: &Request) -> Result<(String, Option<String>), String> {
    let masterlist = match &request.masterlist_yaml {
        Some(text) => text.clone(),
        None => std::fs::read_to_string(&request.masterlist).map_err(|error| {
            format!(
                "the masterlist at '{}' cannot be read: {error}",
                request.masterlist
            )
        })?,
    };
    let prelude = match (&request.prelude_yaml, &request.prelude) {
        (Some(text), _) => Some(text.clone()),
        (None, Some(path)) => Some(
            std::fs::read_to_string(path)
                .map_err(|error| format!("the prelude at '{path}' cannot be read: {error}"))?,
        ),
        (None, None) => None,
    };
    Ok((masterlist, prelude))
}

/// Run one request and return the reply JSON.
pub fn run_request(request_json: &str) -> Result<String, String> {
    let value = json::parse(request_json)
        .map_err(|error| format!("the request is not valid JSON: {error}"))?;
    let request = Request::from_value(&value)?;
    sort(&request)
}

fn sort(request: &Request) -> Result<String, String> {
    let game_type = game_type(&request.game_type)?;
    let data = PathBuf::from(&request.data);
    if !data.is_dir() {
        return Err(format!("the data folder '{}' cannot be read", request.data));
    }

    // libloot derives `<game_path>/Data` from the game path. Every plugin is
    // passed by its absolute path and `data` is also an additional data path,
    // so the derived folder is never used to find a plugin or archive. Using
    // the data folder as both paths means the module needs only `/data`
    // preopened, with no writable or extra mount.
    let mut game = Game::with_local_path(game_type, &data, &data)
        .map_err(|error| format!("libloot cannot open '{}': {error}", request.data))?;
    game.set_additional_data_paths(vec![data.clone()])
        .map_err(|error| format!("libloot cannot use '{}': {error}", request.data))?;

    let active: Vec<&str> = request.active.iter().map(String::as_str).collect();
    game.set_active_plugins(&active)
        .map_err(|error| format!("the active plugin list cannot be set: {error}"))?;

    let mut paths = Vec::with_capacity(request.load_order.len());
    for name in &request.load_order {
        let path = data.join(name);
        if !path.is_file() {
            return Err(format!("the plugin '{name}' is not in the data folder"));
        }
        paths.push(path);
    }
    let path_refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    game.load_plugins(&path_refs)
        .map_err(|error| format!("libloot cannot read the plugins: {error}"))?;

    let (masterlist, prelude) = read_metadata(request)?;
    {
        let database = game.database();
        let mut database = database
            .write()
            .map_err(|error| format!("the metadata database is unavailable: {error}"))?;
        database
            .load_masterlist_from_str(&masterlist, prelude.as_deref())
            .map_err(|error| format!("the masterlist cannot be read: {error}"))?;
    }

    let names: Vec<&str> = request.load_order.iter().map(String::as_str).collect();
    let order = game
        .sort_plugins(&names)
        .map_err(|error| format!("the plugins cannot be sorted: {error}"))?;

    let database = game.database();
    let database = database
        .read()
        .map_err(|error| format!("the metadata database is unavailable: {error}"))?;

    let mut out = String::new();
    out.push_str("{\"order\":[");
    for (index, name) in order.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        json::write_string(name, &mut out);
    }
    out.push_str("],\"plugins\":[");

    let mut first_plugin = true;
    for name in &order {
        let metadata = database
            .plugin_metadata(name, MergeMode::WithoutUserMetadata, EvalMode::Evaluate)
            .map_err(|error| format!("LOOT cannot read its notes for '{name}': {error}"))?;
        let Some(metadata) = metadata else {
            continue;
        };

        let mut messages: Vec<(&'static str, String)> = Vec::new();
        for message in metadata.messages() {
            if let Some(text) = english_text(message.content()) {
                messages.push((level(message.message_type()), text));
            }
        }
        for cleaning in metadata.dirty_info() {
            if let Some(text) = english_text(cleaning.detail()) {
                messages.push(("warn", text));
            }
        }

        let group = metadata.group();
        let cleaning = metadata.dirty_info();
        if messages.is_empty() && group.is_none() && cleaning.is_empty() {
            continue;
        }

        if !first_plugin {
            out.push(',');
        }
        first_plugin = false;

        out.push_str("{\"name\":");
        json::write_string(name, &mut out);
        out.push_str(",\"messages\":[");
        for (index, (message_level, text)) in messages.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("{\"level\":");
            json::write_string(message_level, &mut out);
            out.push_str(",\"text\":");
            json::write_string(text, &mut out);
            out.push('}');
        }
        out.push(']');
        if let Some(group) = group {
            out.push_str(",\"group\":");
            json::write_string(group, &mut out);
        }
        if !cleaning.is_empty() {
            out.push_str(",\"cleaning\":[");
            for (index, cleaning) in cleaning.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str("{\"crc\":");
                out.push_str(&cleaning.crc().to_string());
                out.push_str(",\"utility\":");
                json::write_string(cleaning.cleaning_utility(), &mut out);
                out.push_str(",\"itm\":");
                out.push_str(&cleaning.itm_count().to_string());
                out.push_str(",\"deleted_references\":");
                out.push_str(&cleaning.deleted_reference_count().to_string());
                out.push_str(",\"deleted_navmeshes\":");
                out.push_str(&cleaning.deleted_navmesh_count().to_string());
                if let Some(text) = english_text(cleaning.detail()) {
                    out.push_str(",\"detail\":");
                    json::write_string(&text, &mut out);
                }
                out.push('}');
            }
            out.push(']');
        }
        out.push('}');
    }
    out.push_str("],\"general\":[");

    let general = database
        .general_messages(MergeMode::WithoutUserMetadata, EvalMode::Evaluate)
        .map_err(|error| format!("LOOT cannot read its general notes: {error}"))?;
    let mut first_general = true;
    for message in &general {
        let Some(text) = english_text(message.content()) else {
            continue;
        };
        if !first_general {
            out.push(',');
        }
        first_general = false;
        out.push_str("{\"level\":");
        json::write_string(level(message.message_type()), &mut out);
        out.push_str(",\"text\":");
        json::write_string(&text, &mut out);
        out.push('}');
    }
    out.push_str("]}\n");

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_that_is_not_json_is_an_error() {
        let error = run_request("not json").unwrap_err();
        assert!(error.contains("not valid JSON"), "{error}");
    }

    #[test]
    fn a_request_without_a_data_folder_is_an_error() {
        let request = serde_like_request();
        let error = run_request(&request).unwrap_err();
        assert!(error.contains("data folder"), "{error}");
    }

    fn serde_like_request() -> String {
        "{\"game_type\":\"SkyrimSE\",\"data\":\"this folder does not exist\",\"load_order\":[],\"active\":[],\"masterlist\":\"missing.yaml\",\"prelude\":null}".to_string()
    }
}
