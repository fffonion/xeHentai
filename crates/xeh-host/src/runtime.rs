use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::Poll;

use vm::{
    compile_source_file_with_options, compile_source_with_flavor_and_options, standard_composition,
    CompileSourceFileOptions, HostApiCatalog, HostFunctionRegistry, InvocationItem, InvocationPoll,
    IoHostExt, IoPolicy, Program, SourceFlavor, TimerConfig, TimerHostExt, Value, Vm, VmStatus,
};

use crate::bridge::EmbeddingBridge;
use crate::catalog::{install_reused_state, open_registry, standard_catalog};
use crate::timer::EmbeddingTimer;
use crate::types::HostError;

const DRIVE_STEP_LIMIT: usize = 10_000;
const CONFIG_IO_LIMIT_BYTES: usize = 4 * 1024 * 1024;
const SITE_IO_LIMIT_BYTES: usize = 8 * 1024 * 1024;

pub fn app_main_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/main.rss")
}

/// Entry module for the JSON configuration scenarios.
pub fn config_entry_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/config/entry.rss")
}

pub fn compile_source_with_catalog(
    source: &str,
    catalog: Arc<HostApiCatalog>,
) -> Result<Program, HostError> {
    let compiled = compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::new().with_host_api_catalog(catalog),
    )
    .map_err(|error| HostError::compile(error.to_string()))?;
    Ok(compiled.program)
}

pub fn compile_file_with_catalog(
    path: &Path,
    catalog: Arc<HostApiCatalog>,
) -> Result<Program, HostError> {
    let compiled = compile_source_file_with_options(
        path,
        CompileSourceFileOptions::new().with_host_api_catalog(catalog),
    )
    .map_err(|error| HostError::compile(error.to_string()))?;
    Ok(compiled.program)
}

pub fn bind_vm(vm: &mut Vm, registry: &HostFunctionRegistry) -> Result<(), HostError> {
    registry
        .bind_vm_cached(vm)
        .map_err(|error| HostError::bind(error.to_string()))?;
    vm.set_standard_composition(standard_composition());
    install_reused_state(vm)?;
    vm.set_async_bridge(Box::new(EmbeddingBridge::default()))
        .map_err(HostError::from_vm)?;
    Ok(())
}

pub fn attach_timer(vm: &mut Vm, timer: Arc<EmbeddingTimer>) {
    vm.install_timer_runtime(timer, TimerConfig::default());
}

pub fn resource_count(vm: &mut Vm) -> usize {
    vm.execution_scope().resources().len()
}

pub fn stack_int(vm: &Vm) -> Result<i64, HostError> {
    match vm.stack().last() {
        Some(Value::Int(value)) => Ok(*value),
        Some(other) => Err(HostError::runtime(format!(
            "entry result is {other:?}, not an int"
        ))),
        None => Err(HostError::runtime("entry left an empty stack")),
    }
}

pub async fn drive_to_halt(vm: &mut Vm) -> Result<(), HostError> {
    let mut status = vm.run().map_err(HostError::from_vm)?;
    for _ in 0..DRIVE_STEP_LIMIT {
        match status {
            VmStatus::Halted => return Ok(()),
            VmStatus::Yielded => status = vm.resume().map_err(HostError::from_vm)?,
            VmStatus::Waiting(_) => {
                vm.await_waiting_host_op()
                    .await
                    .map_err(HostError::from_vm)?;
                status = vm.resume().map_err(HostError::from_vm)?;
            }
        }
    }
    Err(HostError::runtime("drive exceeded the step limit"))
}

pub fn drive_until_waiting(vm: &mut Vm) -> Result<VmStatus, HostError> {
    let mut status = vm.run().map_err(HostError::from_vm)?;
    for _ in 0..DRIVE_STEP_LIMIT {
        match status {
            VmStatus::Halted | VmStatus::Waiting(_) => return Ok(status),
            VmStatus::Yielded => status = vm.resume().map_err(HostError::from_vm)?,
        }
    }
    Err(HostError::runtime(
        "drive exceeded the step limit before waiting",
    ))
}

pub async fn release_for_reuse(vm: &mut Vm) -> Result<(), HostError> {
    vm.reset_for_reuse().map_err(HostError::from_vm)?;
    if vm.scope_reset_pending() {
        poll_fn(|cx| vm.poll_reset_for_reuse(cx))
            .await
            .map_err(HostError::from_vm)?;
    }
    if !vm.is_reusable() {
        return Err(HostError::runtime(
            "vm is not reusable after reset quiescence",
        ));
    }
    Ok(())
}

pub fn run_app() -> Result<i64, HostError> {
    run_entry(&app_main_path())
}

pub fn run_entry(path: &Path) -> Result<i64, HostError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| HostError::runtime(error.to_string()))?;
    runtime.block_on(run_entry_async(path))
}

async fn run_entry_async(path: &Path) -> Result<i64, HostError> {
    let catalog = standard_catalog();
    let program = compile_file_with_catalog(path, Arc::clone(&catalog))?;
    let mut vm = Vm::try_new(program).map_err(HostError::from_vm)?;
    let registry = open_registry(&catalog)?;
    bind_vm(&mut vm, &registry)?;
    attach_timer(&mut vm, Arc::new(EmbeddingTimer::new()));
    drive_to_halt(&mut vm).await?;
    stack_int(&vm)
}

/// Runs the exported `run` callable of the configuration entry against one
/// fixture root.
///
/// The root is the only directory the reused Core `io` builtins may read or
/// write. The request and the returned report are JSON text owned by the
/// caller; the VM receives no ambient process path.
pub fn run_config_scenario(root: &Path, request: &str) -> Result<String, HostError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| HostError::runtime(error.to_string()))?;
    runtime.block_on(run_config_scenario_async(root, request))
}

async fn run_config_scenario_async(root: &Path, request: &str) -> Result<String, HostError> {
    let root_text = root
        .to_str()
        .ok_or_else(|| HostError::runtime("fixture root is not valid UTF-8"))?;
    let catalog = standard_catalog();
    let entry = config_entry_path();
    let program = compile_file_with_catalog(&entry, Arc::clone(&catalog)).map_err(|error| {
        HostError::compile(format!("config entry {}: {error}", entry.display()))
    })?;
    let mut vm = Vm::try_new(program).map_err(HostError::from_vm)?;
    // The pinned Core enables trace JIT by default on this platform even when
    // the `cranelift-jit` feature is absent, which fails hot loops with
    // `JitNative("native JIT backend is disabled")`. The config entry iterates
    // over the 25 keys, so keep interpreter-only execution here.
    vm.set_jit_config(vm::JitConfig {
        enabled: false,
        ..Default::default()
    });
    let registry = open_registry(&catalog)?;
    bind_vm(&mut vm, &registry)?;
    attach_timer(&mut vm, Arc::new(EmbeddingTimer::new()));
    vm.configure_io(IoPolicy {
        allowed_roots: vec![root_text.to_string()],
        allow_write: true,
        allow_process: false,
        max_read_bytes: CONFIG_IO_LIMIT_BYTES,
        max_write_bytes: CONFIG_IO_LIMIT_BYTES,
    });
    drive_to_halt(&mut vm).await?;
    let callable = vm
        .resolve_exported_callable("run")
        .map_err(HostError::from_vm)?;
    let mut invocation = vm
        .start_invocation(callable, vec![Value::string(request.to_string())])
        .map_err(HostError::from_vm)?;
    let value = poll_fn(|context| loop {
        match invocation.poll_next_with_context(context) {
            Ok(InvocationPoll::Pending) => return Poll::Pending,
            Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Event(_))))) => continue,
            Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(value))))) => {
                return Poll::Ready(Ok(value));
            }
            Ok(InvocationPoll::Ready(Some(Err(error)))) => {
                return Poll::Ready(Err(HostError::runtime(format!(
                    "config invocation failed: {error:?}"
                ))));
            }
            Ok(InvocationPoll::Ready(None)) => {
                return Poll::Ready(Err(HostError::runtime(
                    "config invocation ended without a result",
                )));
            }
            Err(error) => return Poll::Ready(Err(HostError::from_vm(error))),
        }
    })
    .await?;
    match value {
        Value::String(text) => Ok(text.as_str().to_string()),
        other => Err(HostError::runtime(format!(
            "config entry returned {other:?}, not a string"
        ))),
    }
}

/// Runs the RSS proxy entry with a fixture-owned authorization envelope.
///
/// `authorization` is a JSON object with `allowed_schemes` (strings),
/// `allowed_hosts` (strings), `allowed_ports` (ints) and `allow_private_ips`.
/// It is fixed before the guest starts and is never read from the guest
/// request, environment proxy variables or a user configuration file.
pub fn run_proxy_scenario(
    root: &Path,
    request: &str,
    authorization: &str,
) -> Result<String, HostError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| HostError::runtime(error.to_string()))?;
    runtime.block_on(run_proxy_scenario_async(root, request, authorization))
}

fn authorization_config(authorization: &str) -> Result<vm::HttpConfig, HostError> {
    let value: serde_json::Value = serde_json::from_str(authorization)
        .map_err(|error| HostError::runtime(format!("proxy authorization is not JSON: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| HostError::runtime("proxy authorization must be an object"))?;
    let strings = |name: &str| -> Result<Vec<String>, HostError> {
        object
            .get(name)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| HostError::runtime(format!("proxy authorization {name} is required")))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| HostError::runtime(format!("proxy authorization {name} entry")))
            })
            .collect()
    };
    let ports: Vec<u16> = object
        .get("allowed_ports")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| HostError::runtime("proxy authorization allowed_ports is required"))?
        .iter()
        .map(|item| {
            item.as_u64()
                .and_then(|value| u16::try_from(value).ok())
                .ok_or_else(|| HostError::runtime("proxy authorization port is invalid"))
        })
        .collect::<Result<_, _>>()?;
    let allow_private_ips = object
        .get("allow_private_ips")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(vm::HttpConfig {
        allowed_schemes: strings("allowed_schemes")?,
        allowed_hosts: strings("allowed_hosts")?,
        allowed_ports: ports,
        allow_private_ips,
        connect_timeout: std::time::Duration::from_secs(5),
        request_timeout: std::time::Duration::from_secs(15),
        stream_idle_timeout: std::time::Duration::from_secs(5),
        max_stream_duration: std::time::Duration::from_secs(15),
        ..vm::HttpConfig::default()
    })
}

async fn run_proxy_scenario_async(
    root: &Path,
    request: &str,
    authorization: &str,
) -> Result<String, HostError> {
    let http = authorization_config(authorization)?;
    let catalog = crate::proxy_host::proxy_catalog();
    let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/proxy/entry.rss");
    let program = compile_file_with_catalog(&entry, Arc::clone(&catalog))
        .map_err(|error| HostError::compile(format!("proxy entry {}: {error}", entry.display())))?;
    let mut vm = Vm::try_new(program).map_err(HostError::from_vm)?;
    vm.set_jit_config(vm::JitConfig {
        enabled: false,
        ..Default::default()
    });
    let mut registry = crate::task_control::registry_with_task_hosts(&catalog)?;
    let host = Arc::new(crate::proxy_host::ProxyHost {
        http,
        hooks: crate::dto::TransportHooks::default(),
        session: crate::proxy_exchange::ProxySession::new(root.to_path_buf()),
    });
    crate::proxy_host::register_proxy(&mut registry, &catalog, host)?;
    bind_vm(&mut vm, &registry)?;
    attach_timer(&mut vm, Arc::new(EmbeddingTimer::new()));
    drive_to_halt(&mut vm).await?;
    let callable = vm
        .resolve_exported_callable("run")
        .map_err(HostError::from_vm)?;
    let mut invocation = vm
        .start_invocation(callable, vec![Value::string(request.to_string())])
        .map_err(HostError::from_vm)?;
    let value = poll_fn(|context| loop {
        match invocation.poll_next_with_context(context) {
            Ok(InvocationPoll::Pending) => return Poll::Pending,
            Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Event(_))))) => continue,
            Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(value))))) => {
                return Poll::Ready(Ok(value));
            }
            Ok(InvocationPoll::Ready(Some(Err(error)))) => {
                return Poll::Ready(Err(HostError::runtime(format!(
                    "proxy invocation failed: {error:?}"
                ))));
            }
            Ok(InvocationPoll::Ready(None)) => {
                return Poll::Ready(Err(HostError::runtime(
                    "proxy invocation ended without a result",
                )));
            }
            Err(error) => return Poll::Ready(Err(HostError::from_vm(error))),
        }
    })
    .await?;
    match value {
        Value::String(text) => Ok(text.as_str().to_string()),
        other => Err(HostError::runtime(format!(
            "proxy entry returned {other:?}, not a string"
        ))),
    }
}

/// Runs the RSS site/rule entry with a fixture-owned debug-write root.
///
/// The guest receives only the fixed `debug.htm` target under `root`; it cannot
/// select an ambient host path. This runner is for controlled fixture execution.
pub fn run_site_scenario(root: &Path, request: &str) -> Result<String, HostError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| HostError::runtime(error.to_string()))?;
    runtime.block_on(run_site_scenario_async(root, request))
}

async fn run_site_scenario_async(root: &Path, request: &str) -> Result<String, HostError> {
    let root_text = root
        .to_str()
        .ok_or_else(|| HostError::runtime("site fixture root is not valid UTF-8"))?;
    let debug_path = root.join("debug.htm");
    let debug_text = debug_path
        .to_str()
        .ok_or_else(|| HostError::runtime("site debug path is not valid UTF-8"))?;
    let catalog = standard_catalog();
    let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/site/entry.rss");
    let program = compile_file_with_catalog(&entry, Arc::clone(&catalog))
        .map_err(|error| HostError::compile(format!("site entry {}: {error}", entry.display())))?;
    let mut vm = Vm::try_new(program).map_err(HostError::from_vm)?;
    vm.set_jit_config(vm::JitConfig {
        enabled: false,
        ..Default::default()
    });
    let registry = open_registry(&catalog)?;
    bind_vm(&mut vm, &registry)?;
    attach_timer(&mut vm, Arc::new(EmbeddingTimer::new()));
    vm.configure_io(IoPolicy {
        allowed_roots: vec![root_text.to_string()],
        allow_write: true,
        allow_process: false,
        max_read_bytes: SITE_IO_LIMIT_BYTES,
        max_write_bytes: SITE_IO_LIMIT_BYTES,
    });
    drive_to_halt(&mut vm).await?;
    let callable = vm
        .resolve_exported_callable("run")
        .map_err(HostError::from_vm)?;
    let mut invocation = vm
        .start_invocation(
            callable,
            vec![
                Value::string(request.to_string()),
                Value::string(debug_text.to_string()),
            ],
        )
        .map_err(HostError::from_vm)?;
    let value = poll_fn(|context| loop {
        match invocation.poll_next_with_context(context) {
            Ok(InvocationPoll::Pending) => return Poll::Pending,
            Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Event(_))))) => continue,
            Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(value))))) => {
                return Poll::Ready(Ok(value));
            }
            Ok(InvocationPoll::Ready(Some(Err(error)))) => {
                return Poll::Ready(Err(HostError::runtime(format!(
                    "site invocation failed: {error:?}"
                ))));
            }
            Ok(InvocationPoll::Ready(None)) => {
                return Poll::Ready(Err(HostError::runtime(
                    "site invocation ended without a result",
                )));
            }
            Err(error) => return Poll::Ready(Err(HostError::from_vm(error))),
        }
    })
    .await?;
    match value {
        Value::String(text) => Ok(text.as_str().to_string()),
        other => Err(HostError::runtime(format!(
            "site entry returned {other:?}, not a string"
        ))),
    }
}
