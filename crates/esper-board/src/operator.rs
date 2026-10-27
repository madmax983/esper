//! The operator channel: a line protocol over the board's UART.
//!
//! The operator types one command per line. [`Operator::feed`] takes the
//! UART's bytes as they arrive, and a finished line becomes either a
//! supervisor [`Command`] — delivered by the next `SENSE` — or typed
//! input for a goal's parked run, delivered by that run's `ASK`.
//!
//! | Line | Meaning |
//! |---|---|
//! | `goal <priority> <objective…>` | add a `Once` goal at priority `0..=9` |
//! | `say <goal> <text…>` | typed input for goal `<goal>`'s parked run |
//! | `pause <goal>` / `unpause <goal>` / `retire <goal>` | operator control |
//! | `remember <name> <value…>` / `forget <name>` | a pinned operator note |
//!
//! Everything here is RAM: a reset loses a line the operator typed and
//! no `SENSE` or `ASK` recorded yet, exactly as it loses the bytes still
//! in the UART's FIFO. What a recorded answer delivered is in the journal
//! and survives. Masking happens where the runtime says it does —
//! [`Command::add_goal`] and [`Command::remember`] mask the text they
//! are given, and `ASK`'s dispatcher masks typed input — never here.

use esper_core::goal::{Cadence, Envelope, GoalEvent, GoalId};
use esper_core::registry::Capabilities;
use esper_runtime::agent::wire::{Command, MAX_COMMANDS, Sense, WireError};

/// The longest line the channel accepts; longer lines are dropped whole.
pub const LINE_MAX: usize = 160;

/// Goals with typed input waiting, at most.
pub const INPUT_SLOTS: usize = 4;

/// Typed input longer than the buffer it was asked into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the typed input does not fit")]
pub struct TooLong;

/// What one finished line did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// A supervisor command was queued for the next `SENSE`.
    Queued,
    /// Typed input was held for the goal's next `ASK`.
    Held(GoalId),
    /// The line was not understood or did not fit.
    Refused(&'static str),
}

/// The operator channel's state: a line being typed, commands waiting
/// for `SENSE`, and typed input waiting for `ASK`.
pub struct Operator {
    line: [u8; LINE_MAX],
    len: usize,
    /// The line overran [`LINE_MAX`]; drop it at its newline.
    overrun: bool,
    commands: [Option<Command>; MAX_COMMANDS],
    inputs: [Option<(GoalId, [u8; LINE_MAX], usize)>; INPUT_SLOTS],
    /// The goal whose run is being driven, for `ASK`.
    selected: Option<GoalId>,
    envelope: Envelope,
    capabilities: Capabilities,
}

impl Operator {
    /// A channel whose `goal` lines get `envelope` and `capabilities`.
    #[must_use]
    pub const fn new(envelope: Envelope, capabilities: Capabilities) -> Self {
        Self {
            line: [0; LINE_MAX],
            len: 0,
            overrun: false,
            commands: [None; MAX_COMMANDS],
            inputs: [None; INPUT_SLOTS],
            selected: None,
            envelope,
            capabilities,
        }
    }

    /// Take one byte from the UART. A newline finishes the line and
    /// answers what it did; every other byte answers `None`.
    pub fn feed(&mut self, byte: u8) -> Option<Reply> {
        match byte {
            b'\r' => None,
            b'\n' => {
                let reply = if self.overrun {
                    Reply::Refused("line too long")
                } else {
                    self.finish()
                };
                self.len = 0;
                self.overrun = false;
                Some(reply)
            }
            _ => {
                match self.line.get_mut(self.len) {
                    Some(slot) if !self.overrun => {
                        *slot = byte;
                        self.len += 1;
                    }
                    _ => self.overrun = true,
                }
                None
            }
        }
    }

    fn finish(&mut self) -> Reply {
        let line = self.line;
        let Some(line) = line.get(..self.len) else {
            return Reply::Refused("line too long");
        };
        let (verb, rest) = split(trim(line));
        match verb {
            b"" => Reply::Refused("empty line"),
            b"goal" => {
                let (priority, objective) = split(rest);
                match (digit(priority), objective.is_empty()) {
                    (Some(priority), false) => self.queue(Command::add_goal(
                        objective,
                        priority,
                        Cadence::Once,
                        self.envelope,
                        self.capabilities,
                    )),
                    _ => Reply::Refused("usage: goal <0-9> <objective>"),
                }
            }
            b"say" => {
                let (goal, text) = split(rest);
                match goal_id(goal) {
                    Some(goal) if !text.is_empty() => self.hold(goal, text),
                    _ => Reply::Refused("usage: say <goal> <text>"),
                }
            }
            b"pause" | b"unpause" | b"retire" => {
                let event = match verb {
                    b"pause" => GoalEvent::Pause,
                    b"unpause" => GoalEvent::Unpause,
                    _ => GoalEvent::Retire,
                };
                goal_id(trim(rest)).map_or(
                    Reply::Refused("usage: pause|unpause|retire <goal>"),
                    |goal| self.queue(Ok(Command::Control { goal, event })),
                )
            }
            b"remember" => {
                let (name, value) = split(rest);
                if name.is_empty() || value.is_empty() {
                    return Reply::Refused("usage: remember <name> <value>");
                }
                self.queue(Command::remember(name, value))
            }
            b"forget" => match trim(rest) {
                b"" => Reply::Refused("usage: forget <name>"),
                name => self.queue(Ok(Command::forget(name))),
            },
            _ => Reply::Refused("unknown command"),
        }
    }

    fn queue(&mut self, command: Result<Command, WireError>) -> Reply {
        let Ok(command) = command else {
            return Reply::Refused("the command does not fit");
        };
        self.commands.iter_mut().find(|slot| slot.is_none()).map_or(
            Reply::Refused("commands are full until the next beat"),
            |slot| {
                *slot = Some(command);
                Reply::Queued
            },
        )
    }

    fn hold(&mut self, goal: GoalId, text: &[u8]) -> Reply {
        let mut held = [0u8; LINE_MAX];
        let Some(into) = held.get_mut(..text.len()) else {
            return Reply::Refused("input too long");
        };
        into.copy_from_slice(text);
        // A newer line for the same goal replaces the older one.
        let slot = self
            .inputs
            .iter_mut()
            .find(|slot| matches!(slot, Some((waiting, ..)) if *waiting == goal));
        let slot = match slot {
            Some(slot) => slot,
            None => match self.inputs.iter_mut().find(|slot| slot.is_none()) {
                Some(slot) => slot,
                None => return Reply::Refused("inputs are full"),
            },
        };
        *slot = Some((goal, held, text.len()));
        Reply::Held(goal)
    }

    /// The `SENSE` answer at `now`: every goal with input waiting, and
    /// the queued commands, which this hands over and forgets.
    ///
    /// # Errors
    ///
    /// [`WireError`] when the answer does not fit.
    pub fn sense(&mut self, now: u64) -> Result<Sense, WireError> {
        let mut sense = Sense::at(now);
        for (goal, ..) in self.inputs.iter().flatten() {
            sense.ready(*goal)?;
        }
        for command in &mut self.commands {
            if let Some(command) = command.take() {
                sense.command(command)?;
            }
        }
        Ok(sense)
    }

    /// Route `ASK` to `goal`'s waiting input.
    pub const fn select(&mut self, goal: GoalId) {
        self.selected = Some(goal);
    }

    /// The selected goal's typed input, copied into `out` and forgotten,
    /// or `None` while none has arrived.
    ///
    /// # Errors
    ///
    /// [`TooLong`] when the input does not fit `out`; it stays waiting.
    pub fn take_input(&mut self, out: &mut [u8]) -> Result<Option<usize>, TooLong> {
        let Some(goal) = self.selected else {
            return Ok(None);
        };
        let Some(slot) = self
            .inputs
            .iter_mut()
            .find(|slot| matches!(slot, Some((waiting, ..)) if *waiting == goal))
        else {
            return Ok(None);
        };
        let Some((_, held, len)) = slot else {
            return Ok(None);
        };
        let text = held.get(..*len).ok_or(TooLong)?;
        out.get_mut(..text.len())
            .ok_or(TooLong)?
            .copy_from_slice(text);
        let taken = text.len();
        *slot = None;
        Ok(Some(taken))
    }
}

/// `bytes` without leading or trailing ASCII spaces.
fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|b| *b != b' ').unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| *b != b' ')
        .map_or(start, |end| end + 1);
    bytes.get(start..end).unwrap_or_default()
}

/// The first word of `bytes` and the trimmed rest.
fn split(bytes: &[u8]) -> (&[u8], &[u8]) {
    let bytes = trim(bytes);
    bytes
        .iter()
        .position(|b| *b == b' ')
        .map_or((bytes, &[]), |at| {
            (
                bytes.get(..at).unwrap_or_default(),
                trim(bytes.get(at..).unwrap_or_default()),
            )
        })
}

/// A single decimal digit.
fn digit(bytes: &[u8]) -> Option<u8> {
    match bytes {
        [d @ b'0'..=b'9'] => Some(d - b'0'),
        _ => None,
    }
}

/// A goal id in decimal.
fn goal_id(bytes: &[u8]) -> Option<GoalId> {
    if bytes.is_empty() || bytes.len() > 5 {
        return None;
    }
    let mut value: u32 = 0;
    for byte in bytes {
        let d = byte.checked_sub(b'0').filter(|d| *d <= 9)?;
        value = value * 10 + u32::from(d);
    }
    GoalId::new(u16::try_from(value).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operator() -> Operator {
        Operator::new(
            Envelope {
                per_run_turns: 6,
                per_run_mutations: 2,
                turns: 40,
                mutations: 10,
                runs: 1,
            },
            Capabilities::empty(),
        )
    }

    fn type_line(operator: &mut Operator, line: &[u8]) -> Option<Reply> {
        let mut reply = None;
        for byte in line.iter().chain(b"\n") {
            reply = operator.feed(*byte);
        }
        reply
    }

    #[test]
    fn a_goal_line_is_queued_for_the_next_sense_and_then_forgotten() {
        let mut operator = operator();
        assert_eq!(
            type_line(&mut operator, b"goal 2 drive pin 3 high"),
            Some(Reply::Queued)
        );
        let sense = operator.sense(7).expect("fits");
        assert_eq!(sense.commands().count(), 1);
        assert_eq!(operator.sense(8).expect("fits").commands().count(), 0);
    }

    #[test]
    fn typed_input_waits_for_its_goal_and_is_taken_once() {
        let mut operator = operator();
        let goal = GoalId::new(3).expect("non-zero");
        assert_eq!(
            type_line(&mut operator, b"say 3 yes please"),
            Some(Reply::Held(goal))
        );
        assert!(operator.sense(1).expect("fits").is_ready(goal));
        let mut out = [0u8; 32];
        assert_eq!(operator.take_input(&mut out), Ok(None), "nothing selected");
        operator.select(goal);
        assert_eq!(operator.take_input(&mut out), Ok(Some(10)));
        assert_eq!(out.get(..10), Some(&b"yes please"[..]));
        assert_eq!(operator.take_input(&mut out), Ok(None), "taken once");
    }

    #[test]
    fn malformed_and_overlong_lines_are_refused_without_queueing() {
        let mut operator = operator();
        assert!(matches!(
            type_line(&mut operator, b"goal x nope"),
            Some(Reply::Refused(_))
        ));
        assert!(matches!(
            type_line(&mut operator, b"launch"),
            Some(Reply::Refused(_))
        ));
        let long = [b'a'; LINE_MAX + 1];
        assert!(matches!(
            type_line(&mut operator, &long),
            Some(Reply::Refused("line too long"))
        ));
        assert_eq!(operator.sense(1).expect("fits").commands().count(), 0);
        // The channel recovers on the next line.
        assert_eq!(
            type_line(&mut operator, b"forget wifi"),
            Some(Reply::Queued)
        );
    }

    #[test]
    fn commands_beyond_the_sense_bound_are_refused() {
        let mut operator = operator();
        for _ in 0..MAX_COMMANDS {
            assert_eq!(type_line(&mut operator, b"retire 1"), Some(Reply::Queued));
        }
        assert!(matches!(
            type_line(&mut operator, b"retire 1"),
            Some(Reply::Refused(_))
        ));
    }
}
