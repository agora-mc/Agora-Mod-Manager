//! The `---- Minecraft Crash Report ----` file, whichever loader wrote it.

use super::shared::{
    absent, checked, collect_lines, crash_mixin_line, fact, file_problem, frame_symbol_and_class,
    is_mod_id, jar_from_frame, java_exception_category, jvm_problem, looked_for,
    mod_crash_from_message, mod_crash_line, not_applicable, notes_for, number, reference,
    source_location, text,
};
use agora_game_api::crash::{
    BucketPart, BuiltInParser, BuiltInParserError, Fact, ParserOutput, ParserReads, ParserRequest,
    Producer, Strength, CRASH_CONTRACT_VERSION,
};

pub struct CrashReportParser;

const FORMAT: &str = "minecraft_crash_report";

impl BuiltInParser for CrashReportParser {
    fn parse(
        &self,
        request: &ParserRequest,
        reads: &dyn ParserReads,
    ) -> Result<ParserOutput, BuiltInParserError> {
        let collected = collect_lines(reads, &request.artifact.id, request.limits)?;
        if collected.lines.is_empty() {
            return Err(BuiltInParserError::new(
                "the crash report is empty; there is nothing to read",
            ));
        }
        // The header is sometimes the payload of a log line, as in a crash report the game wrote
        // into its own log. What follows it is the raw report.
        let Some(start) = collected
            .lines
            .iter()
            .position(|line| line.contains("---- Minecraft Crash Report ----"))
        else {
            return Ok(wrong_format());
        };

        let mut facts: Vec<Fact> = Vec::new();
        let mut frames: Vec<(usize, usize)> = Vec::new();
        let mut bucket = Vec::new();

        let mut section: Option<String> = None;
        let mut walkthrough = false;
        let mut current_depth = 0usize;
        let mut have_exception = false;
        let mut trace_index: u32 = 0;

        let mut description: Option<String> = None;
        let mut time: Option<String> = None;
        let mut game_version: Option<String> = None;
        let mut os: Option<String> = None;
        let mut java_version: Option<String> = None;
        let mut java_vendor: Option<String> = None;
        let mut java_bits: Option<u32> = None;
        let mut memory_max_mb: Option<f64> = None;
        let mut launcher_brand: Option<String> = None;
        let mut loader: Option<(String, String)> = None;
        let mut launched_version: Option<String> = None;
        let mut fml_version: Option<String> = None;
        let mut saw_system_details = false;
        let mut saw_mod_list = false;
        let mut mod_list = ModList::None;
        let mut ticking: Option<Ticking> = None;
        let mut mod_crash_order = 0u32;
        // A mixin that lost its target to another transformer is a conflict, not that mixin's fault.
        let mixin_conflict = collected.lines.iter().any(|line| {
            line.contains("was removed by another injector")
                || line.contains("could not find any targets matching")
        });

        for (offset, line) in collected.lines.iter().enumerate().skip(start + 1) {
            let line_no = offset + 1;

            if let Some(name) = section_header(line) {
                if let Some(object) = ticking.take() {
                    if let Some(object_fact) = object.into_fact() {
                        facts.push(object_fact);
                    }
                }
                if name == "Entity being ticked" {
                    ticking = Some(Ticking::new("entity", line_no));
                } else if name == "Block entity being ticked" || name == "BlockEntity being ticked"
                {
                    ticking = Some(Ticking::new("block_entity", line_no));
                }
                section = Some(name.clone());
                walkthrough = true;
                if name == "System Details" {
                    saw_system_details = true;
                }
                mod_list = ModList::None;
                facts.push(fact(
                    "minecraft.section",
                    vec![("name", text(name))],
                    Vec::new(),
                    line_no,
                    "sections",
                ));
                continue;
            }

            if mod_list != ModList::None {
                let indented = mod_list_indented(line);
                let in_table = match mod_list {
                    ModList::Fabric => indented,
                    ModList::Forge => indented && line.contains('|'),
                    ModList::ForgeLegacy => {
                        indented && line.trim().ends_with(')') && line.contains(" (")
                    }
                    ModList::Quilt => indented && line.trim_start().starts_with('|'),
                    ModList::None => false,
                };
                if in_table {
                    if let Some(mod_fact) = parse_mod_line(mod_list, line, line_no) {
                        saw_mod_list = true;
                        facts.push(mod_fact);
                    }
                    continue;
                }
                mod_list = ModList::None;
            }

            if let Some(object) = ticking.as_mut() {
                object.absorb(line);
            }

            if !walkthrough {
                if line.starts_with("A detailed walkthrough") {
                    walkthrough = true;
                    continue;
                }
                if let Some((caused, class, message)) = exception_line(line) {
                    if caused {
                        current_depth += 1;
                    } else if !have_exception {
                        current_depth = 0;
                    }
                    have_exception = true;
                    trace_index = 0;
                    let category = java_exception_category(&class);
                    let text_message = message.clone().unwrap_or_default();
                    facts.push(fact(
                        "core.exception",
                        vec![
                            ("class", text(class.clone())),
                            ("category", text(category)),
                            ("message", text(text_message.clone())),
                            ("depth", number(current_depth as f64)),
                        ],
                        vec![reference("class", class.clone(), Strength::Mention)],
                        line_no,
                        "exception",
                    ));
                    if let Some(problem) = jvm_problem(&class, &text_message, line_no, "exception")
                    {
                        facts.push(problem);
                    }
                    if let Some(problem) = file_problem(&class, &text_message, line_no, "exception")
                    {
                        facts.push(problem);
                    }
                    if let Some(crash) =
                        mod_crash_line(line).or_else(|| mod_crash_from_message(&text_message))
                    {
                        facts.push(crash.into_fact(mod_crash_order, line_no));
                        mod_crash_order += 1;
                    }
                    if !mixin_conflict {
                        if let Some(failure) = crash_mixin_line(line, line_no) {
                            facts.push(failure);
                        }
                    }
                    if current_depth == 0 {
                        bucket = vec![BucketPart {
                            scheme: "crash_report".to_string(),
                            value: class,
                        }];
                    }
                    continue;
                }
                if let Some(frame) = parse_frame(line, trace_index, line_no) {
                    frames.push((facts.len(), current_depth));
                    facts.push(frame);
                    trace_index += 1;
                    continue;
                }
                if let Some(rest) = line.trim_start().strip_prefix("Description: ") {
                    if description.is_none() {
                        description = Some(rest.trim().to_string());
                    }
                    continue;
                }
                if let Some(rest) = line.strip_prefix("Time: ") {
                    if time.is_none() {
                        time = Some(rest.trim().to_string());
                    }
                    continue;
                }
                if let Some(crash) = mod_crash_line(line) {
                    facts.push(crash.into_fact(mod_crash_order, line_no));
                    mod_crash_order += 1;
                }
                continue;
            }

            if let Some(crash) = mod_crash_line(line) {
                facts.push(crash.into_fact(mod_crash_order, line_no));
                mod_crash_order += 1;
                continue;
            }

            if section.as_deref() != Some("System Details") {
                continue;
            }

            let trimmed = line.trim();
            if trimmed == "Fabric Mods:" {
                mod_list = ModList::Fabric;
                saw_mod_list = true;
                continue;
            }
            if trimmed == "Mod List:" {
                mod_list = ModList::Forge;
                saw_mod_list = true;
                continue;
            }
            if trimmed == "Quilt Mods:" {
                mod_list = ModList::Quilt;
                saw_mod_list = true;
                continue;
            }
            if trimmed == "Name Version (Mod Id)" {
                mod_list = ModList::ForgeLegacy;
                saw_mod_list = true;
                continue;
            }
            let Some((key, value)) = trimmed.split_once(": ") else {
                continue;
            };
            let value = value.trim();
            match key {
                "Minecraft Version" => game_version = Some(value.to_string()),
                "Operating System" => os = Some(value.to_string()),
                "Java Version" => {
                    let (version, vendor) = split_vendor(value);
                    java_version = Some(version);
                    if !vendor.is_empty() {
                        java_vendor = Some(vendor);
                    }
                }
                "Java VM Version" => {
                    if value.contains("64-Bit") {
                        java_bits = Some(64);
                    } else if value.contains("32-Bit") || value.contains("Client VM") {
                        java_bits = Some(32);
                    }
                }
                "Memory" => memory_max_mb = memory_max(value),
                "Launched Version" => launched_version = Some(value.to_string()),
                "Launcher name" => launcher_brand = Some(value.to_string()),
                "Fabric Loader" => loader = Some(("fabric".to_string(), value.to_string())),
                "Quilt Loader" => loader = Some(("quilt".to_string(), value.to_string())),
                "Forge" => {
                    let version = value.rsplit(':').next().unwrap_or(value).to_string();
                    loader = Some(("forge".to_string(), version));
                }
                "NeoForge" => {
                    let version = value.rsplit(':').next().unwrap_or(value).to_string();
                    loader = Some(("neoforge".to_string(), version));
                }
                "FML" => fml_version = Some(value.to_string()),
                _ => {}
            }
        }

        if let Some(object) = ticking.take() {
            if let Some(object_fact) = object.into_fact() {
                facts.push(object_fact);
            }
        }

        if loader.is_none() {
            loader = loader_from_launched(launched_version.as_deref());
        }
        if let Some((_, version)) = &mut loader {
            if version.is_empty() {
                if let Some(fallback) = &fml_version {
                    *version = fallback.clone();
                }
            }
        }
        if let Some((id, version)) = &loader {
            facts.push(fact(
                "minecraft.loader",
                vec![("id", text(id.clone())), ("version", text(version.clone()))],
                Vec::new(),
                0,
                "system",
            ));
        }
        if let Some(version) = &game_version {
            facts.push(fact(
                "core.process",
                vec![("image_version", text(version.clone()))],
                Vec::new(),
                0,
                "system",
            ));
        }
        if let Some(os) = &os {
            facts.push(fact(
                "core.system",
                vec![("os", text(os.clone()))],
                Vec::new(),
                0,
                "system",
            ));
        }
        let mut system_attrs = Vec::new();
        if let Some(version) = &java_version {
            system_attrs.push(("java_version", text(version.clone())));
        }
        if let Some(vendor) = &java_vendor {
            system_attrs.push(("java_vendor", text(vendor.clone())));
        }
        if let Some(bits) = java_bits {
            system_attrs.push(("java_bits", number(bits as f64)));
        }
        if let Some(memory) = memory_max_mb {
            system_attrs.push(("memory_max_mb", number(memory)));
        }
        if let Some(launcher) = &launcher_brand {
            system_attrs.push(("launcher_brand", text(launcher.clone())));
        }
        if !system_attrs.is_empty() {
            facts.push(fact(
                "minecraft.system",
                system_attrs,
                Vec::new(),
                0,
                "system",
            ));
        }
        if description.is_some() || time.is_some() {
            let mut attrs = Vec::new();
            if let Some(description) = &description {
                attrs.push(("description", text(description.clone())));
            }
            if let Some(time) = &time {
                attrs.push(("time", text(time.clone())));
            }
            facts.push(fact("minecraft.crash", attrs, Vec::new(), 0, "header"));
        }

        // The deepest cause's first frame is the place to look first.
        if let Some(deepest) = frames.iter().map(|(_, depth)| *depth).max() {
            if let Some((index, _)) = frames.iter().find(|(_, depth)| *depth == deepest) {
                for reference in &mut facts[*index].refs {
                    if reference.kind == "class" {
                        reference.strength = Strength::ExecTop;
                    }
                }
            }
        }

        let loader_name = loader
            .as_ref()
            .map(|(id, _)| id.as_str())
            .or(description.as_deref())
            .unwrap_or("unknown");
        if bucket.is_empty() {
            bucket = vec![BucketPart {
                scheme: "crash_report".to_string(),
                value: loader_name.to_string(),
            }];
        } else if let Some(part) = bucket.first_mut() {
            part.value = format!("{}|{}", part.value, loader_name);
        }

        let found: std::collections::BTreeSet<String> =
            facts.iter().map(|fact| fact.kind.clone()).collect();
        let budget = collected.budget_spent;
        let coverage = vec![
            checked("minecraft.crash"),
            looked_for(
                "core.exception",
                true,
                "the report has no exception chain",
                budget,
            ),
            looked_for("core.frame", true, "the report has no stack frames", budget),
            looked_for(
                "core.process",
                game_version.is_some(),
                "the report has no Minecraft Version line",
                budget,
            ),
            looked_for(
                "core.system",
                os.is_some(),
                "the report has no System Details section",
                budget,
            ),
            looked_for(
                "minecraft.system",
                saw_system_details,
                "the report has no System Details section",
                budget,
            ),
            looked_for(
                "minecraft.loader",
                loader.is_some(),
                "the report does not name a loader",
                budget,
            ),
            looked_for(
                "minecraft.mod",
                saw_mod_list,
                "the report has no mod list",
                budget,
            ),
            checked("minecraft.section"),
            checked("minecraft.mod_crash"),
            checked("minecraft.mixin_failure"),
            checked("minecraft.ticking_object"),
            checked("minecraft.jvm_problem"),
            checked("minecraft.file_problem"),
            not_applicable("core.log_line"),
            not_applicable("core.dependency_problem"),
            not_applicable("core.module"),
            not_applicable("minecraft.loader_problem"),
            not_applicable("minecraft.mod_conflict"),
            not_applicable("minecraft.world_mod"),
        ];
        let coverage = if budget {
            coverage
                .into_iter()
                .map(|entry| {
                    if entry.state == agora_game_api::crash::CoverageState::Absent
                        && !found.contains(&entry.kind)
                    {
                        absent(
                            &entry.kind,
                            "the report was not read to the end: its read budget ran out",
                        )
                    } else {
                        entry
                    }
                })
                .collect()
        } else {
            coverage
        };

        Ok(ParserOutput {
            contract: CRASH_CONTRACT_VERSION,
            producer: Producer {
                format: FORMAT.to_string(),
                version: None,
            },
            facts,
            bucket,
            coverage,
            notes: notes_for(&collected, request.limits),
        })
    }
}

struct Ticking {
    kind: &'static str,
    type_name: Option<String>,
    name: Option<String>,
    dimension: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    z: Option<f64>,
    line: usize,
}

impl Ticking {
    fn new(kind: &'static str, line: usize) -> Self {
        Self {
            kind,
            type_name: None,
            name: None,
            dimension: None,
            x: None,
            y: None,
            z: None,
            line,
        }
    }

    fn absorb(&mut self, line: &str) {
        let trimmed = line.trim();
        let (key, value) = match trimmed.split_once(": ") {
            Some((key, value)) => (key, value.trim()),
            None => return,
        };
        match key {
            "Entity Type" | "Block type" | "Block" | "Block entity type" => {
                if let Some(token) = value.split_whitespace().next() {
                    if !token.is_empty() {
                        self.type_name.get_or_insert_with(|| token.to_string());
                    }
                }
            }
            "Entity Name" => {
                if !value.is_empty() {
                    self.name.get_or_insert_with(|| value.to_string());
                }
            }
            "Name" if self.type_name.is_none() => {
                if let Some(token) = value.split_whitespace().next() {
                    self.type_name.get_or_insert_with(|| token.to_string());
                }
            }
            "Entity's Exact location" => {
                if let Some((x, y, z)) = parse_xyz(value) {
                    self.x.get_or_insert(x);
                    self.y.get_or_insert(y);
                    self.z.get_or_insert(z);
                }
            }
            "Block location" => {
                let inner = value
                    .split("World: ")
                    .nth(1)
                    .or_else(|| value.split("World:").nth(1))
                    .unwrap_or(value)
                    .trim_start_matches('(')
                    .split(')')
                    .next()
                    .unwrap_or("");
                if let Some((x, y, z)) = parse_xyz(inner) {
                    self.x.get_or_insert(x);
                    self.y.get_or_insert(y);
                    self.z.get_or_insert(z);
                }
            }
            "Level dimension" | "Dimension" if !value.is_empty() => {
                self.dimension.get_or_insert_with(|| value.to_string());
            }
            _ => {}
        }
    }

    fn into_fact(self) -> Option<Fact> {
        let type_name = self.type_name?;
        let mut attrs = vec![("kind", text(self.kind)), ("type", text(type_name.clone()))];
        if let Some(name) = self.name {
            attrs.push(("name", text(name)));
        }
        if let Some(dimension) = self.dimension {
            attrs.push(("dimension", text(dimension)));
        }
        if let (Some(x), Some(y), Some(z)) = (self.x, self.y, self.z) {
            attrs.push(("x", number(x)));
            attrs.push(("y", number(y)));
            attrs.push(("z", number(z)));
        }
        let mut refs = Vec::new();
        if let Some((namespace, _)) = type_name.split_once(':') {
            if !matches!(
                namespace.to_ascii_lowercase().as_str(),
                "minecraft" | "forge" | "neoforge"
            ) && is_mod_id(namespace)
            {
                refs.push(reference(
                    "mod_id",
                    namespace.to_string(),
                    Strength::Mention,
                ));
            }
        }
        Some(fact(
            "minecraft.ticking_object",
            attrs,
            refs,
            self.line,
            "ticking",
        ))
    }
}

fn parse_xyz(value: &str) -> Option<(f64, f64, f64)> {
    let mut parts = value.split(',').map(|part| part.trim().parse::<f64>());
    match (parts.next()?, parts.next()?, parts.next()?) {
        (Ok(x), Ok(y), Ok(z)) => Some((x, y, z)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModList {
    None,
    Fabric,
    Forge,
    ForgeLegacy,
    Quilt,
}

fn wrong_format() -> ParserOutput {
    ParserOutput {
        contract: CRASH_CONTRACT_VERSION,
        producer: Producer {
            format: FORMAT.to_string(),
            version: None,
        },
        facts: Vec::new(),
        bucket: Vec::new(),
        coverage: Vec::new(),
        notes: vec![agora_game_api::crash::ParseNote {
            kind: agora_game_api::crash::ParseNoteKind::WrongFormat,
            message: Some("this file does not start a Minecraft crash report".to_string()),
        }],
    }
}

/// A mod-list row is indented deeper than a `Key: value` line, with either two tabs or the eight
/// spaces some crashes use.
fn mod_list_indented(line: &str) -> bool {
    line.starts_with("\t\t") || line.starts_with("        ")
}

fn section_header(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix("-- ")?.strip_suffix(" --")?;
    if inner.is_empty() {
        None
    } else {
        Some(inner.to_string())
    }
}

fn exception_line(line: &str) -> Option<(bool, String, Option<String>)> {
    let (caused, rest) = match line.strip_prefix("Caused by: ") {
        Some(rest) => (true, rest),
        None => {
            if let Some(rest) = line.strip_prefix("Suppressed: ") {
                (false, rest)
            } else {
                if line.chars().next().is_some_and(|c| c.is_whitespace()) {
                    return None;
                }
                (false, line)
            }
        }
    };

    let (class_part, message) = match rest.split_once(": ") {
        Some((class, message)) => (class, Some(message.to_string())),
        None => match rest.strip_suffix(':') {
            Some(class) => (class, None),
            None => (rest, None),
        },
    };
    let class = super::shared::plain_class(class_part);
    if class.is_empty()
        || class.contains(char::is_whitespace)
        || !class.contains('.')
        || !(class.ends_with("Exception")
            || class.ends_with("Error")
            || class.ends_with("Throwable"))
    {
        return None;
    }
    Some((caused, class.to_string(), message))
}

fn parse_frame(line: &str, index: u32, line_no: usize) -> Option<Fact> {
    let rest = line.trim_start().strip_prefix("at ")?;
    let invocation = rest.split_whitespace().next()?;
    let before_paren = invocation.split('(').next().unwrap_or(invocation);
    let (symbol, class) = frame_symbol_and_class(before_paren);
    let (source_file, source_line) = source_location(invocation);
    let mut attrs = vec![
        ("index", number(index as f64)),
        ("kind", text("managed")),
        ("symbol_scheme", text("jvm")),
        ("symbol", text(symbol)),
    ];
    if let Some(file) = &source_file {
        attrs.push(("source_file", text(file.clone())));
    }
    if let Some(line) = source_line {
        attrs.push(("line", number(line)));
    }
    let mut refs = Vec::new();
    if let Some(class) = class {
        refs.push(reference("class", class, Strength::Exec));
    }
    if let Some(jar) = jar_from_frame(line) {
        refs.push(reference("file", jar, Strength::Mention));
    }
    Some(fact("core.frame", attrs, refs, line_no, "exception"))
}

fn parse_mod_line(mode: ModList, line: &str, line_no: usize) -> Option<Fact> {
    match mode {
        ModList::None => None,
        ModList::Fabric => {
            let trimmed = line.trim_start_matches('\t');
            let (id, rest) = trimmed.split_once(':')?;
            let id = id.trim();
            if id.is_empty() || id.contains(char::is_whitespace) {
                return None;
            }
            let rest = rest.trim();
            if rest.is_empty() {
                return None;
            }
            let (name, version) = match rest.rsplit_once(' ') {
                Some((name, version)) => (name.trim().to_string(), version.trim().to_string()),
                None => (String::new(), rest.to_string()),
            };
            Some(mod_fact(id, &version, &name, None, None, line_no))
        }
        ModList::Forge => {
            if !line.contains('|') {
                return None;
            }
            let parts: Vec<&str> = line.split('|').map(str::trim).collect();
            if parts.len() < 5 {
                return None;
            }
            let file = parts[0];
            let id = parts[2];
            if id.is_empty() {
                return None;
            }
            Some(mod_fact(
                id,
                parts[3],
                parts[1],
                Some(file),
                Some(parts[4]),
                line_no,
            ))
        }
        ModList::ForgeLegacy => {
            let trimmed = line.trim();
            let (before, id) = trimmed.rsplit_once(" (")?;
            let id = id.strip_suffix(')')?;
            if id.is_empty() || id == "Mod Id" {
                return None;
            }
            let (name, version) = before.rsplit_once(' ')?;
            Some(mod_fact(id, version, name, None, None, line_no))
        }
        ModList::Quilt => {
            let trimmed = line.trim();
            if !trimmed.starts_with('|') {
                return None;
            }
            let parts: Vec<&str> = trimmed.split('|').map(str::trim).collect();
            if parts.len() < 6 {
                return None;
            }
            let id = parts[3];
            if id.is_empty() || id == "ID" || id.chars().all(|c| c == '-' || c == ' ') {
                return None;
            }
            Some(mod_fact(
                id,
                parts[4],
                parts[2],
                None,
                Some(parts[5]),
                line_no,
            ))
        }
    }
}

fn mod_fact(
    id: &str,
    version: &str,
    name: &str,
    file: Option<&str>,
    state: Option<&str>,
    line_no: usize,
) -> Fact {
    let mut attrs = vec![("id", text(id.to_string()))];
    if !version.is_empty() {
        attrs.push(("version", text(version.to_string())));
    }
    if !name.is_empty() {
        attrs.push(("name", text(name.to_string())));
    }
    if let Some(file) = file.filter(|file| !file.is_empty()) {
        attrs.push(("file", text(file.to_string())));
    }
    if let Some(state) = state.filter(|state| !state.is_empty()) {
        attrs.push(("state", text(state.to_string())));
    }
    fact(
        "minecraft.mod",
        attrs,
        vec![reference("mod_id", id.to_string(), Strength::Mention)],
        line_no,
        "mods",
    )
}

fn split_vendor(value: &str) -> (String, String) {
    match value.split_once(", ") {
        Some((version, vendor)) => (version.trim().to_string(), vendor.trim().to_string()),
        None => (value.trim().to_string(), String::new()),
    }
}

fn memory_max(value: &str) -> Option<f64> {
    let after = value.split("up to ").nth(1)?;
    let bytes: u64 = after.split_whitespace().next()?.parse().ok()?;
    Some(bytes as f64 / 1024.0 / 1024.0)
}

fn loader_from_launched(launched: Option<&str>) -> Option<(String, String)> {
    let launched = launched?;
    for (prefix, id) in [
        ("fabric-loader-", "fabric"),
        ("quilt-loader-", "quilt"),
        ("neoforge-", "neoforge"),
        ("forge-", "forge"),
    ] {
        if let Some(rest) = launched.strip_prefix(prefix) {
            let version = rest.split('-').next().unwrap_or(rest).to_string();
            return Some((id.to_string(), version));
        }
    }
    if let Some((_, rest)) = launched.split_once("-forge-") {
        return Some(("forge".to_string(), rest.to_string()));
    }
    if let Some((_, rest)) = launched.split_once("-neoforge-") {
        return Some(("neoforge".to_string(), rest.to_string()));
    }
    if launched.starts_with("Forge") || launched.contains("-forge") {
        return Some(("forge".to_string(), String::new()));
    }
    if launched.starts_with("Fabric") {
        return Some(("fabric".to_string(), String::new()));
    }
    if launched.starts_with("Quilt") {
        return Some(("quilt".to_string(), String::new()));
    }
    None
}
