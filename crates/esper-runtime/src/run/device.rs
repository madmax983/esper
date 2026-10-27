//! The device's half of the run activities: the product logic every
//! dispatcher shares, `no_std` and allocation-free.
//!
//! A dispatcher on the board owns the model, the GPIO and the clock; it
//! calls these to turn an activity input into a recorded answer, so the
//! rules that make an answer safe to record — the prompt is rendered
//! from the recorded context only, arguments are re-validated before
//! they touch hardware, and a tool outcome is **masked before it is
//! answered** — live in one place. The host dispatcher double calls the
//! same functions.

use core::task::{Context, Poll};

use esper_core::brief::Brief;
use esper_core::decision::ToolArgs;
use esper_core::ids::{Pin, RunId, ToolId};
use esper_core::mask::{mask_bound, mask_report};
use esper_core::registry::lookup_by_id;
use waymaker_core::{ActivityKind, EffectId, EffectSeq, RunId as WaymakerRunId};
use waymaker_embassy::ActivityDispatcher;
use waymaker_embassy::dispatch::Produced;

use super::wire::{
    AskInput, AskRequest, Check, Fault, InferRequest, Inferred, MAX_ASK_INPUT, Observed, Probed,
    SNAPSHOT_CHUNK, ToolRequest, Verified, VerifyRequest, read_snapshot_key,
};
use crate::agent::wire::WireError;
use crate::backend::{
    BRIEFED_PROMPT_CAP, BackendError, ModelBackend, OUTPUT_CAP, PromptCtx, build_prompt,
};
use crate::error::RuntimeError;

/// The largest raw device outcome a dispatcher masks.
///
/// Masking can expand a span into a longer marker, so the scratch
/// buffer is the worst case for this many bytes; the masked result must
/// still fit [`super::wire::MAX_OUTCOME`].
pub const MAX_RAW_OUTCOME: usize = 256;

/// Write `fault` as a failed activity's payload.
#[must_use]
pub fn fail(fault: Fault, out: &mut [u8]) -> Produced {
    out.first_mut().map_or(Produced::Failed(0), |byte| {
        *byte = fault.code();
        Produced::Failed(1)
    })
}

/// Answer an encoded value, or `fault` when it did not encode.
#[must_use]
pub fn produced(encoded: Result<usize, WireError>, fault: Fault, out: &mut [u8]) -> Produced {
    encoded.map_or_else(|_| fail(fault, out), Produced::Completed)
}

/// Map a backend failure to its recorded fault.
#[must_use]
pub const fn backend_fault(error: BackendError) -> Fault {
    match error {
        BackendError::Exhausted => Fault::ScriptExhausted,
        BackendError::OutputTooLong => Fault::OutputTooLong,
        BackendError::UnknownPrompt => Fault::UnknownPrompt,
        BackendError::PolicyMismatch => Fault::PolicyMismatch,
    }
}

/// Render the prompt an [`InferRequest`] describes into `out`,
/// returning its length. The seed's prompt cap bounds it.
///
/// # Errors
///
/// [`Fault::BadRequest`] when the brief does not decode or the cap
/// exceeds `out`.
pub fn render_prompt(request: &InferRequest<'_>, out: &mut [u8]) -> Result<usize, Fault> {
    let brief = match request.brief {
        Some(bytes) => Some(Brief::from_bytes(bytes).map_err(|_| Fault::BadRequest)?),
        None => None,
    };
    let ctx = PromptCtx {
        turns_left: request.turns_left,
        mutations_left: request.mutations_left,
        last_observation: request.last_observation,
        repair: request.repair,
        prior: request.prior,
        brief: brief.as_ref(),
    };
    let cap = usize::from(request.settings.max_prompt_bytes);
    let buf = out.get_mut(..cap).ok_or(Fault::BadRequest)?;
    Ok(build_prompt(&ctx, buf))
}

/// Serve one [`super::wire::INFER`] activity with `model`.
///
/// Renders the prompt, runs the inference into the seed's output cap,
/// and answers the line with the backend's measured usage and the
/// prompt's size.
#[must_use]
pub fn infer(model: &mut dyn ModelBackend, input: &[u8], out: &mut [u8]) -> Produced {
    let Ok(request) = InferRequest::decode(input) else {
        return fail(Fault::BadRequest, out);
    };
    let mut prompt = [0u8; BRIEFED_PROMPT_CAP];
    let prompt_len = match render_prompt(&request, &mut prompt) {
        Ok(len) => len,
        Err(fault) => return fail(fault, out),
    };
    let mut line = [0u8; OUTPUT_CAP];
    let cap = usize::from(request.settings.max_output_bytes).min(OUTPUT_CAP);
    let (Some(prompt), Some(line_buf)) = (prompt.get(..prompt_len), line.get_mut(..cap)) else {
        return fail(Fault::BadRequest, out);
    };
    let n = match model.infer(prompt, line_buf) {
        Ok(n) => n,
        Err(error) => return fail(backend_fault(error), out),
    };
    let usage = model.last_usage();
    let answer = line_buf.get(..n).and_then(|line| {
        Inferred::new(
            line,
            usage.input_tokens,
            usage.output_tokens,
            u32::try_from(prompt_len).ok()?,
        )
        .ok()
    });
    match answer.map(|answer| answer.encode(out)) {
        Some(Ok(len)) => Produced::Completed(len),
        _ => fail(Fault::OutputTooLong, out),
    }
}

/// Re-validate a [`ToolRequest`]'s argument bytes and bind them to the
/// typed dispatch shape. The dispatcher never trusts what it was handed.
///
/// # Errors
///
/// [`Fault::BadRequest`] for an unknown tool or arguments that do not
/// validate.
pub fn bind(request: &ToolRequest<'_>) -> Result<ToolArgs, Fault> {
    let tool = ToolId::new(request.tool);
    let entry = lookup_by_id(tool).ok_or(Fault::BadRequest)?;
    let bound = esper_protocol::validate(entry, request.args).map_err(|_| Fault::BadRequest)?;
    ToolArgs::bind(tool, &bound).map_err(|_| Fault::BadRequest)
}

/// Mask a device outcome and answer it (SPEC §20.2): the answer is the
/// durable record, so the raw bytes never reach the journal.
#[must_use]
pub fn observe(raw: &[u8], out: &mut [u8]) -> Produced {
    if raw.len() > MAX_RAW_OUTCOME {
        return fail(Fault::OutcomeTooLarge, out);
    }
    let mut masked = [0u8; mask_bound(MAX_RAW_OUTCOME)];
    let Ok(report) = mask_report(raw, &mut masked) else {
        return fail(Fault::OutcomeTooLarge, out);
    };
    let answer = masked
        .get(..report.output_len)
        .ok_or(WireError::Invalid)
        .and_then(|masked| Observed::completed(masked, report.secret_digest));
    produced(
        answer.and_then(|answer| answer.encode(out)),
        Fault::OutcomeTooLarge,
        out,
    )
}

/// Answer a transient attempt: the fault never reached the device.
#[must_use]
pub fn transient(out: &mut [u8]) -> Produced {
    produced(Observed::TRANSIENT.encode(out), Fault::OutcomeTooLarge, out)
}

/// Skip ASCII whitespace.
const fn skip_ws(mut bytes: &[u8]) -> &[u8] {
    while let [b' ' | b'\t' | b'\n' | b'\r', rest @ ..] = bytes {
        bytes = rest;
    }
    bytes
}

/// Validate typed operator input against the ask schema.
///
/// Schema 1 is pin select: exactly `{"pin": N}` with `N` in `0..=7`,
/// optional surrounding whitespace.
///
/// # Errors
///
/// A static reason when the schema is unsupported or the input does not
/// match it.
pub fn validate_ask_input(input: &[u8], schema: u8) -> Result<(), &'static str> {
    if schema != 1 {
        return Err("unsupported ask schema");
    }
    let err = "ask input must be {\"pin\": N} with N in 0..=7";
    let mut rest = skip_ws(input);
    rest = rest.strip_prefix(b"{").ok_or(err)?;
    rest = skip_ws(rest);
    rest = rest.strip_prefix(b"\"pin\"").ok_or(err)?;
    rest = skip_ws(rest);
    rest = rest.strip_prefix(b":").ok_or(err)?;
    rest = skip_ws(rest);
    let (digit, tail) = rest.split_first().ok_or(err)?;
    if !(b'0'..=b'7').contains(digit) || tail.first().is_some_and(u8::is_ascii_digit) {
        return Err(err);
    }
    rest = skip_ws(tail);
    rest = rest.strip_prefix(b"}").ok_or(err)?;
    if !skip_ws(rest).is_empty() {
        return Err(err);
    }
    Ok(())
}

/// Parse plain ASCII digits into a `u64`, failing closed on anything
/// else.
fn parse_u64_digits(raw: &[u8]) -> Option<u64> {
    if raw.is_empty() {
        return None;
    }
    let mut number: u64 = 0;
    for byte in raw {
        if !byte.is_ascii_digit() {
            return None;
        }
        number = number
            .checked_mul(10)?
            .checked_add(u64::from(byte - b'0'))?;
    }
    Some(number)
}

/// Parse the device's delay outcome `{"waited_ms":M,"uptime_ms":N}` into
/// the pre-dispatch reading `t0` (`N - M`) and the post-dispatch
/// reading `N`.
///
/// # Errors
///
/// [`RuntimeError::JournalCorrupt`] when the recorded outcome is not the
/// canonical delay shape.
pub fn parse_delay_outcome(outcome: &[u8]) -> Result<(u64, u64), RuntimeError> {
    use esper_protocol::json::{JsonValue, Parser};
    let mut parser = Parser::new(outcome);
    let JsonValue::Object(mut cursor) = parser
        .parse_value()
        .map_err(|_| RuntimeError::JournalCorrupt)?
    else {
        return Err(RuntimeError::JournalCorrupt);
    };
    let mut waited: Option<u64> = None;
    let mut uptime: Option<u64> = None;
    while let Some((key, value)) = cursor
        .next_entry()
        .map_err(|_| RuntimeError::JournalCorrupt)?
    {
        let JsonValue::Number(raw) = value else {
            return Err(RuntimeError::JournalCorrupt);
        };
        let number = parse_u64_digits(raw).ok_or(RuntimeError::JournalCorrupt)?;
        match key {
            b"waited_ms" => waited = Some(number),
            b"uptime_ms" => uptime = Some(number),
            _ => return Err(RuntimeError::JournalCorrupt),
        }
    }
    parser.finish().map_err(|_| RuntimeError::JournalCorrupt)?;
    let (Some(waited), Some(uptime)) = (waited, uptime) else {
        return Err(RuntimeError::JournalCorrupt);
    };
    let t0 = uptime
        .checked_sub(waited)
        .ok_or(RuntimeError::JournalCorrupt)?;
    Ok((t0, uptime))
}

/// What one tool attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// A transient fault: the device was never touched; a later
    /// attempt under the same effect may succeed.
    Transient,
    /// The device acted; its raw outcome is the first `len` bytes of
    /// the buffer it was handed.
    Done(usize),
}

/// The device ports a run's activities reach: the board's model, pins,
/// clock, operator channel and snapshot store.
///
/// Implementations do the physical work only. [`RunDispatcher`] owns the
/// rules that make an answer safe to record — decoding, argument
/// re-validation, masking — so a board cannot skip them.
pub trait RunPorts {
    /// The model backend.
    fn model(&mut self) -> &mut dyn ModelBackend;

    /// Perform one attempt of a committed intent, writing the raw
    /// outcome into `raw` (at most [`MAX_RAW_OUTCOME`] bytes).
    ///
    /// A redelivered `effect` must not act twice: return the first
    /// attempt's outcome.
    ///
    /// # Errors
    ///
    /// The [`Fault`] to record (`EffectArgsMismatch` when `effect` was
    /// seen with other arguments).
    fn tool(
        &mut self,
        effect: EffectId,
        tool: ToolId,
        digest: u64,
        args: ToolArgs,
        raw: &mut [u8],
    ) -> Result<Attempt, Fault>;

    /// Whether `pin` is configured as an output (SPEC §5.2's device
    /// rule for a write).
    fn pin_is_output(&mut self, pin: Pin) -> bool;

    /// The verifier's independent reading for `check`: the pin's level
    /// (`1` high) read directly, or the clock in milliseconds.
    fn read_back(&mut self, check: Check) -> u64;

    /// The operator's typed input for the run being driven, written into
    /// `out`, or `None` while none has arrived.
    ///
    /// # Errors
    ///
    /// [`Fault::BadInput`] when the input does not fit `out`.
    fn operator_input(&mut self, out: &mut [u8]) -> Result<Option<usize>, Fault>;

    /// Store one snapshot chunk under `(digest, index)`.
    ///
    /// # Errors
    ///
    /// The [`Fault`] to record when the store refuses.
    fn store_chunk(&mut self, digest: u64, index: u8, chunk: &[u8]) -> Result<(), Fault>;

    /// Load the snapshot chunk stored under `(digest, index)` into `out`.
    ///
    /// # Errors
    ///
    /// [`Fault::SnapshotMissing`] when there is none.
    fn load_chunk(&mut self, digest: u64, index: u8, out: &mut [u8]) -> Result<usize, Fault>;
}

/// The run activities' dispatcher, over a board's [`RunPorts`]. Product
/// code: the board, the emulated image and the host double all
/// dispatch through it.
pub struct RunDispatcher<'a, P: RunPorts + ?Sized> {
    ports: &'a mut P,
    /// The run being driven: the device deduplicates on `(run, seq)`.
    run: RunId,
}

impl<'a, P: RunPorts + ?Sized> RunDispatcher<'a, P> {
    /// A dispatcher for `run`'s activities over `ports`.
    pub const fn new(ports: &'a mut P, run: RunId) -> Self {
        Self { ports, run }
    }

    fn tool(&mut self, input: &[u8], out: &mut [u8]) -> Produced {
        let Ok(request) = ToolRequest::decode(input) else {
            return fail(Fault::BadRequest, out);
        };
        let args = match bind(&request) {
            Ok(args) => args,
            Err(fault) => return fail(fault, out),
        };
        let effect = EffectId {
            run: WaymakerRunId(self.run.get()),
            seq: EffectSeq(request.seq),
        };
        let mut raw = [0u8; MAX_RAW_OUTCOME];
        match self.ports.tool(
            effect,
            ToolId::new(request.tool),
            request.digest,
            args,
            &mut raw,
        ) {
            Ok(Attempt::Transient) => transient(out),
            Ok(Attempt::Done(len)) => match raw.get(..len) {
                Some(raw) => observe(raw, out),
                None => fail(Fault::OutcomeTooLarge, out),
            },
            Err(fault) => fail(fault, out),
        }
    }

    fn probe(&mut self, input: &[u8], out: &mut [u8]) -> Produced {
        let pin = match input {
            [pin] => Pin::new(*pin).ok(),
            _ => None,
        };
        let Some(pin) = pin else {
            return fail(Fault::BadRequest, out);
        };
        let output = self.ports.pin_is_output(pin);
        produced((Probed { output }).encode(out), Fault::BadRequest, out)
    }

    fn verify(&mut self, input: &[u8], out: &mut [u8]) -> Produced {
        let Ok(request) = VerifyRequest::decode(input) else {
            return fail(Fault::BadRequest, out);
        };
        let observed = self.ports.read_back(request.check);
        produced((Verified { observed }).encode(out), Fault::BadRequest, out)
    }

    fn ask(&mut self, input: &[u8], out: &mut [u8]) -> Produced {
        if AskRequest::decode(input).is_err() {
            return fail(Fault::BadRequest, out);
        }
        let mut buf = [0u8; MAX_ASK_INPUT];
        match self.ports.operator_input(&mut buf) {
            // No input yet: the activity stays outstanding, durably.
            Ok(None) => Produced::Unserviceable,
            Ok(Some(len)) => produced(
                buf.get(..len)
                    .ok_or(WireError::Invalid)
                    .and_then(AskInput::new)
                    .and_then(|answer| answer.encode(out)),
                Fault::BadInput,
                out,
            ),
            Err(fault) => fail(fault, out),
        }
    }

    fn snapshot(&mut self, kind: ActivityKind, input: &[u8], out: &mut [u8]) -> Produced {
        let Ok((digest, index, chunk)) = read_snapshot_key(input) else {
            return fail(Fault::BadRequest, out);
        };
        if kind == super::wire::SNAP_STORE {
            return match self.ports.store_chunk(digest, index, chunk) {
                Ok(()) => Produced::Completed(0),
                Err(fault) => fail(fault, out),
            };
        }
        let mut buf = [0u8; SNAPSHOT_CHUNK];
        match self.ports.load_chunk(digest, index, &mut buf) {
            Ok(len) => match (buf.get(..len), out.get_mut(..len)) {
                (Some(src), Some(dst)) => {
                    dst.copy_from_slice(src);
                    Produced::Completed(len)
                }
                _ => fail(Fault::BadRequest, out),
            },
            Err(fault) => fail(fault, out),
        }
    }
}

impl<P: RunPorts + ?Sized> ActivityDispatcher for RunDispatcher<'_, P> {
    type Error = core::convert::Infallible;

    fn poll_dispatch(
        &mut self,
        _task: &mut Context<'_>,
        _id: EffectId,
        kind: ActivityKind,
        input: &[u8],
        out: &mut [u8],
    ) -> Poll<Result<Produced, Self::Error>> {
        Poll::Ready(Ok(match kind {
            super::wire::INFER => infer(self.ports.model(), input, out),
            super::wire::PROBE => self.probe(input, out),
            super::wire::TOOL => self.tool(input, out),
            super::wire::VERIFY => self.verify(input, out),
            super::wire::ASK => self.ask(input, out),
            super::wire::SNAP_STORE | super::wire::SNAP_LOAD => self.snapshot(kind, input, out),
            _ => Produced::Unserviceable,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::wire::PAGE_RECORD_MAX;
    use waymaker_embassy::Decode;

    #[test]
    fn ask_schema_1_accepts_exact_pin_select() {
        assert!(validate_ask_input(br#"{"pin": 4}"#, 1).is_ok());
        assert!(validate_ask_input(b" { \"pin\" : 7 } ", 1).is_ok());
        assert!(validate_ask_input(b"{\"pin\":0}", 1).is_ok());
    }

    #[test]
    fn ask_schema_1_rejects_anything_else() {
        for bad in [
            br#"{"pin": 8}"#.as_slice(),
            br#"{"pin": 10}"#.as_slice(),
            br#"{"pin": -1}"#.as_slice(),
            br#"{"pin":}"#.as_slice(),
            br#"{"pin": 4, "extra": 1}"#.as_slice(),
            b"",
            b"pin 4",
        ] {
            assert!(validate_ask_input(bad, 1).is_err(), "{bad:?}");
        }
        assert!(validate_ask_input(br#"{"pin": 4}"#, 2).is_err());
        assert!(validate_ask_input(br#"{"pin": 4}"#, 0).is_err());
    }

    #[test]
    fn parse_delay_outcome_recovers_t0() {
        assert_eq!(
            parse_delay_outcome(br#"{"waited_ms":250,"uptime_ms":1250}"#).unwrap(),
            (1000, 1250)
        );
        assert!(parse_delay_outcome(br#"{"waited_ms":250}"#).is_err());
        assert!(parse_delay_outcome(br#"{"waited_ms":999,"uptime_ms":1}"#).is_err());
        assert!(parse_delay_outcome(b"not json").is_err());
    }

    #[test]
    fn parse_u64_digits_is_strict() {
        assert_eq!(parse_u64_digits(b"0"), Some(0));
        assert_eq!(parse_u64_digits(b"18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_u64_digits(b"18446744073709551616"), None);
        assert_eq!(parse_u64_digits(b""), None);
        assert_eq!(parse_u64_digits(b"12a"), None);
    }

    /// SPEC §20.1-20.2: a secret-bearing outcome is masked before it is
    /// answered — the record holds only the masked bytes and the
    /// secret's digest.
    #[test]
    fn an_observation_is_masked_before_it_is_answered() {
        let mut raw = Vec::new();
        for part in [
            &b"token="[..],
            b"sk",
            b"-live-",
            b"abc123def456xyz",
            b"; notify ",
        ] {
            raw.extend_from_slice(part);
        }
        raw.extend_from_slice(b"nurse@example.com");
        let mut out = [0u8; PAGE_RECORD_MAX];
        let Produced::Completed(n) = observe(&raw, &mut out) else {
            panic!("masking a small outcome must succeed");
        };
        let answer = Observed::decode(&out[..n]).unwrap();
        let mut expected = vec![0u8; mask_bound(raw.len())];
        let report = mask_report(&raw, &mut expected).unwrap();
        assert_eq!(answer.outcome(), &expected[..report.output_len]);
        assert_eq!(answer.secret_digest, report.secret_digest);
        assert_ne!(answer.secret_digest, 0);
        let text = core::str::from_utf8(answer.outcome()).unwrap();
        assert!(text.contains("[redacted:secret#1]"), "{text}");
        assert!(text.contains("[redacted:pii#2]"), "{text}");
        assert!(!text.contains("abc123def456xyz"), "{text}");
        assert!(!text.contains("nurse@example.com"), "{text}");
    }

    #[test]
    fn an_outcome_past_the_raw_cap_fails_closed() {
        let raw = [b'x'; MAX_RAW_OUTCOME + 1];
        let mut out = [0u8; PAGE_RECORD_MAX];
        assert_eq!(observe(&raw, &mut out), Produced::Failed(1));
        assert_eq!(Fault::from_payload(&out[..1]), Some(Fault::OutcomeTooLarge));
        let fits = [b'x'; MAX_RAW_OUTCOME];
        assert!(matches!(observe(&fits, &mut out), Produced::Completed(_)));
    }

    /// Masking can expand an outcome past the page: a raw outcome of
    /// short e-mail addresses grows past [`MAX_OUTCOME`] and fails
    /// closed rather than recording a truncated answer.
    #[test]
    fn an_outcome_that_masks_past_the_page_fails_closed() {
        let mut raw = Vec::new();
        while raw.len() + 7 <= MAX_RAW_OUTCOME {
            raw.extend_from_slice(b"a@b.co ");
        }
        let mut out = [0u8; PAGE_RECORD_MAX];
        assert_eq!(observe(&raw, &mut out), Produced::Failed(1));
        assert_eq!(Fault::from_payload(&out[..1]), Some(Fault::OutcomeTooLarge));
    }
}
