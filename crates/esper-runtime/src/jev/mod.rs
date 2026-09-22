//! The Jev host-backend adapter: `TypeSafe` AI's `System One` model behind
//! [`ModelBackend`].
//!
//! Jev generates no text. One `POST` to the System One endpoint carries
//! the application state plus typed questions, and one parallel pass
//! returns typed, probabilistic decisions: a [`Choice`] (one option
//! from up to 255), a [`Score`] (a position on an ordered rubric), and
//! a [`Noul`] (a yes/no probability). Valid outputs are fixed by the
//! caller's schema, so Jev cannot emit anything outside them — there
//! is no JSON to repair and no free-form line for the decoder to
//! reject.
//!
//! The adapter maps one `ReAct` turn to one Jev call (§19):
//!
//! - `decision`: a `Choice` over the turn's eight-option decision
//!   space — one `Candidate` per catalog tool plus `ask` and
//!   `finish`.
//! - `should_ask`: a `Noul` gate. At or above [`NOUL_GATE_BPS`] the
//!   adapter overrides the choice with the `ask` candidate: the run
//!   cannot proceed safely without human input.
//! - `confidence`: a `Score` over four ordered levels. The score is
//!   recorded on every [`JevReceipt`]; the [`ScoreBand`] mapping it to
//!   the deterministic monitor's escalation policy is documented in
//!   SPEC §19.3 (monitor wiring is a later rung).
//!
//! The cascade (§19.4): Jev selects *which* decision to take, and
//! deterministic arg templates fill the concrete tool arguments from
//! the turn's last observation (pin and sensor numbers) with fixed
//! fallbacks. Jev never sees or emits a pin number, a URL, or a JSON
//! blob — every concrete value in an emitted line comes from the
//! caller-enumerated template. A decision outside the template space
//! is inexpressible through this backend, loudly, by construction.
//!
//! Model-bundle identity (§19.5): [`JevBackend::bundle_id`] digests the
//! adapter tag, the Jev model version string, and the endpoint, so the
//! seed binds the exact model that answered. Every turn's returned
//! probabilities are kept on the backend's [`JevReceipt`] log.
//! Jev answers are reproducible to about 0.02, not bit-identical —
//! the receipts record what was actually returned.
//!
//! Host-only: this module is a network client. It lives behind the
//! `host` feature and never enters the `no_std`/`no_alloc` crates or
//! the firmware story. Two transports ship: [`MockTransport`]
//! replays recorded API responses (the cassette format, with a
//! record mode for authoring), and [`LiveTransport`] performs the
//! real HTTPS `POST` when built with a bearer key — the key comes
//! from secure storage at the call site, never from the library
//! itself. [`LiveTransport::unconfigured`] builds the keyless form,
//! which fails every call with [`JevError::LiveDeferred`]. The wire
//! shape (top-level `state`/`model`/`questions`) and the response
//! shapes were verified against the live API on 2026-09-22; see
//! SPEC §19.9 for the measured evidence.
//!
//! [`ModelBackend`]: crate::backend::ModelBackend
//! [`Choice`]: https://docs.typesafe.ai/api#choice
//! [`Score`]: https://docs.typesafe.ai/api#score
//! [`Noul`]: https://docs.typesafe.ai/api#noul

// HOST-ONLY (E3+jev): a network client; never compiled without `host`.
#![cfg(feature = "host")]

mod https;

use std::collections::HashMap;

use thiserror::Error;
use zeroize::Zeroize;

use crate::backend::{BackendError, BundleId, ModelBackend, TokenUsage, fnv1a64};
use esper_protocol::json::{JsonValue, Parser, parse_u8};

/// The System One endpoint, from the Jev API docs.
pub const JEV_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

/// The pinned Jev model version.
///
/// Pins the version so the bundle digest — and any evaluation —
/// is reproducible. The value (`jev-1.13.0`) is what public material
/// resolved `jev-latest` to (2026-09-18), and a live call on
/// 2026-09-22 answered as exactly this version, so the pin is
/// confirmed against the API itself. A version change is a
/// different bundle by construction.
pub const JEV_MODEL_VERSION: &str = "jev-1.13.0";

/// The `Noul` gate threshold in basis points: at or above 0.50 the
/// adapter overrides the choice with `ask`.
pub const NOUL_GATE_BPS: u16 = 5_000;

/// The adapter's domain tag, mixed into every bundle digest.
const BUNDLE_TAG: &[u8] = b"esper-jev-v1";

/// A probability in basis points (`0..=10_000`), so the receipts stay
/// integer, deterministic, and hashable. Jev answers are reproducible
/// to about 0.02 (200 bps), not bit-identical — see SPEC §19.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Prob(u16);

impl Prob {
    /// Build from basis points; `None` when out of range.
    #[must_use]
    pub const fn from_bps(bps: u16) -> Option<Self> {
        if bps <= 10_000 { Some(Self(bps)) } else { None }
    }

    /// Build from a 0.0–1.0 float; `None` when out of range or NaN.
    #[must_use]
    pub fn from_f64(prob: f64) -> Option<Self> {
        if !(0.0..=1.0).contains(&prob) {
            return None;
        }
        // `scaled` is in `0.0..=10000.0`: the saturating `as` cast is
        // exact on this range, and `from_bps` re-checks the bound.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bps = (prob * 10_000.0).round() as u16;
        Self::from_bps(bps)
    }

    /// The value in basis points.
    #[must_use]
    pub const fn as_bps(self) -> u16 {
        self.0
    }
}

/// The confidence bands the `Score` answer maps to (§19.3). The
/// deterministic monitor does not consume these yet; the mapping is
/// the documented escalation policy for a later rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreBand {
    /// Below 1.5: route the turn to human review.
    Review,
    /// 1.5–3.0: proceed, flagged for extra scrutiny.
    Caution,
    /// Above 3.0: act on the decision.
    Act,
}

/// Map a fractional score (thousandths of a level, 1000–4000) to its
/// band. Out-of-range scores fail closed to [`ScoreBand::Review`].
#[must_use]
pub const fn score_band(score_milli: u16) -> ScoreBand {
    if score_milli < 1_500 {
        ScoreBand::Review
    } else if score_milli <= 3_000 {
        ScoreBand::Caution
    } else {
        ScoreBand::Act
    }
}

/// One turn's decision space: the eight options of the `decision`
/// `Choice`. The option *names* are the schema Jev selects from; the
/// *lines* are rendered by deterministic arg templates (§19.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Candidate {
    ReadPin,
    WritePin,
    SampleSensor,
    DelayWait,
    UptimeRead,
    StatusReport,
    Ask,
    Finish,
}

/// The decision space in `Choice` order. The order is the contract:
/// cassette indices and probability vectors are positional.
const CANDIDATES: [Candidate; 8] = [
    Candidate::ReadPin,
    Candidate::WritePin,
    Candidate::SampleSensor,
    Candidate::DelayWait,
    Candidate::UptimeRead,
    Candidate::StatusReport,
    Candidate::Ask,
    Candidate::Finish,
];

impl Candidate {
    /// The option name as it appears in the `Choice` schema.
    const fn option_name(self) -> &'static str {
        match self {
            Self::ReadPin => "call_gpio_pin_read",
            Self::WritePin => "call_gpio_pin_write",
            Self::SampleSensor => "call_sensor_sample_read",
            Self::DelayWait => "call_timer_delay_wait",
            Self::UptimeRead => "call_timer_uptime_read",
            Self::StatusReport => "call_device_status_report",
            Self::Ask => "ask",
            Self::Finish => "finish",
        }
    }

    /// The human-readable option description sent to Jev.
    const fn description(self) -> &'static str {
        match self {
            Self::ReadPin => "Read the level of a GPIO pin.",
            Self::WritePin => "Drive a GPIO pin high.",
            Self::SampleSensor => "Sample a sensor.",
            Self::DelayWait => "Wait 100 milliseconds.",
            Self::UptimeRead => "Read the monotonic uptime clock.",
            Self::StatusReport => "Report device status.",
            Self::Ask => "Ask the operator for input.",
            Self::Finish => "Finish the run.",
        }
    }

    /// Find a candidate by option name.
    fn by_name(name: &[u8]) -> Option<Self> {
        CANDIDATES
            .iter()
            .find(|candidate| candidate.option_name().as_bytes() == name)
            .copied()
    }

    /// The candidate's position in `CANDIDATES`.
    fn index(self) -> u8 {
        let position = CANDIDATES
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0);
        // Eight candidates: the position always fits `u8`.
        #[allow(clippy::cast_possible_truncation)]
        let index = position as u8;
        index
    }

    /// Render the decision line. `pin` and `sensor` come from
    /// [`derive_hints`]; every other argument is a fixed template
    /// (§19.4). The rendering is byte-exact and always decodable —
    /// Jev cannot produce a malformed line through this backend.
    fn render_line(self, hints: ArgHints) -> String {
        match self {
            Self::ReadPin => format!("CALL gpio_pin_read {{\"pin\": {}}}", hints.pin),
            Self::WritePin => format!(
                "CALL gpio_pin_write {{\"pin\": {}, \"level\": \"high\"}}",
                hints.pin
            ),
            Self::SampleSensor => {
                format!("CALL sensor_sample_read {{\"sensor\": {}}}", hints.sensor)
            }
            Self::DelayWait => "CALL timer_delay_wait {\"ms\": 100}".to_string(),
            Self::UptimeRead => "CALL timer_uptime_read {}".to_string(),
            Self::StatusReport => "CALL device_status_report {\"detail\": \"summary\"}".to_string(),
            Self::Ask => {
                "ASK {\"prompt\": \"Operator input is needed to continue.\", \"schema\": 1}"
                    .to_string()
            }
            Self::Finish => {
                "FINISH {\"status\": \"completed\", \"summary\": \"Run complete.\"}".to_string()
            }
        }
    }
}

/// The concrete values the arg templates may fill: a pin and a sensor
/// number, derived from the turn's last observation, defaulting to 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArgHints {
    pin: u8,
    sensor: u8,
}

/// Extract the `LAST` observation line from a rendered prompt. The
/// prompt is the engine's deterministic format (`TOOLS` block, `STATE`
/// line, optional `REPAIR` line, optional `LAST` line, `EMIT`
/// trailer); only the `LAST` line matters here — the rest of the
/// prompt travels to Jev verbatim as the call's `state`.
fn last_observation_line(prompt: &[u8]) -> Option<&[u8]> {
    for line in prompt.split(|byte| *byte == b'\n') {
        if let Some(observation) = line.strip_prefix(b"LAST ") {
            return Some(observation);
        }
    }
    None
}

/// Derive the arg hints from the turn's last observation. Parses the
/// observation JSON for numeric `pin`/`sensor` fields; anything
/// unparseable (or absent) falls back to 0. The templates never emit
/// what the observation did not say — a string `"4"` or a missing
/// field is not a number.
fn derive_hints(prompt: &[u8]) -> ArgHints {
    let mut hints = ArgHints { pin: 0, sensor: 0 };
    let Some(observation) = last_observation_line(prompt) else {
        return hints;
    };
    // A truncated observation is partial evidence: the marker means
    // the world said more than we kept. The templates do not act on
    // partial evidence, even when the surviving prefix parses.
    if observation.ends_with(b"[truncated]") {
        return hints;
    }
    let mut parser = Parser::new(observation);
    let Ok(JsonValue::Object(mut object)) = parser.parse_value() else {
        return hints;
    };
    while let Ok(Some((key, value))) = object.next_entry() {
        match (key, value) {
            (b"pin", JsonValue::Number(digits)) => {
                if let Ok(pin) = parse_u8(digits) {
                    hints.pin = pin;
                }
            }
            (b"sensor", JsonValue::Number(digits)) => {
                if let Ok(sensor) = parse_u8(digits) {
                    hints.sensor = sensor;
                }
            }
            _ => {}
        }
    }
    hints
}

/// Push a JSON string literal with escaping. The prompt is ASCII in
/// practice; control bytes use `\u00XX`, and anything else passes
/// through byte-identical so the encoding stays deterministic.
fn push_escaped(out: &mut Vec<u8>, text: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    for &byte in text {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x00..=0x1F => {
                out.extend_from_slice(b"\\u00");
                out.push(HEX[usize::from(byte >> 4)]);
                out.push(HEX[usize::from(byte & 0x0F)]);
            }
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

/// Build the `System One` request JSON for one turn.
///
/// The prompt travels as `state`, plus the pinned model version and
/// the three typed questions — `decision` (`Choice` over
/// `CANDIDATES`), `should_ask` (`Noul`), `confidence` (`Score`).
/// Field order is fixed so the bytes — and therefore the mock
/// cassette keys — are deterministic.
#[must_use]
pub fn build_request_json(prompt: &[u8], model_version: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"state\":");
    push_escaped(&mut out, prompt);
    out.extend_from_slice(b",\"model\":");
    push_escaped(&mut out, model_version.as_bytes());
    out.extend_from_slice(
        b",\"questions\":{\"decision\":{\"type\":\"choice\",\"instructions\":\"Select the next ReAct turn decision.\",\"options\":{",
    );
    for (index, candidate) in CANDIDATES.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        push_escaped(&mut out, candidate.option_name().as_bytes());
        out.push(b':');
        push_escaped(&mut out, candidate.description().as_bytes());
    }
    out.extend_from_slice(
        b"}},\"should_ask\":{\"type\":\"noul\",\"instructions\":\"The run cannot proceed safely without human input.\"},",
    );
    out.extend_from_slice(
        b"\"confidence\":{\"type\":\"score\",\"instructions\":\"Rate the confidence of the selected decision.\",\"levels\":{\"1\":\"guessing\",\"2\":\"uncertain\",\"3\":\"confident\",\"4\":\"certain\"}}}}",
    );
    out
}

/// One Jev answer: the parsed, validated content of a System One
/// response's `answers` object. Probabilities are basis points; the
/// score is thousandths of a rubric level (1000–4000).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JevAnswer {
    /// The chosen option's position in `CANDIDATES`, before gating.
    pub raw_choice: u8,
    /// The emitted option's position, after the [`NOUL_GATE_BPS`]
    /// gate may have overridden it with `ask`.
    pub choice: u8,
    /// Whether the `Noul` gate overrode the choice.
    pub gated: bool,
    /// The `Choice` distribution, in `CANDIDATES` order.
    pub choice_probs_bps: [u16; 8],
    /// P(the run cannot proceed safely without human input).
    pub noul_p_bps: u16,
    /// The fractional `Score`, thousandths of a level.
    pub score_milli: u16,
    /// The per-level `Score` distribution, levels 1–4.
    pub score_probs_bps: [u16; 4],
}

/// A JSON value, parsed by the adapter's own host-side reader. The
/// `no_std` protocol parser caps nesting at [`MAX_JSON_DEPTH`][esper_protocol::json::MAX_JSON_DEPTH]
/// for the flat decision grammar; a System One response nests four
/// deep, so the adapter parses it here instead of stretching the
/// protocol crate's denial-of-service bounds.
#[derive(Debug, Clone, PartialEq)]
enum JVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Self>),
    Obj(Vec<(String, Self)>),
}

impl JVal {
    /// The value as an object entry list.
    fn as_obj(&self) -> Option<&[(String, Self)]> {
        if let Self::Obj(entries) = self {
            Some(entries)
        } else {
            None
        }
    }
}

/// The deepest nesting the adapter's reader accepts. Four covers the
/// documented response shape with headroom; deeper input is
/// malformed, not a real answer.
const MAX_RESPONSE_DEPTH: usize = 8;

/// The adapter's host-side JSON reader: strict enough for fail-closed
/// parsing, small enough to read in one sitting.
struct JReader<'a> {
    buf: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> JReader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    /// Parse one value; the reader must be positioned at its first
    /// byte.
    fn parse_value(&mut self) -> Result<JVal, JevError> {
        self.skip_ws();
        let byte = self.peek().ok_or(JevError::BadResponse)?;
        match byte {
            b'n' => self.parse_literal(b"null", JVal::Null),
            b't' => self.parse_literal(b"true", JVal::Bool(true)),
            b'f' => self.parse_literal(b"false", JVal::Bool(false)),
            b'"' => Ok(JVal::Str(self.parse_string()?)),
            b'[' => self.parse_array(),
            b'{' => self.parse_object(),
            b'-' | b'0'..=b'9' => {
                let (number, _) = self.parse_number()?;
                Ok(JVal::Num(number))
            }
            _ => Err(JevError::BadResponse),
        }
    }

    fn parse_literal(&mut self, literal: &[u8], value: JVal) -> Result<JVal, JevError> {
        for expected in literal {
            match self.bump() {
                Some(byte) if byte == *expected => {}
                _ => return Err(JevError::BadResponse),
            }
        }
        Ok(value)
    }

    /// Close a composite: the closing bracket is already consumed.
    const fn finish_array(&mut self, items: Vec<JVal>) -> JVal {
        self.depth -= 1;
        JVal::Arr(items)
    }

    fn parse_array(&mut self) -> Result<JVal, JevError> {
        self.enter()?;
        self.pos += 1; // consume `[`
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b']') {
                self.pos += 1;
                return Ok(self.finish_array(items));
            }
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.bump() {
                Some(b',') => {}
                Some(b']') => return Ok(self.finish_array(items)),
                _ => return Err(JevError::BadResponse),
            }
        }
    }

    /// Close an object: the closing brace is already consumed.
    const fn finish_object(&mut self, entries: Vec<(String, JVal)>) -> JVal {
        self.depth -= 1;
        JVal::Obj(entries)
    }

    fn parse_object(&mut self) -> Result<JVal, JevError> {
        self.enter()?;
        self.pos += 1; // consume `{`
        let mut entries: Vec<(String, JVal)> = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'}') {
                self.pos += 1;
                return Ok(self.finish_object(entries));
            }
            if !matches!(self.peek(), Some(b'"')) {
                return Err(JevError::BadResponse);
            }
            let key = self.parse_string()?;
            if entries.iter().any(|(name, _)| name == &key) {
                return Err(JevError::BadResponse);
            }
            self.skip_ws();
            match self.bump() {
                Some(b':') => {}
                _ => return Err(JevError::BadResponse),
            }
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(b',') => {}
                Some(b'}') => return Ok(self.finish_object(entries)),
                _ => return Err(JevError::BadResponse),
            }
        }
    }

    const fn enter(&mut self) -> Result<(), JevError> {
        if self.depth >= MAX_RESPONSE_DEPTH {
            return Err(JevError::BadResponse);
        }
        self.depth += 1;
        Ok(())
    }

    /// Parse a JSON string starting at the opening quote, handling
    /// escapes (including `\u` surrogate pairs).
    fn parse_string(&mut self) -> Result<String, JevError> {
        self.pos += 1; // consume the opening quote
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(JevError::BadResponse),
                Some(b'"') => return Ok(out),
                Some(b'\\') => match self.bump() {
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'/') => out.push('/'),
                    Some(b'b') => out.push('\u{0008}'),
                    Some(b'f') => out.push('\u{000C}'),
                    Some(b'n') => out.push('\n'),
                    Some(b'r') => out.push('\r'),
                    Some(b't') => out.push('\t'),
                    Some(b'u') => out.push(self.parse_unicode()?),
                    _ => return Err(JevError::BadResponse),
                },
                Some(byte) if byte < 0x20 => return Err(JevError::BadResponse),
                Some(byte) => out.push(byte as char),
            }
        }
    }

    /// Parse the four hex digits after `\u`, combining a high/low
    /// surrogate pair when present.
    fn parse_unicode(&mut self) -> Result<char, JevError> {
        let high = self.parse_hex4()?;
        if (0xD800..0xDC00).contains(&high) {
            if self.bump() == Some(b'\\') && self.bump() == Some(b'u') {
                let low = self.parse_hex4()?;
                if (0xDC00..0xE000).contains(&low) {
                    let scalar = 0x1_0000 + ((high - 0xD800) << 10) + (low - 0xDC00);
                    return char::from_u32(scalar).ok_or(JevError::BadResponse);
                }
            }
            return Err(JevError::BadResponse);
        }
        if (0xDC00..0xE000).contains(&high) {
            return Err(JevError::BadResponse);
        }
        char::from_u32(high).ok_or(JevError::BadResponse)
    }

    /// Parse exactly four hex digits.
    fn parse_hex4(&mut self) -> Result<u32, JevError> {
        let mut value: u32 = 0;
        for _ in 0..4 {
            let digit = self.bump().ok_or(JevError::BadResponse)?;
            let nibble = match digit {
                b'0'..=b'9' => u32::from(digit - b'0'),
                b'a'..=b'f' => u32::from(digit - b'a') + 10,
                b'A'..=b'F' => u32::from(digit - b'A') + 10,
                _ => return Err(JevError::BadResponse),
            };
            value = value * 16 + nibble;
        }
        Ok(value)
    }

    /// Parse a JSON number starting at the current position, checking
    /// the grammar (`-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`)
    /// before converting. Returns the value and the raw bytes.
    fn parse_number(&mut self) -> Result<(f64, &[u8]), JevError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.bump() {
            Some(b'0') => {}
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(JevError::BadResponse),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JevError::BadResponse);
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JevError::BadResponse);
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let raw = self.buf.get(start..self.pos).ok_or(JevError::BadResponse)?;
        let text = core::str::from_utf8(raw).map_err(|_| JevError::BadResponse)?;
        let value = text.parse::<f64>().map_err(|_| JevError::BadResponse)?;
        Ok((value, raw))
    }

    /// Parse a top-level value and require end of input: trailing
    /// bytes are a malformed response, not a longer one.
    fn parse_document(&mut self) -> Result<JVal, JevError> {
        let value = self.parse_value()?;
        self.skip_ws();
        if self.peek().is_some() {
            return Err(JevError::BadResponse);
        }
        Ok(value)
    }
}

/// Parse the `decision` answer: the choice name plus the eight
/// positional probabilities.
fn parse_decision(value: &JVal) -> Result<(u8, [u16; 8]), JevError> {
    let object = value.as_obj().ok_or(JevError::BadResponse)?;
    let mut choice: Option<u8> = None;
    let mut probabilities: [Option<u16>; 8] = [None; 8];
    for (key, field) in object {
        match key.as_str() {
            "type" => {
                if field != &JVal::Str("choice".to_string()) {
                    return Err(JevError::BadResponse);
                }
            }
            "choice" => {
                let JVal::Str(name) = field else {
                    return Err(JevError::BadResponse);
                };
                let candidate =
                    Candidate::by_name(name.as_bytes()).ok_or(JevError::UnknownChoice)?;
                choice = Some(candidate.index());
            }
            "probabilities" => {
                let probs = field.as_obj().ok_or(JevError::BadResponse)?;
                for (name, prob) in probs {
                    let candidate =
                        Candidate::by_name(name.as_bytes()).ok_or(JevError::BadResponse)?;
                    let JVal::Num(number) = prob else {
                        return Err(JevError::BadResponse);
                    };
                    let bps = Prob::from_f64(*number).ok_or(JevError::BadProbability)?;
                    probabilities[usize::from(candidate.index())] = Some(bps.as_bps());
                }
            }
            // A concentration statistic of the distribution; the
            // adapter records the distribution itself.
            _ => {}
        }
    }
    let Some(choice) = choice else {
        return Err(JevError::BadResponse);
    };
    let mut out = [0u16; 8];
    for (index, slot) in probabilities.iter().enumerate() {
        out[index] = (*slot).ok_or(JevError::BadResponse)?;
    }
    Ok((choice, out))
}

/// Parse the `should_ask` answer: a single probability.
fn parse_noul(value: &JVal) -> Result<u16, JevError> {
    let object = value.as_obj().ok_or(JevError::BadResponse)?;
    let mut noul: Option<u16> = None;
    for (key, field) in object {
        match key.as_str() {
            "type" => {
                if field != &JVal::Str("noul".to_string()) {
                    return Err(JevError::BadResponse);
                }
            }
            "noul" => {
                let JVal::Num(number) = field else {
                    return Err(JevError::BadResponse);
                };
                let prob = Prob::from_f64(*number).ok_or(JevError::BadProbability)?;
                noul = Some(prob.as_bps());
            }
            _ => {}
        }
    }
    noul.ok_or(JevError::BadResponse)
}

/// Parse the `confidence` answer: the fractional score plus the four
/// positional level probabilities.
fn parse_score(value: &JVal) -> Result<(u16, [u16; 4]), JevError> {
    let object = value.as_obj().ok_or(JevError::BadResponse)?;
    let mut score: Option<u16> = None;
    let mut probabilities: [Option<u16>; 4] = [None; 4];
    for (key, field) in object {
        match key.as_str() {
            "type" => {
                if field != &JVal::Str("score".to_string()) {
                    return Err(JevError::BadResponse);
                }
            }
            "score" => {
                let JVal::Num(number) = field else {
                    return Err(JevError::BadResponse);
                };
                if !(1.0..=4.0).contains(number) {
                    return Err(JevError::BadResponse);
                }
                // `number` is in `1.0..=4.0`, so `milli` is in
                // `1000..=4000`: the saturating `as` cast is exact.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let milli = (number * 1000.0).round() as u16;
                score = Some(milli);
            }
            "probabilities" => {
                let probs = field.as_obj().ok_or(JevError::BadResponse)?;
                for (level, prob) in probs {
                    let index = match level.as_str() {
                        "1" => 0,
                        "2" => 1,
                        "3" => 2,
                        "4" => 3,
                        _ => return Err(JevError::BadResponse),
                    };
                    let JVal::Num(number) = prob else {
                        return Err(JevError::BadResponse);
                    };
                    let bps = Prob::from_f64(*number).ok_or(JevError::BadProbability)?;
                    probabilities[index] = Some(bps.as_bps());
                }
            }
            _ => {}
        }
    }
    let Some(score) = score else {
        return Err(JevError::BadResponse);
    };
    let mut out = [0u16; 4];
    for (index, slot) in probabilities.iter().enumerate() {
        out[index] = (*slot).ok_or(JevError::BadResponse)?;
    }
    Ok((score, out))
}

/// Parse one System One response into a [`JevAnswer`], applying the
/// [`NOUL_GATE_BPS`] gate. The response `model` must equal
/// `expected_model`: a version change mid-run is a different bundle,
/// and the run is bound to the configured one.
fn parse_response(bytes: &[u8], expected_model: &str) -> Result<JevAnswer, JevError> {
    let mut reader = JReader::new(bytes);
    let root = reader.parse_document()?;
    let root = root.as_obj().ok_or(JevError::BadResponse)?;
    let mut model: Option<&str> = None;
    let mut decision: Option<(u8, [u16; 8])> = None;
    let mut noul: Option<u16> = None;
    let mut score: Option<(u16, [u16; 4])> = None;
    for (key, value) in root {
        match key.as_str() {
            "model" => {
                let JVal::Str(version) = value else {
                    return Err(JevError::BadResponse);
                };
                model = Some(version.as_str());
            }
            "answers" => {
                let answers = value.as_obj().ok_or(JevError::BadResponse)?;
                for (question, answer) in answers {
                    match question.as_str() {
                        "decision" => decision = Some(parse_decision(answer)?),
                        "should_ask" => noul = Some(parse_noul(answer)?),
                        "confidence" => score = Some(parse_score(answer)?),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let Some(model) = model else {
        return Err(JevError::BadResponse);
    };
    if model != expected_model {
        return Err(JevError::VersionMismatch);
    }
    let (raw_choice, choice_probs_bps) = decision.ok_or(JevError::BadResponse)?;
    let noul_p_bps = noul.ok_or(JevError::BadResponse)?;
    let (score_milli, score_probs_bps) = score.ok_or(JevError::BadResponse)?;
    let gated = noul_p_bps >= NOUL_GATE_BPS;
    let choice = if gated {
        Candidate::Ask.index()
    } else {
        raw_choice
    };
    Ok(JevAnswer {
        raw_choice,
        choice,
        gated,
        choice_probs_bps,
        noul_p_bps,
        score_milli,
        score_probs_bps,
    })
}
/// One turn's recorded Jev output: the run-record home for the
/// returned probabilities (§19.5). The model version lives on the
/// backend (one per run); the probabilities live here, one per turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JevReceipt {
    /// The parsed answer, after gating.
    pub answer: JevAnswer,
    /// The measured usage of the turn: the prompt bytes read, and
    /// zero output tokens — Jev generates no text, and output is
    /// unmetered.
    pub usage: TokenUsage,
}

/// What can go wrong between the adapter and Jev.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JevError {
    /// The transport failed: the cassette has no recorded response
    /// for this request, or the network call failed.
    #[error("the Jev transport failed: {0}")]
    Transport(&'static str),
    /// The response is not valid System One JSON, or a required
    /// field is missing or misshaped.
    #[error("the Jev response is not a valid System One answer")]
    BadResponse,
    /// The response chose an option outside the turn's schema.
    #[error("the Jev response chose an unknown option")]
    UnknownChoice,
    /// A probability fell outside 0..=1.
    #[error("a Jev probability is outside 0..=1")]
    BadProbability,
    /// The response named a different model version than the backend
    /// was constructed with. The run is bound to the configured
    /// version; a silent version change would break that binding.
    #[error("the Jev model version changed mid-run")]
    VersionMismatch,
    /// The live endpoint answered with a non-200 HTTP status: 4xx is
    /// our request or our key, 5xx is the server. Either way the turn
    /// came back with no answer.
    #[error("the Jev API returned HTTP status {0}")]
    HttpStatus(u16),
    /// A live call was attempted on a transport built without a key
    /// ([`LiveTransport::unconfigured`]). Configure one with
    /// [`LiveTransport::new`] instead of retrying the deferral.
    #[error("live Jev calls need an API key: use LiveTransport::new")]
    LiveDeferred,
}

/// The byte pipe to Jev: request JSON in, response JSON out. The
/// adapter owns the schema on both sides; the transport only moves
/// bytes.
pub trait JevTransport: Send {
    /// Send one request and return the raw response JSON.
    ///
    /// # Errors
    ///
    /// Returns [`JevError::Transport`] when there is no recorded
    /// response (mock) or the call fails (live).
    fn ask(&mut self, request_json: &[u8]) -> Result<Vec<u8>, JevError>;
}

/// The recorded transport: request hashes map to recorded responses.
///
/// A request with no recorded response is
/// [`JevError::Transport`], which the backend surfaces as
/// [`BackendError::UnknownPrompt`] — the adapter never guesses, the
/// same fail-closed rule as the tiny backend's distill table.
pub struct MockTransport {
    /// `fnv1a64(request_json)` → recorded response JSON.
    pairs: HashMap<u64, Vec<u8>>,
    /// Calls served so far.
    calls: u64,
}

impl MockTransport {
    /// Build from a cassette document (see `spec/jev-cassettes/`):
    /// `{"model_version": ..., "endpoint": ...,
    /// "pairs": [{"request_hash": "<decimal u64>", "response": { ... }}}, ...]}`.
    /// The response objects are re-emitted canonically; only their
    /// meaning crosses into the mock.
    ///
    /// # Errors
    ///
    /// Returns [`JevError::BadResponse`] when the cassette is not
    /// valid JSON of the documented shape.
    pub fn from_cassette(json: &str) -> Result<Self, JevError> {
        let mut reader = JReader::new(json.as_bytes());
        let root = reader.parse_document()?;
        let root = root.as_obj().ok_or(JevError::BadResponse)?;
        let mut pairs = HashMap::new();
        for (key, value) in root {
            if key.as_str() != "pairs" {
                continue;
            }
            let JVal::Arr(entries) = value else {
                return Err(JevError::BadResponse);
            };
            for entry in entries {
                let pair = entry.as_obj().ok_or(JevError::BadResponse)?;
                let mut hash: Option<u64> = None;
                let mut response: Option<Vec<u8>> = None;
                for (field, field_value) in pair {
                    match field.as_str() {
                        "request_hash" => {
                            // A decimal string: request hashes are
                            // full-width `u64`s, past what a JSON
                            // number carries exactly.
                            let JVal::Str(digits) = field_value else {
                                return Err(JevError::BadResponse);
                            };
                            hash = Some(parse_decimal_u64(digits.as_bytes())?);
                        }
                        "response" => {
                            let mut out = Vec::new();
                            emit_jval(field_value, &mut out);
                            response = Some(out);
                        }
                        _ => {}
                    }
                }
                let (Some(hash), Some(response)) = (hash, response) else {
                    return Err(JevError::BadResponse);
                };
                pairs.insert(hash, response);
            }
        }
        Ok(Self { pairs, calls: 0 })
    }

    /// Calls served so far.
    #[must_use]
    pub const fn calls(&self) -> u64 {
        self.calls
    }
}

impl JevTransport for MockTransport {
    fn ask(&mut self, request_json: &[u8]) -> Result<Vec<u8>, JevError> {
        let hash = fnv1a64(request_json);
        self.calls += 1;
        self.pairs.get(&hash).cloned().ok_or(JevError::Transport(
            "the mock cassette has no recorded response for this request",
        ))
    }
}

/// Parse decimal digits as `u64`, rejecting empty input,
/// non-digits, and overflow.
fn parse_decimal_u64(digits: &[u8]) -> Result<u64, JevError> {
    if digits.is_empty() {
        return Err(JevError::BadResponse);
    }
    let mut value: u64 = 0;
    for &digit in digits {
        if !digit.is_ascii_digit() {
            return Err(JevError::BadResponse);
        }
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(digit - b'0')))
            .ok_or(JevError::BadResponse)?;
    }
    Ok(value)
}

/// Re-emit a parsed JSON value canonically (for cassette loading).
/// Numbers round-trip through `f64`, so `0.9000` becomes `0.9`: the
/// meaning is identical and `parse_response` only reads the meaning.
fn emit_jval(value: &JVal, out: &mut Vec<u8>) {
    match value {
        JVal::Null => out.extend_from_slice(b"null"),
        JVal::Bool(true) => out.extend_from_slice(b"true"),
        JVal::Bool(false) => out.extend_from_slice(b"false"),
        JVal::Num(number) => {
            let text = format!("{number:?}");
            out.extend_from_slice(text.as_bytes());
        }
        JVal::Str(text) => push_escaped(out, text.as_bytes()),
        JVal::Arr(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                emit_jval(item, out);
            }
            out.push(b']');
        }
        JVal::Obj(entries) => {
            out.push(b'{');
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                push_escaped(out, key.as_bytes());
                out.push(b':');
                emit_jval(item, out);
            }
            out.push(b'}');
        }
    }
}

/// The live transport: the real HTTPS `POST` to the System One endpoint.
///
/// Built with a bearer key ([`LiveTransport::new`]), `ask` performs
/// one blocking HTTPS call per turn: `POST {endpoint}` with
/// `Authorization: Bearer <key>`, `Content-Type: application/json`,
/// and the [`build_request_json`] body. The TLS client is `rustls`
/// with the Mozilla root set; it honors `HTTPS_PROXY` like a
/// conventional client. A non-200 status fails closed as
/// [`JevError::HttpStatus`].
///
/// Built without a key ([`LiveTransport::unconfigured`]), every call
/// fails with [`JevError::LiveDeferred`] — the honest stub for
/// keyless contexts (unit tests, cassette authoring).
pub struct LiveTransport {
    /// The endpoint, usually [`JEV_ENDPOINT`].
    pub endpoint: String,
    /// The bearer key; `None` on the unconfigured form. Supplied by
    /// the caller, zeroized on drop, never logged.
    api_key: Option<String>,
}

impl LiveTransport {
    /// Build a live transport with no key: every call defers.
    #[must_use]
    pub fn unconfigured(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            api_key: None,
        }
    }

    /// Build a live transport with a bearer key: calls perform the
    /// real HTTPS `POST`.
    ///
    /// The key is supplied by the caller — from secure storage at
    /// the call site. It is never hardcoded, never read from disk
    /// or the environment by the library, and never logged: it
    /// travels only in the `Authorization` header, and its bytes are
    /// zeroized when the transport drops.
    #[must_use]
    pub fn new(endpoint: &str, api_key: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            api_key: Some(api_key.to_string()),
        }
    }
}

impl Drop for LiveTransport {
    fn drop(&mut self) {
        self.api_key.zeroize();
    }
}

impl JevTransport for LiveTransport {
    fn ask(&mut self, request_json: &[u8]) -> Result<Vec<u8>, JevError> {
        // The documented call: POST {endpoint} with
        // `Authorization: Bearer <key>` and `Content-Type:
        // application/json`. The key comes from the caller; without
        // one the call defers honestly instead of failing obscurely.
        let api_key = self.api_key.as_deref().ok_or(JevError::LiveDeferred)?;
        let endpoint = https::parse_endpoint(&self.endpoint)?;
        let response = https::post(&endpoint, api_key, request_json)?;
        if response.status != 200 {
            return Err(JevError::HttpStatus(response.status));
        }
        Ok(response.body)
    }
}

/// A recorded answer in cassette-authoring form: the option name plus
/// the probabilities the API would return. [`synthesize_response`]
/// renders it as System One JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedAnswer {
    /// The option name Jev chose.
    pub choice: &'static str,
    /// The eight positional choice probabilities, basis points.
    pub choice_probs_bps: [u16; 8],
    /// P(should ask), basis points.
    pub noul_p_bps: u16,
    /// The fractional score, thousandths of a level.
    pub score_milli: u16,
    /// The four positional level probabilities, basis points.
    pub score_probs_bps: [u16; 4],
}

/// Render a [`RecordedAnswer`] as System One response JSON: the shape
/// the mock cassettes record and `parse_response` consumes. Used to
/// author cassettes without a live key.
#[must_use]
pub fn synthesize_response(answer: &RecordedAnswer, model_version: &str) -> Vec<u8> {
    /// Format basis points as a JSON decimal (`9000` → `0.9000`).
    fn decimal(out: &mut Vec<u8>, bps: u16) {
        if bps >= 10_000 {
            out.extend_from_slice(b"1.0000");
        } else {
            let text = format!("0.{bps:04}");
            out.extend_from_slice(text.as_bytes());
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"model\":");
    push_escaped(&mut out, model_version.as_bytes());
    out.extend_from_slice(b",\"answers\":{\"decision\":{\"type\":\"choice\",\"choice\":");
    push_escaped(&mut out, answer.choice.as_bytes());
    out.extend_from_slice(b",\"probabilities\":{");
    for (index, candidate) in CANDIDATES.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        push_escaped(&mut out, candidate.option_name().as_bytes());
        out.push(b':');
        decimal(&mut out, answer.choice_probs_bps[index]);
    }
    out.extend_from_slice(b"},\"confidence\":0.8000},\"should_ask\":{\"type\":\"noul\",\"noul\":");
    decimal(&mut out, answer.noul_p_bps);
    out.extend_from_slice(b"},\"confidence\":{\"type\":\"score\",\"score\":");
    let milli = answer.score_milli;
    let text = format!("{}.{:03}", milli / 1000, milli % 1000);
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b",\"probabilities\":{\"1\":");
    decimal(&mut out, answer.score_probs_bps[0]);
    out.extend_from_slice(b",\"2\":");
    decimal(&mut out, answer.score_probs_bps[1]);
    out.extend_from_slice(b",\"3\":");
    decimal(&mut out, answer.score_probs_bps[2]);
    out.extend_from_slice(b",\"4\":");
    decimal(&mut out, answer.score_probs_bps[3]);
    out.extend_from_slice(
        b"},\"confidence\":0.7500}},\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}",
    );
    out
}

/// The Jev host backend: one System One call per turn, mapped through
/// the typed schema into a decodable decision line.
///
/// The backend is crash-consistent under the same contract as the
/// other backends: `unemit` pops the last receipt, so a re-inferred
/// turn rebuilds the identical request (the prompt is deterministic)
/// and the mock serves the identical recorded response. Jev itself
/// is reproducible to about 0.02, not bit-identical — that caveat
/// applies to the *live* path; the mock is exact.
pub struct JevBackend {
    transport: Box<dyn JevTransport>,
    model_version: String,
    endpoint: String,
    bundle: BundleId,
    usage: TokenUsage,
    receipts: Vec<JevReceipt>,
}

impl JevBackend {
    /// Build a Jev backend over any transport. The bundle digest
    /// binds the adapter tag, the model version, and the endpoint.
    #[must_use]
    pub fn new(transport: Box<dyn JevTransport>, model_version: &str, endpoint: &str) -> Self {
        let mut hasher_input = Vec::new();
        hasher_input.extend_from_slice(BUNDLE_TAG);
        hasher_input.push(0xFF);
        hasher_input.extend_from_slice(model_version.as_bytes());
        hasher_input.push(0xFF);
        hasher_input.extend_from_slice(endpoint.as_bytes());
        let bundle = BundleId(fnv1a64(&hasher_input));
        Self {
            transport,
            model_version: model_version.to_string(),
            endpoint: endpoint.to_string(),
            bundle,
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
            receipts: Vec::new(),
        }
    }

    /// The pinned model version this backend answers as.
    #[must_use]
    pub fn model_version(&self) -> &str {
        &self.model_version
    }

    /// The endpoint this backend targets.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The per-turn receipts: the run-record home for the returned
    /// probabilities.
    #[must_use]
    pub fn receipts(&self) -> &[JevReceipt] {
        &self.receipts
    }

    /// Answer one turn: build the request, call the transport, parse
    /// the response, gate the choice, and render the decision line.
    fn answer(&mut self, prompt: &[u8], out: &mut [u8]) -> Result<usize, BackendError> {
        let request = build_request_json(prompt, &self.model_version);
        let response = self.transport.ask(&request).map_err(|error| match error {
            // A 4xx is our request or our key: a client-side defect,
            // not a missing answer. (The nested-`input` shape the API
            // 400s on was exactly this class of bug.)
            JevError::HttpStatus(status) if (400..500).contains(&status) => {
                BackendError::PolicyMismatch
            }
            // The mock never guesses: an unrecorded request is the
            // same fail-closed outcome as the tiny backend's missing
            // distill entry. A live network failure or any other
            // status is the same shape: no answer came back.
            JevError::Transport(_) | JevError::HttpStatus(_) => BackendError::UnknownPrompt,
            // A malformed or unshaped response fails the adapter's
            // integrity check, like a tampered distill entry; a
            // deferred live call is a configuration dead end with the
            // same hard stop.
            JevError::BadResponse
            | JevError::UnknownChoice
            | JevError::BadProbability
            | JevError::VersionMismatch
            | JevError::LiveDeferred => BackendError::PolicyMismatch,
        })?;
        let answer =
            parse_response(&response, &self.model_version).map_err(|error| match error {
                // An out-of-schema choice is a missing answer the same
                // way an unrecorded request is: the adapter has nothing
                // valid to emit.
                JevError::UnknownChoice | JevError::Transport(_) | JevError::LiveDeferred => {
                    BackendError::UnknownPrompt
                }
                // `HttpStatus` cannot reach here — the transport
                // consumed it — but the match must stay exhaustive.
                JevError::HttpStatus(_)
                | JevError::BadResponse
                | JevError::BadProbability
                | JevError::VersionMismatch => BackendError::PolicyMismatch,
            })?;
        let choice = CANDIDATES
            .get(usize::from(answer.choice))
            .copied()
            .ok_or(BackendError::PolicyMismatch)?;
        let hints = derive_hints(prompt);
        let line = choice.render_line(hints);
        let bytes = line.as_bytes();
        if bytes.len() > out.len() {
            return Err(BackendError::OutputTooLong);
        }
        out[..bytes.len()].copy_from_slice(bytes);
        let usage = TokenUsage {
            input_tokens: u32::try_from(prompt.len()).unwrap_or(u32::MAX),
            output_tokens: 0,
        };
        self.usage = TokenUsage {
            input_tokens: self.usage.input_tokens.saturating_add(usage.input_tokens),
            output_tokens: self.usage.output_tokens.saturating_add(usage.output_tokens),
        };
        self.receipts.push(JevReceipt { answer, usage });
        Ok(bytes.len())
    }
}

impl ModelBackend for JevBackend {
    fn infer(&mut self, prompt: &[u8], out: &mut [u8]) -> Result<usize, BackendError> {
        self.answer(prompt, out)
    }

    fn bundle_id(&self) -> BundleId {
        self.bundle
    }

    fn last_usage(&self) -> TokenUsage {
        self.usage
    }

    /// Pop the last receipt, mirroring `ScriptedBackend`: the
    /// pre-commit crash path re-infers the same prompt and the mock
    /// serves the same recorded response.
    fn unemit(&mut self) {
        self.receipts.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use esper_core::decision::decode_line;

    /// The canonical request for a one-byte prompt. Golden: the
    /// request bytes are the mock cassette keys, so any change here
    /// invalidates every cassette.
    const GOLDEN_REQUEST: &str = "{\"state\":\"X\",\"model\":\"jev-1.13.0\",\"questions\":{\"decision\":{\"type\":\"choice\",\"instructions\":\"Select the next ReAct turn decision.\",\"options\":{\"call_gpio_pin_read\":\"Read the level of a GPIO pin.\",\"call_gpio_pin_write\":\"Drive a GPIO pin high.\",\"call_sensor_sample_read\":\"Sample a sensor.\",\"call_timer_delay_wait\":\"Wait 100 milliseconds.\",\"call_timer_uptime_read\":\"Read the monotonic uptime clock.\",\"call_device_status_report\":\"Report device status.\",\"ask\":\"Ask the operator for input.\",\"finish\":\"Finish the run.\"}},\"should_ask\":{\"type\":\"noul\",\"instructions\":\"The run cannot proceed safely without human input.\"},\"confidence\":{\"type\":\"score\",\"instructions\":\"Rate the confidence of the selected decision.\",\"levels\":{\"1\":\"guessing\",\"2\":\"uncertain\",\"3\":\"confident\",\"4\":\"certain\"}}}}";

    fn sample_answer() -> RecordedAnswer {
        RecordedAnswer {
            choice: "call_gpio_pin_read",
            choice_probs_bps: [9_000, 200, 200, 200, 200, 100, 50, 50],
            noul_p_bps: 100,
            score_milli: 3_100,
            score_probs_bps: [500, 1_000, 5_500, 3_000],
        }
    }

    #[test]
    fn request_json_is_byte_stable() {
        let bytes = build_request_json(b"X", "jev-1.13.0");
        assert_eq!(bytes, GOLDEN_REQUEST.as_bytes());
    }

    #[test]
    fn request_json_escapes_control_bytes() {
        let bytes = build_request_json(b"a\"b\\c\x01", "m");
        let text = core::str::from_utf8(&bytes).expect("valid utf8");
        assert!(text.contains("\"state\":\"a\\\"b\\\\c\\u0001\""));
    }

    #[test]
    fn synthesize_then_parse_round_trip() {
        let bytes = synthesize_response(&sample_answer(), "jev-1.13.0");
        let answer = parse_response(&bytes, "jev-1.13.0").expect("valid");
        assert_eq!(answer.raw_choice, 0);
        assert_eq!(answer.choice, 0);
        assert!(!answer.gated);
        assert_eq!(answer.choice_probs_bps, sample_answer().choice_probs_bps);
        assert_eq!(answer.noul_p_bps, 100);
        assert_eq!(answer.score_milli, 3_100);
        assert_eq!(answer.score_probs_bps, sample_answer().score_probs_bps);
    }

    #[test]
    fn noul_at_gate_overrides_choice_with_ask() {
        let mut answer = sample_answer();
        answer.noul_p_bps = NOUL_GATE_BPS;
        let bytes = synthesize_response(&answer, "jev-1.13.0");
        let parsed = parse_response(&bytes, "jev-1.13.0").expect("valid");
        assert_eq!(parsed.raw_choice, 0);
        assert_eq!(parsed.choice, Candidate::Ask.index());
        assert!(parsed.gated);
    }

    #[test]
    fn noul_below_gate_keeps_choice() {
        let mut answer = sample_answer();
        answer.noul_p_bps = NOUL_GATE_BPS - 1;
        let bytes = synthesize_response(&answer, "jev-1.13.0");
        let parsed = parse_response(&bytes, "jev-1.13.0").expect("valid");
        assert_eq!(parsed.choice, 0);
        assert!(!parsed.gated);
    }

    #[test]
    fn version_mismatch_fails_closed() {
        let bytes = synthesize_response(&sample_answer(), "jev-9.9.9");
        let error = parse_response(&bytes, "jev-1.13.0").expect_err("must fail");
        assert_eq!(error, JevError::VersionMismatch);
    }

    #[test]
    fn unknown_choice_fails_closed() {
        let mut answer = sample_answer();
        answer.choice = "call_teleport_home";
        let bytes = synthesize_response(&answer, "jev-1.13.0");
        let error = parse_response(&bytes, "jev-1.13.0").expect_err("must fail");
        assert_eq!(error, JevError::UnknownChoice);
    }

    #[test]
    fn out_of_range_probability_fails_closed() {
        let bytes = br#"{"model":"jev-1.13.0","answers":{"decision":{"type":"choice","choice":"ask","probabilities":{"call_gpio_pin_read":1.5000,"call_gpio_pin_write":0.0000,"call_sensor_sample_read":0.0000,"call_timer_delay_wait":0.0000,"call_timer_uptime_read":0.0000,"call_device_status_report":0.0000,"ask":0.0000,"finish":0.0000},"confidence":0.9},"should_ask":{"type":"noul","noul":0.1000},"confidence":{"type":"score","score":2.000,"probabilities":{"1":0.2500,"2":0.2500,"3":0.2500,"4":0.2500},"confidence":0.5}}}"#;
        let error = parse_response(bytes, "jev-1.13.0").expect_err("must fail");
        assert_eq!(error, JevError::BadProbability);
    }

    #[test]
    fn missing_answer_fails_closed() {
        let bytes = br#"{"model":"jev-1.13.0","answers":{"decision":{"type":"choice","choice":"ask","probabilities":{"call_gpio_pin_read":0.1250,"call_gpio_pin_write":0.1250,"call_sensor_sample_read":0.1250,"call_timer_delay_wait":0.1250,"call_timer_uptime_read":0.1250,"call_device_status_report":0.1250,"ask":0.1250,"finish":0.1250},"confidence":0.9},"should_ask":{"type":"noul","noul":0.1000}}}"#;
        let error = parse_response(bytes, "jev-1.13.0").expect_err("must fail");
        assert_eq!(error, JevError::BadResponse);
    }

    #[test]
    fn garbage_response_fails_closed() {
        let error = parse_response(b"not json at all", "jev-1.13.0").expect_err("must fail");
        assert_eq!(error, JevError::BadResponse);
    }

    #[test]
    fn bundle_binds_version_and_endpoint() {
        let one = JevBackend::new(
            Box::new(LiveTransport::unconfigured(JEV_ENDPOINT)),
            "jev-1.13.0",
            JEV_ENDPOINT,
        );
        let other_version = JevBackend::new(
            Box::new(LiveTransport::unconfigured(JEV_ENDPOINT)),
            "jev-1.14.0",
            JEV_ENDPOINT,
        );
        let other_endpoint = JevBackend::new(
            Box::new(LiveTransport::unconfigured(JEV_ENDPOINT)),
            "jev-1.13.0",
            "https://example.invalid/v1/systemone",
        );
        assert_ne!(one.bundle_id(), other_version.bundle_id());
        assert_ne!(one.bundle_id(), other_endpoint.bundle_id());
    }

    #[test]
    fn score_bands_match_spec_thresholds() {
        assert_eq!(score_band(1_000), ScoreBand::Review);
        assert_eq!(score_band(1_499), ScoreBand::Review);
        assert_eq!(score_band(1_500), ScoreBand::Caution);
        assert_eq!(score_band(3_000), ScoreBand::Caution);
        assert_eq!(score_band(3_001), ScoreBand::Act);
        assert_eq!(score_band(4_000), ScoreBand::Act);
    }

    #[test]
    fn hints_come_from_the_last_observation() {
        let prompt = b"TOOLS x\nSTATE 1\nLAST {\"pin\": 4, \"level\": \"low\"}\nEMIT one line\n";
        let hints = derive_hints(prompt);
        assert_eq!(hints, ArgHints { pin: 4, sensor: 0 });
    }

    #[test]
    fn hints_ignore_string_pins() {
        // `{"pin": "4"}` is not a number: the template never emits
        // what the observation did not say as a number.
        let prompt = b"STATE 1\nLAST {\"pin\": \"4\"}\nEMIT one line\n";
        assert_eq!(derive_hints(prompt), ArgHints { pin: 0, sensor: 0 });
    }

    #[test]
    fn hints_default_without_an_observation() {
        let prompt = b"STATE 1\nEMIT one line\n";
        assert_eq!(derive_hints(prompt), ArgHints { pin: 0, sensor: 0 });
    }

    #[test]
    fn hints_default_on_a_truncated_observation() {
        // A truncated observation may end mid-JSON; the hints fall
        // back to the defaults rather than emitting a guess.
        let prompt = b"STATE 1\nLAST {\"pin\": 4, \"lev[truncated]\nEMIT one line\n";
        assert_eq!(derive_hints(prompt), ArgHints { pin: 0, sensor: 0 });
    }

    #[test]
    fn every_candidate_renders_a_decodable_line() {
        let hints = ArgHints { pin: 4, sensor: 1 };
        for candidate in CANDIDATES {
            let line = candidate.render_line(hints);
            decode_line(line.as_bytes()).expect("every template line must decode");
        }
    }

    #[test]
    fn templates_fill_concrete_args() {
        let hints = ArgHints { pin: 4, sensor: 1 };
        assert_eq!(
            Candidate::WritePin.render_line(hints),
            "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}"
        );
        assert_eq!(
            Candidate::ReadPin.render_line(hints),
            "CALL gpio_pin_read {\"pin\": 4}"
        );
        assert_eq!(
            Candidate::SampleSensor.render_line(hints),
            "CALL sensor_sample_read {\"sensor\": 1}"
        );
    }

    #[test]
    fn mock_cassette_serves_by_request_hash() {
        let prompt = b"X";
        let request = build_request_json(prompt, "jev-1.13.0");
        let hash = fnv1a64(&request);
        let response = synthesize_response(&sample_answer(), "jev-1.13.0");
        let response_text = core::str::from_utf8(&response).expect("valid utf8");
        let cassette = format!(
            "{{\"model_version\":\"jev-1.13.0\",\"endpoint\":\"{JEV_ENDPOINT}\",\"pairs\":[{{\"request_hash\":\"{hash}\",\"response\":{response_text}}}]}}"
        );
        let mut transport = MockTransport::from_cassette(&cassette).expect("valid cassette");
        let served = transport.ask(&request).expect("recorded");
        // The cassette loader re-emits numbers canonically (`0.9`
        // for `0.9000`); the answers must match, not the bytes.
        let served_answer = parse_response(&served, "jev-1.13.0").expect("valid");
        let expected_answer = parse_response(&response, "jev-1.13.0").expect("valid");
        assert_eq!(served_answer, expected_answer);
        assert_eq!(transport.calls(), 1);
    }

    #[test]
    fn mock_cassette_misses_fail_closed() {
        let transport = MockTransport::from_cassette(
            "{\"model_version\":\"jev-1.13.0\",\"endpoint\":\"x\",\"pairs\":[]}",
        )
        .expect("valid cassette");
        let mut transport = transport;
        let error = transport
            .ask(b"{\"state\":\"unrecorded\"}")
            .expect_err("must fail");
        assert_eq!(
            error,
            JevError::Transport("the mock cassette has no recorded response for this request")
        );
    }

    #[test]
    fn request_json_has_top_level_wire_shape() {
        // Verified live 2026-09-22: the direct API takes `state`,
        // `model`, and `questions` as top-level siblings. Nesting
        // them under `input` returns HTTP 400.
        let bytes = build_request_json(b"X", "jev-1.13.0");
        let mut reader = JReader::new(&bytes);
        let root = reader.parse_document().expect("valid request");
        let mut keys: Vec<&str> = root
            .as_obj()
            .expect("top-level object")
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["model", "questions", "state"]);
    }

    #[test]
    fn live_transport_defers_without_a_key() {
        let mut transport = LiveTransport::unconfigured(JEV_ENDPOINT);
        let error = transport.ask(b"{}").expect_err("must defer");
        assert_eq!(error, JevError::LiveDeferred);
    }

    #[test]
    fn live_transport_new_takes_the_endpoint_and_a_key() {
        let transport = LiveTransport::new(JEV_ENDPOINT, "bearer-key");
        assert_eq!(transport.endpoint, JEV_ENDPOINT);
        // The key is not readable back: it travels only in the
        // `Authorization` header, and is zeroized on drop.
    }

    /// A transport that fails with a fixed HTTP status, for the
    /// error-mapping tests.
    struct StatusTransport(u16);

    impl JevTransport for StatusTransport {
        fn ask(&mut self, _request_json: &[u8]) -> Result<Vec<u8>, JevError> {
            Err(JevError::HttpStatus(self.0))
        }
    }

    #[test]
    fn http_4xx_from_the_transport_maps_to_policy_mismatch() {
        // A client-side defect (bad request shape, bad key) is an
        // integrity failure, not a missing answer.
        let mut backend =
            JevBackend::new(Box::new(StatusTransport(400)), "jev-1.13.0", JEV_ENDPOINT);
        let mut out = [0u8; 64];
        let error = backend
            .infer(b"STATE 1\nEMIT one line\n", &mut out)
            .expect_err("must fail");
        assert_eq!(error, BackendError::PolicyMismatch);
    }

    #[test]
    fn http_5xx_from_the_transport_maps_to_unknown_prompt() {
        // A server-side failure means no answer came back: the same
        // fail-closed bucket as a dead transport.
        let mut backend =
            JevBackend::new(Box::new(StatusTransport(503)), "jev-1.13.0", JEV_ENDPOINT);
        let mut out = [0u8; 64];
        let error = backend
            .infer(b"STATE 1\nEMIT one line\n", &mut out)
            .expect_err("must fail");
        assert_eq!(error, BackendError::UnknownPrompt);
    }

    #[test]
    fn prob_rejects_out_of_range() {
        assert!(Prob::from_bps(10_000).is_some());
        assert!(Prob::from_bps(10_001).is_none());
        assert!(Prob::from_f64(0.5).is_some());
        assert!(Prob::from_f64(1.5).is_none());
        assert!(Prob::from_f64(f64::NAN).is_none());
    }
}
