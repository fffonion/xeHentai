use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vm::{
    compile_source_file_with_options, compile_source_with_flavor_and_options, standard_composition,
    CompileSourceFileOptions, HostApiCatalog, HostFunctionRegistry, Program, SourceFlavor,
    TimerConfig, TimerHostExt, Value, Vm, VmStatus,
};

use crate::bridge::EmbeddingBridge;
use crate::catalog::{install_reused_state, open_registry, standard_catalog};
use crate::timer::EmbeddingTimer;
use crate::types::HostError;

const DRIVE_STEP_LIMIT: usize = 10_000;

pub fn app_main_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/main.rss")
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
