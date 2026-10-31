//! Worker permits owned by the host, not by the guest that reports them.
//!
//! Pause, resume, and exit take and return these permits. The guest reads the
//! count back; a local integer in the same function is not the permit.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use vm::{
    register_catalog_function, CallOutcome, CallReturn, HostApiCatalog, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostParamSchema, HostTypeSchema, Value, Vm, VmError,
    VmResult,
};

use crate::types::HostError;

#[derive(Clone)]
pub struct PermitState {
    held: Arc<AtomicUsize>,
}

impl Default for PermitState {
    fn default() -> Self {
        Self {
            held: Arc::new(AtomicUsize::new(0)),
        }
    }
}

pub fn install(vm: &mut Vm) {
    vm.host_context().set_module_state(PermitState::default());
}

pub fn functions() -> Vec<HostFunctionSchema> {
    vec![
        HostFunctionSchema::with_return(
            "xeh_worker::acquire",
            vec![HostParamSchema::value("count", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        )
        .with_description("Take worker permits and return the host-owned count"),
        HostFunctionSchema::with_return(
            "xeh_worker::release_all",
            Vec::new(),
            HostTypeSchema::Int,
        )
        .with_description("Return every held worker permit and return the host-owned count"),
        HostFunctionSchema::with_return("xeh_worker::permits", Vec::new(), HostTypeSchema::Int)
            .with_description("Read the host-owned worker permit count"),
    ]
}

pub fn register(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> Result<(), HostError> {
    register_one(registry, catalog, "xeh_worker::acquire", || Box::new(AcquireFn))?;
    register_one(registry, catalog, "xeh_worker::release_all", || {
        Box::new(ReleaseAllFn)
    })?;
    register_one(registry, catalog, "xeh_worker::permits", || Box::new(PermitsFn))?;
    Ok(())
}

fn register_one<F>(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
    factory: F,
) -> Result<(), HostError>
where
    F: Fn() -> Box<dyn HostFunction> + Send + Sync + 'static,
{
    let schema = catalog
        .function(name)
        .ok_or_else(|| HostError::bind(format!("missing worker permit schema {name}")))?
        .clone();
    register_catalog_function(registry, catalog, name, schema, factory)
        .map_err(|error| HostError::bind(error.to_string()))
}

fn state_of(vm: &mut Vm) -> Result<PermitState, VmError> {
    vm.host_context()
        .module_state::<PermitState>()
        .cloned()
        .ok_or_else(|| VmError::HostError("xeh_worker permits are not installed for this VM".to_string()))
}

fn count_arg(value: Option<&Value>) -> Result<usize, VmError> {
    match value {
        Some(Value::Int(count)) if *count >= 0 => Ok(*count as usize),
        _ => Err(VmError::HostError(
            "xeh_worker::acquire count must be a non-negative int".to_string(),
        )),
    }
}

struct AcquireFn;

impl HostFunction for AcquireFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let state = state_of(vm)?;
        let count = count_arg(args.first())?;
        let held = state.held.fetch_add(count, Ordering::AcqRel) + count;
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(held as i64))))
    }
}

struct ReleaseAllFn;

impl HostFunction for ReleaseAllFn {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let state = state_of(vm)?;
        state.held.store(0, Ordering::Release);
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(0))))
    }
}

struct PermitsFn;

impl HostFunction for PermitsFn {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let state = state_of(vm)?;
        let held = state.held.load(Ordering::Acquire);
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(held as i64))))
    }
}
