// SPDX-License-Identifier: GPL-3.0-or-later
use std::process::{Command, Output};

fn run_xtask(args: &[&str]) -> Output {
    let lease_tmp = std::env::var_os("XEH_LEASE_TMP").expect("XEH_LEASE_TMP");
    let python = std::env::var_os("XEH_PYTHON").expect("XEH_PYTHON");
    Command::new(env!("CARGO_BIN_EXE_xeh-xtask"))
        .args(args)
        .env("XEH_LEASE_TMP", lease_tmp)
        .env("XEH_PYTHON", python)
        .env_remove("XEH_EMIT_ROOT")
        .env_remove("XEH_EVIDENCE_DIR")
        .env_remove("XEH_CAPTURE_OUT")
        .output()
        .expect("run xeh-xtask")
}

#[test]
fn rules_suite_executes_rss_rule_cases() {
    let output = run_xtask(&["rss-test", "--suite", "rules"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "rules suite failed: stdout={stdout}\nstderr={stderr}"
    );
    let summary = stdout
        .lines()
        .find(|line| line.starts_with("rules-suite cases="))
        .expect("rules suite case count");
    let count = summary
        .split_whitespace()
        .find_map(|field| field.strip_prefix("cases="))
        .and_then(|value| value.parse::<usize>().ok())
        .expect("numeric rules case count");
    assert!(count > 0, "rules suite executed no cases: {summary}");
}

#[test]
fn site_diff_cli_executes_fixed_oracle_and_mutation_checks() {
    let output = run_xtask(&["diff", "--suite", "site"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "site diff failed: stdout={stdout}\\nstderr={stderr}"
    );
    assert!(
        stdout.contains("site-diff=pass"),
        "missing diff result: {stdout}"
    );
    assert!(stdout.contains("oracle_revision=43fc55c0c662f195d048becf6276a26ce06b09d9"));
    assert!(
        stdout.contains("negative=metadata:"),
        "mutation gate not reported: {stdout}"
    );
}
