//! The run workflow's activity payloads, one Waymaker record each
//! (SPEC §22.2).
//!
//! Every input and every answer has a compile-time maximum no larger
//! than [`PAGE_RECORD_MAX`], so each record stages through one 512-byte
//! scratch page on the device. Encoders write into caller-owned
//! buffers; decoders read borrowed bytes or fill fixed-capacity owned
//! answers; nothing allocates.
//!
//! | Kind | Input | Answer |
//! |---|---|---|
//! | [`INFER`] | [`InferRequest`]: the prompt context, brief included | [`Inferred`]: one model line, measured usage, prompt size |
//! | [`PROBE`] | a pin | [`Probed`]: the pin's direction (the device business rule) |
//! | [`TOOL`] | [`ToolRequest`]: Esper effect id, attempt, tool, validated args | [`Observed`]: transient flag, masked outcome, secret digest |
//! | [`VERIFY`] | [`VerifyRequest`]: what the read-back must show | [`Verified`]: the independent reading |
//! | [`ASK`] | [`AskRequest`]: prompt and schema | [`AskInput`]: the operator's typed input |
//! | [`SNAP_STORE`] | snapshot key + one chunk | empty acknowledgement |
//! | [`SNAP_LOAD`] | snapshot key | one chunk |
//!
//! The run's own input — the [`RunStart`] — carries the seed, the brief,
//! and for a continued run a reference to its parent's fold. Terminal
//! payloads are [`TerminalRecord`]s. A failed activity carries one
//! [`Fault`] byte.

use esper_core::brief::{BRIEF_CAP, Brief};
use esper_core::state::TerminalStatus;
use waymaker_core::ActivityKind;
use waymaker_embassy::Decode;

use crate::agent::wire::{PAGE_RECORD_MAX, Reader, WireError, Writer};
use crate::backend::{InferenceSettings, OUTPUT_CAP, PriorCtx, RepairHint};
use crate::error::RuntimeError;
use crate::seed::{RunSeed, SEED_INPUT_LEN, SeedSnapshot};

/// Run one inference over the rendered prompt.
pub const INFER: ActivityKind = ActivityKind(0x0200);
/// Read a pin's direction: the device business rule a write must pass.
pub const PROBE: ActivityKind = ActivityKind(0x0201);
/// Dispatch one attempt of a committed tool intent.
pub const TOOL: ActivityKind = ActivityKind(0x0202);
/// The independent read-back every mutation must pass.
pub const VERIFY: ActivityKind = ActivityKind(0x0203);
/// Ask the operator for typed input; unserviceable until it arrives.
pub const ASK: ActivityKind = ActivityKind(0x0204);
/// Store one chunk of a rollover snapshot.
pub const SNAP_STORE: ActivityKind = ActivityKind(0x0205);
/// Load one chunk of a parent's rollover snapshot.
pub const SNAP_LOAD: ActivityKind = ActivityKind(0x0206);

/// The observation bytes the next prompt carries (SPEC §17.3).
pub const LAST_OBSERVATION_CAP: usize = 128;

/// The largest masked tool outcome one [`Observed`] answer carries. A
/// device outcome that masks wider fails closed ([`Fault::OutcomeTooLarge`]).
pub const MAX_OUTCOME: usize = 448;

/// The largest operator input one [`AskInput`] carries.
pub const MAX_ASK_INPUT: usize = 448;

/// The largest snapshot chunk one [`SNAP_STORE`] or [`SNAP_LOAD`] record
/// carries.
pub const SNAPSHOT_CHUNK: usize = 448;

/// The largest encoded [`InferRequest`].
pub const MAX_INFER_REQUEST_BYTES: usize =
    4 + 2 + 2 + (1 + 1 + LAST_OBSERVATION_CAP) + (1 + 2) + (1 + 7) + (1 + 2 + BRIEF_CAP);
/// The largest encoded [`Inferred`].
pub const MAX_INFERRED_BYTES: usize = 2 + OUTPUT_CAP + 4 + 4 + 4;
/// The largest encoded [`ToolRequest`].
pub const MAX_TOOL_REQUEST_BYTES: usize = 4 + 1 + 1 + 1 + 8 + 2 + OUTPUT_CAP;
/// The largest encoded [`Observed`].
pub const MAX_OBSERVED_BYTES: usize = 1 + 8 + 2 + MAX_OUTCOME;
/// The largest encoded [`VerifyRequest`].
pub const MAX_VERIFY_REQUEST_BYTES: usize = 4 + 1 + 8;
/// The largest encoded [`AskRequest`].
pub const MAX_ASK_REQUEST_BYTES: usize = 1 + 2 + OUTPUT_CAP;
/// The largest encoded snapshot key.
pub const SNAPSHOT_KEY_BYTES: usize = 8 + 1;
/// The largest encoded [`TerminalRecord`].
pub const MAX_TERMINAL_BYTES: usize = 1 + 1 + MAX_REASON + 2 + OUTPUT_CAP;
/// The largest encoded [`RunStart`].
pub const MAX_RUN_START_BYTES: usize = SEED_INPUT_LEN + (1 + 2 + BRIEF_CAP) + (1 + 8 + 2);

/// The longest machine-readable terminal reason.
pub const MAX_REASON: usize = 48;

const _: () = {
    assert!(MAX_INFER_REQUEST_BYTES <= PAGE_RECORD_MAX);
    assert!(MAX_INFERRED_BYTES <= PAGE_RECORD_MAX);
    assert!(MAX_TOOL_REQUEST_BYTES <= PAGE_RECORD_MAX);
    assert!(MAX_OBSERVED_BYTES <= PAGE_RECORD_MAX);
    assert!(MAX_VERIFY_REQUEST_BYTES <= PAGE_RECORD_MAX);
    assert!(MAX_ASK_REQUEST_BYTES <= PAGE_RECORD_MAX);
    assert!(SNAPSHOT_KEY_BYTES + SNAPSHOT_CHUNK <= PAGE_RECORD_MAX);
    assert!(2 + MAX_ASK_INPUT <= PAGE_RECORD_MAX);
    assert!(MAX_TERMINAL_BYTES <= PAGE_RECORD_MAX);
    assert!(MAX_RUN_START_BYTES <= PAGE_RECORD_MAX);
};

// ---------------------------------------------------------------------
// Faults

/// Why a dispatcher could not answer an activity. Carried as the one
/// payload byte of a failed activity, so replay fails the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The model has no more output (a scripted harness ran dry).
    ScriptExhausted,
    /// The model's line does not fit the output buffer.
    OutputTooLong,
    /// The tiny backend has no line for the prompt.
    UnknownPrompt,
    /// A distill entry failed its integrity check.
    PolicyMismatch,
    /// The device saw a committed effect id with different arguments.
    EffectArgsMismatch,
    /// The device has no such resource.
    UnknownResource,
    /// The masked outcome is wider than [`MAX_OUTCOME`].
    OutcomeTooLarge,
    /// The activity input did not decode.
    BadRequest,
    /// The operator input does not match the ask schema.
    BadInput,
    /// The snapshot chunk is missing.
    SnapshotMissing,
}

impl Fault {
    /// The wire code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::ScriptExhausted => 1,
            Self::OutputTooLong => 2,
            Self::UnknownPrompt => 3,
            Self::PolicyMismatch => 4,
            Self::EffectArgsMismatch => 5,
            Self::UnknownResource => 6,
            Self::OutcomeTooLarge => 7,
            Self::BadRequest => 8,
            Self::BadInput => 9,
            Self::SnapshotMissing => 10,
        }
    }

    /// The fault a recorded failure payload names; an empty or unknown
    /// payload is a dispatcher that failed without saying why.
    #[must_use]
    pub const fn from_payload(payload: &[u8]) -> Option<Self> {
        let [code] = payload else {
            return None;
        };
        Some(match *code {
            1 => Self::ScriptExhausted,
            2 => Self::OutputTooLong,
            3 => Self::UnknownPrompt,
            4 => Self::PolicyMismatch,
            5 => Self::EffectArgsMismatch,
            6 => Self::UnknownResource,
            7 => Self::OutcomeTooLarge,
            8 => Self::BadRequest,
            9 => Self::BadInput,
            10 => Self::SnapshotMissing,
            _ => return None,
        })
    }

    /// The runtime error a recorded fault surfaces as.
    #[must_use]
    pub const fn error(self) -> RuntimeError {
        use crate::backend::BackendError;
        match self {
            Self::ScriptExhausted => RuntimeError::World("model script exhausted"),
            Self::OutputTooLong => RuntimeError::Backend(BackendError::OutputTooLong),
            Self::UnknownPrompt => RuntimeError::Backend(BackendError::UnknownPrompt),
            Self::PolicyMismatch => RuntimeError::Backend(BackendError::PolicyMismatch),
            Self::EffectArgsMismatch => RuntimeError::ReplayDiverged,
            Self::UnknownResource => RuntimeError::IllegalTransition,
            Self::OutcomeTooLarge => RuntimeError::World("tool outcome exceeds the record page"),
            Self::BadRequest => RuntimeError::BadFrame,
            Self::BadInput => RuntimeError::World("ask input must be {\"pin\": N} with N in 0..=7"),
            Self::SnapshotMissing => RuntimeError::JournalCorrupt,
        }
    }
}

/// The error a failed activity's recorded payload surfaces as.
#[must_use]
pub const fn fault_error(payload: &[u8]) -> RuntimeError {
    match Fault::from_payload(payload) {
        Some(fault) => fault.error(),
        None => RuntimeError::World("dispatcher failed"),
    }
}

// ---------------------------------------------------------------------
// INFER

/// What the dispatcher needs to render the prompt and run one
/// inference. Self-contained: the dispatcher holds no run state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferRequest<'a> {
    /// The seed's prompt and output caps.
    pub settings: InferenceSettings,
    /// Model turns left before this inference.
    pub turns_left: u16,
    /// Mutations left.
    pub mutations_left: u16,
    /// The last committed observation, capped at
    /// [`LAST_OBSERVATION_CAP`].
    pub last_observation: Option<&'a [u8]>,
    /// The repair hint, once an invalid line burned a turn.
    pub repair: Option<RepairHint>,
    /// The folded-history marker for a continued run.
    pub prior: Option<PriorCtx>,
    /// The brief's bytes, for a supervised run.
    pub brief: Option<&'a [u8]>,
}

const fn repair_code(variant: &str) -> Option<u8> {
    match variant.as_bytes() {
        b"malformed" => Some(0),
        b"invalid_args" => Some(1),
        _ => None,
    }
}

const fn repair_name(code: u8) -> Option<&'static str> {
    match code {
        0 => Some("malformed"),
        1 => Some("invalid_args"),
        _ => None,
    }
}

impl<'a> InferRequest<'a> {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short;
    /// [`WireError::Invalid`] for an over-cap observation or brief, or
    /// an unknown repair variant.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        w.bytes(&self.settings.bytes())?;
        w.u16(self.turns_left)?;
        w.u16(self.mutations_left)?;
        match self.last_observation {
            Some(bytes) if bytes.len() <= LAST_OBSERVATION_CAP => {
                w.u8(1)?;
                w.u8(u8::try_from(bytes.len()).map_err(|_| WireError::Invalid)?)?;
                w.bytes(bytes)?;
            }
            Some(_) => return Err(WireError::Invalid),
            None => w.u8(0)?,
        }
        match self.repair {
            Some(hint) => {
                w.u8(1)?;
                w.u8(hint.attempt)?;
                w.u8(repair_code(hint.variant).ok_or(WireError::Invalid)?)?;
            }
            None => w.u8(0)?,
        }
        match self.prior {
            Some(prior) => {
                w.u8(1)?;
                w.u32(prior.epoch)?;
                w.u8(prior.failed_paths)?;
                w.u8(prior.pending)?;
                w.u8(prior.facts)?;
            }
            None => w.u8(0)?,
        }
        match self.brief {
            Some(bytes) if bytes.len() <= BRIEF_CAP => {
                w.u8(1)?;
                w.u16(u16::try_from(bytes.len()).map_err(|_| WireError::Invalid)?)?;
                w.bytes(bytes)?;
            }
            Some(_) => return Err(WireError::Invalid),
            None => w.u8(0)?,
        }
        Ok(w.len())
    }

    /// Decode, borrowing from `bytes`.
    ///
    /// # Errors
    ///
    /// [`WireError`] for malformed bytes.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let settings = InferenceSettings {
            max_prompt_bytes: r.u16()?,
            max_output_bytes: r.u16()?,
        };
        let turns_left = r.u16()?;
        let mutations_left = r.u16()?;
        let last_observation = if r.flag()? {
            let len = usize::from(r.u8()?);
            if len > LAST_OBSERVATION_CAP {
                return Err(WireError::Invalid);
            }
            Some(r.take(len)?)
        } else {
            None
        };
        let repair = if r.flag()? {
            let attempt = r.u8()?;
            let variant = repair_name(r.u8()?).ok_or(WireError::Invalid)?;
            Some(RepairHint { attempt, variant })
        } else {
            None
        };
        let prior = if r.flag()? {
            Some(PriorCtx {
                epoch: r.u32()?,
                failed_paths: r.u8()?,
                pending: r.u8()?,
                facts: r.u8()?,
            })
        } else {
            None
        };
        let brief = if r.flag()? {
            let len = usize::from(r.u16()?);
            if len > BRIEF_CAP {
                return Err(WireError::Invalid);
            }
            Some(r.take(len)?)
        } else {
            None
        };
        r.end()?;
        Ok(Self {
            settings,
            turns_left,
            mutations_left,
            last_observation,
            repair,
            prior,
            brief,
        })
    }
}

/// One inference's answer: the model's line, what the backend measured,
/// and the rendered prompt's size (the rollover meter's input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inferred {
    line: [u8; OUTPUT_CAP],
    len: u16,
    /// Input tokens the backend measured.
    pub input_tokens: u32,
    /// Output tokens the backend measured.
    pub output_tokens: u32,
    /// The rendered prompt's byte length.
    pub prompt_bytes: u32,
}

impl Inferred {
    /// An answer over `line`.
    ///
    /// # Errors
    ///
    /// [`WireError::Invalid`] when `line` exceeds [`OUTPUT_CAP`].
    pub fn new(
        line: &[u8],
        input_tokens: u32,
        output_tokens: u32,
        prompt_bytes: u32,
    ) -> Result<Self, WireError> {
        let mut buf = [0u8; OUTPUT_CAP];
        buf.get_mut(..line.len())
            .ok_or(WireError::Invalid)?
            .copy_from_slice(line);
        Ok(Self {
            line: buf,
            len: u16::try_from(line.len()).map_err(|_| WireError::Invalid)?,
            input_tokens,
            output_tokens,
            prompt_bytes,
        })
    }

    /// The model's line.
    #[must_use]
    pub fn line(&self) -> &[u8] {
        self.line.get(..usize::from(self.len)).unwrap_or(&[])
    }

    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        w.u16(self.len)?;
        w.bytes(self.line())?;
        w.u32(self.input_tokens)?;
        w.u32(self.output_tokens)?;
        w.u32(self.prompt_bytes)?;
        Ok(w.len())
    }
}

impl Decode for Inferred {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let len = usize::from(r.u16()?);
        if len > OUTPUT_CAP {
            return Err(WireError::Invalid);
        }
        let line = r.take(len)?;
        let answer = Self::new(line, r.u32()?, r.u32()?, r.u32()?)?;
        r.end()?;
        Ok(answer)
    }
}

// ---------------------------------------------------------------------
// PROBE

/// A pin's direction, as the device reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probed {
    /// Whether the pin is an output (else an input).
    pub output: bool,
}

impl Probed {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is empty.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        w.u8(u8::from(self.output))?;
        Ok(w.len())
    }
}

impl Decode for Probed {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let output = r.flag()?;
        r.end()?;
        Ok(Self { output })
    }
}

// ---------------------------------------------------------------------
// TOOL

/// One dispatch attempt of a committed intent. Every attempt of one
/// intent carries the same Esper effect id, so the device deduplicates
/// a redelivered attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolRequest<'a> {
    /// The Esper effect sequence (stable across attempts).
    pub seq: u32,
    /// The attempt, from 1.
    pub attempt: u8,
    /// The tool id.
    pub tool: u8,
    /// Whether the tool mutates the world.
    pub write: bool,
    /// The decode-time digest of `args`.
    pub digest: u64,
    /// The validated argument bytes.
    pub args: &'a [u8],
}

impl<'a> ToolRequest<'a> {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short;
    /// [`WireError::Invalid`] for args over [`OUTPUT_CAP`].
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        if self.args.len() > OUTPUT_CAP {
            return Err(WireError::Invalid);
        }
        let mut w = Writer::new(out);
        w.u32(self.seq)?;
        w.u8(self.attempt)?;
        w.u8(self.tool)?;
        w.u8(u8::from(self.write))?;
        w.u64(self.digest)?;
        w.u16(u16::try_from(self.args.len()).map_err(|_| WireError::Invalid)?)?;
        w.bytes(self.args)?;
        Ok(w.len())
    }

    /// Decode, borrowing from `bytes`.
    ///
    /// # Errors
    ///
    /// [`WireError`] for malformed bytes.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let seq = r.u32()?;
        let attempt = r.u8()?;
        let tool = r.u8()?;
        let write = r.flag()?;
        let digest = r.u64()?;
        let len = usize::from(r.u16()?);
        if len > OUTPUT_CAP || attempt == 0 {
            return Err(WireError::Invalid);
        }
        let args = r.take(len)?;
        r.end()?;
        Ok(Self {
            seq,
            attempt,
            tool,
            write,
            digest,
            args,
        })
    }
}

/// One attempt's answer. The dispatcher masks the outcome **before**
/// answering: the answer is the durable record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    /// The attempt hit a transient fault and never reached the device.
    pub transient: bool,
    /// FNV-1a-64 over the redacted secret bytes; `0` when none.
    pub secret_digest: u64,
    outcome: [u8; MAX_OUTCOME],
    len: u16,
}

impl Observed {
    /// A transient attempt: no outcome.
    pub const TRANSIENT: Self = Self {
        transient: true,
        secret_digest: 0,
        outcome: [0; MAX_OUTCOME],
        len: 0,
    };

    /// A completed attempt over an already-masked outcome.
    ///
    /// # Errors
    ///
    /// [`WireError::Invalid`] when `masked` exceeds [`MAX_OUTCOME`].
    pub fn completed(masked: &[u8], secret_digest: u64) -> Result<Self, WireError> {
        let mut outcome = [0u8; MAX_OUTCOME];
        outcome
            .get_mut(..masked.len())
            .ok_or(WireError::Invalid)?
            .copy_from_slice(masked);
        Ok(Self {
            transient: false,
            secret_digest,
            outcome,
            len: u16::try_from(masked.len()).map_err(|_| WireError::Invalid)?,
        })
    }

    /// The masked outcome.
    #[must_use]
    pub fn outcome(&self) -> &[u8] {
        self.outcome.get(..usize::from(self.len)).unwrap_or(&[])
    }

    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        w.u8(u8::from(self.transient))?;
        w.u64(self.secret_digest)?;
        w.u16(self.len)?;
        w.bytes(self.outcome())?;
        Ok(w.len())
    }
}

impl Decode for Observed {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let transient = r.flag()?;
        let secret_digest = r.u64()?;
        let len = usize::from(r.u16()?);
        if len > MAX_OUTCOME || (transient && (len != 0 || secret_digest != 0)) {
            return Err(WireError::Invalid);
        }
        let outcome = r.take(len)?;
        r.end()?;
        if transient {
            Ok(Self::TRANSIENT)
        } else {
            Self::completed(outcome, secret_digest)
        }
    }
}

// ---------------------------------------------------------------------
// VERIFY

/// What a mutation's independent read-back must show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// The pin's physical level.
    Pin {
        /// The pin.
        pin: u8,
        /// Whether it must read high.
        high: bool,
    },
    /// The clock must have reached `target_ms`.
    Clock {
        /// The target reading.
        target_ms: u64,
    },
}

impl Check {
    /// Whether `observed` (a level, `1` for high, or a clock reading)
    /// passes this check.
    #[must_use]
    pub const fn passes(self, observed: u64) -> bool {
        match self {
            Self::Pin { high, .. } => (observed != 0) == high,
            Self::Clock { target_ms } => observed >= target_ms,
        }
    }
}

/// One verification request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyRequest {
    /// The Esper effect sequence being verified.
    pub seq: u32,
    /// What the read-back must show.
    pub check: Check,
}

impl VerifyRequest {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        w.u32(self.seq)?;
        match self.check {
            Check::Pin { pin, high } => {
                w.u8(0)?;
                w.u64(u64::from(pin) | (u64::from(high) << 8))?;
            }
            Check::Clock { target_ms } => {
                w.u8(1)?;
                w.u64(target_ms)?;
            }
        }
        Ok(w.len())
    }

    /// Decode.
    ///
    /// # Errors
    ///
    /// [`WireError`] for malformed bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let seq = r.u32()?;
        let check = match r.u8()? {
            0 => {
                let packed = r.u64()?;
                let pin = u8::try_from(packed & 0xff).map_err(|_| WireError::Invalid)?;
                let high = match packed >> 8 {
                    0 => false,
                    1 => true,
                    _ => return Err(WireError::Invalid),
                };
                if pin >= 8 {
                    return Err(WireError::Invalid);
                }
                Check::Pin { pin, high }
            }
            1 => Check::Clock {
                target_ms: r.u64()?,
            },
            _ => return Err(WireError::Invalid),
        };
        r.end()?;
        Ok(Self { seq, check })
    }
}

/// The verifier's independent reading: a level (`1` high, `0` low) or
/// a clock reading in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    /// The reading.
    pub observed: u64,
}

impl Verified {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        w.u64(self.observed)?;
        Ok(w.len())
    }
}

impl Decode for Verified {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let observed = r.u64()?;
        r.end()?;
        Ok(Self { observed })
    }
}

// ---------------------------------------------------------------------
// ASK

/// A request for typed operator input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AskRequest<'a> {
    /// The response schema id.
    pub schema: u8,
    /// The prompt shown to the operator.
    pub prompt: &'a [u8],
}

impl<'a> AskRequest<'a> {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short;
    /// [`WireError::Invalid`] for a prompt over [`OUTPUT_CAP`].
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        if self.prompt.len() > OUTPUT_CAP {
            return Err(WireError::Invalid);
        }
        let mut w = Writer::new(out);
        w.u8(self.schema)?;
        w.u16(u16::try_from(self.prompt.len()).map_err(|_| WireError::Invalid)?)?;
        w.bytes(self.prompt)?;
        Ok(w.len())
    }

    /// Decode, borrowing from `bytes`.
    ///
    /// # Errors
    ///
    /// [`WireError`] for malformed bytes.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let schema = r.u8()?;
        let len = usize::from(r.u16()?);
        if len > OUTPUT_CAP {
            return Err(WireError::Invalid);
        }
        let prompt = r.take(len)?;
        r.end()?;
        Ok(Self { schema, prompt })
    }
}

/// The operator's typed input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AskInput {
    bytes: [u8; MAX_ASK_INPUT],
    len: u16,
}

impl AskInput {
    /// An answer over `input`.
    ///
    /// # Errors
    ///
    /// [`WireError::Invalid`] when `input` exceeds [`MAX_ASK_INPUT`].
    pub fn new(input: &[u8]) -> Result<Self, WireError> {
        let mut bytes = [0u8; MAX_ASK_INPUT];
        bytes
            .get_mut(..input.len())
            .ok_or(WireError::Invalid)?
            .copy_from_slice(input);
        Ok(Self {
            bytes,
            len: u16::try_from(input.len()).map_err(|_| WireError::Invalid)?,
        })
    }

    /// The input bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }

    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        w.u16(self.len)?;
        w.bytes(self.as_bytes())?;
        Ok(w.len())
    }
}

impl Decode for AskInput {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let len = usize::from(r.u16()?);
        if len > MAX_ASK_INPUT {
            return Err(WireError::Invalid);
        }
        let answer = Self::new(r.take(len)?)?;
        r.end()?;
        Ok(answer)
    }
}

// ---------------------------------------------------------------------
// Snapshot chunks

/// The key of one snapshot chunk: the snapshot's digest and the chunk
/// index.
#[must_use]
pub const fn snapshot_key(digest: u64, index: u8) -> [u8; SNAPSHOT_KEY_BYTES] {
    let mut out = [0u8; SNAPSHOT_KEY_BYTES];
    let bytes = digest.to_le_bytes();
    let mut i = 0;
    while i < 8 {
        out[i] = bytes[i];
        i += 1;
    }
    out[8] = index;
    out
}

/// Split a [`SNAP_STORE`] input into its key and chunk.
///
/// # Errors
///
/// [`WireError`] for a short key or an over-long chunk.
pub fn read_snapshot_key(bytes: &[u8]) -> Result<(u64, u8, &[u8]), WireError> {
    let mut r = Reader::new(bytes);
    let digest = r.u64()?;
    let index = r.u8()?;
    let rest = bytes.get(SNAPSHOT_KEY_BYTES..).unwrap_or(&[]);
    if rest.len() > SNAPSHOT_CHUNK {
        return Err(WireError::Invalid);
    }
    Ok((digest, index, rest))
}

/// One loaded snapshot chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    bytes: [u8; SNAPSHOT_CHUNK],
    len: u16,
}

impl Chunk {
    /// The chunk's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

impl Decode for Chunk {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut out = [0u8; SNAPSHOT_CHUNK];
        out.get_mut(..bytes.len())
            .ok_or(WireError::Invalid)?
            .copy_from_slice(bytes);
        Ok(Self {
            bytes: out,
            len: u16::try_from(bytes.len()).map_err(|_| WireError::Invalid)?,
        })
    }
}

/// An empty acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stored;

impl Decode for Stored {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.is_empty() {
            Ok(Self)
        } else {
            Err(WireError::Trailing)
        }
    }
}

// ---------------------------------------------------------------------
// Terminal payloads

/// A run's terminal record: status, machine-readable reason, summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalRecord<'a> {
    /// The terminal status.
    pub status: TerminalStatus,
    /// The machine-readable reason (at most [`MAX_REASON`] bytes).
    pub reason: &'a [u8],
    /// The summary (at most [`OUTPUT_CAP`] bytes).
    pub summary: &'a [u8],
}

impl<'a> TerminalRecord<'a> {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short;
    /// [`WireError::Invalid`] for an over-long reason or summary.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        if self.reason.len() > MAX_REASON || self.summary.len() > OUTPUT_CAP {
            return Err(WireError::Invalid);
        }
        let mut w = Writer::new(out);
        w.u8(self.status.code())?;
        w.u8(u8::try_from(self.reason.len()).map_err(|_| WireError::Invalid)?)?;
        w.bytes(self.reason)?;
        w.u16(u16::try_from(self.summary.len()).map_err(|_| WireError::Invalid)?)?;
        w.bytes(self.summary)?;
        Ok(w.len())
    }

    /// Decode, borrowing from `bytes`.
    ///
    /// # Errors
    ///
    /// [`WireError`] for malformed bytes or an unknown status.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let status = TerminalStatus::from_code(r.u8()?)?;
        let reason_len = usize::from(r.u8()?);
        if reason_len > MAX_REASON {
            return Err(WireError::Invalid);
        }
        let reason = r.take(reason_len)?;
        let summary_len = usize::from(r.u16()?);
        if summary_len > OUTPUT_CAP {
            return Err(WireError::Invalid);
        }
        let summary = r.take(summary_len)?;
        r.end()?;
        Ok(Self {
            status,
            reason,
            summary,
        })
    }
}

// ---------------------------------------------------------------------
// The run's input

/// Where a continued run's parent fold is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRef {
    /// FNV-1a-64 over the encoded snapshot: its store key and its
    /// integrity check.
    pub digest: u64,
    /// The encoded snapshot's length.
    pub len: u16,
}

impl SnapshotRef {
    /// The number of [`SNAPSHOT_CHUNK`]-sized chunks.
    #[must_use]
    pub const fn chunks(&self) -> usize {
        (self.len as usize).div_ceil(SNAPSHOT_CHUNK)
    }
}

/// A run's Waymaker input: the seed (its 151-byte binding), the brief,
/// and — for a continued run — the parent fold's reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunStart {
    /// The run's seed.
    pub seed: RunSeed,
    /// The parent fold, for a continued run.
    pub snapshot: Option<SnapshotRef>,
}

impl RunStart {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        w.bytes(&self.seed.snapshot().input_bytes())?;
        match self.seed.brief.as_ref() {
            Some(brief) => {
                let bytes = brief.as_bytes();
                w.u8(1)?;
                w.u16(u16::try_from(bytes.len()).map_err(|_| WireError::Invalid)?)?;
                w.bytes(bytes)?;
            }
            None => w.u8(0)?,
        }
        match self.snapshot {
            Some(snapshot) => {
                w.u8(1)?;
                w.u64(snapshot.digest)?;
                w.u16(snapshot.len)?;
            }
            None => w.u8(0)?,
        }
        Ok(w.len())
    }

    /// Decode and re-validate the binding: the brief must hash to the
    /// digest the seed bytes bind.
    ///
    /// # Errors
    ///
    /// [`WireError`] for malformed bytes, a malformed brief, or a brief
    /// that does not match the bound digest.
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let snapshot = SeedSnapshot::from_input_bytes(&r.array::<SEED_INPUT_LEN>()?)
            .ok_or(WireError::Invalid)?;
        let brief = if r.flag()? {
            let len = usize::from(r.u16()?);
            Some(Brief::from_bytes(r.take(len)?)?)
        } else {
            None
        };
        if snapshot.brief_digest != brief.as_ref().map(Brief::digest) {
            return Err(WireError::Invalid);
        }
        let fold = if r.flag()? {
            Some(SnapshotRef {
                digest: r.u64()?,
                len: r.u16()?,
            })
        } else {
            None
        };
        r.end()?;
        Ok(Self {
            seed: RunSeed::from_snapshot(&snapshot, brief),
            snapshot: fold,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_request_round_trips_at_its_maximum() {
        let obs = [b'o'; LAST_OBSERVATION_CAP];
        let brief = [b'b'; BRIEF_CAP];
        let request = InferRequest {
            settings: InferenceSettings::briefed_settings(),
            turns_left: 9,
            mutations_left: 4,
            last_observation: Some(&obs),
            repair: Some(RepairHint {
                attempt: 2,
                variant: "invalid_args",
            }),
            prior: Some(PriorCtx {
                epoch: 7,
                failed_paths: 1,
                pending: 2,
                facts: 3,
            }),
            brief: Some(&brief),
        };
        let mut buf = [0u8; PAGE_RECORD_MAX];
        let n = request.encode(&mut buf).unwrap();
        assert_eq!(n, MAX_INFER_REQUEST_BYTES);
        assert_eq!(InferRequest::decode(&buf[..n]).unwrap(), request);
    }

    #[test]
    fn answers_round_trip_at_their_maximum() {
        let mut buf = [0u8; PAGE_RECORD_MAX];
        let inferred = Inferred::new(&[b'x'; OUTPUT_CAP], 1, 2, 3).unwrap();
        let n = inferred.encode(&mut buf).unwrap();
        assert_eq!(n, MAX_INFERRED_BYTES);
        assert_eq!(Inferred::decode(&buf[..n]).unwrap(), inferred);

        let observed = Observed::completed(&[b'y'; MAX_OUTCOME], 9).unwrap();
        let n = observed.encode(&mut buf).unwrap();
        assert_eq!(n, MAX_OBSERVED_BYTES);
        assert_eq!(Observed::decode(&buf[..n]).unwrap(), observed);

        let n = Observed::TRANSIENT.encode(&mut buf).unwrap();
        assert_eq!(Observed::decode(&buf[..n]).unwrap(), Observed::TRANSIENT);

        let input = AskInput::new(&[b'z'; MAX_ASK_INPUT]).unwrap();
        let n = input.encode(&mut buf).unwrap();
        assert_eq!(AskInput::decode(&buf[..n]).unwrap(), input);
    }

    #[test]
    fn tool_and_ask_requests_round_trip_at_their_maximum() {
        let mut buf = [0u8; PAGE_RECORD_MAX];
        let args = [b'a'; OUTPUT_CAP];
        let request = ToolRequest {
            seq: 3,
            attempt: 2,
            tool: 2,
            write: true,
            digest: 77,
            args: &args,
        };
        let n = request.encode(&mut buf).unwrap();
        assert_eq!(n, MAX_TOOL_REQUEST_BYTES);
        assert_eq!(ToolRequest::decode(&buf[..n]).unwrap(), request);

        let ask = AskRequest {
            schema: 1,
            prompt: &args,
        };
        let n = ask.encode(&mut buf).unwrap();
        assert_eq!(n, MAX_ASK_REQUEST_BYTES);
        assert_eq!(AskRequest::decode(&buf[..n]).unwrap(), ask);
    }

    #[test]
    fn verify_requests_round_trip() {
        let mut buf = [0u8; MAX_VERIFY_REQUEST_BYTES];
        for check in [
            Check::Pin { pin: 7, high: true },
            Check::Pin {
                pin: 0,
                high: false,
            },
            Check::Clock { target_ms: 1250 },
        ] {
            let request = VerifyRequest { seq: 4, check };
            let n = request.encode(&mut buf).unwrap();
            assert_eq!(VerifyRequest::decode(&buf[..n]).unwrap(), request);
        }
        assert!(Check::Clock { target_ms: 10 }.passes(10));
        assert!(!Check::Clock { target_ms: 10 }.passes(9));
        assert!(Check::Pin { pin: 1, high: true }.passes(1));
        assert!(!Check::Pin { pin: 1, high: true }.passes(0));
    }

    #[test]
    fn terminal_records_round_trip_at_their_maximum() {
        let mut buf = [0u8; PAGE_RECORD_MAX];
        let reason = [b'r'; MAX_REASON];
        let summary = [b's'; OUTPUT_CAP];
        let record = TerminalRecord {
            status: TerminalStatus::Stuck,
            reason: &reason,
            summary: &summary,
        };
        let n = record.encode(&mut buf).unwrap();
        assert_eq!(n, MAX_TERMINAL_BYTES);
        assert_eq!(TerminalRecord::decode(&buf[..n]).unwrap(), record);
    }

    #[test]
    fn faults_round_trip_and_unknown_payloads_are_dispatcher_failures() {
        for code in 1..=10u8 {
            let fault = Fault::from_payload(&[code]).unwrap();
            assert_eq!(fault.code(), code);
        }
        assert_eq!(Fault::from_payload(&[]), None);
        assert_eq!(Fault::from_payload(&[0]), None);
        assert!(matches!(fault_error(&[]), RuntimeError::World(_)));
    }

    #[test]
    fn a_tampered_brief_does_not_decode() {
        let objective = esper_core::compact::Note::from_bytes(b"keep pin 3 low").unwrap();
        let memory = esper_core::memory::Memory::new();
        let brief = Brief::compose(
            &objective,
            &esper_core::memory::recall(&memory, esper_core::goal::GoalId::MIN),
        );
        let start = RunStart {
            seed: RunSeed {
                brief: Some(brief),
                ..RunSeed::default_slice()
            },
            snapshot: None,
        };
        let mut buf = [0u8; MAX_RUN_START_BYTES];
        let n = start.encode(&mut buf).unwrap();
        assert_eq!(RunStart::decode(&buf[..n]).unwrap(), start);
        // Flip a byte inside the brief text: the bound digest no longer
        // matches.
        buf[SEED_INPUT_LEN + 3 + 2] ^= 0x01;
        assert!(RunStart::decode(&buf[..n]).is_err());
    }
}
