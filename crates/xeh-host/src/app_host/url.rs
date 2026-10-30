//! Percent-encoding helpers owned by the application host.
//!
//! The pinned Core publishes no `quote_plus`/`unquote` primitive. Glype URL
//! rewriting needs exactly CPython's `urllib.parse.quote_plus` (safe set
//! `A-Za-z0-9_.-~`, space to `+`, uppercase hex) and `unquote` (percent bytes
//! decoded as UTF-8 with replacement, plus signs kept literal). These are
//! generic string primitives with no proxy policy.

use vm::{
    CallOutcome, CallReturn, HostFunction, HostFunctionSchema, HostParamSchema, HostTypeSchema,
    Value, Vm, VmResult,
};

pub const QUOTE_PLUS_NAME: &str = "xeh_url::quote_plus";
pub const UNQUOTE_NAME: &str = "xeh_url::unquote";

pub fn functions() -> Vec<HostFunctionSchema> {
    vec![
        HostFunctionSchema::with_return(
            QUOTE_PLUS_NAME,
            vec![HostParamSchema::value("text", HostTypeSchema::String)],
            HostTypeSchema::String,
        )
        .with_description("CPython quote_plus over UTF-8 bytes"),
        HostFunctionSchema::with_return(
            UNQUOTE_NAME,
            vec![HostParamSchema::value("text", HostTypeSchema::String)],
            HostTypeSchema::String,
        )
        .with_description("CPython unquote over UTF-8 bytes; plus signs stay literal"),
    ]
}

pub fn quote_plus(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                output.push(*byte as char)
            }
            b' ' => output.push('+'),
            other => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                output.push('%');
                output.push(HEX[usize::from(other >> 4)] as char);
                output.push(HEX[usize::from(other & 0x0F)] as char);
            }
        }
    }
    output
}

pub fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = hex_value(bytes[index + 1]);
            let low = hex_value(bytes[index + 2]);
            if let (Some(high), Some(low)) = (high, low) {
                output.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub struct QuotePlusFn;

impl HostFunction for QuotePlusFn {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let Some(Value::String(text)) = args.first() else {
            return Err(vm::VmError::HostError(
                "xeh_url::quote_plus expects a string".to_string(),
            ));
        };
        Ok(CallOutcome::Return(CallReturn::one(Value::String(
            std::sync::Arc::new(quote_plus(text)),
        ))))
    }
}

pub struct UnquoteFn;

impl HostFunction for UnquoteFn {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let Some(Value::String(text)) = args.first() else {
            return Err(vm::VmError::HostError(
                "xeh_url::unquote expects a string".to_string(),
            ));
        };
        Ok(CallOutcome::Return(CallReturn::one(Value::String(
            std::sync::Arc::new(unquote(text)),
        ))))
    }
}
