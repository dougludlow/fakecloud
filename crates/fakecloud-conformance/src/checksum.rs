//! Per-operation model checksums, as printed by `fakecloud-conformance
//! checksums`.
//!
//! The computation lives in `fakecloud-conformance-checksum`, shared with the
//! `#[test_action]` proc macro, so the CLI always prints exactly the value an
//! annotation needs to compile.

use std::path::Path;

pub use fakecloud_conformance_checksum::operation_checksum;

/// Checksums for every operation of one model.
#[derive(Debug)]
pub struct ModelChecksums {
    /// `aws-models/<model_key>.json`.
    pub model_key: String,
    /// The `service_name` from `service-map.json` (e.g. `monitoring`).
    pub service_name: String,
    /// `(operation name, checksum)`, sorted by operation name.
    pub operations: Vec<(String, String)>,
}

fn read_json(path: &Path) -> Result<serde_json::Value, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse {}: {}", path.display(), e))
}

/// Compute the checksum of every operation of every model listed in
/// `service-map.json`, sorted by service name.
pub fn model_checksums(models_dir: &Path) -> Result<Vec<ModelChecksums>, String> {
    let service_map = read_json(&models_dir.join("service-map.json"))?;
    let entries = service_map
        .as_object()
        .ok_or("service-map.json must be an object")?;
    let mut out = Vec::new();
    for (model_key, entry) in entries {
        let model_path = models_dir.join(format!("{}.json", model_key));
        if !model_path.exists() {
            eprintln!(
                "Warning: Model file not found for {}: {}",
                model_key,
                model_path.display()
            );
            continue;
        }
        let root = read_json(&model_path)?;
        let mut names: Vec<String> = fakecloud_conformance_checksum::operation_targets(&root)
            .iter()
            .map(|t| t.rsplit('#').next().unwrap_or(t).to_string())
            .collect();
        names.sort();
        names.dedup();
        let operations = names
            .into_iter()
            .map(|name| {
                let cs = operation_checksum(&root, &name)
                    .ok_or_else(|| format!("{model_key}: operation {name} not resolvable"))?;
                Ok((name, cs))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let service_name = entry
            .get("service_name")
            .and_then(|v| v.as_str())
            .unwrap_or(model_key)
            .to_string();
        out.push(ModelChecksums {
            model_key: model_key.clone(),
            service_name,
            operations,
        });
    }
    out.sort_by(|a, b| a.service_name.cmp(&b.service_name));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn models_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("aws-models")
    }

    fn sqs() -> serde_json::Value {
        read_json(&models_dir().join("sqs.json")).unwrap()
    }

    #[test]
    fn checksum_is_deterministic() {
        let root = sqs();
        let c1 = operation_checksum(&root, "CreateQueue").unwrap();
        let c2 = operation_checksum(&root, "CreateQueue").unwrap();
        assert_eq!(c1, c2);
        assert_eq!(c1.len(), 8);
    }

    #[test]
    fn different_operations_have_different_checksums() {
        let root = sqs();
        let c1 = operation_checksum(&root, "CreateQueue").unwrap();
        let c2 = operation_checksum(&root, "SendMessage").unwrap();
        assert_ne!(c1, c2);
    }
}
