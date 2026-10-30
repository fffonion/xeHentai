//! Application-owned typed host helpers.
//!
//! Two capabilities the pinned Core does not publish are implemented on this
//! application's own namespaces:
//!
//! * `xeh_json::is_valid` answers the strict `json::decode` question without
//!   exposing a decoded value. It delegates to the Core decoder's public
//!   descriptor adapter, so duplicate keys, trailing input, unsupported
//!   numbers, bad escapes, Unicode surrogates and the depth limit behave
//!   exactly like `json::decode`. The application owns no JSON parser.
//! * `xeh_io::try_write_text` replaces a whole file with UTF-8 text and keeps
//!   ordinary policy/size/OS failures as a named `IoWriteTextResult`. It reads
//!   the per-VM [`vm::IoPolicy`] snapshot and runs the file work through the
//!   public async host bridge, so cancellation and the generic operation
//!   lifecycle stay with the VM.

pub mod io;
pub mod json;

use io::IO_WRITE_TEXT_NAME;
use json::JSON_IS_VALID_NAME;

use vm::{HostApiCatalog, HostFunctionRegistry, HostFunctionSchema, HostStructSchema};

use crate::types::HostError;

/// Named structs the application helper functions return.
pub fn structs() -> Vec<HostStructSchema> {
    io::structs()
}

/// Guest contracts the application helper functions publish.
pub fn functions() -> Vec<HostFunctionSchema> {
    vec![json::is_valid_schema(), io::try_write_text_schema()]
}

/// Registers both helpers against the application catalog.
///
/// The JSON helper resolves the pinned Core decoder descriptor once, here, so
/// a Core revision that removes or reshapes `json::decode` fails the whole
/// registry install instead of silently changing the predicate.
pub fn register(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> Result<(), HostError> {
    let decode = json::decode_adapter()?;
    register_one(registry, catalog, JSON_IS_VALID_NAME, move || {
        Box::new(json::IsValidFn::new(decode))
    })?;
    register_one(registry, catalog, IO_WRITE_TEXT_NAME, || {
        Box::new(io::TryWriteTextFn)
    })?;
    Ok(())
}

fn register_one<F>(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
    factory: F,
) -> Result<(), HostError>
where
    F: Fn() -> Box<dyn vm::HostFunction> + Send + Sync + 'static,
{
    let schema = catalog
        .function(name)
        .ok_or_else(|| HostError::bind(format!("missing application schema {name}")))?
        .clone();
    vm::register_catalog_function(registry, catalog, name, schema, factory)
        .map_err(|error| HostError::bind(error.to_string()))
}
