//! Regression guard: every `test_action` checksum annotation in this crate's
//! tests must equal what `fakecloud-conformance checksums` prints for that
//! operation. The annotations compile, so they equal the proc macro's value;
//! this proves the CLI agrees with the macro on every annotated operation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use fakecloud_conformance::checksum::model_checksums;
use fakecloud_conformance_checksum::resolve_model_key;
use regex::Regex;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn cli_checksums_match_every_test_action_annotation() {
    let models_dir = crate_dir().join("..").join("..").join("aws-models");
    let service_map: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(models_dir.join("service-map.json")).unwrap(),
    )
    .unwrap();

    let by_key: HashMap<String, HashMap<String, String>> = model_checksums(&models_dir)
        .unwrap()
        .into_iter()
        .map(|m| (m.model_key, m.operations.into_iter().collect()))
        .collect();

    // Built with concat! so this file's own source does not look like an
    // annotation to the scan below.
    let marker = concat!("#[", "test_action(");
    let annotation = Regex::new(
        r#"#\[test_action\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,\s*checksum\s*=\s*"([^"]*)"\s*,?\s*\)\]"#,
    )
    .unwrap();

    let mut files = Vec::new();
    rust_files(&crate_dir().join("tests"), &mut files);
    files.sort();

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for file in &files {
        let src = std::fs::read_to_string(file).unwrap();
        let parsed = annotation.captures_iter(&src).count();
        // Attributes start a line; this skips mentions inside comments.
        let raw = src
            .lines()
            .filter(|l| l.trim_start().starts_with(marker))
            .count();
        assert_eq!(
            parsed,
            raw,
            "{}: {} test_action annotations but only {} parsed; extend the regex",
            file.display(),
            raw,
            parsed
        );
        for cap in annotation.captures_iter(&src) {
            let (service, action, annotated) = (&cap[1], &cap[2], &cap[3]);
            let cli = resolve_model_key(&service_map, service)
                .and_then(|key| by_key.get(&key))
                .and_then(|ops| ops.get(action));
            match cli {
                Some(cs) if cs == annotated => checked += 1,
                other => failures.push(format!(
                    "{}: {service}.{action} annotated {annotated}, CLI {other:?}",
                    file.display()
                )),
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} annotation(s) disagree with the CLI:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        checked > 1000,
        "only {checked} annotations found; scan broken?"
    );
}
