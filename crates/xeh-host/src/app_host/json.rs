//! `xeh_json::is_valid`: the strict `json::decode` predicate on the
//! application namespace.
//!
//! The predicate does not parse JSON itself. It resolves the pinned Core
//! `json::decode` host descriptor once at registry install and calls that
//! descriptor's args-only adapter with one guaranteed string value, so the
//! accepted grammar (unique keys at every depth, no trailing input, no NaN or
//! Infinity, valid escapes/surrogate pairs and the decoder depth limit) is the
//! same code path `json::decode` uses.
//!
//! A successful decode is discarded: the adapter is resource-free and does not
//! touch the VM, so returning only `true` cannot leak a decoded value. A decode
//! rejection becomes `false`; every other VM error (including an argument type
//! error) propagates so callers cannot mistake an engine fault for invalid
//! JSON.

use vm::{
    CallOutcome, HostAdapterDescriptor, HostFunction, HostFunctionDescriptor, HostFunctionSchema,
    HostParamSchema, HostTypeSchema, StaticHostArgsFunction, Value, Vm, VmError, VmResult,
};

use crate::types::HostError;

/// Guest name of the predicate.
pub const JSON_IS_VALID_NAME: &str = "xeh_json::is_valid";

/// The stable prefix the pinned Core `json::decode` wrapper uses for JSON
/// parse/strict rejections. Only an error carrying this prefix means "the text
/// is not accepted JSON"; every other error is an engine/type fault.
const DECODE_REJECTION_PREFIX: &str = "json_decode failed: ";

/// Guest contract for the predicate.
pub fn is_valid_schema() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        JSON_IS_VALID_NAME,
        vec![HostParamSchema::value("text", HostTypeSchema::String)],
        HostTypeSchema::Bool,
    )
    .with_description("Whether text is accepted by the strict json::decode parser")
}

/// Resolves the pinned Core `json::decode` descriptor and its pure,
/// non-yielding args adapter.
///
/// The descriptor kind is checked rather than assumed: a stack/owned adapter
/// cannot be called with an argument slice without a VM, so any other kind is a
/// fail-closed install error naming the mismatch.
pub(crate) fn decode_adapter() -> Result<StaticHostArgsFunction, HostError> {
    let descriptor = decode_descriptor()?;
    match descriptor.adapter {
        HostAdapterDescriptor::StaticArgs(adapter)
        | HostAdapterDescriptor::StaticNonYieldingArgs(adapter) => Ok(adapter),
        _ => Err(HostError::bind(
            "core 'json::decode' does not carry a StaticArgs adapter at the pinned revision; \
             the application predicate requires an args-only descriptor"
                .to_string(),
        )),
    }
}

fn decode_descriptor() -> Result<HostFunctionDescriptor, HostError> {
    let module = vm::standard_host_modules()
        .iter()
        .find(|module| module.name == "json")
        .ok_or_else(|| {
            HostError::bind(
                "core standard_host_modules has no 'json' module at the pinned revision"
                    .to_string(),
            )
        })?;
    module
        .owned
        .iter()
        .map(|factory| factory())
        .find(|descriptor| descriptor.schema.name == "json::decode")
        .ok_or_else(|| {
            HostError::bind(
                "core 'json' module does not own a 'json::decode' descriptor at the pinned \
                 revision"
                    .to_string(),
            )
        })
}

pub(crate) struct IsValidFn {
    decode: StaticHostArgsFunction,
}

impl IsValidFn {
    pub(crate) fn new(decode: StaticHostArgsFunction) -> Self {
        Self { decode }
    }

    fn evaluate(&self, args: &[Value]) -> VmResult<bool> {
        let text = string_arg(args.first(), "text")?;
        match (self.decode)(&[Value::string(text)]) {
            Ok(CallOutcome::Return(_)) => Ok(true),
            Ok(other) => Err(VmError::HostError(format!(
                "{JSON_IS_VALID_NAME} received an unexpected decoder outcome {other:?}"
            ))),
            Err(VmError::HostError(message)) if message.starts_with(DECODE_REJECTION_PREFIX) => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
}

impl HostFunction for IsValidFn {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let valid = self.evaluate(args)?;
        Ok(CallOutcome::Return(vm::CallReturn::one(Value::Bool(valid))))
    }
}

fn string_arg(value: Option<&Value>, name: &str) -> VmResult<String> {
    match value {
        Some(Value::String(text)) => Ok(text.as_str().to_string()),
        _ => Err(VmError::HostError(format!(
            "{JSON_IS_VALID_NAME} expects a string '{name}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_adapter, decode_descriptor, IsValidFn, JSON_IS_VALID_NAME};
    use vm::{HostAdapterDescriptor, Value, VmError};

    #[test]
    fn pinned_core_decode_descriptor_is_an_args_only_adapter() {
        let descriptor = decode_descriptor().expect("json::decode descriptor");
        assert_eq!(descriptor.schema.name, "json::decode");
        assert!(
            matches!(descriptor.adapter, HostAdapterDescriptor::StaticArgs(_)),
            "the pinned Core json::decode adapter must be the static args adapter"
        );
        assert_eq!(descriptor.schema.params.len(), 1);
        assert_eq!(descriptor.schema.params[0].name, "text");
    }

    #[test]
    fn predicate_delegates_strict_rejections_and_propagates_type_errors() {
        let host = IsValidFn::new(decode_adapter().expect("decode adapter"));
        assert!(host.evaluate(&[Value::string("null")]).expect("null"));
        assert!(!host
            .evaluate(&[Value::string("{\"a\":1,\"a\":2}")])
            .expect("duplicate key is invalid"));
        let error = host
            .evaluate(&[Value::Int(1)])
            .expect_err("a non-string argument must not be treated as invalid JSON");
        assert!(
            matches!(error, VmError::HostError(ref message) if message.contains(JSON_IS_VALID_NAME)),
            "the guard must name the helper: {error}"
        );
    }
}
