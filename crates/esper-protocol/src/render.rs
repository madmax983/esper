//! The contract renderers (SPEC §17.3): the compact model-facing signature
//! and the minimal JSON Schema, both derived from the one contract table.
//!
//! Both renderers write into a caller-owned [`core::fmt::Write`]: no
//! allocation, and the caller picks the buffer size.
//!
//! Contract-table text is ASCII by construction, so the renderers emit
//! names, options, and descriptions verbatim without JSON string
//! escaping. A contract entry with non-ASCII text would render wrong;
//! the table has none, and the in-crate tests assert the byte-exact
//! output of every entry, which pins this.

use crate::contract::{ArgKind, ToolContract};

/// Render the compact model-facing signature of one tool.
///
/// This is the line the model sees in the prompt's tool list (design
/// §8). Format:
///
/// ```text
/// gpio_pin_write(pin:u8[0-7], level:low|high) -- Set the logic level of a GPIO pin. Re-dispatch is safe (set-operation). [idempotent_write, verify:read_back, bound:64B]
/// ```
///
/// An argument renders as `name:u8[lo-hi]`, `name:u16[lo-hi]`, or
/// `name:opt1|opt2`; a tool with no arguments renders as `name()`.
///
/// # Errors
///
/// Returns [`core::fmt::Error`] when the writer reports one.
pub fn render_signature(
    contract: &ToolContract,
    out: &mut dyn core::fmt::Write,
) -> core::fmt::Result {
    out.write_str(contract.name)?;
    out.write_char('(')?;
    for (index, arg) in contract.args.iter().enumerate() {
        if index > 0 {
            out.write_str(", ")?;
        }
        out.write_str(arg.name)?;
        match arg.kind {
            ArgKind::U8 { lo, hi } => write!(out, ":u8[{lo}-{hi}]")?,
            ArgKind::U16 { lo, hi } => write!(out, ":u16[{lo}-{hi}]")?,
            ArgKind::Enum { options } => {
                out.write_char(':')?;
                for (option_index, option) in options.iter().enumerate() {
                    if option_index > 0 {
                        out.write_char('|')?;
                    }
                    out.write_str(option)?;
                }
            }
        }
    }
    write!(
        out,
        ") -- {} [{}, verify:{}, bound:{}B]",
        contract.description,
        contract.permission.name(),
        contract.verification.name(),
        contract.result_bound
    )
}

/// Render a minimal JSON Schema for one tool.
///
/// This is the schema-pipeline output for training and host
/// interoperability (design §7, item 1); it is not what the on-device
/// model sees. Format is one compact line, fields in fixed order:
///
/// ```text
/// {"name":"gpio_pin_write","type":"object","properties":{"pin":{"type":"integer","minimum":0,"maximum":7},"level":{"type":"string","enum":["low","high"]}},"required":["pin","level"],"additionalProperties":false}
/// ```
///
/// Integer kinds render with `minimum`/`maximum`; enums render with an
/// `enum` array in option order. `required` lists the required arguments
/// in contract order.
///
/// # Errors
///
/// Returns [`core::fmt::Error`] when the writer reports one.
pub fn render_json_schema(
    contract: &ToolContract,
    out: &mut dyn core::fmt::Write,
) -> core::fmt::Result {
    out.write_str("{\"name\":\"")?;
    out.write_str(contract.name)?;
    out.write_str("\",\"type\":\"object\",\"properties\":{")?;
    for (index, arg) in contract.args.iter().enumerate() {
        if index > 0 {
            out.write_char(',')?;
        }
        write!(out, "\"{}\":", arg.name)?;
        match arg.kind {
            ArgKind::U8 { lo, hi } => {
                write!(
                    out,
                    "{{\"type\":\"integer\",\"minimum\":{lo},\"maximum\":{hi}}}"
                )?;
            }
            ArgKind::U16 { lo, hi } => {
                write!(
                    out,
                    "{{\"type\":\"integer\",\"minimum\":{lo},\"maximum\":{hi}}}"
                )?;
            }
            ArgKind::Enum { options } => {
                out.write_str("{\"type\":\"string\",\"enum\":[")?;
                for (option_index, option) in options.iter().enumerate() {
                    if option_index > 0 {
                        out.write_char(',')?;
                    }
                    write!(out, "\"{option}\"")?;
                }
                out.write_str("]}")?;
            }
        }
    }
    out.write_str("},\"required\":[")?;
    let mut first_required = true;
    for arg in contract.args.iter().filter(|arg| arg.required) {
        if !first_required {
            out.write_char(',')?;
        }
        first_required = false;
        write!(out, "\"{}\"", arg.name)?;
    }
    out.write_str("],\"additionalProperties\":false}")
}

#[cfg(test)]
mod tests {
    use super::{render_json_schema, render_signature};
    use crate::contract::{catalog, lookup_by_name};

    /// A fixed-buffer [`core::fmt::Write`] for tests: the renderers stay
    /// allocation-free everywhere, including under test.
    struct Sink<'a> {
        buf: &'a mut [u8],
        len: usize,
    }

    impl core::fmt::Write for Sink<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let bytes = s.as_bytes();
            let end = self.len.checked_add(bytes.len()).ok_or(core::fmt::Error)?;
            let slot = self.buf.get_mut(self.len..end).ok_or(core::fmt::Error)?;
            slot.copy_from_slice(bytes);
            self.len = end;
            Ok(())
        }
    }

    fn render_to(
        name: &str,
        render: fn(&crate::contract::ToolContract, &mut dyn core::fmt::Write) -> core::fmt::Result,
    ) -> [u8; 512] {
        let tool = lookup_by_name(name).expect("test tool must exist");
        let mut buf = [0u8; 512];
        let len = {
            let mut sink = Sink {
                buf: &mut buf,
                len: 0,
            };
            render(tool, &mut sink).expect("render must succeed");
            sink.len
        };
        let mut out = [0u8; 512];
        out[..len].copy_from_slice(&buf[..len]);
        out
    }

    fn rendered_str(bytes: &[u8; 512]) -> &str {
        let len = bytes
            .iter()
            .position(|byte| *byte == 0)
            .expect("rendered text is NUL-terminated by the zeroed buffer");
        core::str::from_utf8(&bytes[..len]).expect("rendered text is ASCII")
    }

    fn signature_of(name: &str) -> [u8; 512] {
        render_to(name, render_signature)
    }

    fn schema_of(name: &str) -> [u8; 512] {
        render_to(name, render_json_schema)
    }

    #[test]
    fn gpio_pin_write_signature_is_byte_stable() {
        assert_eq!(
            rendered_str(&signature_of("gpio_pin_write")),
            "gpio_pin_write(pin:u8[0-7], level:low|high) -- Set the logic level of a GPIO pin. Re-dispatch is safe (set-operation). [idempotent_write, verify:read_back, bound:64B]"
        );
    }

    #[test]
    fn gpio_pin_write_json_schema_is_byte_stable() {
        assert_eq!(
            rendered_str(&schema_of("gpio_pin_write")),
            "{\"name\":\"gpio_pin_write\",\"type\":\"object\",\"properties\":{\"pin\":{\"type\":\"integer\",\"minimum\":0,\"maximum\":7},\"level\":{\"type\":\"string\",\"enum\":[\"low\",\"high\"]}},\"required\":[\"pin\",\"level\"],\"additionalProperties\":false}"
        );
    }

    #[test]
    fn every_tool_signature_is_byte_stable() {
        let expected = [
            "gpio_pin_read(pin:u8[0-7]) -- Read the logic level of a GPIO pin. [read_only, verify:none, bound:64B]",
            "gpio_pin_write(pin:u8[0-7], level:low|high) -- Set the logic level of a GPIO pin. Re-dispatch is safe (set-operation). [idempotent_write, verify:read_back, bound:64B]",
            "sensor_sample_read(sensor:u8[0-3]) -- Read one sensor channel. The value is the channel's fixed raw reading. [read_only, verify:none, bound:64B]",
            "timer_uptime_read() -- Read the monotonic uptime clock, in milliseconds. [read_only, verify:none, bound:64B]",
            "timer_delay_wait(ms:u16[1-5000]) -- Advance the virtual clock by the given milliseconds. Redelivery under the same effect id does not advance the clock twice. [idempotent_write, verify:read_back, bound:64B]",
            "device_status_report(detail:summary|full) -- Report device state. Summary is a small object; full adds pin and sensor detail. [read_only, verify:none, bound:128B]",
        ];
        for (tool, want) in catalog().iter().zip(expected.iter()) {
            assert_eq!(
                rendered_str(&signature_of(tool.name)),
                *want,
                "tool {}",
                tool.name
            );
        }
    }

    #[test]
    fn every_tool_json_schema_is_byte_stable() {
        let expected = [
            "{\"name\":\"gpio_pin_read\",\"type\":\"object\",\"properties\":{\"pin\":{\"type\":\"integer\",\"minimum\":0,\"maximum\":7}},\"required\":[\"pin\"],\"additionalProperties\":false}",
            "{\"name\":\"gpio_pin_write\",\"type\":\"object\",\"properties\":{\"pin\":{\"type\":\"integer\",\"minimum\":0,\"maximum\":7},\"level\":{\"type\":\"string\",\"enum\":[\"low\",\"high\"]}},\"required\":[\"pin\",\"level\"],\"additionalProperties\":false}",
            "{\"name\":\"sensor_sample_read\",\"type\":\"object\",\"properties\":{\"sensor\":{\"type\":\"integer\",\"minimum\":0,\"maximum\":3}},\"required\":[\"sensor\"],\"additionalProperties\":false}",
            "{\"name\":\"timer_uptime_read\",\"type\":\"object\",\"properties\":{},\"required\":[],\"additionalProperties\":false}",
            "{\"name\":\"timer_delay_wait\",\"type\":\"object\",\"properties\":{\"ms\":{\"type\":\"integer\",\"minimum\":1,\"maximum\":5000}},\"required\":[\"ms\"],\"additionalProperties\":false}",
            "{\"name\":\"device_status_report\",\"type\":\"object\",\"properties\":{\"detail\":{\"type\":\"string\",\"enum\":[\"summary\",\"full\"]}},\"required\":[\"detail\"],\"additionalProperties\":false}",
        ];
        for (tool, want) in catalog().iter().zip(expected.iter()) {
            assert_eq!(
                rendered_str(&schema_of(tool.name)),
                *want,
                "tool {}",
                tool.name
            );
        }
    }

    #[test]
    fn argless_tool_renders_empty_parens() {
        assert!(
            rendered_str(&signature_of("timer_uptime_read")).starts_with("timer_uptime_read() -- ")
        );
    }
}
