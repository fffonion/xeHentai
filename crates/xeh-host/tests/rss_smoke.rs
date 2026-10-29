use std::process::Command;

use xeh_host::{app_main_path, run_app, run_entry};

const APP_RESULT: i64 = 22;

#[test]
fn app_entry_returns_the_rss_computed_score() {
    let source = std::fs::read_to_string(app_main_path()).expect("app entry");
    let types =
        std::fs::read_to_string(app_main_path().parent().expect("app dir").join("types.rss"))
            .expect("types module");
    assert!(
        types.contains("score_of({ score: 0, note: null })"),
        "an absent optional field is null; guest schemas reject omission"
    );
    assert!(source.contains("types::decoded_score"));
    assert!(types.contains("json::encode"));
    assert!(source.contains("re::match"));
    assert_eq!(run_app().expect("app entry"), APP_RESULT);
}

#[test]
fn runner_executes_the_requested_file() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/rss/smoke/three.rss");
    assert_eq!(run_entry(&path).expect("three.rss"), 3);
}

#[test]
fn host_binary_prints_the_rss_result() {
    let output = Command::new(env!("CARGO_BIN_EXE_xeh-host"))
        .output()
        .expect("xeh-host binary");
    assert!(
        output.status.success(),
        "binary failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout").trim(),
        APP_RESULT.to_string()
    );
}
