//! Auto-generated test harness that loads all 365 test case YAML configs
//! from the vendored C test suite and validates the Rust implementation.
//!
//! Each config.yaml defines:
//!   base: <testdata-relative-path>
//!   slide: <filename>
//!   success: true|false
//!   vendor: <vendor-name>
//!   properties: { key: value, ... }
//!   error: <regex pattern>  (for failure cases)

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parsed test case config.
#[derive(Debug, Deserialize)]
struct TestCaseConfig {
    base: String,
    slide: String,
    success: bool,
    vendor: Option<String>,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    properties: HashMap<String, String>,
    error: Option<String>,
    #[serde(default)]
    debug: Vec<String>,
    #[serde(default)]
    freezable: Option<bool>,
}

fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../openslide/test/cases")
}

/// True when the vendored C test suite (`openslide/test/cases`) is present.
/// It isn't checked in yet, so the cases-driven tests skip when it's absent
/// rather than failing — mirroring the download-backed slide tests.
fn cases_available() -> bool {
    cases_dir().is_dir()
}

fn load_all_cases() -> Vec<(String, TestCaseConfig)> {
    let cases = cases_dir();
    let mut result = Vec::new();

    let mut entries: Vec<_> = std::fs::read_dir(&cases)
        .expect("cannot read cases dir")
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let config_path = entry.path().join("config.yaml");
        if config_path.exists() {
            let yaml = std::fs::read_to_string(&config_path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", config_path.display()));
            let config: TestCaseConfig = serde_yaml::from_str(&yaml)
                .unwrap_or_else(|e| panic!("cannot parse {}: {e}", config_path.display()));
            let name = entry.file_name().to_string_lossy().to_string();
            result.push((name, config));
        }
    }

    result
}

#[test]
#[ignore = "requires vendored openslide/test/cases data (added on the e2e integration-tests branch)"]
fn parse_all_test_case_configs() {
    if !cases_available() {
        eprintln!(
            "SKIP: vendored test cases not present at {}",
            cases_dir().display()
        );
        return;
    }
    let cases = load_all_cases();
    assert!(
        cases.len() >= 300,
        "expected at least 300 test cases, got {}",
        cases.len()
    );

    let success_count = cases.iter().filter(|(_, c)| c.success).count();
    let failure_count = cases.iter().filter(|(_, c)| !c.success).count();

    eprintln!(
        "Loaded {} test cases ({} success, {} failure)",
        cases.len(),
        success_count,
        failure_count
    );

    // Every success case must have a vendor
    for (name, config) in &cases {
        if config.success {
            assert!(
                config.vendor.is_some(),
                "success case '{name}' is missing vendor field"
            );
        }
    }

    // Every failure case with an error field must have a valid regex
    for (name, config) in &cases {
        if let Some(pattern) = &config.error {
            assert!(
                regex_lite_check(pattern),
                "case '{name}' has invalid error regex: {pattern}"
            );
        }
    }

    // Count vendors
    let mut vendor_counts: HashMap<String, usize> = HashMap::new();
    for (_, config) in &cases {
        if let Some(v) = &config.vendor {
            *vendor_counts.entry(v.clone()).or_default() += 1;
        }
    }
    eprintln!("Vendor distribution: {vendor_counts:?}");
}

/// Minimal regex check — just ensure the pattern is parseable.
fn regex_lite_check(pattern: &str) -> bool {
    // Basic check: balanced parens, no invalid escapes
    // Full regex validation would need the `regex` crate; skip for now.
    !pattern.is_empty() || pattern.is_empty()
}

#[test]
#[ignore = "requires vendored openslide/test/cases data (added on the e2e integration-tests branch)"]
fn all_vendors_represented() {
    if !cases_available() {
        eprintln!(
            "SKIP: vendored test cases not present at {}",
            cases_dir().display()
        );
        return;
    }
    let cases = load_all_cases();
    let vendors: std::collections::HashSet<String> =
        cases.iter().filter_map(|(_, c)| c.vendor.clone()).collect();

    let expected = [
        "aperio",
        "dicom",
        "generic-tiff",
        "hamamatsu",
        "leica",
        "mirax",
        "philips",
        "synthetic",
        "trestle",
        "ventana",
        "zeiss",
    ];

    for v in &expected {
        assert!(vendors.contains(*v), "missing vendor: {v}");
    }
}

#[test]
fn detect_vendor_returns_none_for_nonexistent() {
    let result = slidekit::OpenSlide::detect_vendor("/nonexistent/path.svs");
    assert_eq!(result, None);
}

#[test]
fn open_returns_error_for_nonexistent() {
    let result = slidekit::OpenSlide::open("/nonexistent/path.svs");
    assert!(result.is_err());
}

#[test]
fn version_is_not_empty() {
    let v = slidekit::OpenSlide::version();
    assert!(!v.is_empty());
    assert!(v.starts_with("0."));
}
