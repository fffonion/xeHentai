use serde_json::{Map, Value};

#[derive(Debug)]
pub struct ManifestError {
    pub message: String,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

const REQUIRED_FIELDS: &[&str] = &[
    "id", "source", "input", "expected", "target", "tests", "status",
];

const IMPLEMENTATION_SUCCESS: &[&str] = &["implemented", "accepted", "passing"];

pub fn feature_ids(manifest_json: &str) -> Result<Vec<String>, ManifestError> {
    let value = parse(manifest_json)?;
    Ok(features(&value)?
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_string))
        .collect())
}

pub fn reject_unmapped(manifest_json: &str, required_ids: &[&str]) -> Result<(), String> {
    check_manifest(manifest_json, required_ids).map_err(|err| err.message)
}

pub fn check_manifest(manifest_json: &str, required_ids: &[&str]) -> Result<(), ManifestError> {
    let value = parse(manifest_json)?;
    let items = features(&value)?;
    let mut present = Vec::new();
    for item in items {
        let id = require_fields(item)?;
        present.push(id);
    }
    let mut missing = Vec::new();
    for required in required_ids {
        if !present.iter().any(|id| id == required) {
            missing.push((*required).to_string());
        }
    }
    if !missing.is_empty() {
        return Err(ManifestError {
            message: format!(
                "manifest is incomplete: missing required feature {}",
                missing.join(", ")
            ),
        });
    }
    Ok(())
}

fn parse(manifest_json: &str) -> Result<Value, ManifestError> {
    serde_json::from_str(manifest_json).map_err(|err| ManifestError {
        message: format!("manifest is not valid JSON: {err}"),
    })
}

fn features(value: &Value) -> Result<&Vec<Value>, ManifestError> {
    value
        .get("features")
        .and_then(Value::as_array)
        .ok_or_else(|| ManifestError {
            message: "manifest is missing the features array".to_string(),
        })
}

fn require_fields(item: &Value) -> Result<String, ManifestError> {
    let Some(object) = item.as_object() else {
        return Err(ManifestError {
            message: "manifest feature must be an object".to_string(),
        });
    };
    for field in REQUIRED_FIELDS {
        if !object.contains_key(*field) {
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<missing id>");
            return Err(ManifestError {
                message: format!("feature {id} is missing field {field}"),
            });
        }
    }
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ManifestError {
            message: "feature id must be a string".to_string(),
        })?
        .to_string();
    if id.is_empty() {
        return Err(ManifestError {
            message: "feature id must not be empty".to_string(),
        });
    }
    let status = object
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| ManifestError {
            message: format!("feature {id} status must be a string"),
        })?;
    if IMPLEMENTATION_SUCCESS.contains(&status) {
        return Err(ManifestError {
            message: format!(
                "feature {id} claims status {status}; application implementation is pending and cannot be marked accepted"
            ),
        });
    }
    if object.get("tests").and_then(Value::as_array).is_none() {
        return Err(ManifestError {
            message: format!("feature {id} tests must be an array"),
        });
    }
    Ok(id)
}

pub fn check_coverage_claims(manifest_json: &str) -> Result<(), String> {
    let value: Value = serde_json::from_str(manifest_json)
        .map_err(|err| format!("manifest is not valid JSON: {err}"))?;
    let features = value
        .get("features")
        .and_then(Value::as_array)
        .ok_or("manifest is missing the features array")?;
    let mut by_id = std::collections::BTreeMap::new();
    for item in features {
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            by_id.insert(id.to_string(), item);
        }
    }
    for (id, item) in &by_id {
        if id.starts_with("cli.option.")
            && item.get("status").and_then(Value::as_str) == Some("oracle_characterized")
        {
            let executed = item
                .pointer("/expected/observed/executed")
                .and_then(Value::as_bool);
            if executed != Some(true) {
                return Err(format!("{id} is characterized without execution evidence"));
            }
        }
    }
    if let Some(parent) = by_id.get("behavior.sec4.rpc-methods") {
        let method_ids: Vec<String> = by_id
            .keys()
            .filter(|id| id.starts_with("rpc.method."))
            .cloned()
            .collect();
        let characterized = method_ids
            .iter()
            .filter(|id| {
                by_id[id.as_str()].get("status").and_then(Value::as_str)
                    == Some("oracle_characterized")
            })
            .count();
        let failed = method_ids
            .iter()
            .filter(|id| {
                by_id[id.as_str()].get("status").and_then(Value::as_str) == Some("oracle_failed")
            })
            .count();
        let claimed = parent
            .pointer("/expected/observed/methods_characterized")
            .and_then(Value::as_u64);
        if claimed != Some(characterized as u64) {
            return Err(format!(
                "rpc methods_characterized {claimed:?} does not match derived {characterized}"
            ));
        }
        let claimed_failed = parent
            .pointer("/expected/observed/methods_failed")
            .and_then(Value::as_u64);
        if claimed_failed != Some(failed as u64) {
            return Err(format!(
                "rpc methods_failed {claimed_failed:?} does not match derived {failed}"
            ));
        }
        if parent.get("status").and_then(Value::as_str) == Some("oracle_characterized")
            && (failed > 0 || characterized != method_ids.len())
        {
            return Err("rpc parent is characterized while method coverage is partial".to_string());
        }
    }
    if let Some(parent) = by_id.get("behavior.sec4.proxy-schemes") {
        for scheme in ["http", "https", "socks"] {
            let id = format!("behavior.sec4.proxy-schemes.{scheme}");
            let child = by_id.get(&id).ok_or_else(|| format!("missing {id}"))?;
            if parent.get("status").and_then(Value::as_str) == Some("oracle_characterized")
                && child.get("status").and_then(Value::as_str) != Some("oracle_characterized")
            {
                return Err(format!("proxy parent is characterized while {id} is not"));
            }
        }
    }
    Ok(())
}

pub fn count_statuses(manifest: &Value) -> Map<String, Value> {
    let mut counts = Map::new();
    let items = manifest
        .get("features")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    counts.insert("features".to_string(), Value::from(items.len()));
    let mut by_status = Map::new();
    for item in items {
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("missing")
            .to_string();
        let entry = by_status.entry(status).or_insert(Value::from(0));
        *entry = Value::from(entry.as_u64().unwrap_or(0) + 1);
    }
    counts.insert("by_status".to_string(), Value::Object(by_status));
    counts.insert("implementation_accepted".to_string(), Value::from(0_u64));
    counts
}
