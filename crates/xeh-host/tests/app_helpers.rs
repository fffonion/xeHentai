// SPDX-License-Identifier: GPL-3.0-or-later
//! Focused coverage for the two application-owned typed host helpers.
//!
//! `xeh_json::is_valid` and `xeh_io::try_write_text` are installed by this
//! application's catalog/registry; the pinned Core only provides the underlying
//! strict `json::decode` adapter and the generic `IoPolicy`/async host bridge.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use vm::{HostStructField, HostTypeSchema, IoHostExt, IoPolicy, Value, Vm};
use xeh_host::{
    attach_timer, bind_vm, compile_source_with_catalog, drive_to_halt, open_registry,
    resource_count, standard_catalog, EmbeddingTimer,
};

struct TestRoot(PathBuf);

impl TestRoot {
    fn create() -> Self {
        let base = PathBuf::from(std::env::var_os("XEH_LEASE_TMP").expect("XEH_LEASE_TMP"));
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = base.join(format!("app-helpers-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&path)
            .unwrap_or_else(|error| panic!("create test root {}: {error}", path.display()));
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn prepare_vm(source: &str) -> Vm {
    let catalog = standard_catalog();
    let program = compile_source_with_catalog(source, Arc::clone(&catalog))
        .unwrap_or_else(|error| panic!("compile guest: {error}\n--- source ---\n{source}"));
    let registry = open_registry(&catalog).expect("application registry");
    let mut vm = Vm::try_new(program).expect("vm");
    bind_vm(&mut vm, &registry).expect("bind");
    attach_timer(&mut vm, Arc::new(EmbeddingTimer::new()));
    vm.set_default_host_fallback_enabled(false);
    vm
}

fn configure_policy(vm: &mut Vm, root: &Path, allow_write: bool, max_write_bytes: usize) {
    vm.configure_io(IoPolicy {
        allowed_roots: vec![root.display().to_string()],
        allow_write,
        allow_process: false,
        max_read_bytes: 1024 * 1024,
        max_write_bytes,
    });
}

async fn drive(vm: &mut Vm) -> Value {
    drive_to_halt(vm)
        .await
        .unwrap_or_else(|error| panic!("drive guest: {error}"));
    vm.stack()
        .last()
        .cloned()
        .unwrap_or_else(|| panic!("guest left an empty stack"))
}

fn map_field<'a>(value: &'a Value, key: &str) -> &'a Value {
    let Value::Map(map) = value else {
        panic!("expected the guest map, got {value:?}");
    };
    map.get(&Value::string(key))
        .unwrap_or_else(|| panic!("guest map is missing '{key}': {value:?}"))
}

fn bool_field(value: &Value, key: &str) -> bool {
    match map_field(value, key) {
        Value::Bool(flag) => *flag,
        other => panic!("'{key}' is {other:?}, not a bool"),
    }
}

fn string_field(value: &Value, key: &str) -> String {
    match map_field(value, key) {
        Value::String(text) => text.as_str().to_string(),
        other => panic!("'{key}' is {other:?}, not a string"),
    }
}

fn assert_quiescent(vm: &mut Vm) {
    assert_eq!(resource_count(vm), 0, "guest resources must be released");
    assert!(
        vm.execution_scope().operations().is_empty(),
        "pending host operations must be zero"
    );
}

#[test]
fn app_catalog_owns_only_the_two_new_helpers() {
    let core = vm::standard_host_catalog();
    assert!(
        core.function("json::is_valid").is_none(),
        "the pinned Core must not publish json::is_valid"
    );
    assert!(
        core.function("io::try_write_text").is_none(),
        "the pinned Core must not publish io::try_write_text"
    );
    assert!(
        core.structs()
            .iter()
            .all(|schema| schema.name != "IoWriteTextResult"),
        "the pinned Core must not declare IoWriteTextResult"
    );

    let app = standard_catalog();

    let is_valid = app
        .function("xeh_json::is_valid")
        .expect("xeh_json::is_valid schema");
    assert_eq!(is_valid.params.len(), 1);
    assert_eq!(is_valid.params[0].name, "text");
    assert_eq!(is_valid.params[0].ty, HostTypeSchema::String);
    assert_eq!(is_valid.return_type, HostTypeSchema::Bool);

    let write = app
        .function("xeh_io::try_write_text")
        .expect("xeh_io::try_write_text schema");
    assert_eq!(write.params.len(), 2);
    assert_eq!(write.params[0].name, "path");
    assert_eq!(write.params[0].ty, HostTypeSchema::String);
    assert_eq!(write.params[1].name, "text");
    assert_eq!(write.params[1].ty, HostTypeSchema::String);
    let expected_fields = vec![
        HostStructField::new("ok", HostTypeSchema::Bool),
        HostStructField::new("error_code", HostTypeSchema::String),
    ];
    assert_eq!(
        write.return_type,
        HostTypeSchema::named_struct("IoWriteTextResult", expected_fields.clone())
    );
    let declared = app
        .structs()
        .iter()
        .find(|schema| schema.name == "IoWriteTextResult")
        .expect("IoWriteTextResult named body");
    assert_eq!(declared.fields, expected_fields);

    let quote = app
        .function("xeh_url::quote_plus")
        .expect("xeh_url::quote_plus schema");
    assert_eq!(quote.params.len(), 1);
    assert_eq!(quote.params[0].name, "text");
    assert_eq!(quote.params[0].ty, HostTypeSchema::String);
    assert_eq!(quote.return_type, HostTypeSchema::String);
    let unquote = app
        .function("xeh_url::unquote")
        .expect("xeh_url::unquote schema");
    assert_eq!(unquote.params.len(), 1);
    assert_eq!(unquote.params[0].name, "text");
    assert_eq!(unquote.params[0].ty, HostTypeSchema::String);
    assert_eq!(unquote.return_type, HostTypeSchema::String);

    assert_eq!(
        app.functions().len(),
        core.functions().len() + 4,
        "the application catalog adds exactly the four named helpers"
    );
    for preserved in [
        "json::decode_object_entries",
        "json::encode_object_entries",
        "io::open",
        "io::read_all",
    ] {
        assert!(
            app.function(preserved).is_some(),
            "Core function '{preserved}' must stay available"
        );
    }
    assert!(
        app.function("json::is_valid").is_none()
            && app.function("json::decode").is_none()
            && app.function("json::encode").is_none(),
        "the application must not shadow Core json builtins"
    );
    assert!(
        app.function("io::try_write_text").is_none(),
        "the application must not shadow Core io builtins"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn guest_json_validation_matches_the_strict_decoder() {
    let deep_ok = format!("{}{}", "[".repeat(100), "]".repeat(100));
    let deep_bad = format!("{}{}", "[".repeat(200), "]".repeat(200));
    let source = format!(
        r#"
        use json;
        use xeh_json;
        let mut out: map = {{}};
        out["null"] = xeh_json::is_valid("null");
        out["true"] = xeh_json::is_valid("true");
        out["int"] = xeh_json::is_valid("1");
        out["scalar"] = xeh_json::is_valid("\"x\"");
        out["array"] = xeh_json::is_valid("[]");
        out["object"] = xeh_json::is_valid("{{\"a\":1}}");
        out["dup_top"] = xeh_json::is_valid("{{\"a\":1,\"a\":2}}");
        out["dup_nested"] = xeh_json::is_valid("{{\"a\":{{\"b\":1,\"b\":2}}}}");
        out["trailing"] = xeh_json::is_valid("1 2");
        out["nan"] = xeh_json::is_valid("NaN");
        out["surrogate"] = xeh_json::is_valid("\"\\ud800\"");
        out["pair"] = xeh_json::is_valid("\"\\ud83d\\ude00\"");
        out["deep_ok"] = xeh_json::is_valid("{deep_ok}");
        out["deep_bad"] = xeh_json::is_valid("{deep_bad}");
        out["after_false"] = xeh_json::is_valid("{{\"ok\":[1,2,3]}}");
        let decoded: string = json::decode::<string>("\"still-ok\"");
        out["decoded"] = decoded;
        out;
        "#,
        deep_ok = deep_ok,
        deep_bad = deep_bad,
    );
    let mut vm = prepare_vm(&source);
    let value = drive(&mut vm).await;

    for key in [
        "null",
        "true",
        "int",
        "scalar",
        "array",
        "object",
        "pair",
        "deep_ok",
        "after_false",
    ] {
        assert!(bool_field(&value, key), "'{key}' must be accepted JSON");
    }
    for key in [
        "dup_top",
        "dup_nested",
        "trailing",
        "nan",
        "surrogate",
        "deep_bad",
    ] {
        assert!(!bool_field(&value, key), "'{key}' must be rejected JSON");
    }
    assert_eq!(
        string_field(&value, "decoded"),
        "still-ok",
        "the guest must continue with a healthy Core decode after a rejection"
    );
    assert_quiescent(&mut vm);
}

#[tokio::test(flavor = "current_thread")]
async fn guest_text_writer_enforces_policy_and_continues() {
    let root = TestRoot::create();
    let outside = TestRoot::create();
    let unicode_text = "héllo 世界";
    let cap = unicode_text.len();
    let unicode_path = root.path().join("unicode.txt");
    let sentinel_path = root.path().join("sentinel.txt");
    let after_path = root.path().join("after.txt");
    let directory_path = root.path().join("adir");
    let outside_path = outside.path().join("escape.txt");
    let victim_path = outside.path().join("victim.txt");
    let link_path = root.path().join("link.txt");
    let dangling_path = root.path().join("dangling.txt");
    let missing_target = outside.path().join("missing.txt");
    let keep_path = root.path().join("keep.txt");

    std::fs::create_dir(&directory_path).expect("directory fixture");
    std::fs::write(&sentinel_path, b"SENTINEL").expect("sentinel fixture");
    std::fs::write(&victim_path, b"victim").expect("victim fixture");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&victim_path, &link_path).expect("valid symlink fixture");
        std::os::unix::fs::symlink(&missing_target, &dangling_path)
            .expect("dangling symlink fixture");
    }

    let source = format!(
        r#"
        use io;
        use xeh_io;
        let handle = io::open("{keep}", "w");
        let kept = io::write(handle, "kept");
        let kept_flush = io::flush(handle);
        let mut out: map = {{}};
        let unicode = xeh_io::try_write_text("{unicode}", "{text}");
        out["unicode_ok"] = unicode.ok;
        out["unicode_err"] = unicode.error_code;
        let exact = xeh_io::try_write_text("{unicode}", "{text}");
        out["exact_ok"] = exact.ok;
        let over = xeh_io::try_write_text("{sentinel}", "{text}x");
        out["over_ok"] = over.ok;
        out["over_err"] = over.error_code;
        let directory = xeh_io::try_write_text("{directory}", "x");
        out["dir_ok"] = directory.ok;
        out["dir_err"] = directory.error_code;
        let outside = xeh_io::try_write_text("{outside}", "x");
        out["outside_ok"] = outside.ok;
        out["outside_err"] = outside.error_code;
        let escaped = xeh_io::try_write_text("{link}", "x");
        out["symlink_ok"] = escaped.ok;
        out["symlink_err"] = escaped.error_code;
        let dangling = xeh_io::try_write_text("{dangling}", "x");
        out["dangling_ok"] = dangling.ok;
        out["dangling_err"] = dangling.error_code;
        let after = xeh_io::try_write_text("{after}", "after");
        out["after_ok"] = after.ok;
        out["after_err"] = after.error_code;
        out["kept"] = kept;
        out["kept_flush"] = kept_flush;
        out["closed"] = io::close(handle);
        out;
        "#,
        keep = keep_path.display(),
        unicode = unicode_path.display(),
        text = unicode_text,
        sentinel = sentinel_path.display(),
        directory = directory_path.display(),
        outside = outside_path.display(),
        link = link_path.display(),
        dangling = dangling_path.display(),
        after = after_path.display(),
    );
    let mut vm = prepare_vm(&source);
    configure_policy(&mut vm, root.path(), true, cap);
    let value = drive(&mut vm).await;

    assert!(bool_field(&value, "unicode_ok"));
    assert_eq!(string_field(&value, "unicode_err"), "");
    assert_eq!(
        std::fs::read(&unicode_path).expect("unicode output"),
        unicode_text.as_bytes(),
        "the writer must store the exact UTF-8 bytes"
    );
    assert!(bool_field(&value, "exact_ok"), "the exact byte cap allows");
    assert!(!bool_field(&value, "over_ok"));
    assert_eq!(string_field(&value, "over_err"), "write_limit");
    assert_eq!(
        std::fs::read(&sentinel_path).expect("sentinel output"),
        b"SENTINEL",
        "a rejected write must not truncate the existing file"
    );

    assert!(!bool_field(&value, "dir_ok"));
    assert_eq!(string_field(&value, "dir_err"), "io_error");
    assert!(directory_path.is_dir(), "the directory must survive");

    assert!(!bool_field(&value, "outside_ok"));
    assert_eq!(string_field(&value, "outside_err"), "policy_denied");
    assert!(
        !outside_path.exists(),
        "an outside path must not be created"
    );

    assert!(!bool_field(&value, "symlink_ok"));
    assert_eq!(string_field(&value, "symlink_err"), "policy_denied");
    assert_eq!(
        std::fs::read(&victim_path).expect("victim output"),
        b"victim",
        "a symlink escape must not touch its target"
    );
    assert!(!bool_field(&value, "dangling_ok"));
    assert_eq!(string_field(&value, "dangling_err"), "policy_denied");
    assert!(
        !missing_target.exists(),
        "a dangling symlink escape must not create its target"
    );

    assert!(bool_field(&value, "after_ok"));
    assert_eq!(string_field(&value, "after_err"), "");
    assert_eq!(std::fs::read(&after_path).expect("after output"), b"after");

    assert_eq!(
        map_field(&value, "kept"),
        &Value::Int(4),
        "the unrelated handle writes its four bytes"
    );
    assert!(bool_field(&value, "kept_flush"));
    assert!(bool_field(&value, "closed"), "the unrelated handle closes");
    assert_eq!(
        std::fs::read(&keep_path).expect("kept output"),
        b"kept",
        "the unrelated handle keeps its content"
    );
    assert_quiescent(&mut vm);
}

#[tokio::test(flavor = "current_thread")]
async fn helpers_fail_closed_without_the_application_registry() {
    let root = TestRoot::create();
    let write_path = root.path().join("unbound.txt");
    let sources = [
        "use xeh_json; let valid = xeh_json::is_valid(\"null\");".to_string(),
        format!(
            "use xeh_io; let written = xeh_io::try_write_text(\"{}\", \"x\");",
            write_path.display()
        ),
    ];
    for source in sources {
        let catalog = standard_catalog();
        let program = compile_source_with_catalog(&source, Arc::clone(&catalog))
            .expect("the helper contract compiles from the application catalog");
        let mut vm = Vm::try_new(program).expect("vm");
        vm.set_default_host_fallback_enabled(false);
        let message = match vm::HostFunctionRegistry::empty().bind_vm_cached(&mut vm) {
            Err(error) => error.to_string(),
            Ok(()) => drive_to_halt(&mut vm)
                .await
                .expect_err("an unbound application helper must fail closed")
                .to_string(),
        };
        assert!(
            message.contains("xeh_json") || message.contains("xeh_io"),
            "the fail-closed error must name the unbound helper: {message}"
        );
    }
    assert!(
        !write_path.exists(),
        "an unbound writer must not touch disk"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn guest_text_writer_denies_without_policy_and_counts_bytes() {
    let root = TestRoot::create();
    let denied_path = root.path().join("denied.txt");
    let source = format!(
        r#"
        use xeh_io;
        let denied = xeh_io::try_write_text("{denied}", "x");
        [denied.ok, denied.error_code];
        "#,
        denied = denied_path.display(),
    );
    let mut vm = prepare_vm(&source);
    let value = drive(&mut vm).await;
    let Value::Array(items) = &value else {
        panic!("expected an array result, got {value:?}");
    };
    assert_eq!(items.first(), Some(&Value::Bool(false)));
    assert_eq!(
        items.get(1),
        Some(&Value::String(Arc::new("policy_denied".to_string())))
    );
    assert!(!denied_path.exists(), "no policy must not create a file");
    assert_quiescent(&mut vm);

    let read_only_path = root.path().join("read-only.txt");
    let source = format!(
        r#"
        use xeh_io;
        let denied = xeh_io::try_write_text("{path}", "x");
        [denied.ok, denied.error_code];
        "#,
        path = read_only_path.display(),
    );
    let mut vm = prepare_vm(&source);
    configure_policy(&mut vm, root.path(), false, 1024);
    let value = drive(&mut vm).await;
    let Value::Array(items) = &value else {
        panic!("expected an array result, got {value:?}");
    };
    assert_eq!(items.first(), Some(&Value::Bool(false)));
    assert_eq!(
        items.get(1),
        Some(&Value::String(Arc::new("policy_denied".to_string())))
    );
    assert!(
        !read_only_path.exists(),
        "allow_write=false must not create"
    );
    assert_quiescent(&mut vm);

    let wide_path = root.path().join("wide.txt");
    let source = format!(
        r#"
        use xeh_io;
        let wide = xeh_io::try_write_text("{path}", "éé");
        [wide.ok, wide.error_code];
        "#,
        path = wide_path.display(),
    );
    let mut vm = prepare_vm(&source);
    configure_policy(&mut vm, root.path(), true, 3);
    let value = drive(&mut vm).await;
    let Value::Array(items) = &value else {
        panic!("expected an array result, got {value:?}");
    };
    assert_eq!(items.first(), Some(&Value::Bool(false)));
    assert_eq!(
        items.get(1),
        Some(&Value::String(Arc::new("write_limit".to_string()))),
        "four UTF-8 bytes exceed a three-byte budget even though they are two chars"
    );
    assert!(!wide_path.exists(), "an over-budget write must not create");
    assert_quiescent(&mut vm);
}
