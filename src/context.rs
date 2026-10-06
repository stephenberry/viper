//! Context assembly: project instruction files, skills, and the system prompt.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::config::{Settings, agent_dir, expand_home};
use crate::tools::Tool;
use crate::util::escape_xml;

const CONTEXT_FILE_NAMES: [&str; 5] = ["AGENTS.override.md", "AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

#[derive(Debug, Clone)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

fn load_context_file(dir: &Path) -> Option<ContextFile> {
    CONTEXT_FILE_NAMES.iter().map(|name| dir.join(name)).find(|path| path.is_file()).and_then(|path| {
        match std::fs::read_to_string(&path) {
            Ok(content) => Some(ContextFile { content: content.trim_start_matches('\u{FEFF}').to_string(), path }),
            Err(err) => {
                eprintln!("warning: could not read {}: {err}", path.display());
                None
            }
        }
    })
}

/// The global context file followed by one per directory from the filesystem root down to `cwd`.
pub fn load_context_files(cwd: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    if let Some(global) = load_context_file(&agent_dir()) {
        seen.insert(global.path.clone());
        files.push(global);
    }
    let mut ancestors: Vec<ContextFile> = Vec::new();
    for dir in cwd.ancestors() {
        if let Some(file) = load_context_file(dir)
            && seen.insert(file.path.clone())
        {
            ancestors.push(file);
        }
    }
    ancestors.reverse();
    files.extend(ancestors);
    files
}

/// The enclosing git repository root, if any.
pub fn find_git_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find(|dir| dir.join(".git").exists()).map(Path::to_path_buf)
}

// ---------------------------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    /// Only invocable through `/skill:name`; not advertised to the model.
    pub disable_model_invocation: bool,
}

/// Parse the leading `---` frontmatter of a Markdown file into key/value pairs and the body.
///
/// Supports the YAML subset skills use: `key: value` (optionally quoted), block scalars
/// (`|`, `>`, with `-`/`+` chomping), and plain values continued on indented lines. Nested maps
/// are skipped.
pub fn parse_frontmatter(text: &str) -> (Vec<(String, String)>, String) {
    let text = text.trim_start_matches('\u{FEFF}');
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return (Vec::new(), text.to_string());
    }
    let mut header: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        header.push(line);
    }
    if !closed {
        return (Vec::new(), text.to_string());
    }
    let body = lines.collect::<Vec<_>>().join("\n");

    let mut fields = Vec::new();
    let mut i = 0;
    while i < header.len() {
        let line = header[i];
        i += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') || line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim().to_string();
        let value = value.trim();
        let mut continuation = Vec::new();
        while i < header.len() && (header[i].starts_with(char::is_whitespace) || header[i].trim().is_empty()) {
            continuation.push(header[i]);
            i += 1;
        }
        let parsed = if let Some(style) = value.strip_prefix(['|', '>']).map(|_| value.chars().next().unwrap()) {
            let indent = continuation
                .iter()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.len() - l.trim_start().len())
                .min()
                .unwrap_or(0);
            let block: Vec<&str> =
                continuation.iter().map(|l| if l.len() >= indent { &l[indent..] } else { l.trim() }).collect();
            let joined = if style == '|' {
                block.join("\n")
            } else {
                let mut out = String::new();
                for line in &block {
                    if line.is_empty() {
                        out.push('\n');
                    } else {
                        if !out.is_empty() && !out.ends_with('\n') {
                            out.push(' ');
                        }
                        out.push_str(line);
                    }
                }
                out
            };
            joined.trim_end().to_string()
        } else {
            let mut full = value.to_string();
            for line in continuation.iter().filter(|l| !l.trim().is_empty()) {
                full.push(' ');
                full.push_str(line.trim());
            }
            unquote(&full)
        };
        fields.push((key, parsed));
    }
    (fields, body)
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            return inner.replace("\\\"", "\"").replace("\\n", "\n").replace("\\\\", "\\");
        }
        if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            return inner.replace("''", "'");
        }
    }
    value.to_string()
}

fn load_skill(file: &Path) -> Result<Option<Skill>> {
    let text = std::fs::read_to_string(file).with_context(|| format!("could not read {}", file.display()))?;
    let (fields, _) = parse_frontmatter(&text);
    let get = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let Some(description) = get("description").filter(|d| !d.trim().is_empty()) else {
        return Ok(None);
    };
    let base_dir = file.parent().unwrap_or(Path::new(".")).to_path_buf();
    let name = get("name")
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| base_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let disable_model_invocation = get("disable-model-invocation").is_some_and(|v| v.trim() == "true");
    Ok(Some(Skill { name, description, file_path: file.to_path_buf(), base_dir, disable_model_invocation }))
}

fn find_skill_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let skill_file = root.join("SKILL.md");
    if skill_file.is_file() {
        out.push(skill_file);
        // A skill directory's subdirectories hold its resources, not more skills.
        return;
    }
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        if name == "node_modules" || name == ".git" {
            continue;
        }
        if path.is_dir() {
            find_skill_files(&path, out);
        }
    }
}

/// Directories searched for skills, highest priority first.
pub fn skill_dirs(cwd: &Path, settings: &Settings) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let stop = find_git_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    for dir in cwd.ancestors() {
        dirs.push(dir.join(".viper").join("skills"));
        dirs.push(dir.join(".agents").join("skills"));
        if dir == stop {
            break;
        }
    }
    dirs.extend(settings.skill_paths.iter().map(|p| {
        let path = expand_home(p);
        if path.is_absolute() { path } else { cwd.join(path) }
    }));
    dirs.push(agent_dir().join("skills"));
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".agents").join("skills"));
    }
    dirs
}

/// Discover skills. Name collisions keep the first (highest priority) skill.
pub fn load_skills(cwd: &Path, settings: &Settings) -> (Vec<Skill>, Vec<String>) {
    let mut skills: Vec<Skill> = Vec::new();
    let mut warnings = Vec::new();
    if !settings.enable_skills {
        return (skills, warnings);
    }
    let mut seen_files = HashSet::new();
    for dir in skill_dirs(cwd, settings) {
        let mut files = Vec::new();
        find_skill_files(&dir, &mut files);
        for file in files {
            let canonical = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
            if !seen_files.insert(canonical) {
                continue;
            }
            match load_skill(&file) {
                Ok(Some(skill)) => {
                    if skills.iter().any(|s| s.name == skill.name) {
                        warnings.push(format!(
                            "skill '{}' at {} shadowed by an earlier skill",
                            skill.name,
                            file.display()
                        ));
                    } else {
                        skills.push(skill);
                    }
                }
                Ok(None) => warnings.push(format!("skipped {}: missing description", file.display())),
                Err(err) => warnings.push(format!("{err:#}")),
            }
        }
    }
    (skills, warnings)
}

/// Expand `/skill:name args` into the skill's instructions followed by the user's request.
pub fn expand_skill_command(text: &str, skills: &[Skill]) -> Result<Option<String>> {
    let Some(rest) = text.strip_prefix("/skill:") else { return Ok(None) };
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest.trim(), ""),
    };
    let skill = skills.iter().find(|s| s.name == name).with_context(|| format!("unknown skill '{name}'"))?;
    let text = std::fs::read_to_string(&skill.file_path)
        .with_context(|| format!("could not read {}", skill.file_path.display()))?;
    let (_, body) = parse_frontmatter(&text);
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.file_path.display(),
        skill.base_dir.display(),
        body.trim()
    );
    Ok(Some(if args.is_empty() { block } else { format!("{block}\n\n{args}") }))
}

// ---------------------------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------------------------

pub struct PromptInputs<'a> {
    pub cwd: &'a Path,
    pub tools: &'a [std::sync::Arc<dyn Tool>],
    /// Replaces the default preamble, tool list, and rules.
    pub custom_prompt: Option<&'a str>,
    pub append: Option<&'a str>,
    pub context_files: &'a [ContextFile],
    pub skills: &'a [Skill],
}

fn section(name: &str, content: &str) -> String {
    format!("<{name}>\n{content}\n</{name}>")
}

pub fn build_system_prompt(inputs: &PromptInputs<'_>) -> String {
    let mut sections = Vec::new();
    let names: Vec<&str> = inputs.tools.iter().map(|t| t.name()).collect();

    if let Some(custom) = inputs.custom_prompt {
        sections.push(custom.to_string());
    } else {
        sections.push(
            "You are an expert coding assistant operating inside viper, a coding agent harness. You help users by \
             reading files, executing commands, editing code, and writing new files."
                .to_string(),
        );
        let tools = if inputs.tools.is_empty() {
            "(none)".to_string()
        } else {
            inputs.tools.iter().map(|t| format!("- {}: {}", t.name(), t.snippet())).collect::<Vec<_>>().join("\n")
        };
        sections.push(section("tools", &tools));

        let mut rules: Vec<String> = Vec::new();
        let mut add = |rule: &str| {
            if !rules.iter().any(|r| r == rule) {
                rules.push(rule.to_string());
            }
        };
        if names.contains(&"bash") && !names.iter().any(|n| ["grep", "find", "ls"].contains(n)) {
            add("Use bash for file operations like ls, rg, find");
        }
        for tool in inputs.tools {
            for rule in tool.guidelines() {
                add(rule);
            }
        }
        add("Be concise in your responses");
        add("Show file paths clearly when working with files");
        let rules: Vec<String> = rules.iter().map(|r| format!("- {r}")).collect();
        sections.push(section("rules", &rules.join("\n")));
    }

    if let Some(append) = inputs.append.filter(|a| !a.trim().is_empty()) {
        sections.push(section("addendum", append.trim()));
    }

    if !inputs.context_files.is_empty() {
        let mut parts = vec!["Project-specific instructions and guidelines:".to_string()];
        for file in inputs.context_files {
            parts.push(format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                file.path.display(),
                file.content.trim_end()
            ));
        }
        sections.push(section("project_context", &parts.join("\n\n")));
    }

    let visible: Vec<&Skill> = inputs.skills.iter().filter(|s| !s.disable_model_invocation).collect();
    let reader = if names.contains(&"read") {
        Some("Use the read tool to load a skill's file when the task matches its description.")
    } else if names.contains(&"bash") {
        Some("Use bash to load a skill's file when the task matches its description.")
    } else {
        None
    };
    if let (Some(reader), false) = (reader, visible.is_empty()) {
        let mut lines = vec![
            "The following skills provide specialized instructions for specific tasks.".to_string(),
            reader.to_string(),
            "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
            String::new(),
            "<available_skills>".to_string(),
        ];
        for skill in visible {
            lines.push("  <skill>".into());
            lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
            lines.push(format!("    <description>{}</description>", escape_xml(&skill.description)));
            lines.push(format!("    <location>{}</location>", escape_xml(&skill.file_path.to_string_lossy())));
            lines.push("  </skill>".into());
        }
        lines.push("</available_skills>".into());
        sections.push(section("skills", &lines.join("\n")));
    }

    sections.push(section("cwd", &inputs.cwd.to_string_lossy().replace('\\', "/")));
    sections.join("\n\n")
}

/// Resolve a prompt setting that may be literal text or a path to a file.
pub fn resolve_prompt_text(value: &str, cwd: &Path) -> Result<String> {
    let path = expand_home(value);
    let path = if path.is_absolute() { path } else { cwd.join(path) };
    if value.len() < 4096 && path.is_file() {
        return std::fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_variants() {
        let text = "---\nname: pdf-tools\ndescription: >-\n  Extract text\n  from PDFs.\ndisable-model-invocation: true\nmetadata:\n  a: b\n---\n# Body\n";
        let (fields, body) = parse_frontmatter(text);
        assert_eq!(fields[0], ("name".into(), "pdf-tools".into()));
        assert_eq!(fields[1], ("description".into(), "Extract text from PDFs.".into()));
        assert_eq!(fields[2], ("disable-model-invocation".into(), "true".into()));
        assert_eq!(body, "# Body");
        let (fields, _) = parse_frontmatter("---\ndescription: \"Quoted: yes\"\n---\n");
        assert_eq!(fields[0].1, "Quoted: yes");
        let (fields, body) = parse_frontmatter("no frontmatter");
        assert!(fields.is_empty());
        assert_eq!(body, "no frontmatter");
    }

    #[test]
    fn discovers_skills_and_expands_commands() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".viper/skills/greet");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "---\ndescription: Say hi\n---\nSay hello politely.\n").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let settings = Settings::default();
        let (skills, _) = load_skills(dir.path(), &settings);
        let skill = skills.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(skill.description, "Say hi");
        let expanded = expand_skill_command("/skill:greet to Bob", &skills).unwrap().unwrap();
        assert!(expanded.starts_with("<skill name=\"greet\""));
        assert!(expanded.contains("Say hello politely."));
        assert!(expanded.ends_with("</skill>\n\nto Bob"));
        assert!(expand_skill_command("/skill:missing", &skills).is_err());
    }

    #[test]
    fn system_prompt_includes_sections() {
        let tools = crate::tools::select_tools(&["read".into(), "bash".into()]).unwrap();
        let prompt = build_system_prompt(&PromptInputs {
            cwd: Path::new("/proj"),
            tools: &tools,
            custom_prompt: None,
            append: Some("extra"),
            context_files: &[ContextFile { path: "/proj/AGENTS.md".into(), content: "Be nice".into() }],
            skills: &[],
        });
        assert!(prompt.contains("- read: Read file contents"));
        assert!(prompt.contains("- Use bash for file operations like ls, rg, find"));
        assert!(prompt.contains("<addendum>\nextra\n</addendum>"));
        assert!(prompt.contains("<project_instructions path=\"/proj/AGENTS.md\">\nBe nice\n</project_instructions>"));
        assert!(prompt.ends_with("<cwd>\n/proj\n</cwd>"));
    }
}
