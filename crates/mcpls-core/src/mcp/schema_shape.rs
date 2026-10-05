//! Shaping of the tool schemas advertised by `tools/list`.
//!
//! Each tool's `inputSchema` and `outputSchema` must be self-contained, so the
//! schema generator inlines every referenced definition into every tool, and
//! it derives `description` text from rustdoc, including `# Examples` code.
//! [`shape_tool_schemas`] runs once on the assembled router and shortens that
//! text: rustdoc sections and code fences are cut, a description keeps its
//! first paragraph up to a byte cap, and a definition that several tools carry
//! keeps only a one-line form. It changes `description` annotations only, so
//! property names, types, `required` lists and `$ref`s are untouched.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rmcp::model::{JsonObject, Tool};
use schemars::Schema;
use schemars::transform::{RecursiveTransform, Transform};
use serde_json::Value;

const SCHEMA_DESCRIPTION_CAP: DescriptionCap = DescriptionCap::truncating(100);

/// A definition several tools carry keeps its own one-line description at most
/// this long, whole or not at all.
const SHARED_DEFINITION_DESCRIPTION_CAP: DescriptionCap = DescriptionCap::dropping(80);

/// Descriptions nested inside a shared definition (fields, variants) are
/// repeated in every tool, so only short ones survive, whole.
const SHARED_NESTED_DESCRIPTION_CAP: DescriptionCap = DescriptionCap::dropping(24);

const DEFINITIONS_KEY: &str = "$defs";

/// What happens to a first paragraph longer than the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overflow {
    /// Cut at the last sentence end, else the last word boundary.
    Truncate,
    /// Remove the description; a cut one-liner would mislead more than none.
    Drop,
}

/// Byte ceiling for one schema description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DescriptionCap {
    bytes: usize,
    overflow: Overflow,
}

impl DescriptionCap {
    const fn truncating(bytes: usize) -> Self {
        Self {
            bytes,
            overflow: Overflow::Truncate,
        }
    }

    const fn dropping(bytes: usize) -> Self {
        Self {
            bytes,
            overflow: Overflow::Drop,
        }
    }

    /// The first paragraph of `text` without rustdoc sections, cut to the cap
    /// at the last sentence end, else the last word boundary, never inside a
    /// backtick span; or nothing, for a cap that drops.
    fn shorten(self, text: &str) -> String {
        let paragraph = first_paragraph(without_rustdoc_sections(text));
        if paragraph.len() <= self.bytes {
            return paragraph.to_owned();
        }
        if self.overflow == Overflow::Drop {
            return String::new();
        }
        let window = &paragraph[..paragraph.floor_char_boundary(self.bytes)];
        let cut = window
            .rmatch_indices(['.', '!', '?'])
            .map(|(index, mark)| index.saturating_add(mark.len()))
            .find(|end| paragraph[*end..].starts_with(char::is_whitespace))
            .or_else(|| window.rfind(char::is_whitespace))
            .unwrap_or(window.len());
        let mut shortened = paragraph[..cut].trim_end();
        while shortened.matches('`').count() % 2 == 1 {
            shortened = shortened[..shortened.rfind('`').unwrap_or(0)].trim_end();
        }
        shortened.to_owned()
    }
}

/// `text` up to the first line that opens a rustdoc section (`# Examples`,
/// `# Errors`, ...) or a code fence.
pub(super) fn without_rustdoc_sections(text: &str) -> &str {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with('#') || line.starts_with("```") {
            return text[..offset].trim_end();
        }
        offset = offset.saturating_add(line.len());
    }
    text.trim_end()
}

fn first_paragraph(text: &str) -> &str {
    text.split("\n\n").next().unwrap_or(text).trim()
}

/// Replaces every string `description` with its shortened form, dropping it
/// when nothing is left.
struct ShortenDescriptions(DescriptionCap);

impl Transform for ShortenDescriptions {
    fn transform(&mut self, schema: &mut Schema) {
        let Some(object) = schema.as_object_mut() else {
            return;
        };
        let Some(Value::String(description)) = object.get("description") else {
            return;
        };
        let shortened = self.0.shorten(description);
        if shortened.is_empty() {
            object.remove("description");
        } else {
            object.insert("description".to_owned(), Value::String(shortened));
        }
    }
}

/// Definition name to the number of tools whose input or output schema
/// carries it.
struct DefinitionUsage(BTreeMap<String, usize>);

impl DefinitionUsage {
    fn count(tools: &[&mut Tool]) -> Self {
        let mut usage = BTreeMap::new();
        for tool in tools {
            let names: BTreeSet<&String> = schemas_of(tool)
                .filter_map(|schema| schema.get(DEFINITIONS_KEY)?.as_object())
                .flat_map(|definitions| definitions.keys())
                .collect();
            for name in names {
                let tools = usage.entry(name.clone()).or_insert(0_usize);
                *tools = tools.saturating_add(1);
            }
        }
        Self(usage)
    }

    fn is_shared(&self, name: &str) -> bool {
        self.0.get(name).is_some_and(|tools| *tools >= 2)
    }
}

fn schemas_of(tool: &Tool) -> impl Iterator<Item = &JsonObject> {
    std::iter::once(&*tool.input_schema).chain(tool.output_schema.as_deref())
}

/// Shortens the descriptions in the input and output schemas of every tool.
///
/// Counts first which definitions several tools carry, then shapes each
/// schema: definitions shared by two or more tools get the one-line cap, the
/// rest of the schema the regular cap.
pub(super) fn shape_tool_schemas<'a>(tools: impl IntoIterator<Item = &'a mut Tool>) {
    let mut tools: Vec<&mut Tool> = tools.into_iter().collect();
    let usage = DefinitionUsage::count(&tools);
    for tool in &mut tools {
        shape_schema(Arc::make_mut(&mut tool.input_schema), &usage);
        if let Some(output) = tool.output_schema.as_mut() {
            shape_schema(Arc::make_mut(output), &usage);
        }
    }
}

fn shape_schema(object: &mut JsonObject, usage: &DefinitionUsage) {
    let definitions = object.remove(DEFINITIONS_KEY);
    let mut root = Schema::from(std::mem::take(object));
    RecursiveTransform(ShortenDescriptions(SCHEMA_DESCRIPTION_CAP)).transform(&mut root);
    if let Value::Object(shaped) = Value::from(root) {
        *object = shaped;
    }
    let Some(Value::Object(definitions)) = definitions else {
        return;
    };
    let shaped = definitions
        .into_iter()
        .map(|(name, definition)| {
            let mut schema = Schema::try_from(definition).unwrap_or_default();
            if usage.is_shared(&name) {
                shape_shared_definition(&mut schema);
            } else {
                RecursiveTransform(ShortenDescriptions(SCHEMA_DESCRIPTION_CAP))
                    .transform(&mut schema);
            }
            (name, Value::from(schema))
        })
        .collect();
    object.insert(DEFINITIONS_KEY.to_owned(), Value::Object(shaped));
}

fn shape_shared_definition(definition: &mut Schema) {
    let own = definition
        .as_object()
        .and_then(|object| object.get("description")?.as_str())
        .map(|text| SHARED_DEFINITION_DESCRIPTION_CAP.shorten(text))
        .filter(|text| !text.is_empty());
    RecursiveTransform(ShortenDescriptions(SHARED_NESTED_DESCRIPTION_CAP)).transform(definition);
    if let Some(object) = definition.as_object_mut() {
        match own {
            Some(text) => object.insert("description".to_owned(), Value::String(text)),
            None => object.remove("description"),
        };
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_rustdoc_sections_and_fences_are_cut() {
        let text = "Summary line.\n\nMore.\n\n# Examples\n\n```\nlet a = 1;\n```\n";
        assert_eq!(without_rustdoc_sections(text), "Summary line.\n\nMore.");
        assert_eq!(without_rustdoc_sections("a\n```\ncode\n```"), "a");
        assert_eq!(without_rustdoc_sections("plain"), "plain");
    }

    #[test]
    fn test_shorten_keeps_first_paragraph() {
        let cap = DescriptionCap::truncating(100);
        assert_eq!(cap.shorten("First.\n\nSecond paragraph."), "First.");
        assert_eq!(cap.shorten("Short"), "Short");
    }

    #[test]
    fn test_shorten_cuts_at_a_sentence_end_within_the_cap() {
        let cap = DescriptionCap::truncating(40);
        let text = "One sentence here. Another sentence that runs well past the cap.";
        assert_eq!(cap.shorten(text), "One sentence here.");
    }

    #[test]
    fn test_shorten_cuts_at_a_word_boundary_without_a_sentence_end() {
        let cap = DescriptionCap::truncating(12);
        assert_eq!(cap.shorten("alpha beta gamma delta"), "alpha beta");
    }

    #[test]
    fn test_shorten_never_leaves_an_open_backtick_span() {
        let cap = DescriptionCap::truncating(20);
        let shortened = cap.shorten("Use the `positions_degraded` field when present");
        assert_eq!(shortened.matches('`').count() % 2, 0, "{shortened}");
    }

    #[test]
    fn test_shorten_never_splits_a_multibyte_character() {
        let cap = DescriptionCap::truncating(5);
        assert_eq!(cap.shorten("ééééé ééééé"), "éé");
    }

    #[test]
    fn test_shorten_is_idempotent() {
        let cap = DescriptionCap::truncating(30);
        let once = cap.shorten("A fairly long description. With a second sentence in it.");
        assert_eq!(cap.shorten(&once), once);
    }

    #[test]
    fn test_property_named_description_is_not_treated_as_text() {
        let mut schema = Schema::try_from(serde_json::json!({
            "properties": {"description": {"type": "string", "description": "x\n\nignored"}}
        }))
        .unwrap();
        RecursiveTransform(ShortenDescriptions(SCHEMA_DESCRIPTION_CAP)).transform(&mut schema);
        assert_eq!(
            Value::from(schema),
            serde_json::json!({
                "properties": {"description": {"type": "string", "description": "x"}}
            })
        );
    }

    #[test]
    fn test_shared_definitions_keep_a_short_description_whole_or_drop_it() {
        let short = "A short summary.";
        let long = "A definition summary that is longer than the shared one-line cap allows it to be, so it goes.";
        let definition = |own: &str| {
            serde_json::json!({
                "type": "object",
                "description": own,
                "properties": {"a": {"type": "string", "description": "A nested field description that is long."}}
            })
        };
        let tool = |name: &str, defs: Value| {
            let mut object = JsonObject::new();
            object.insert("$defs".into(), defs);
            Tool::new(name.to_owned(), "d", Arc::new(object))
        };
        let mut tools = [
            tool(
                "a",
                serde_json::json!({"Short": definition(short), "Long": definition(long), "Own": definition(long)}),
            ),
            tool(
                "b",
                serde_json::json!({"Short": definition(short), "Long": definition(long)}),
            ),
        ];
        shape_tool_schemas(tools.iter_mut());

        let defs = &tools[0].input_schema["$defs"];
        assert_eq!(defs["Short"]["description"], short);
        assert!(defs["Long"].get("description").is_none());
        assert!(
            defs["Short"]["properties"]["a"]
                .get("description")
                .is_none()
        );
        assert!(defs["Own"]["description"].as_str().unwrap().len() <= SCHEMA_DESCRIPTION_CAP.bytes);
        assert!(defs["Own"]["properties"]["a"].get("description").is_some());
    }

    use super::super::server::McplsServer;

    /// Total serialized `tools` array budget (compact bytes).
    const TOOLS_LIST_TOTAL_BUDGET_BYTES: usize = 130_000;
    /// Largest serialized single `Tool` (compact bytes).
    const TOOL_BUDGET_BYTES: usize = 9_000;
    /// Sum of all schema `description` bytes.
    const SCHEMA_DESCRIPTION_BUDGET_BYTES: usize = 35_000;

    fn compact_len(value: &impl serde::Serialize) -> usize {
        serde_json::to_vec(value).unwrap().len()
    }

    fn for_each_description(value: &Value, path: &str, visit: &mut impl FnMut(&str, &str)) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let child_path = format!("{path}/{key}");
                    match (key.as_str(), child) {
                        ("description", Value::String(text)) => visit(&child_path, text),
                        _ => for_each_description(child, &child_path, visit),
                    }
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    for_each_description(item, &format!("{path}/{index}"), visit);
                }
            }
            _ => {}
        }
    }

    fn schema_description_bytes(tools: &[Tool]) -> usize {
        let mut total = 0;
        for tool in tools {
            for schema in schemas_of(tool) {
                for_each_description(&Value::Object(schema.clone()), "", &mut |_, text| {
                    total += text.len();
                });
            }
        }
        total
    }

    /// Prints the per-tool and total sizes; run with
    /// `cargo test -p mcpls-core --lib measure_tools_list -- --ignored --nocapture`.
    #[test]
    #[ignore = "run manually to measure the tools/list payload"]
    fn measure_tools_list() {
        for (label, tools) in [
            ("unshaped", McplsServer::unshaped_tool_router().list_all()),
            ("shaped", McplsServer::build_tool_router(None).list_all()),
        ] {
            let mut rows: Vec<(String, usize)> = tools
                .iter()
                .map(|tool| (tool.name.to_string(), compact_len(tool)))
                .collect();
            rows.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
            println!("== {label}: total {} B", compact_len(&tools));
            println!(
                "   schema descriptions {} B",
                schema_description_bytes(&tools)
            );
            for (name, bytes) in rows.iter().take(6) {
                println!("   {name}: {bytes} B");
            }
        }
    }

    fn shaped_tools() -> Vec<Tool> {
        McplsServer::build_tool_router(None).list_all()
    }

    #[test]
    fn test_tools_list_stays_within_the_size_budget() {
        let tools = shaped_tools();
        let total = compact_len(&tools);
        let mut over: Vec<String> = tools
            .iter()
            .filter(|tool| compact_len(tool) > TOOL_BUDGET_BYTES)
            .map(|tool| {
                format!(
                    "{}: {} B (permitted {TOOL_BUDGET_BYTES} B)",
                    tool.name,
                    compact_len(tool)
                )
            })
            .collect();
        if total > TOOLS_LIST_TOTAL_BUDGET_BYTES {
            over.push(format!(
                "total: {total} B (permitted {TOOLS_LIST_TOTAL_BUDGET_BYTES} B)"
            ));
        }
        let descriptions = schema_description_bytes(&tools);
        if descriptions > SCHEMA_DESCRIPTION_BUDGET_BYTES {
            over.push(format!(
                "schema descriptions: {descriptions} B (permitted {SCHEMA_DESCRIPTION_BUDGET_BYTES} B)"
            ));
        }
        assert!(
            over.is_empty(),
            "tools/list budget exceeded:\n{}",
            over.join("\n")
        );
    }

    #[test]
    fn test_no_schema_description_embeds_rustdoc() {
        for tool in shaped_tools() {
            for schema in schemas_of(&tool) {
                for_each_description(&Value::Object(schema.clone()), "", &mut |path, text| {
                    let kept = without_rustdoc_sections(text);
                    assert_eq!(
                        kept.len(),
                        text.trim_end().len(),
                        "{}{path}: rustdoc section or code fence in {text:?}",
                        tool.name
                    );
                });
            }
        }
    }

    fn collect_refs(value: &Value, refs: &mut Vec<String>, ids: &mut usize) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    match (key.as_str(), child) {
                        ("$ref", Value::String(target)) => refs.push(target.clone()),
                        ("$id", Value::String(_)) => *ids += 1,
                        _ => collect_refs(child, refs, ids),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| collect_refs(item, refs, ids)),
            _ => {}
        }
    }

    #[test]
    fn test_every_ref_resolves_inside_its_own_schema_document() {
        for tool in shaped_tools() {
            for schema in schemas_of(&tool) {
                let (mut refs, mut ids) = (Vec::new(), 0);
                collect_refs(&Value::Object(schema.clone()), &mut refs, &mut ids);
                assert_eq!(ids, 0, "{}: `$id` in a tool schema", tool.name);
                let definitions = schema.get(DEFINITIONS_KEY).and_then(Value::as_object);
                for target in refs {
                    let name = target.strip_prefix("#/$defs/").unwrap_or_else(|| {
                        panic!(
                            "{}: `$ref` {target} is not a same-document `$defs` pointer",
                            tool.name
                        )
                    });
                    assert!(
                        definitions.is_some_and(|defs| defs.contains_key(name)),
                        "{}: `$ref` {target} does not resolve",
                        tool.name
                    );
                }
            }
        }
    }

    /// The schema with every string `description` and `title` removed: what
    /// the shaping must leave untouched.
    fn without_annotations(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(key, child)| {
                        !(matches!(key.as_str(), "description" | "title") && child.is_string())
                    })
                    .map(|(key, child)| (key.clone(), without_annotations(child)))
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(without_annotations).collect()),
            other => other.clone(),
        }
    }

    #[test]
    fn test_shaping_changes_only_description_annotations() {
        let unshaped = McplsServer::unshaped_tool_router().list_all();
        let shaped = shaped_tools();
        assert_eq!(unshaped.len(), shaped.len());
        for (before, after) in unshaped.iter().zip(&shaped) {
            assert_eq!(before.name, after.name);
            assert_eq!(before.description, after.description, "{}", before.name);
            for (old, new) in schemas_of(before).zip(schemas_of(after)) {
                assert_eq!(
                    without_annotations(&Value::Object(old.clone())),
                    without_annotations(&Value::Object(new.clone())),
                    "{}: structure changed",
                    before.name
                );
            }
            assert_eq!(
                before.output_schema.is_some(),
                after.output_schema.is_some(),
                "{}",
                before.name
            );
        }
    }
}
