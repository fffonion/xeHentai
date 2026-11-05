//! Runtime RSS defaults and updateable source overrides.

use std::fs;

#[test]
fn missing_rss_file_uses_the_embedded_default() {
    let root = tempfile::tempdir().expect("resource root");
    let source =
        crate::resources::read_rss_source(root.path(), "rss/example.rss", "compiled default")
            .expect("embedded source");

    assert_eq!(source, "compiled default");
}

#[test]
fn installed_rss_file_overrides_the_embedded_default() {
    let root = tempfile::tempdir().expect("resource root");
    let path = root.path().join("rss/example.rss");
    fs::create_dir_all(path.parent().expect("rss parent")).expect("rss directory");
    fs::write(&path, "updated source").expect("override");

    let source =
        crate::resources::read_rss_source(root.path(), "rss/example.rss", "compiled default")
            .expect("disk override");

    assert_eq!(source, "updated source");
}

#[test]
fn rss_read_errors_are_not_hidden_by_embedded_defaults() {
    let root = tempfile::tempdir().expect("resource root");
    fs::write(root.path().join("rss"), "blocking file").expect("blocking path");

    let result =
        crate::resources::read_rss_source(root.path(), "rss/example.rss", "compiled default");

    assert!(
        result.is_err(),
        "non-missing RSS errors must remain visible"
    );
}

#[test]
fn all_production_rss_modules_load_without_external_rss_files() {
    let root = tempfile::tempdir().expect("empty resource root");

    crate::filters::Filters::load_with_embedded(root.path()).expect("embedded filter script");
    crate::rpc::RpcDispatcher::load_with_embedded(root.path()).expect("embedded RPC script");
    crate::updater::Updater::load_with_embedded(root.path()).expect("embedded updater scripts");
}
