//! The application's guest boundary: VM construction and declared `Option<T>`.
//!
//! Two contracts live here because both are decided by the pinned Core, not by
//! a caller:
//!
//! * **Native JIT backend.** The `pd-vm` dependency enables `runtime`,
//!   `http-client` and `cranelift-jit`, so `JitConfig::enabled` can stay at its
//!   supported default and hot loops compile instead of reaching a backend
//!   that was never linked.
//!
//! * **A guest `Option<T>` is never a physical Null.** It is a map with a
//!   Boolean `tag` (`true` = Some) and the active `payload`, and one tag-only
//!   map for None. Host returns and host-supplied struct values are encoded and
//!   decoded through the Core helpers (`pd_vm::option`), so the carrier and the
//!   declared `Option<T>` schema cannot drift, and absence is decided by the
//!   tag alone: `Some("")` stays present.

use vm::{Program, Value, Vm};

/// Open one VM with the compiled native JIT backend.
pub(crate) fn open_vm(program: Program) -> Vm {
    Vm::new(program)
}

/// The carrier for `Option::Some(payload)`.
pub(crate) fn some(payload: Value) -> Value {
    vm::option::some_carrier(payload)
}

/// The carrier for `Option::None`.
pub(crate) fn none() -> Value {
    vm::option::none_carrier()
}

/// Decode one value declared as `Option<T>` by its own schema.
///
/// `Ok(Some(payload))` for a Some carrier, `Ok(None)` for the tag-only None
/// carrier, and a closed reason for a value that is not a canonical carrier.
/// `Some(())` is rejected because it would carry the guest Unit, and no
/// declared `Option` in this application is `Option<()>`.
pub(crate) fn decode(value: &Value) -> Result<Option<&Value>, &'static str> {
    match vm::option::payload(value) {
        Ok((true, Some(payload))) => Ok(Some(payload)),
        Ok((true, None)) => Err("Option::Some carries no payload"),
        Ok((false, None)) => Ok(None),
        Ok((false, Some(_))) => Err("Option::None carries a payload"),
        Err(_) => Err("value is not a canonical Option"),
    }
}
