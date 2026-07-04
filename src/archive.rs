use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use flate2::read::ZlibDecoder;
use percent_encoding::percent_decode_str;
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{fs, io::AsyncWriteExt};

use crate::models::{DocumentPayload, Repository, RepositorySnapshot, Team, TocItem};

#[derive(Debug, Clone)]
pub struct Archive {
    root: PathBuf,
}

impl Archive {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn repository_dir(&self, team: &Team, repo: &Repository) -> PathBuf {
        self.root
            .join("teams")
            .join(stable_name(&team.name, &team.id))
            .join(stable_name(&repo.name, &repo.id))
    }

    pub async fn save_repository(&self, team: &Team, snapshot: &RepositorySnapshot) -> Result<()> {
        let root = self.repository_dir(team, &snapshot.repository);
        fs::create_dir_all(root.join("raw/docs")).await?;
        fs::create_dir_all(root.join("docs")).await?;
        fs::create_dir_all(root.join("assets")).await?;
        fs::create_dir_all(root.join("tables")).await?;
        fs::create_dir_all(root.join("diagrams")).await?;
        atomic_json(&root.join("raw/app-data.json"), &snapshot.raw_app_data).await?;
        atomic_json(&root.join("raw/toc.json"), &snapshot.toc).await?;
        atomic_json(&root.join("raw/repository.json"), &snapshot.repository).await?;
        let readme = format!(
            "# {}\n\n{}\n\n- 语雀命名空间：`{}`\n- 远端知识库 ID：`{}`\n",
            snapshot.repository.name,
            snapshot.repository.description.as_deref().unwrap_or(""),
            snapshot.repository.namespace,
            snapshot.repository.id
        );
        atomic_write(&root.join("README.md"), readme.as_bytes()).await?;
        self.save_summary(team, snapshot).await?;
        Ok(())
    }

    pub async fn save_document_raw(
        &self,
        team: &Team,
        repo: &Repository,
        doc: &DocumentPayload,
    ) -> Result<PathBuf> {
        let path = self
            .repository_dir(team, repo)
            .join("raw/docs")
            .join(format!("{}.json", safe_component(&doc.doc_id)));
        atomic_json(&path, &doc.raw).await?;
        if let Some(lake) = &doc.body_lake {
            atomic_write(&path.with_extension("lake"), lake.as_bytes()).await?;
        }
        if let Some(html) = &doc.body_html {
            atomic_write(&path.with_extension("html"), html.as_bytes()).await?;
        }
        let mut card_sources = Vec::new();
        if let Some(lake) = &doc.body_lake {
            card_sources.push(lake.as_str());
        }
        if let Some(html) = &doc.body_html {
            card_sources.push(html.as_str());
        }
        let cards = extract_lake_cards(&card_sources)?;
        if !cards.is_empty() {
            let card_path = self
                .repository_dir(team, repo)
                .join("diagrams")
                .join(safe_component(&doc.doc_id))
                .join("lake-cards.json");
            atomic_json(&card_path, &cards).await?;
        }
        Ok(path)
    }

    pub async fn save_markdown(
        &self,
        host: &str,
        team: &Team,
        snapshot: &RepositorySnapshot,
        toc: &TocItem,
        doc: &DocumentPayload,
        markdown: &str,
    ) -> Result<PathBuf> {
        let relative = document_relative_path(&snapshot.toc, toc);
        let path = self
            .repository_dir(team, &snapshot.repository)
            .join("docs")
            .join(relative);
        let source_url = format!(
            "{}/{}/{}",
            host.trim_end_matches('/'),
            snapshot.repository.namespace,
            doc.slug
        );
        let header = format!(
            "---\nyuque_doc_id: \"{}\"\nyuque_slug: \"{}\"\nyuque_source: \"{}\"\n---\n\n",
            escape_yaml(&doc.doc_id),
            escape_yaml(&doc.slug),
            escape_yaml(&source_url)
        );
        atomic_write(&path, format!("{header}{markdown}").as_bytes()).await?;
        Ok(path)
    }

    pub async fn save_sheet(
        &self,
        team: &Team,
        repo: &Repository,
        doc: &DocumentPayload,
    ) -> Result<Vec<PathBuf>> {
        let Some(sheet) = &doc.sheet else {
            return Ok(Vec::new());
        };
        let table_dir = self
            .repository_dir(team, repo)
            .join("tables")
            .join(safe_component(&doc.doc_id));
        fs::create_dir_all(&table_dir).await?;
        atomic_write(&table_dir.join("sheet.zlib"), &sheet_bytes(sheet)).await?;

        let decoded = decode_sheet(sheet).context("解压 Lake Sheet 失败")?;
        atomic_write(&table_dir.join("sheet.json"), decoded.as_bytes()).await?;
        let value: Value = serde_json::from_str(&decoded).context("Lake Sheet 不是有效 JSON")?;
        let mut files = Vec::new();
        if let Some(sheets) = value.as_array() {
            for (index, sheet) in sheets.iter().enumerate() {
                let name = sheet.get("name").and_then(Value::as_str).unwrap_or("Sheet");
                let path = table_dir.join(format!("{:02}-{}.csv", index + 1, safe_component(name)));
                write_sheet_csv(&path, sheet.get("data").unwrap_or(&Value::Null)).await?;
                files.push(path);
            }
        }
        Ok(files)
    }

    pub async fn save_diagrams(
        &self,
        team: &Team,
        repo: &Repository,
        doc: &DocumentPayload,
    ) -> Result<Vec<PathBuf>> {
        let Some(raw) = &doc.diagram_raw else {
            return Ok(Vec::new());
        };
        let extraction = extract_diagrams(raw);
        let diagram_dir = self
            .repository_dir(team, repo)
            .join("diagrams")
            .join(safe_component(&doc.doc_id));
        fs::create_dir_all(&diagram_dir).await?;

        let mut files = Vec::new();
        let request_path = diagram_dir.join("request-default.raw.json");
        atomic_json(&request_path, raw).await?;
        files.push(request_path);

        if !extraction.raw_diagrams.is_empty() {
            let raw_path = diagram_dir.join("diagram.raw.json");
            atomic_json(&raw_path, &extraction.raw_diagrams).await?;
            files.push(raw_path);

            let normalized_path = diagram_dir.join("diagram.normalized.json");
            atomic_json(&normalized_path, &extraction.normalized_diagrams).await?;
            files.push(normalized_path);
        }

        let report_path = diagram_dir.join("diagram-report.json");
        atomic_json(&report_path, &extraction.report()).await?;
        files.push(report_path);
        Ok(files)
    }

    pub fn asset_dir(&self, team: &Team, repo: &Repository, doc_id: &str) -> PathBuf {
        self.repository_dir(team, repo)
            .join("assets")
            .join(safe_component(doc_id))
    }

    pub fn document_path(
        &self,
        team: &Team,
        snapshot: &RepositorySnapshot,
        toc: &TocItem,
    ) -> PathBuf {
        self.repository_dir(team, &snapshot.repository)
            .join("docs")
            .join(document_relative_path(&snapshot.toc, toc))
    }

    pub async fn save_manifest(&self, host: &str, teams: &[Team]) -> Result<()> {
        let manifest = json!({
            "format_version": 1,
            "host": host,
            "generated_at": crate::state::now_epoch(),
            "teams": teams,
        });
        atomic_json(&self.root.join("manifest.json"), &manifest).await
    }

    async fn save_summary(&self, team: &Team, snapshot: &RepositorySnapshot) -> Result<()> {
        let mut lines = vec![format!("# {}", snapshot.repository.name), String::new()];
        for item in snapshot
            .toc
            .iter()
            .filter(|item| item.visible && item.slug.is_some())
        {
            let depth = ancestor_depth(&snapshot.toc, item);
            let link = document_relative_path(&snapshot.toc, item)
                .to_string_lossy()
                .replace('\\', "/");
            lines.push(format!(
                "{}- [{}](docs/{})",
                "  ".repeat(depth),
                item.title,
                link
            ));
        }
        let path = self
            .repository_dir(team, &snapshot.repository)
            .join("SUMMARY.md");
        atomic_write(&path, lines.join("\n").as_bytes()).await
    }
}

pub async fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    atomic_write(path, &bytes).await
}

pub async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let part = path.with_extension(format!(
        "{}part",
        path.extension()
            .and_then(|v| v.to_str())
            .map(|v| format!("{v}."))
            .unwrap_or_default()
    ));
    let mut file = fs::File::create(&part).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    fs::rename(&part, path).await?;
    Ok(())
}

pub fn content_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn safe_component(value: &str) -> String {
    let mut result: String = value
        .chars()
        .map(|c| {
            if matches!(
                c,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\n' | '\r'
            ) || c.is_control()
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    result = result.trim().trim_matches('.').to_string();
    if result.is_empty() {
        "untitled".into()
    } else {
        result.chars().take(120).collect()
    }
}

fn stable_name(name: &str, id: &str) -> String {
    format!("{}__{}", safe_component(name), safe_component(id))
}

fn document_relative_path(toc: &[TocItem], item: &TocItem) -> PathBuf {
    let map: HashMap<&str, &TocItem> = toc.iter().map(|v| (v.uuid.as_str(), v)).collect();
    let mut parents = Vec::new();
    let mut current = item.parent_uuid.as_deref();
    let mut guard = 0;
    while let Some(uuid) = current {
        if guard > toc.len() {
            break;
        }
        let Some(parent) = map.get(uuid) else { break };
        parents.push(stable_name(&parent.title, &parent.uuid));
        current = parent.parent_uuid.as_deref();
        guard += 1;
    }
    parents.reverse();
    let mut path = PathBuf::new();
    parents.into_iter().for_each(|part| path.push(part));
    let stable_id = if item.id.is_empty() {
        &item.uuid
    } else {
        &item.id
    };
    path.push(format!("{}.md", stable_name(&item.title, stable_id)));
    path
}

fn ancestor_depth(toc: &[TocItem], item: &TocItem) -> usize {
    let map: HashMap<&str, &TocItem> = toc.iter().map(|v| (v.uuid.as_str(), v)).collect();
    let mut depth = 0;
    let mut current = item.parent_uuid.as_deref();
    while let Some(uuid) = current {
        let Some(parent) = map.get(uuid) else {
            break;
        };
        depth += 1;
        if depth > toc.len() {
            break;
        }
        current = parent.parent_uuid.as_deref();
    }
    depth
}

fn sheet_bytes(sheet: &str) -> Vec<u8> {
    sheet.chars().map(|ch| (ch as u32 & 0xff) as u8).collect()
}

fn decode_sheet(sheet: &str) -> Result<String> {
    let bytes = sheet_bytes(sheet);
    let mut decoder = ZlibDecoder::new(bytes.as_slice());
    let mut decoded = String::new();
    decoder.read_to_string(&mut decoded)?;
    Ok(decoded)
}

async fn write_sheet_csv(path: &Path, data: &Value) -> Result<()> {
    let Some(rows) = data.as_object() else {
        return atomic_write(path, b"").await;
    };
    let row_max = rows
        .keys()
        .filter_map(|v| v.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    let col_max = rows
        .values()
        .filter_map(Value::as_object)
        .flat_map(|row| row.keys())
        .filter_map(|v| v.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    let mut writer = csv::Writer::from_writer(Vec::new());
    for row in 0..=row_max {
        let values = (0..=col_max).map(|col| {
            data.get(row.to_string())
                .and_then(|r| r.get(col.to_string()))
                .and_then(|c| c.get("v"))
                .map(cell_text)
                .unwrap_or_default()
        });
        writer.write_record(values)?;
    }
    let bytes = writer.into_inner()?;
    atomic_write(path, &bytes).await
}

fn cell_text(value: &Value) -> String {
    match value {
        Value::String(v) => v.clone(),
        Value::Number(v) => v.to_string(),
        Value::Bool(v) => v.to_string(),
        Value::Object(v) => v
            .get("text")
            .or_else(|| v.get("value"))
            .map(cell_text)
            .unwrap_or_else(|| value.to_string()),
        Value::Array(v) => v.iter().map(cell_text).collect::<Vec<_>>().join(","),
        Value::Null => String::new(),
    }
}

fn escape_yaml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn extract_lake_cards(sources: &[&str]) -> Result<Vec<Value>> {
    let tag_re = Regex::new(r#"<card\b[^>]*>"#)?;
    let name_re = Regex::new(r#"\bname=\"([^\"]+)\""#)?;
    let value_re = Regex::new(r#"\bvalue=\"data:([^\"]+)\""#)?;
    let mut cards = Vec::new();
    for source in sources {
        for tag in tag_re.find_iter(source) {
            let text = tag.as_str();
            let Some(encoded) = value_re.captures(text).and_then(|c| c.get(1)) else {
                continue;
            };
            let name = name_re
                .captures(text)
                .and_then(|c| c.get(1))
                .map(|v| v.as_str())
                .unwrap_or("unknown");
            let encoded = encoded
                .as_str()
                .replace("&quot;", "\"")
                .replace("&amp;", "&");
            let decoded = percent_decode_str(&encoded).decode_utf8_lossy();
            let data = serde_json::from_str::<Value>(&decoded)
                .unwrap_or_else(|_| Value::String(decoded.into_owned()));
            cards.push(json!({ "name": name, "data": data }));
        }
    }
    Ok(cards)
}

#[derive(Debug, Default)]
struct DiagramExtraction {
    raw_diagrams: Vec<Value>,
    normalized_diagrams: Vec<Value>,
    summaries: Vec<Value>,
    card_count: usize,
    parsed_card_count: usize,
    board_card_count: usize,
    image_card_count: usize,
    diagram_card_count: usize,
    card_names: BTreeMap<String, usize>,
}

impl DiagramExtraction {
    fn report(&self) -> Value {
        let recoverable = !self.raw_diagrams.is_empty();
        let reason = if recoverable {
            "diagramData found"
        } else if self.parsed_card_count > 0 {
            "parsed cards only; no diagramData"
        } else if self.card_count > 0 {
            "card tags found but no parseable data"
        } else {
            "no structured card data"
        };
        json!({
            "recoverable": recoverable,
            "reason": reason,
            "card_count": self.card_count,
            "parsed_card_count": self.parsed_card_count,
            "board_card_count": self.board_card_count,
            "image_card_count": self.image_card_count,
            "diagram_card_count": self.diagram_card_count,
            "card_names": self.card_names,
            "diagrams": self.summaries,
        })
    }
}

fn extract_diagrams(raw: &Value) -> DiagramExtraction {
    let mut extraction = DiagramExtraction::default();
    let mut seen_payloads = HashSet::new();
    let data = raw.get("data").unwrap_or(raw);

    if let Some(content) = data.get("content").and_then(Value::as_str) {
        if let Ok(value) = serde_json::from_str::<Value>(content) {
            if value.get("diagramData").is_some() {
                push_diagram(&mut extraction, "content.diagramData", value);
            }
        } else {
            collect_card_diagrams(content, "content.card", &mut extraction, &mut seen_payloads);
        }
    }

    for (source, text) in [
        (
            "content_html.card",
            data.get("content_html").and_then(Value::as_str),
        ),
        (
            "sourcecode.card",
            data.get("sourcecode").and_then(Value::as_str),
        ),
        (
            "body_lake.card",
            data.get("body_lake").and_then(Value::as_str),
        ),
        (
            "body_draft_lake.card",
            data.get("body_draft_lake").and_then(Value::as_str),
        ),
        (
            "body_html.card",
            data.get("body_html").and_then(Value::as_str),
        ),
    ]
    .into_iter()
    .filter_map(|(source, text)| text.map(|text| (source, text)))
    {
        collect_card_diagrams(text, source, &mut extraction, &mut seen_payloads);
    }

    extraction
}

fn collect_card_diagrams(
    text: &str,
    source: &str,
    extraction: &mut DiagramExtraction,
    seen_payloads: &mut HashSet<String>,
) {
    let Ok(tag_re) = Regex::new(r#"<card\b[^>]*>"#) else {
        return;
    };
    for tag in tag_re.find_iter(text) {
        extraction.card_count += 1;
        let tag = tag.as_str();
        let name = attr_value(tag, "name").unwrap_or_else(|| "unknown".into());
        *extraction.card_names.entry(name.clone()).or_default() += 1;
        if name == "board" {
            extraction.board_card_count += 1;
        } else if name == "image" {
            extraction.image_card_count += 1;
        }
        let Some(value) = attr_value(tag, "value") else {
            continue;
        };
        let Some(decoded) = decode_card_data(&value) else {
            continue;
        };
        if !seen_payloads.insert(decoded.clone()) {
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(&decoded) else {
            continue;
        };
        extraction.parsed_card_count += 1;
        if json.get("diagramData").is_some() {
            extraction.diagram_card_count += 1;
            push_diagram(extraction, source, json);
        }
    }
}

fn push_diagram(extraction: &mut DiagramExtraction, source: &str, value: Value) {
    let summary = diagram_summary(source, &value);
    let normalized = normalize_diagram(source, &value, &summary);
    extraction
        .raw_diagrams
        .push(json!({ "source": source, "data": value }));
    extraction.normalized_diagrams.push(normalized);
    extraction.summaries.push(summary);
}

fn diagram_summary(source: &str, value: &Value) -> Value {
    let diagram_data = value.get("diagramData").unwrap_or(value);
    let body = diagram_data.get("body").and_then(Value::as_array);
    let mut stats = DiagramStats::default();
    if let Some(nodes) = body {
        for node in nodes {
            collect_diagram_stats(node, 1, &mut stats);
        }
    }
    json!({
        "source": source,
        "body_count": body.map(Vec::len),
        "total_nodes": stats.total_nodes,
        "max_children": stats.max_children,
        "max_tree_depth": stats.max_tree_depth,
        "fold_like_keys": stats.fold_like_keys.into_iter().collect::<Vec<_>>(),
        "shape_counts": stats.shape_counts,
    })
}

#[derive(Debug, Default)]
struct DiagramStats {
    total_nodes: usize,
    max_children: usize,
    max_tree_depth: usize,
    fold_like_keys: BTreeMap<String, ()>,
    shape_counts: BTreeMap<String, usize>,
}

fn collect_diagram_stats(node: &Value, depth: usize, stats: &mut DiagramStats) {
    let Some(object) = node.as_object() else {
        return;
    };
    stats.total_nodes += 1;
    stats.max_tree_depth = stats.max_tree_depth.max(depth);
    for key in object.keys() {
        let lower = key.to_ascii_lowercase();
        if lower.contains("fold")
            || lower.contains("collapse")
            || lower.contains("expand")
            || lower.contains("visible")
            || lower.contains("hide")
        {
            stats.fold_like_keys.insert(key.clone(), ());
        }
    }
    if let Some(shape) = object.get("shape").and_then(Value::as_str) {
        *stats.shape_counts.entry(shape.to_string()).or_default() += 1;
    }
    let Some(children) = object.get("children").and_then(Value::as_array) else {
        return;
    };
    stats.max_children = stats.max_children.max(children.len());
    for child in children {
        collect_diagram_stats(child, depth + 1, stats);
    }
}

fn normalize_diagram(source: &str, value: &Value, summary: &Value) -> Value {
    let diagram_data = value.get("diagramData").unwrap_or(value);
    let roots = diagram_data
        .get("body")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(normalize_node).collect::<Vec<_>>())
        .unwrap_or_default();
    json!({
        "source": source,
        "format": value.get("format"),
        "type": value.get("type"),
        "version": value.get("version"),
        "summary": summary,
        "roots": roots,
    })
}

fn normalize_node(node: &Value) -> Value {
    let Some(object) = node.as_object() else {
        return Value::Null;
    };
    let mut output = serde_json::Map::new();
    for key in [
        "id",
        "shape",
        "html",
        "text",
        "name",
        "type",
        "layout",
        "treeEdge",
        "icons",
        "priority",
        "zIndex",
        "quadrant",
        "defaultContentStyle",
    ] {
        if let Some(value) = object.get(key) {
            output.insert(key.to_string(), value.clone());
        }
    }
    let children = object
        .get("children")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(normalize_node).collect::<Vec<_>>())
        .unwrap_or_default();
    output.insert("children".into(), Value::Array(children));
    Value::Object(output)
}

fn attr_value(tag: &str, name: &str) -> Option<String> {
    let escaped = regex::escape(name);
    let double = Regex::new(&format!(r#"\b{escaped}="([^"]*)""#)).ok()?;
    if let Some(value) = double.captures(tag).and_then(|captures| captures.get(1)) {
        return Some(value.as_str().to_string());
    }
    let single = Regex::new(&format!(r#"\b{escaped}='([^']*)'"#)).ok()?;
    single
        .captures(tag)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
}

fn decode_card_data(value: &str) -> Option<String> {
    let encoded = value.strip_prefix("data:")?;
    let encoded = encoded
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    Some(
        percent_decode_str(&encoded)
            .decode_utf8_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::ZlibEncoder};

    use super::*;

    #[test]
    fn safe_names_keep_readable_unicode() {
        assert_eq!(safe_component("研发/文档:一"), "研发_文档_一");
        assert_eq!(safe_component("..."), "untitled");
    }

    #[test]
    fn decodes_sheet_binary_string() {
        let input = r#"[{"name":"Sheet1","data":{"0":{"0":{"v":"A"}}}}]"#;
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input.as_bytes()).unwrap();
        let encoded = encoder.finish().unwrap();
        let binary_string: String = encoded.into_iter().map(char::from).collect();
        assert_eq!(decode_sheet(&binary_string).unwrap(), input);
    }

    #[test]
    fn extracts_card_payload() {
        let cards = extract_lake_cards(&[
            r#"<card type="inline" name="diagram" value="data:%7B%22id%22%3A1%7D">"#,
        ])
        .unwrap();
        assert_eq!(cards[0]["name"], "diagram");
        assert_eq!(cards[0]["data"]["id"], 1);
    }

    #[test]
    fn document_paths_are_stable_for_duplicate_titles() {
        let first = TocItem {
            id: "101".into(),
            uuid: "u1".into(),
            parent_uuid: None,
            title: "同名".into(),
            slug: Some("a".into()),
            item_type: "DOC".into(),
            visible: true,
            raw: Value::Null,
        };
        let mut second = first.clone();
        second.id = "102".into();
        second.uuid = "u2".into();
        second.slug = Some("b".into());
        let toc = vec![first.clone(), second.clone()];
        assert_ne!(
            document_relative_path(&toc, &first),
            document_relative_path(&toc, &second)
        );
    }
}
