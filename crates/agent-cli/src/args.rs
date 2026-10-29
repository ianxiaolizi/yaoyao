//! Command line parsing for the `agent` binary.
//!
//! One pass, one place. The flags shared by every command — `--session`,
//! `--json`, `--events` — are taken out wherever they appear, and whatever is
//! left is the command followed by its own arguments in their original order.
//! A command therefore never scans `argv` for a global flag, and a `--flag` it
//! does not recognize reaches it untouched instead of being silently eaten.
//!
//! It lives here rather than in `apps/agent` so that it can be tested the way
//! the other CLI tests are: by calling into the crate, not by spawning a
//! process.

use std::fmt;

use agent_protocol::ids::SessionId;

/// The session a command acts on when `--session` does not name one.
pub const DEFAULT_SESSION: &str = "default";

/// One parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// The command, as typed.
    pub command: String,
    /// The session to act on. `--session` is stripped from `rest` whether it
    /// came before or after the command, so `rest[0]` is always the command
    /// and `rest[1..]` are its own arguments.
    pub session: SessionId,
    /// Whether `--session` was actually typed.
    ///
    /// Commands that act on one session do not need this: [`DEFAULT_SESSION`]
    /// is a real session and they mean it. A command whose default scope is
    /// *every* session — `agent metrics` — has to be able to tell "the caller
    /// named the default session" from "the caller named nothing", and the
    /// value alone cannot say.
    pub session_given: bool,
    /// The profile to compose (`--profile`), if the line named one.
    pub profile: Option<String>,
    /// The model for this run (`--model`), if the line named one.
    pub model: Option<String>,
    pub json: bool,
    pub events: bool,
    /// The command and its arguments, in the order they were given.
    pub rest: Vec<String>,
}

/// Why a command line could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgsError {
    /// No command was given. Not a failure so much as an empty question: the
    /// caller prints usage and nothing else.
    NoCommand,
    /// A flag that takes a value was given without one.
    MissingValue { option: String },
    /// `--json` and `--events` were both given. They are two shapes of the same
    /// output, and picking one for the caller would make the other silently do
    /// nothing — which is the kind of bug that gets read as "the flag is
    /// broken" much later.
    ConflictingOutput,
}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCommand => write!(f, "no command"),
            Self::MissingValue { option } => write!(f, "{option} needs a value"),
            Self::ConflictingOutput => {
                write!(f, "--json and --events are two shapes of one output; give one")
            }
        }
    }
}

/// A flag that takes a value.
///
/// These are read out wherever they appear on the line, before and after the
/// command alike, because they describe the process rather than the command:
/// which session the work belongs to, and which configuration it runs under.
/// A command's *own* options are a different thing — those stay in `rest`, and
/// the command reads them with [`Args::option`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueOption {
    Session,
    Profile,
    Model,
}

impl ValueOption {
    fn named(name: &str) -> Option<Self> {
        match name {
            "--session" => Some(Self::Session),
            "--profile" => Some(Self::Profile),
            "--model" => Some(Self::Model),
            _ => None,
        }
    }
}

/// Parse everything after the program name.
pub fn parse(argv: &[String]) -> Result<Args, ArgsError> {
    let mut rest = Vec::new();
    let mut session = None;
    let mut profile = None;
    let mut model = None;
    let mut json = false;
    let mut events = false;

    let mut index = 0;
    while index < argv.len() {
        let arg = argv[index].as_str();

        // `--name value` and `--name=value` are one flag spelled two ways. A
        // command's argument that happens to contain an `=` is left alone: the
        // `=` form is only recognized for a flag this parser knows.
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if ValueOption::named(name).is_some() => (name, Some(value)),
            _ => (arg, None),
        };

        if let Some(option) = ValueOption::named(name) {
            let value = match inline {
                Some("") => {
                    return Err(ArgsError::MissingValue { option: name.to_string() });
                }
                Some(value) => value.to_string(),
                None => {
                    let missing = || ArgsError::MissingValue { option: name.to_string() };
                    let value = argv.get(index + 1).ok_or_else(missing)?;
                    // The value was consumed as `--name`'s argument, so the
                    // loop does not see it as a word of its own.
                    index += 1;
                    value.clone()
                }
            };
            match option {
                ValueOption::Session => session = Some(SessionId::from_string(&value)),
                ValueOption::Profile => profile = Some(value),
                ValueOption::Model => model = Some(value),
            }
        } else if name == "--json" {
            json = true;
        } else if name == "--events" {
            events = true;
        } else {
            rest.push(argv[index].clone());
        }
        index += 1;
    }

    if json && events {
        return Err(ArgsError::ConflictingOutput);
    }
    let command = rest.first().cloned().ok_or(ArgsError::NoCommand)?;

    Ok(Args {
        command,
        session_given: session.is_some(),
        session: session.unwrap_or_else(|| SessionId::from_string(DEFAULT_SESSION)),
        profile,
        model,
        json,
        events,
        rest,
    })
}

impl Args {
    /// The command's own arguments, with the command name dropped.
    pub fn command_args(&self) -> &[String] {
        self.rest.get(1..).unwrap_or(&[])
    }

    /// The command's non-flag arguments, joined by single spaces.
    ///
    /// A continuation is a sentence, and quoting it should not be the price of
    /// having one. The cost is that it cannot begin with `--`.
    pub fn joined_text(&self) -> String {
        self.command_args()
            .iter()
            .filter(|a| !a.starts_with("--"))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The value of `--name <value>` or `--name=<value>`, if given.
    ///
    /// Reads only; a command's own options are left in `rest` for whoever wants
    /// to look at the line as it was typed.
    ///
    /// `Ok(None)` means the flag was not given. A flag that *was* given without
    /// a value is an error, exactly as it is for the shared flags: the flag last
    /// on the line, `--name=` with nothing after it, and `--name <flag>` where
    /// the next word is another flag all yield [`ArgsError::MissingValue`]. The
    /// alternative — reading those as "not given" — makes a typo indistinguishable
    /// from an intention, which is the one thing a command cannot decide for the
    /// person who typed it.
    ///
    /// A value that itself begins with `--` is still expressible as
    /// `--name=--value`: the equals form is the escape hatch, because taking the
    /// next word regardless of what it looks like would silently eat the flag
    /// that follows.
    pub fn option(&self, name: &str) -> Result<Option<String>, ArgsError> {
        let missing = || ArgsError::MissingValue { option: name.to_string() };
        let args = self.command_args();
        if let Some(index) = args.iter().position(|arg| arg == name) {
            let Some(value) = args.get(index + 1) else {
                return Err(missing());
            };
            if value.starts_with("--") {
                return Err(missing());
            }
            return Ok(Some(value.clone()));
        }
        let with_equals = format!("{name}=");
        let Some(value) = args.iter().find_map(|arg| arg.strip_prefix(&with_equals)) else {
            return Ok(None);
        };
        if value.is_empty() {
            return Err(missing());
        }
        Ok(Some(value.to_string()))
    }
}
