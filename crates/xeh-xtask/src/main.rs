use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use xeh_xtask::{
    capture, compare_trees, rss_test, run_diff, run_download_diff, run_download_suite,
    run_proxy_diff, run_proxy_suite, run_rules_suite, run_session_diff, run_session_suite,
    run_site_diff, run_site_suite, run_state_suite, run_suite, run_task_diff,
    verify_deny_unverified, workspace_root,
};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(args: &[String]) -> Result<(), String> {
    if args.len() >= 4 && args[0] == "oracle" && args[1] == "capture" && args[2] == "--suite" {
        return capture(&args[3]);
    }
    if args.len() >= 6
        && args[0] == "oracle"
        && args[1] == "compare"
        && args[2] == "--left"
        && args[4] == "--right"
    {
        return compare_trees(
            std::path::Path::new(&args[3]),
            std::path::Path::new(&args[5]),
        );
    }
    if args.len() == 3 && args[0] == "rss-test" && args[1] == "--suite" {
        return match args[2].as_str() {
            "config" => run_suite(),
            "state" => run_state_suite(),
            "session" => run_session_suite(),
            "site" => run_site_suite(),
            "rules" => run_rules_suite(),
            "proxy" => run_proxy_suite(),
            "download" => run_download_suite(),
            other => Err(format!("rss-test suite '{other}' is not implemented")),
        };
    }
    if args.len() == 3 && args[0] == "diff" && args[1] == "--suite" {
        return match args[2].as_str() {
            "config" => run_diff(),
            "task-api" => run_task_diff(),
            "session" => run_session_diff(),
            "site" => run_site_diff(),
            "proxy" => run_proxy_diff(),
            "download" => run_download_diff(),
            other => Err(format!("diff suite '{other}' is not implemented")),
        };
    }
    if args.len() == 1 && args[0] == "rss-test" {
        return rss_test();
    }
    if args.len() >= 2 && args[0] == "verify" && args[1] == "--all" {
        if !args.iter().any(|arg| arg == "--deny-unverified") {
            return Err("verify --all requires --deny-unverified in this scope".to_string());
        }
        return verify_deny_unverified(&workspace_root());
    }
    if args.len() >= 2 && args[0] == "manifest" && args[1] == "check" {
        let root = if args.get(2).map(String::as_str) == Some("--root") {
            PathBuf::from(args.get(3).ok_or("manifest check --root requires a path")?)
        } else {
            workspace_root()
        };
        return manifest_check(&root);
    }
    Err("usage: xeh-xtask oracle capture --suite <name> | oracle compare --left <dir> --right <dir> | verify --all --deny-unverified | manifest check [--root <dir>] | rss-test [--suite config|state|session|site|rules|proxy|download] | diff --suite config|task-api|session|site|proxy|download".to_string())
}

fn manifest_check(root: &std::path::Path) -> Result<(), String> {
    let manifest = std::fs::read_to_string(root.join("compat/manifest.json"))
        .map_err(|err| format!("manifest is absent: {err}"))?;
    let catalog = std::fs::read_to_string(root.join("compat/catalog.json"))
        .map_err(|err| format!("catalog is absent: {err}"))?;
    let behaviors = std::fs::read_to_string(root.join("compat/required-behaviors.json"))
        .map_err(|err| format!("required behaviors are absent: {err}"))?;
    let catalog_ids = ids_from(&catalog)?;
    let behavior_ids = ids_from(&behaviors)?;
    let mut required = catalog_ids;
    required.extend(behavior_ids);
    let refs: Vec<&str> = required.iter().map(String::as_str).collect();
    xeh_xtask::reject_unmapped(&manifest, &refs)?;
    xeh_xtask::check_coverage_claims(&manifest)?;
    println!("manifest-complete features={}", refs.len());
    Ok(())
}

fn ids_from(text: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| format!("id list is not JSON: {err}"))?;
    Ok(value
        .get("ids")
        .and_then(|item| item.as_array())
        .ok_or("id list is missing ids")?
        .iter()
        .filter_map(|item| item.as_str().map(str::to_string))
        .collect())
}
