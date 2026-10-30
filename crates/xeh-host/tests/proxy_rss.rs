use serde_json::{json, Value};

const AUTH: &str = "{\"allowed_schemes\":[\"http\"],\"allowed_hosts\":[\"127.0.0.1\"],\"allowed_ports\":[9],\"allow_private_ips\":true}";

fn run(request: Value) -> Value {
    let text = serde_json::to_string(&request).expect("request");
    let raw = xeh_host::run_proxy_scenario(std::path::Path::new("."), &text, AUTH)
        .expect("proxy scenario");
    serde_json::from_str(&raw).expect("report")
}

#[test]
fn pool_sequence_runs_on_the_retained_store_owner() {
    let report = run(json!({
        "operation": "pool",
        "steps": [
            {"op": "proxy_add", "address": "http://a.invalid:1"},
            {"op": "proxy_add", "address": "http://b.invalid:2"},
            {"op": "proxy_select", "now": 100.0},
            {"op": "proxy_not_good", "address": "http://a.invalid:1", "weight": 5, "expire": 0.0, "now": 100.0},
            {"op": "proxy_select", "now": 200.0},
            {"op": "proxy_has"}
        ]
    }));
    println!("{}", serde_json::to_string_pretty(&report).expect("pretty"));
    let steps = report["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 6);
    assert_eq!(steps[0]["proxy"]["order"], json!(["http://a.invalid:1"]));
    let selected: Value =
        serde_json::from_str(steps[2]["value"].as_str().expect("value")).expect("selected JSON");
    assert_eq!(selected["address"], json!("http://a.invalid:1"));
    assert_eq!(selected["route"]["kind"], json!("http"));
    assert_eq!(selected["route"]["host"], json!("a.invalid"));
    assert_eq!(selected["route"]["port"], json!(1));
    assert_eq!(selected["route"]["trusted_remote_dns"], json!(true));
    assert_eq!(
        steps[3]["proxy"]["disabled"]["http://a.invalid:1"],
        json!(100.0)
    );
    assert_eq!(steps[4]["error"], json!("RuntimeError"));
    assert_eq!(steps[4]["proxy"]["disabled"], json!({}));
    assert_eq!(
        steps[5]["proxy"]["order"],
        json!(["http://a.invalid:1", "http://b.invalid:2"])
    );
}

#[test]
fn policy_matches_source_predicates() {
    let report = run(json!({
        "operation": "policy",
        "config": {"proxy_image": false, "proxy_image_only": true},
        "urls": ["https://e-hentai.org/g/1/x/", "https://example.invalid/x", ""]
    }));
    println!("{}", serde_json::to_string_pretty(&report).expect("pretty"));
    let rows = report["urls"].as_array().expect("rows");
    assert_eq!(rows[0]["image_only"], json!(false));
    assert_eq!(rows[1]["image_only"], json!(true));
    assert_eq!(rows[2]["image_only"], json!(true));
}

#[test]
fn glyph_mkurl_and_cookie_flow_compiles_and_runs() {
    let report = run(json!({
        "operation": "glype",
        "address": "http://127.0.0.1:9/browse.php?u=x&b=4",
        "session_headers": [{"name": "User-Agent", "value": "xeh-fixture/1"}],
        "targets": []
    }));
    println!("{}", serde_json::to_string_pretty(&report).expect("pretty"));
    assert_eq!(report["operation"], json!("glype"));
}
