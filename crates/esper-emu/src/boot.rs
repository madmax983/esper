//! What the emulated image runs: Esper's firmware code paths, end to end
//! on the core, folded into one digest every core must agree on.
//!
//! Each stage exercises the real `no_std` code — nothing here is a double
//! of the thing it checks — and fails the boot with a named reason when
//! an answer is wrong:
//!
//! 1. `prompt` — compose a GOAL brief and build a briefed prompt.
//! 2. `infer` — the tiny backend answers it from a distilled table.
//! 3. `decode` — the decoder validates the line against the catalog.
//! 4. `mask` — a secret-shaped value is redacted before durability.
//! 5. `checkpoint` — a goal and a memory entry survive an item-wise
//!    store and restore with their digest.
//! 6. `wire` — an operator command crosses the `SENSE` codec intact.
//! 7. `run` — the run workflow drives a read, a write and a finish
//!    through a reset that lands after the write acted and before its
//!    answer was recorded: it must complete with exactly one physical
//!    write ([`crate::workload`]).

use esper_core::beat::{Beat, BeatSpan};
use esper_core::brief::Brief;
use esper_core::checkpoint::{AgentCheckpoint, HEADER_BYTES, Header, MAX_ITEM_BYTES, Restore};
use esper_core::compact::Note;
use esper_core::decision::{Decision, decode_line};
use esper_core::goal::{Cadence, Envelope, GoalId, GoalLedger, GoalSpec};
use esper_core::ids::{Digest, ToolId};
use esper_core::mask::{mask_bound, mask_report};
use esper_core::memory::{Memory, MemoryEntry, MemoryKey, Provenance, recall};
use esper_core::registry::Capabilities;
use esper_runtime::agent::wire::{Command, MAX_SENSE_BYTES, Sense};
use esper_runtime::{
    BRIEFED_PROMPT_CAP, DistillEntry, ModelBackend, OUTPUT_CAP, PromptCtx, TinyBackend,
    build_prompt, fingerprint_prompt, fnv1a64,
};
use waymaker_embassy::Decode;

/// Stages a boot reports.
const STAGES: [&str; 7] = [
    "prompt",
    "infer",
    "decode",
    "mask",
    "checkpoint",
    "wire",
    "run",
];

/// What a boot did: the stages it passed and the digest over every
/// answer it computed.
pub struct Report {
    passed: usize,
    /// FNV-1a-64 over every stage's output, in order.
    pub digest: u64,
}

impl Report {
    /// The stages passed, in order.
    pub fn stages(&self) -> impl Iterator<Item = &'static str> {
        STAGES.into_iter().take(self.passed)
    }
}

/// A running FNV-1a-64, so the digest never needs every output in one
/// buffer.
struct Fold(u64);

impl Fold {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn add(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Run every stage. The first wrong answer fails the boot.
///
/// # Errors
///
/// A static reason naming the stage and what was wrong.
pub fn run() -> Result<Report, &'static str> {
    let mut fold = Fold::new();
    let mut passed = 0;
    let mut pass = |fold: &mut Fold, bytes: &[u8]| {
        fold.add(bytes);
        passed += 1;
    };

    let mut prompt = [0u8; BRIEFED_PROMPT_CAP];
    let prompt = prompt_stage(&mut prompt)?;
    pass(&mut fold, prompt);

    let mut line = [0u8; OUTPUT_CAP];
    let line = infer_stage(prompt, &mut line)?;
    pass(&mut fold, line);

    pass(&mut fold, &decode_stage(line)?.to_le_bytes());

    let mut masked = [0u8; mask_bound(SECRET_INPUT_CAP)];
    pass(&mut fold, mask_stage(&mut masked)?);

    pass(&mut fold, &checkpoint_stage()?);

    let mut sense = [0u8; MAX_SENSE_BYTES];
    pass(&mut fold, wire_stage(&mut sense)?);

    let outcome = run_stage()?;
    fold.add(&outcome.records.to_le_bytes());
    pass(
        &mut fold,
        outcome.terminal.get(..outcome.len).ok_or("run: terminal")?,
    );

    Ok(Report {
        passed,
        digest: fold.0,
    })
}

/// The observation the prompt carries and the memory entry records.
const OBSERVATION: &[u8] = br#"{"pin":3,"level":"low"}"#;

/// The line the distilled table answers with.
const CALL_LINE: &[u8] = br#"CALL gpio_pin_read {"pin": 3}"#;

/// Room for the runtime-assembled secret-bearing input.
const SECRET_INPUT_CAP: usize = 64;

/// Stage 1: a GOAL brief composed into a briefed prompt.
fn prompt_stage(out: &mut [u8]) -> Result<&[u8], &'static str> {
    let objective = Note::from_bytes(b"keep pin 3 low").map_err(|_| "prompt: objective")?;
    let memory = Memory::new();
    let brief = Brief::compose(&objective, &recall(&memory, GoalId::MIN));
    let ctx = PromptCtx {
        turns_left: 10,
        mutations_left: 4,
        last_observation: Some(OBSERVATION),
        repair: None,
        prior: None,
        brief: Some(&brief),
    };
    let len = build_prompt(&ctx, out);
    let prompt = out.get(..len).ok_or("prompt: length")?;
    if !contains(prompt, b"GOAL keep pin 3 low\n")
        || !prompt.ends_with(b"EMIT one decision line.\n")
    {
        return Err("prompt: brief or trailer missing");
    }
    Ok(prompt)
}

/// Stage 2: the tiny backend answers the prompt from a distilled table
/// keyed by the prompt's fingerprint.
fn infer_stage<'o>(prompt: &[u8], out: &'o mut [u8]) -> Result<&'o [u8], &'static str> {
    let table = [DistillEntry {
        prompt_fp: fingerprint_prompt(prompt),
        line_hash: fnv1a64(CALL_LINE),
        line: CALL_LINE,
    }];
    let mut tiny = TinyBackend::new(&table);
    let n = tiny
        .infer(prompt, out)
        .map_err(|_| "infer: backend refused")?;
    let emitted = out.get(..n).ok_or("infer: length")?;
    if emitted != CALL_LINE {
        return Err("infer: wrong line");
    }
    Ok(emitted)
}

/// Stage 3: the decoder validates the line against the catalog. Returns
/// the call's argument digest.
fn decode_stage(line: &[u8]) -> Result<u64, &'static str> {
    let Ok(Decision::Call(call)) = decode_line(line) else {
        return Err("decode: not a call");
    };
    if call.tool() != ToolId::new(1) {
        return Err("decode: wrong tool");
    }
    Ok(call.args_digest().get())
}

/// Stage 4: a secret-shaped value is redacted. The secret is assembled
/// at runtime so no secret-shaped literal lives in the source.
fn mask_stage(out: &mut [u8]) -> Result<&[u8], &'static str> {
    let mut raw = [0u8; SECRET_INPUT_CAP];
    let mut len = 0;
    for part in [
        &b"token "[..],
        b"sk",
        b"_live_",
        b"ABCDEFGHIJKLMNOPQRSTUVWX",
    ] {
        raw.get_mut(len..len + part.len())
            .ok_or("mask: input")?
            .copy_from_slice(part);
        len += part.len();
    }
    let raw = raw.get(..len).ok_or("mask: input")?;
    let report = mask_report(raw, out).map_err(|_| "mask: bound")?;
    let masked = out.get(..report.output_len).ok_or("mask: length")?;
    if contains(masked, b"ABCDEFGHIJKLMNOP") || !contains(masked, b"[redacted:secret#1]") {
        return Err("mask: secret survived");
    }
    Ok(masked)
}

/// Stage 5: a goal and a memory entry survive an item-wise store and
/// restore with their digest. Returns the stored header.
fn checkpoint_stage() -> Result<[u8; HEADER_BYTES], &'static str> {
    let state = checkpoint_state()?;
    let mut header_bytes = [0u8; HEADER_BYTES];
    state
        .header()
        .encode(&mut header_bytes)
        .map_err(|_| "checkpoint: header")?;
    let header = Header::decode(&header_bytes).map_err(|_| "checkpoint: header decode")?;
    let mut restored = AgentCheckpoint::new();
    let mut restore = Restore::begin(header, &mut restored);
    let mut item = [0u8; MAX_ITEM_BYTES];
    for index in 0..state.item_count() {
        let n = state
            .encode_item(index, &mut item)
            .map_err(|_| "checkpoint: item")?;
        restore
            .push(&mut restored, item.get(..n).ok_or("checkpoint: item")?)
            .map_err(|_| "checkpoint: restore")?;
    }
    restore.finish().map_err(|_| "checkpoint: digest")?;
    if restored != state {
        return Err("checkpoint: restored state differs");
    }
    Ok(header_bytes)
}

/// The state stage 5 stores: one admitted goal, one remembered
/// observation.
fn checkpoint_state() -> Result<AgentCheckpoint, &'static str> {
    let spec = GoalSpec {
        objective: b"watch pin 3",
        priority: 1,
        cadence: Cadence::Every(BeatSpan::ONE),
        envelope: Envelope {
            per_run_turns: 4,
            per_run_mutations: 1,
            turns: 40,
            mutations: 10,
            runs: 10,
        },
        capabilities: Capabilities::empty(),
    };
    let mut ledger = GoalLedger::new();
    let goal = ledger
        .admit(&spec, &Capabilities::empty(), Beat::new(1))
        .map_err(|_| "checkpoint: admit")?;
    ledger.insert(goal).map_err(|_| "checkpoint: insert")?;
    let entry = MemoryEntry::new(
        MemoryKey::Observation {
            tool: ToolId::new(1),
            args_digest: Digest::of_bytes(br#"{"pin": 3}"#),
        },
        b"gpio_pin_read",
        OBSERVATION,
        Provenance {
            goal: Some(goal.id),
            run: None,
            beat: Beat::new(1),
        },
    )
    .map_err(|_| "checkpoint: entry")?;
    let mut memory = Memory::new();
    memory.write(entry).map_err(|_| "checkpoint: write")?;
    Ok(AgentCheckpoint {
        last_beat: Beat::new(1),
        spawn_seq: 1,
        next_deadline: 0,
        ledger,
        memory,
    })
}

/// Stage 6: an operator command crosses the `SENSE` codec intact.
fn wire_stage(out: &mut [u8]) -> Result<&[u8], &'static str> {
    let mut sense = Sense::at(42);
    let command = Command::remember(b"site", b"greenhouse north").map_err(|_| "wire: command")?;
    sense.command(command).map_err(|_| "wire: add")?;
    let n = sense.encode(out).map_err(|_| "wire: encode")?;
    let encoded = out.get(..n).ok_or("wire: length")?;
    if Sense::decode(encoded).map_err(|_| "wire: decode")? != sense {
        return Err("wire: round trip differs");
    }
    Ok(encoded)
}

/// Stage 7: the run workflow, end to end, through one reset.
fn run_stage() -> Result<crate::workload::RunOutcome, &'static str> {
    let outcome = crate::workload::drive()?;
    let record = esper_runtime::run::wire::TerminalRecord::decode(
        outcome.terminal.get(..outcome.len).ok_or("run: terminal")?,
    )
    .map_err(|_| "run: terminal does not decode")?;
    if !outcome.completed || record.summary != b"pin 3 high" {
        return Err("run: did not complete");
    }
    if outcome.boots != 2 {
        return Err("run: the reset did not fire");
    }
    if outcome.writes != 1 || !outcome.pin3_high {
        return Err("run: the write was lost or repeated");
    }
    Ok(outcome)
}
