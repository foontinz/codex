use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use codex_otel::SessionTelemetry;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use futures::StreamExt;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::agents_md;
use crate::client::ModelClient;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::config::Config;
use crate::models_manager::manager::ModelsManager;
use crate::models_manager::manager::RefreshStrategy;

pub(crate) const DEFAULT_STARTUP_AGENTS_SUMMARY_MODEL: &str = "gpt-5.3-codex";

const STARTUP_AGENTS_DISCOVERY_HEADER: &str = "## Startup AGENTS discovery tree (gitignore-aware)";
const STARTUP_AGENTS_SUMMARY_PROMPT: &str = r#"You produce concise AGENTS routing hints.

Return valid JSON that strictly matches the provided schema.
For each file path in the input:
- why: one short sentence describing what guidance this file provides.
- when: one short sentence describing when the agent should read/apply this file.

Rules:
- include every input path exactly once
- do not invent paths
- keep `why` and `when` to 160 characters each
- keep text concise and actionable"#;

const MAX_SUMMARY_WHY_CHARS: usize = 160;
const MAX_SUMMARY_WHEN_CHARS: usize = 160;

#[derive(Clone, Debug, PartialEq, Eq)]
struct TreeEntry {
    relative_path: PathBuf,
    depth: usize,
    kind: TreeEntryKind,
}

impl TreeEntry {
    fn display_name(&self) -> String {
        let mut name = self.relative_path.file_name().map_or_else(
            || relative_path_text(&self.relative_path),
            |value| value.to_string_lossy().to_string(),
        );

        match self.kind {
            TreeEntryKind::Directory => name.push('/'),
            TreeEntryKind::Symlink => name.push('@'),
            TreeEntryKind::Other => name.push('?'),
            TreeEntryKind::File => {}
        }

        name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TreeEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug)]
struct SummaryTarget {
    relative_path: String,
    contents: String,
}

#[derive(Clone, Debug)]
struct DiscoveryTree {
    entries: Vec<TreeEntry>,
    summary_targets: Vec<SummaryTarget>,
}

#[derive(Clone, Debug)]
struct AgentsSummary {
    why: String,
    when: String,
}

#[derive(Debug, Deserialize)]
struct AgentsSummariesResponse {
    summaries: Vec<AgentsSummaryItem>,
}

#[derive(Debug, Deserialize)]
struct AgentsSummaryItem {
    path: String,
    why: String,
    when: String,
}

#[derive(Clone)]
struct SummaryTargetCandidate {
    priority: u8,
    relative_path: PathBuf,
    absolute_path: PathBuf,
    kind: TreeEntryKind,
}

pub(crate) async fn build_startup_agents_discovery_section(
    config: &Config,
    model_client: &ModelClient,
    models_manager: &ModelsManager,
    session_telemetry: &SessionTelemetry,
) -> Result<String> {
    info!(
        cwd = %config.cwd.display(),
        "building startup AGENTS discovery section"
    );
    let discovery = discover_tree(config.cwd.as_path())?;
    debug!(
        entry_count = discovery.entries.len(),
        summary_target_count = discovery.summary_targets.len(),
        "completed startup AGENTS tree discovery"
    );
    let summaries = if discovery.summary_targets.is_empty() {
        info!("no startup AGENTS files found for summary generation");
        BTreeMap::new()
    } else {
        summarize_targets(
            config,
            model_client,
            models_manager,
            session_telemetry,
            &discovery.summary_targets,
        )
        .await?
    };
    info!(
        summary_count = summaries.len(),
        "built startup AGENTS discovery summaries"
    );

    Ok(render_discovery_section(
        config.cwd.as_path(),
        &discovery.entries,
        &summaries,
    ))
}

pub(crate) fn prepend_discovery_section(
    existing: Option<String>,
    discovery_section: String,
) -> Option<String> {
    if discovery_section.trim().is_empty() {
        return existing;
    }

    match existing {
        Some(existing) if !existing.trim().is_empty() => {
            Some(format!("{discovery_section}\n\n{existing}"))
        }
        Some(_) | None => Some(discovery_section),
    }
}

fn discover_tree(cwd: &Path) -> Result<DiscoveryTree> {
    let mut directory_entries = Vec::new();
    let mut summary_targets_by_dir: BTreeMap<PathBuf, SummaryTargetCandidate> = BTreeMap::new();
    let canonical_cwd = cwd.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize startup agents discovery cwd {}",
            cwd.display()
        )
    })?;

    let mut builder = WalkBuilder::new(cwd);
    builder.standard_filters(true);
    builder.require_git(false);
    let walker = builder.build();

    for maybe_entry in walker {
        let entry = maybe_entry.with_context(|| {
            format!(
                "failed to walk startup agents discovery tree from {}",
                cwd.display()
            )
        })?;

        if entry.depth() == 0 {
            continue;
        }

        let path = entry.path();
        let relative_path = path
            .strip_prefix(cwd)
            .with_context(|| {
                format!(
                    "failed to compute relative path for startup agents discovery: {}",
                    path.display()
                )
            })?
            .to_path_buf();

        let kind = entry
            .file_type()
            .map(TreeEntryKind::from)
            .unwrap_or(TreeEntryKind::Other);

        if kind == TreeEntryKind::Directory {
            directory_entries.push(TreeEntry {
                relative_path: relative_path.clone(),
                depth: relative_path.components().count().saturating_sub(1),
                kind,
            });
        }

        if kind != TreeEntryKind::File && kind != TreeEntryKind::Symlink {
            continue;
        }

        let Some(file_name) = relative_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        let Some(priority) = agents_md::filename_priority(file_name) else {
            continue;
        };

        let canonical_candidate_path = path.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize startup agents file {}",
                path.display()
            )
        })?;
        if !canonical_candidate_path.starts_with(&canonical_cwd) {
            continue;
        }

        let parent = relative_path
            .parent()
            .map_or_else(PathBuf::new, Path::to_path_buf);

        let should_replace = match summary_targets_by_dir.get(&parent) {
            Some(existing) => priority < existing.priority,
            None => true,
        };

        if should_replace {
            summary_targets_by_dir.insert(
                parent,
                SummaryTargetCandidate {
                    priority,
                    relative_path,
                    absolute_path: canonical_candidate_path,
                    kind,
                },
            );
        }
    }

    let mut summary_target_candidates = summary_targets_by_dir
        .into_values()
        .collect::<Vec<SummaryTargetCandidate>>();
    summary_target_candidates
        .sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let mut entries = directory_entries;
    entries.extend(summary_target_candidates.iter().map(|candidate| {
        TreeEntry {
            relative_path: candidate.relative_path.clone(),
            depth: candidate
                .relative_path
                .components()
                .count()
                .saturating_sub(1),
            kind: candidate.kind,
        }
    }));
    entries.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let mut summary_targets: Vec<SummaryTarget> = summary_target_candidates
        .iter()
        .map(|candidate| {
            let data = std::fs::read(&candidate.absolute_path).with_context(|| {
                format!(
                    "failed to read startup agents file {}",
                    candidate.absolute_path.display()
                )
            })?;
            let contents = String::from_utf8_lossy(&data).to_string();
            Ok(SummaryTarget {
                relative_path: relative_path_text(&candidate.relative_path),
                contents,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    summary_targets.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));

    Ok(DiscoveryTree {
        entries,
        summary_targets,
    })
}

async fn summarize_targets(
    config: &Config,
    model_client: &ModelClient,
    models_manager: &ModelsManager,
    session_telemetry: &SessionTelemetry,
    summary_targets: &[SummaryTarget],
) -> Result<BTreeMap<String, AgentsSummary>> {
    let expected_paths: BTreeSet<String> = summary_targets
        .iter()
        .map(|target| target.relative_path.clone())
        .collect();

    let selected_model = config.startup_agents_summary_model.as_deref();
    let has_openai_provider_connected = config.model_provider.is_openai();
    let primary_model = if selected_model.is_none() && !has_openai_provider_connected {
        Some(
            models_manager
                .get_default_model(&config.model, RefreshStrategy::OnlineIfUncached)
                .await,
        )
    } else {
        None
    };
    let model_name = select_startup_agents_summary_model(
        selected_model,
        has_openai_provider_connected,
        primary_model.as_deref(),
    );
    info!(
        model = model_name,
        selected_model = selected_model.unwrap_or(""),
        has_openai_provider_connected,
        summary_target_count = summary_targets.len(),
        "requesting startup AGENTS summaries"
    );
    let model_info = models_manager.get_model_info(&model_name, config).await;

    let input_payload = build_summary_input_payload(summary_targets)?;

    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: input_payload,
            }],
            end_turn: None,
            phase: None,
        }],
        tools: Vec::new(),
        parallel_tool_calls: false,
        base_instructions: BaseInstructions {
            text: STARTUP_AGENTS_SUMMARY_PROMPT.to_string(),
        },
        personality: None,
        output_schema: Some(summary_output_schema()),
    };

    let mut client_session = model_client.new_session();
    let mut stream = client_session
        .stream(
            &prompt,
            &model_info,
            session_telemetry,
            None,
            ReasoningSummaryConfig::None,
            None,
            None,
        )
        .await?;

    let mut response_text = String::new();
    while let Some(event) = stream.next().await.transpose()? {
        match event {
            ResponseEvent::OutputTextDelta(delta) => response_text.push_str(&delta),
            ResponseEvent::OutputItemDone(item) => {
                if response_text.is_empty()
                    && let ResponseItem::Message { content, .. } = item
                    && let Some(text) = crate::compact::content_items_to_text(&content)
                {
                    response_text.push_str(&text);
                }
            }
            ResponseEvent::Completed { .. } => {
                break;
            }
            _ => {}
        }
    }

    let parsed: AgentsSummariesResponse = serde_json::from_str(&response_text)
        .with_context(|| format!("invalid startup agents summary response: {response_text}"))?;
    debug!(
        response_char_len = response_text.len(),
        summaries_count = parsed.summaries.len(),
        "received startup AGENTS summary response"
    );

    validate_summaries(parsed, expected_paths)
}

fn select_startup_agents_summary_model(
    selected_model: Option<&str>,
    has_openai_provider_connected: bool,
    primary_model: Option<&str>,
) -> String {
    if let Some(selected_model) = selected_model {
        return selected_model.to_string();
    }

    if has_openai_provider_connected {
        return DEFAULT_STARTUP_AGENTS_SUMMARY_MODEL.to_string();
    }

    primary_model.unwrap_or_default().to_string()
}

fn build_summary_input_payload(summary_targets: &[SummaryTarget]) -> Result<String> {
    let files = summary_targets
        .iter()
        .map(|target| {
            json!({
                "path": target.relative_path,
                "contents": target.contents,
            })
        })
        .collect::<Vec<_>>();

    let files_json = serde_json::to_string_pretty(&files)?;

    Ok(format!(
        "Summarize each AGENTS file and decide when to read it.\n\nFiles:\n{files_json}"
    ))
}

fn summary_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summaries": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "why": { "type": "string", "maxLength": MAX_SUMMARY_WHY_CHARS },
                        "when": { "type": "string", "maxLength": MAX_SUMMARY_WHEN_CHARS }
                    },
                    "required": ["path", "why", "when"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["summaries"],
        "additionalProperties": false
    })
}

fn validate_summaries(
    parsed: AgentsSummariesResponse,
    expected_paths: BTreeSet<String>,
) -> Result<BTreeMap<String, AgentsSummary>> {
    let mut indexed = BTreeMap::new();

    for item in parsed.summaries {
        let path = item.path.trim().to_string();
        let mut why = item.why.trim().to_string();
        if why.chars().count() > MAX_SUMMARY_WHY_CHARS {
            why = why.chars().take(MAX_SUMMARY_WHY_CHARS).collect();
        }
        let mut when = item.when.trim().to_string();
        if when.chars().count() > MAX_SUMMARY_WHEN_CHARS {
            when = when.chars().take(MAX_SUMMARY_WHEN_CHARS).collect();
        }

        if path.is_empty() {
            anyhow::bail!("startup agents summary returned an empty path");
        }
        if why.is_empty() {
            anyhow::bail!("startup agents summary returned an empty why for {path}");
        }
        if when.is_empty() {
            anyhow::bail!("startup agents summary returned an empty when for {path}");
        }

        if indexed.contains_key(&path) {
            anyhow::bail!("startup agents summary returned a duplicate path: {path}");
        }

        indexed.insert(path, AgentsSummary { why, when });
    }

    let actual_paths: BTreeSet<String> = indexed.keys().cloned().collect();
    if actual_paths != expected_paths {
        warn!(
            expected_paths = ?expected_paths,
            actual_paths = ?actual_paths,
            "startup AGENTS summary paths mismatch"
        );
        anyhow::bail!(
            "startup agents summary paths do not match discovered AGENTS files; expected={expected_paths:?}, actual={actual_paths:?}"
        );
    }
    debug!(
        summary_count = indexed.len(),
        "validated startup AGENTS summaries"
    );

    Ok(indexed)
}

fn render_discovery_section(
    cwd: &Path,
    entries: &[TreeEntry],
    summaries: &BTreeMap<String, AgentsSummary>,
) -> String {
    let mut lines = vec![
        STARTUP_AGENTS_DISCOVERY_HEADER.to_string(),
        format!("cwd: {}", cwd.display()),
        ".".to_string(),
    ];

    for entry in entries {
        let line_indent = "  ".repeat(entry.depth + 1);
        let display_name = entry.display_name();
        lines.push(format!("{line_indent}{display_name}"));

        let summary_key = relative_path_text(&entry.relative_path);
        if let Some(summary) = summaries.get(&summary_key) {
            let summary_indent = "  ".repeat(entry.depth + 2);
            lines.push(format!("{summary_indent}why: {}", summary.why));
            lines.push(format!("{summary_indent}when: {}", summary.when));
        }
    }

    lines.join("\n")
}

fn relative_path_text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

impl From<std::fs::FileType> for TreeEntryKind {
    fn from(file_type: std::fs::FileType) -> Self {
        if file_type.is_dir() {
            TreeEntryKind::Directory
        } else if file_type.is_file() {
            TreeEntryKind::File
        } else if file_type.is_symlink() {
            TreeEntryKind::Symlink
        } else {
            TreeEntryKind::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    use super::AgentsSummariesResponse;
    use super::AgentsSummary;
    use super::AgentsSummaryItem;
    use super::TreeEntry;
    use super::TreeEntryKind;
    use super::discover_tree;
    use super::prepend_discovery_section;
    use super::render_discovery_section;
    use super::select_startup_agents_summary_model;
    use super::summary_output_schema;
    use super::validate_summaries;

    #[test]
    fn discover_tree_prefers_agents_override() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service_dir = temp.path().join("service");
        fs::create_dir_all(&service_dir)?;
        fs::write(service_dir.join("AGENTS.md"), "default")?;
        fs::write(service_dir.join("AGENTS.override.md"), "override")?;

        let tree = discover_tree(temp.path())?;
        let targets = tree
            .summary_targets
            .iter()
            .map(|target| target.relative_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(targets, vec!["service/AGENTS.override.md"]);
        Ok(())
    }

    #[test]
    fn discover_tree_respects_gitignore() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        fs::write(temp.path().join(".gitignore"), "ignored/\n")?;
        fs::create_dir_all(temp.path().join("ignored"))?;
        fs::create_dir_all(temp.path().join("included"))?;
        fs::write(temp.path().join("ignored/AGENTS.md"), "ignored")?;
        fs::write(temp.path().join("included/AGENTS.md"), "included")?;

        let tree = discover_tree(temp.path())?;
        let targets = tree
            .summary_targets
            .iter()
            .map(|target| target.relative_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(targets, vec!["included/AGENTS.md"]);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn discover_tree_includes_symlinked_agents_for_summary() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new()?;
        let target_dir = temp.path().join("shared");
        let linked_dir = temp.path().join("linked");
        fs::create_dir_all(&target_dir)?;
        fs::create_dir_all(&linked_dir)?;

        fs::write(target_dir.join("AGENTS.md"), "shared rules")?;
        symlink(target_dir.join("AGENTS.md"), linked_dir.join("AGENTS.md"))?;

        let tree = discover_tree(temp.path())?;
        let targets = tree
            .summary_targets
            .iter()
            .map(|target| target.relative_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(targets, vec!["linked/AGENTS.md", "shared/AGENTS.md"]);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn discover_tree_excludes_symlinked_agents_pointing_outside_cwd() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new()?;
        let external = TempDir::new()?;
        let linked_dir = temp.path().join("linked");
        fs::create_dir_all(&linked_dir)?;

        let external_agents_path = external.path().join("AGENTS.md");
        fs::write(&external_agents_path, "external rules")?;
        symlink(&external_agents_path, linked_dir.join("AGENTS.md"))?;

        let tree = discover_tree(temp.path())?;
        let targets = tree
            .summary_targets
            .iter()
            .map(|target| target.relative_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(targets, Vec::<&str>::new());
        Ok(())
    }

    #[test]
    fn discover_tree_tolerates_non_utf8_agents_contents() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().join("service");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("AGENTS.md"), [0x66, 0x6f, 0x80, 0x6f])?;

        let tree = discover_tree(temp.path())?;
        assert_eq!(tree.summary_targets.len(), 1);
        assert_eq!(tree.summary_targets[0].relative_path, "service/AGENTS.md");
        assert_eq!(tree.summary_targets[0].contents, "fo\u{fffd}o");
        Ok(())
    }

    #[test]
    fn render_discovery_section_places_summary_after_agents_entry() {
        let entries = vec![
            TreeEntry {
                relative_path: "folder".into(),
                depth: 0,
                kind: TreeEntryKind::Directory,
            },
            TreeEntry {
                relative_path: "folder/AGENTS.md".into(),
                depth: 1,
                kind: TreeEntryKind::File,
            },
        ];

        let mut summaries = BTreeMap::new();
        summaries.insert(
            "folder/AGENTS.md".to_string(),
            AgentsSummary {
                why: "defines coding rules".to_string(),
                when: "before editing files under folder".to_string(),
            },
        );

        let rendered = render_discovery_section(Path::new("/repo"), &entries, &summaries);
        let lines = rendered.lines().collect::<Vec<_>>();
        let agents_index = lines
            .iter()
            .position(|line| *line == "    AGENTS.md")
            .expect("AGENTS line should exist");

        assert_eq!(lines[agents_index + 1], "      why: defines coding rules");
        assert_eq!(
            lines[agents_index + 2],
            "      when: before editing files under folder"
        );
    }

    #[test]
    fn discover_tree_entries_include_only_dirs_and_agents_targets() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let service_dir = temp.path().join("service");
        fs::create_dir_all(&service_dir)?;
        fs::write(service_dir.join("file.txt"), "ignore me")?;
        fs::write(service_dir.join("AGENTS.md"), "service rules")?;

        let tree = discover_tree(temp.path())?;
        let entries = tree
            .entries
            .iter()
            .map(|entry| super::relative_path_text(&entry.relative_path))
            .collect::<Vec<_>>();

        assert_eq!(entries, vec!["service", "service/AGENTS.md"]);
        Ok(())
    }

    #[test]
    fn validate_summaries_requires_complete_path_set() {
        let expected_paths = ["a/AGENTS.md".to_string(), "b/AGENTS.md".to_string()]
            .into_iter()
            .collect();

        let parsed = AgentsSummariesResponse {
            summaries: vec![AgentsSummaryItem {
                path: "a/AGENTS.md".to_string(),
                why: "rules".to_string(),
                when: "before edits".to_string(),
            }],
        };

        let error =
            validate_summaries(parsed, expected_paths).expect_err("missing path should fail");
        assert!(
            error
                .to_string()
                .contains("paths do not match discovered AGENTS files")
        );
    }

    #[test]
    fn validate_summaries_caps_why_and_when_lengths() {
        let expected_paths = ["a/AGENTS.md".to_string()].into_iter().collect();
        let parsed = AgentsSummariesResponse {
            summaries: vec![AgentsSummaryItem {
                path: "a/AGENTS.md".to_string(),
                why: "w".repeat(super::MAX_SUMMARY_WHY_CHARS + 25),
                when: "x".repeat(super::MAX_SUMMARY_WHEN_CHARS + 25),
            }],
        };

        let summaries = validate_summaries(parsed, expected_paths).expect("summary should parse");
        let item = summaries
            .get("a/AGENTS.md")
            .expect("summary should exist for expected path");

        assert_eq!(item.why.chars().count(), super::MAX_SUMMARY_WHY_CHARS);
        assert_eq!(item.when.chars().count(), super::MAX_SUMMARY_WHEN_CHARS);
    }

    #[test]
    fn summary_output_schema_sets_max_lengths_for_why_and_when() {
        let schema = summary_output_schema();
        assert_eq!(
            schema["properties"]["summaries"]["items"]["properties"]["why"]["maxLength"],
            serde_json::json!(super::MAX_SUMMARY_WHY_CHARS)
        );
        assert_eq!(
            schema["properties"]["summaries"]["items"]["properties"]["when"]["maxLength"],
            serde_json::json!(super::MAX_SUMMARY_WHEN_CHARS)
        );
    }

    #[test]
    fn prepend_discovery_section_prepends_existing_instructions() {
        let merged = prepend_discovery_section(
            Some("raw instructions".to_string()),
            "tree section".to_string(),
        );

        assert_eq!(merged, Some("tree section\n\nraw instructions".to_string()));
    }

    #[test]
    fn select_startup_agents_summary_model_prefers_selected_model() {
        let model =
            select_startup_agents_summary_model(Some("gpt-5.2-codex"), true, Some("gpt-5.1-codex"));

        assert_eq!(model, "gpt-5.2-codex");
    }

    #[test]
    fn select_startup_agents_summary_model_uses_mini_for_openai_provider() {
        let model = select_startup_agents_summary_model(None, true, Some("gpt-5.1-codex"));

        assert_eq!(model, super::DEFAULT_STARTUP_AGENTS_SUMMARY_MODEL);
    }

    #[test]
    fn select_startup_agents_summary_model_uses_primary_for_non_openai_provider() {
        let model = select_startup_agents_summary_model(None, false, Some("llama3.2"));

        assert_eq!(model, "llama3.2");
    }
}
