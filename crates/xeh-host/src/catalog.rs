use std::sync::Arc;

use vm::{
    standard_composition, standard_host_catalog, standard_host_modules, HostApiCatalog,
    HostFunctionRegistry, HostModuleDescriptor, Vm,
};

use crate::types::HostError;

/// Catalog modules whose guest functions are installed from Core descriptors.
///
/// `http`, `io`, and `timer` publish their owned typed surfaces. `json` now
/// publishes one additional typed surface at the pinned revision: the ordered
/// object-entry codec (`json::decode_object_entries` /
/// `json::encode_object_entries`). The plain `json::encode` / `json::decode`
/// builtins are unchanged and stay on the namespaced-builtin path reached
/// through `standard_composition`; this list only adds the two named,
/// resource-free ordered-entry imports once, from the same catalog snapshot.
pub const REUSED_CATALOG_MODULES: &[&str] = &["http", "io", "json", "timer"];

/// The application catalog: the pinned Core standard catalog plus the two
/// application-owned typed helpers.
///
/// The Core module tables, resource declarations and restricted binding stay
/// exactly as `standard_host_catalog()` publishes them; the helpers are added
/// under their own `xeh_json` / `xeh_io` namespaces, so no Core function is
/// replaced or overloaded. They are ordinary named catalog entries, not a
/// namespaced-builtin override.
pub fn standard_catalog() -> Arc<HostApiCatalog> {
    let core = standard_host_catalog();
    let mut builder = HostApiCatalog::builder();
    for function in core.functions() {
        builder.function(function.clone());
    }
    for resource in core.resources() {
        builder.resource(resource.clone());
    }
    for schema in core.structs() {
        builder.named_struct(schema.clone());
    }
    for schema in crate::app_host::structs() {
        builder.named_struct(schema);
    }
    for function in crate::app_host::functions() {
        builder.function(function);
    }
    Arc::new(builder.build().expect("application host catalog"))
}

pub fn open_registry(catalog: &HostApiCatalog) -> Result<HostFunctionRegistry, HostError> {
    let mut registry = HostFunctionRegistry::empty();
    registry.set_standard_composition(standard_composition());
    install_reused_hosts(&mut registry, catalog)?;
    crate::app_host::register(&mut registry, catalog)?;
    Ok(registry)
}

pub fn install_reused_hosts(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> Result<(), HostError> {
    let mut installed = Vec::new();
    for module in standard_host_modules() {
        if !REUSED_CATALOG_MODULES.contains(&module.name) {
            continue;
        }
        let Some(surface) = module.catalog_module() else {
            return Err(HostError::bind(format!(
                "core module '{}' has no catalog surface at this revision",
                module.name
            )));
        };
        surface
            .install_from_catalog(registry, catalog)
            .map_err(|error| HostError::bind(error.to_string()))?;
        if module.name == "io" {
            // Catalog-backed io operations (including the new typed writer)
            // must be installed before builtin authorization so allow_builtin
            // recognizes them as named host imports instead of static IDs.
            let catalog_names: Vec<String> = surface
                .descriptors()
                .into_iter()
                .map(|descriptor| descriptor.schema.name)
                .collect();
            grant_owned_builtins(registry, module.owned, &catalog_names)?;
        }
        installed.push(module.name);
    }
    for name in REUSED_CATALOG_MODULES {
        if !installed.contains(name) {
            return Err(HostError::bind(format!(
                "core module '{name}' is absent from standard_host_modules; the pinned features do not publish it"
            )));
        }
    }
    Ok(())
}

pub fn install_reused_state(vm: &mut Vm) -> Result<(), HostError> {
    for module in standard_host_modules() {
        if !REUSED_CATALOG_MODULES.contains(&module.name) {
            continue;
        }
        let Some(surface) = module.catalog_module() else {
            continue;
        };
        install_surface_state(vm, &surface)?;
    }
    Ok(())
}

fn install_surface_state(vm: &mut Vm, surface: &HostModuleDescriptor) -> Result<(), HostError> {
    surface
        .install_state_requirements(vm)
        .map_err(|error| HostError::bind(error.to_string()))?;
    Ok(())
}

fn grant_owned_builtins(
    registry: &mut HostFunctionRegistry,
    owned: &'static [fn() -> vm::HostFunctionDescriptor],
    catalog_names: &[String],
) -> Result<(), HostError> {
    for factory in owned {
        let name = factory().schema.name;
        // Some owned functions are formal catalog imports rather than static
        // namespaced builtins. The surface has already installed those, and
        // `allow_builtin` would reject them because they have no static ID.
        if catalog_names
            .iter()
            .any(|catalog_name| catalog_name == &name)
        {
            continue;
        }
        registry
            .allow_builtin(&name)
            .map_err(|error| HostError::bind(error.to_string()))?;
    }
    Ok(())
}
