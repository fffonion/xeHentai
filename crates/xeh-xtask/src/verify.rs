use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::manifest::{check_manifest, count_statuses, ManifestError};

#[derive(Debug)]
pub struct VerifyReport {
    pub implementation_accepted: u64,
    pub features: u64,
    pub message: String,
}

pub fn verify_deny_unverified(root: &Path) -> Result<(), String> {
    let path = root.join("compat/manifest.json");
    let text = fs::read_to_string(&path).map_err(|err| {
        format!(
            "fail-closed\nimplementation_accepted=0\nreason=manifest is absent ({err}); application behavior is pending"
        )
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|err| {
        format!("fail-closed\nimplementation_accepted=0\nreason=manifest is not valid JSON: {err}")
    })?;
    let counts = count_statuses(&value);
    let features = counts.get("features").and_then(Value::as_u64).unwrap_or(0);
    let accepted = counts
        .get("implementation_accepted")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let catalog = catalog_ids(root);
    let behavior = behavior_ids(root);
    let mut required = catalog;
    required.extend(behavior);
    let required_refs: Vec<&str> = required.iter().map(String::as_str).collect();
    let coverage = check_manifest(&text, &required_refs);
    let by_status = counts.get("by_status").cloned().unwrap_or(json!({}));
    let pending = features;
    let mut lines = vec![
        "fail-closed".to_string(),
        format!("implementation_accepted={accepted}"),
        format!("features={features}"),
        format!("pending={pending}"),
        format!("by_status={by_status}"),
        "reason=application behavior is pending; oracle characterization is not an implementation result".to_string(),
    ];
    if let Err(ManifestError { message }) = coverage {
        lines.push(format!("coverage={message}"));
    }
    if accepted != 0 || features == 0 {
        lines.push("reason=accepted count is not a completed application".to_string());
    }
    Err(lines.join("\n"))
}

fn catalog_ids(root: &Path) -> Vec<String> {
    read_id_list(root.join("compat/catalog.json"))
}

fn behavior_ids(root: &Path) -> Vec<String> {
    read_id_list(root.join("compat/required-behaviors.json"))
}

fn read_id_list(path: std::path::PathBuf) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    value
        .get("ids")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
