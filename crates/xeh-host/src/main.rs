use std::process::ExitCode;

fn main() -> ExitCode {
    match xeh_host::run_app() {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{}", sanitize(&error.to_string()));
            ExitCode::from(1)
        }
    }
}

fn sanitize(message: &str) -> String {
    let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    message.replace(&workspace.to_string_lossy().to_string(), "<workspace>")
}
