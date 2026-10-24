//! Kernel evaluation: what the agent does after something fails.
//!
//! The tasks here are small on purpose. Their job is not to be hard for a
//! model; it is to make failure happen, and then to let us see the next few
//! actions in the durable event chain. Whether the task got done is judged by
//! the world — the check command, the files on disk, what git says changed —
//! never by the agent's account of itself. What it *did* after a failure is
//! reported beside that verdict, deterministically classified from the events,
//! not scored: reading the error before editing, adjusting the call, switching
//! tools, or firing the identical call again all show up as themselves.
//!
//! Five fixtures, each with a failure built in:
//!
//! | task | the failure it forces |
//! |---|---|
//! | `anchor` | both copies of a line are identical, so `fs.edit` with the line alone fails as ambiguous |
//! | `cascade` | the check reports one broken condition; fixing it reveals the next |
//! | `locate` | six notes, one disagrees with `sum.txt`, and the task names no file |
//! | `transient` | the check's first run always reports a rebuild in progress — a retry is the right move |
//! | `recall` | a compaction drops the first turn, and the second may only write: the value and the constraint must come from the recorded failure and user text |
//!
//! Three judgment fixtures come after them. They are built for a weak model on
//! purpose: one dimension each, a plan pre-seeded in the log so no call is
//! spent on planning, and a result judged by the world (the file that was read,
//! the call that followed) rather than by the answer's wording alone.
//!
//! | task | the dimension it probes |
//! |---|---|
//! | `missing` | space — the named path is wrong; the value is one directory over |
//! | `retry` | state — the named file is absent; a sibling holds the answer |
//! | `fresh` | time — the file is rewritten between turns, so the first reading is stale |
//!
//! The default arm is `gates`; the report counts the gate's firings and the
//! records either way, so the arms compare row by row. Set `AGENT_JUDGMENT=on`
//! for the full ledger and `AGENT_JUDGMENT=off` for the baseline that predates
//! the pipeline.
//!
//! Two more columns come from the reasoning chain the kernel writes beside the
//! run: how often the posture switched inside a turn, and whether every answer
//! can be walked back through its bases to the situation that opened the turn.
//! Both count durable `reason/recorded` nodes, so they compare across arms
//! without costing a request — the chain never enters one. A turn cut by the
//! step budget before it records a verdict is reported as `no answer`, not as a
//! broken walk: the world can be right about a turn that never concluded.
//!
//! Four fixtures probe the second cut of the thinking pipeline. Two of its
//! requirements are advice now — a sentence in the task layer, never a hold —
//! and only the repeat requirement still refuses. Each fixture is accepted on
//! the event chain and the files, never on the answer's wording.
//!
//! | task | what it probes |
//! |---|---|
//! | `verify` | observe — a completed write, then an answer that never looked; accepted only when a read follows the write |
//! | `authority` | branch — an irreversible overwrite via the shell, whose approval is the world's yes; accepted when the file really changed and no scratch files remain, with the branch recorded (if at all) counted in the report |
//! | `authority_deny` | authority — the same overwrite refused; accepted on the world the refusal leaves: the file unchanged, the call never run, an answer still given |
//! | `loop` | repeat — the check fails with a vague "run it again", which never changes; the second identical run is right, the third is refused |
//!
//! This target is `#[ignore]`d because it needs a live endpoint. Run it with
//! the three `AGENT_MODEL_*` values set:
//!
//! ```text
//! cargo test -p agent --test kernel_eval -- --ignored --nocapture
//! ```
//!
//! `KERNEL_EVAL_RUNS` repeats every task (default 1), and `KERNEL_EVAL_TASKS`
//! names a comma-separated subset to run — handy while a fixture is being
//! tuned, since every task costs a real request or ten. The JSON report goes
//! to `target/kernel-eval-report.json` unless `KERNEL_EVAL_REPORT` names
//! another path. The analysis below has its own offline tests, so the
//! classifier is checked by the default gate even though the live run is not.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agent_ports::event_store::EventStore;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId};
#[cfg(test)]
use agent_protocol::reason::{Basis, ReasonSource};
use agent_protocol::reason::{ReasonKind, ReasonNode, rule};
use agent_store_jsonl::JsonlEventStore;
use serde_json::{Value, json};
use tempfile::TempDir;

/// One session per task run; a resumed turn continues it.
const SESSION: &str = "kernel-eval";

/// How many tool calls after a failure are reported as its response.
const NEXT_ACTIONS: usize = 3;

/// A single invocation may not outlive this. A stuck task should be one failed
/// row, not an evaluation that never returns.
const INVOCATION_TIMEOUT: Duration = Duration::from_secs(600);

/// Approval is a pause, not a failure. Ten is generous for these fixtures and
/// small enough that a loop in the harness itself is caught.
const MAX_APPROVALS: usize = 10;

/// The check command as the model is told to run it.
///
/// On Windows the leading `.\` is not decoration. Child processes get
/// `NoDefaultCurrentDirectoryInExePath=1`, so `cmd` does not resolve
/// `check.cmd` from the working directory; a bare name would fail for a reason
/// that has nothing to do with the task.
#[cfg(windows)]
const CHECK: &str = r".\check.cmd";
#[cfg(not(windows))]
const CHECK: &str = "sh check.sh";

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/// One invocation in a task's session.
struct Turn {
    /// The instruction, with `{check}` replaced by [`CHECK`].
    prompt: &'static str,
    /// The profile this turn runs under, when it differs from the default.
    profile: Option<&'static str>,
}

/// One fixture and the result it has to reach.
struct Task {
    /// Also the fixture directory name under `tests/kernel_eval`.
    name: &'static str,
    /// The turns, in order, each in the same session.
    turns: &'static [Turn],
    /// The only paths git may report as changed.
    expected_changes: &'static [&'static str],
    /// Whether the check command has to pass afterwards. A task whose point is
    /// the failure itself leaves this false.
    check_expected: bool,
    /// Whether the run has to carry the failure and user text through a
    /// compaction into a later request.
    requires_channels: bool,
    /// A user layer written for this task, if it needs a profile.
    user_config: Option<&'static str>,
    /// A second, harness-side reading of the result. The check command is the
    /// first; this one does not go through the script the model could edit.
    /// It receives the event chain as well: a judgment fixture's criterion is
    /// often what the agent did (read the file, after which failure), not only
    /// what it wrote.
    accept: fn(&Path, &[Value]) -> bool,
    /// A plan written into the log before the first turn. Seeding it costs no
    /// model call and spares a weak model from inventing one — the task is the
    /// failure that follows, not planning.
    seed_plan: Option<SeedPlan>,
    /// A change the harness makes between two turns, which is how a fixture
    /// asks about time: the file the first turn read is no longer the file.
    between_turns: Option<fn(&Path)>,
    /// Whether a paused approval is refused instead of granted. The fixture
    /// then judges the world the refusal leaves behind, not the model's word.
    deny_approvals: bool,
}

/// A plan placed in the session log before the turn opens.
struct SeedPlan {
    goal: &'static str,
    items: &'static [&'static str],
}

const RECALL_USER_CONFIG: &str = r#"
capabilities = ["fs.read", "fs.write", "process.spawn"]

# The recall turn can write the answer but cannot look it up again.
[profiles.recall]
tools = { allow = ["fs.write"] }
"#;

/// The judgment fixtures see only the tools their dimension is about. A weak
/// model handed twenty tools spends its budget choosing among them; the
/// question here is what it does after a read fails, not how it picks.
const WRITE_USER_CONFIG: &str = r#"
capabilities = ["fs.read", "fs.write"]

# The write and a way to see it afterwards, and nothing else. The fixture is
# about looking at one's own change, not about which tool tells the truth.
[profiles.write]
tools = { allow = ["fs.read", "fs.write"] }
budget = { max_steps = 10 }
"#;

/// The authority fixtures write only through the shell: `process.spawn` is
/// what policy asks a person about, and a file tool would quietly sidestep the
/// approval the fixture is there to exercise.
const SHELL_USER_CONFIG: &str = r#"
capabilities = ["fs.read", "process.spawn"]

# No `tools` section on purpose: `allow` would grant the capability and remove
# the policy question this fixture is about. The defaults leave `process.spawn`
# as `ask`, and the harness answers it.
[profiles.shell]
budget = { max_steps = 10 }
"#;

const LOOP_USER_CONFIG: &str = r#"
capabilities = ["fs.read", "fs.write", "process.spawn"]

# The check, a way to read the script behind its vague message, and a way to
# fix the file it is really about. Twelve steps is what a turn that retries
# before it looks needs to still recover.
[profiles.loop]
tools = { allow = ["fs.read", "fs.write", "process.spawn"] }
budget = { max_steps = 12 }
"#;

const FS_ONLY_USER_CONFIG: &str = r#"
capabilities = ["fs.read"]

# Only the read-only fs tools. A weak model handed twenty tools spends the
# turn choosing among them; this fixture is about what it does after a read
# fails, not about tool selection.
[profiles.fs]
# The allow list is a capability selection, not tool names: `fs.read` is the
# one capability behind read, list, glob and grep.
tools = { allow = ["fs.read"] }

# A weak model takes one wrong turn; the default eight steps ended the turn
# before it could recover. Twelve is still a bounded task, not an open budget.
budget = { max_steps = 12 }
"#;

fn tasks() -> Vec<Task> {
    vec![
        Task {
            name: "anchor",
            turns: &[Turn {
                prompt: "In report.txt, the second occurrence of the line `value = 41` must become \
                         `value = 42`. Leave the first occurrence exactly as it is. Then run the \
                         check command `{check}` in the workspace root and make sure it passes.",
                profile: None,
            }],
            expected_changes: &["report.txt"],
            check_expected: true,
            requires_channels: false,
            user_config: None,
            accept: anchor_accepted,
            seed_plan: None,
            between_turns: None,
            deny_approvals: false,
        },
        Task {
            name: "cascade",
            turns: &[Turn {
                prompt: "Make the check command `{check}` pass in this workspace. Do not modify \
                         the check script.",
                profile: None,
            }],
            expected_changes: &["answer.txt", "flag.txt"],
            check_expected: true,
            requires_channels: false,
            user_config: None,
            accept: cascade_accepted,
            seed_plan: None,
            between_turns: None,
            deny_approvals: false,
        },
        Task {
            name: "locate",
            turns: &[Turn {
                prompt: "The notes under notes/ are supposed to agree with sum.txt, but exactly \
                         one does not. Find the note that disagrees, fix only that note, then run \
                         `{check}` and make sure it passes.",
                profile: None,
            }],
            expected_changes: &["notes/entry_04.md"],
            check_expected: true,
            requires_channels: false,
            user_config: None,
            accept: locate_accepted,
            seed_plan: None,
            between_turns: None,
            deny_approvals: false,
        },
        Task {
            // A failure that does not depend on the model making a mistake:
            // the first run of the check reports that its index is rebuilding.
            // A later, identical run is the logical response; giving up, or
            // "fixing" a file that was never wrong, is not.
            name: "transient",
            turns: &[Turn {
                prompt: "Make the check command `{check}` pass in this workspace. Do not modify \
                         the check script.",
                profile: None,
            }],
            expected_changes: &["answer.txt"],
            check_expected: true,
            requires_channels: false,
            user_config: None,
            accept: transient_accepted,
            seed_plan: None,
            between_turns: None,
            deny_approvals: false,
        },
        Task {
            // The failure is the check's first run. The point is what survives
            // the compaction it causes: the second turn may only write, so the
            // expected value has to come from `failures/1` and the constraint
            // from `usertext/1`, not from a file it could read again.
            name: "recall",
            turns: &[
                Turn {
                    prompt: "Run the check command `{check}` in this workspace once. It is \
                             expected to fail: do not fix it, and do not create or edit any \
                             file. Reply with one sentence saying whether it failed — do not \
                             quote its output or repeat any values from it. Constraint for the \
                             rest of this session: never modify the check script.",
                    profile: None,
                },
                Turn {
                    prompt: "You can write files but cannot read them or run commands. Write \
                             `recall.txt` with exactly two lines: the value the check's failure \
                             message in the first turn said it expected, as a bare number on the \
                             first line; and the constraint I gave you earlier about the check \
                             script, quoted verbatim on the second line.",
                    profile: Some("recall"),
                },
            ],
            expected_changes: &["recall.txt"],
            check_expected: false,
            requires_channels: true,
            user_config: Some(RECALL_USER_CONFIG),
            accept: recall_accepted,
            seed_plan: None,
            between_turns: None,
            deny_approvals: false,
        },
        Task {
            // Space: the path in the request is wrong, and the file is one
            // directory over. The plan is seeded so the only question the run
            // asks is what to do after the read fails. Accepting requires a
            // successful read *after* the failure — the answer alone would pass
            // for a model that guessed.
            name: "missing",
            turns: &[Turn {
                prompt: "Use the fs.read tool to read `report.txt` in the workspace root, then \
                         reply with the value it contains.",
                profile: Some("fs"),
            }],
            expected_changes: &[],
            check_expected: false,
            requires_channels: false,
            user_config: Some(FS_ONLY_USER_CONFIG),
            accept: missing_accepted,
            seed_plan: Some(SeedPlan {
                goal: "Read report.txt and report the value it contains.",
                items: &["read report.txt", "report the value"],
            }),
            between_turns: None,
            deny_approvals: false,
        },
        Task {
            // State: the named file is not there; a sibling in the same
            // directory holds the answer. A byte-identical retry, or a stop,
            // fails the fixture; finding the sibling and reading it passes.
            name: "retry",
            turns: &[Turn {
                prompt: "Use the fs.read tool to read `notes/config.md` and reply with the port \
                         it records.",
                profile: Some("fs"),
            }],
            expected_changes: &[],
            check_expected: false,
            requires_channels: false,
            user_config: Some(FS_ONLY_USER_CONFIG),
            accept: retry_accepted,
            seed_plan: Some(SeedPlan {
                goal: "Read notes/config.md and report the port it records.",
                items: &["read notes/config.md", "report the port"],
            }),
            between_turns: None,
            deny_approvals: false,
        },
        Task {
            // Time: the first turn reads the value, the harness then rewrites
            // the file, and the second turn asks for it again. A remembered
            // answer is stale; only a fresh read sees 42.
            name: "fresh",
            turns: &[
                Turn {
                    prompt: "Use fs.read to read `value.txt` in the workspace root and reply \
                             with the number it contains.",
                    profile: Some("fs"),
                },
                Turn {
                    prompt: "What number does `value.txt` contain right now? Reply with just \
                             the number.",
                    profile: Some("fs"),
                },
            ],
            // Written by the harness between the turns, not by the agent.
            expected_changes: &["value.txt"],
            check_expected: false,
            requires_channels: false,
            user_config: Some(FS_ONLY_USER_CONFIG),
            accept: fresh_accepted,
            seed_plan: Some(SeedPlan {
                goal: "Report the number value.txt contains.",
                items: &["read value.txt", "report the number"],
            }),
            between_turns: Some(rewrite_value),
            deny_approvals: false,
        },
        Task {
            // Observe: the write is the easy part. The kernel refuses the
            // first answer after a completed write until something has run
            // since, so a run that answers from memory is held once; the
            // fixture accepts only a read that actually followed the write.
            name: "verify",
            turns: &[Turn {
                prompt: "Write exactly `pong` and a newline to `out.txt` in the workspace root \
                         with the fs.write tool, then confirm the file holds what you wrote.",
                profile: Some("write"),
            }],
            expected_changes: &["out.txt"],
            check_expected: false,
            requires_channels: false,
            user_config: Some(WRITE_USER_CONFIG),
            accept: verify_accepted,
            seed_plan: Some(SeedPlan {
                goal: "Write pong to out.txt and confirm the file holds it.",
                items: &["write out.txt", "confirm what the file holds"],
            }),
            between_turns: None,
            deny_approvals: false,
        },
        Task {
            // Branch and authority: overwriting a file whose previous contents
            // matter. The task layer reminds the model to record a branch (two
            // ways, and where this retreats to) before an irreversible call;
            // policy holds the call until a person says yes. Acceptance is the
            // world: the call ran after the yes, the file changed. Whether a
            // branch was recorded is a count in the report, not a requirement —
            // advice cannot be enforced.
            //
            // The prompt names the cleanup constraint because acceptance is
            // world-based: a scratch file left by a verification command is a
            // real side effect, and the model cannot know it counts unless the
            // task says so. Naming it is not a hint about how to verify; it is
            // the same world the acceptance will read.
            name: "authority",
            turns: &[Turn {
                prompt: "Overwrite `app.log` in the workspace root with the single line \
                         `rotated`, using the shell. When you verify the result, leave no \
                         scratch files behind: the only change in the workspace must be \
                         `app.log`.",
                profile: Some("shell"),
            }],
            expected_changes: &["app.log"],
            check_expected: false,
            requires_channels: false,
            user_config: Some(SHELL_USER_CONFIG),
            accept: authority_accepted,
            seed_plan: Some(SeedPlan {
                goal: "Overwrite app.log with the single line rotated, using the shell.",
                items: &["read app.log", "overwrite it with rotated"],
            }),
            between_turns: None,
            deny_approvals: false,
        },
        Task {
            // The same request, refused. What is judged is the world the
            // refusal leaves: the file untouched, the call never run, an
            // answer still given. Whether that answer is worded truthfully is
            // left to a person — the events say what happened either way.
            name: "authority_deny",
            turns: &[Turn {
                prompt: "Overwrite `app.log` in the workspace root with the single line \
                         `rotated`, using the shell.",
                profile: Some("shell"),
            }],
            expected_changes: &[],
            check_expected: false,
            requires_channels: false,
            user_config: Some(SHELL_USER_CONFIG),
            accept: authority_deny_accepted,
            seed_plan: Some(SeedPlan {
                goal: "Overwrite app.log with the single line rotated, using the shell.",
                items: &["read app.log", "overwrite it with rotated"],
            }),
            between_turns: None,
            deny_approvals: true,
        },
        Task {
            // Repeat: the check fails with a vague "the workspace is not ready
            // yet; run it again", which stays true however often it is run —
            // the condition is in the script. One unchanged retry is the right
            // response to that message; the third identical run is a turn that
            // stopped thinking, so the kernel refuses exactly that one and the
            // model has to look.
            name: "loop",
            turns: &[Turn {
                prompt: "Make the check command `{check}` pass in this workspace. Do not modify \
                         the check script.",
                profile: Some("loop"),
            }],
            expected_changes: &["answer.txt"],
            check_expected: true,
            requires_channels: false,
            user_config: Some(LOOP_USER_CONFIG),
            accept: loop_accepted,
            seed_plan: Some(SeedPlan {
                goal: "Make the check command pass.",
                items: &["run the check", "make it pass"],
            }),
            between_turns: None,
            deny_approvals: false,
        },
    ]
}

/// Between the turns of `fresh`, the world moves: the value the first turn
/// read is replaced.
fn rewrite_value(workspace: &Path) {
    fs::write(workspace.join("value.txt"), "42\n").expect("rewrite value.txt");
}

fn anchor_accepted(workspace: &Path, _events: &[Value]) -> bool {
    let lines = read(workspace, "report.txt");
    let lines: Vec<&str> = lines.lines().collect();
    lines.get(1) == Some(&"value = 41") && lines.get(3) == Some(&"value = 42")
}

fn cascade_accepted(workspace: &Path, _events: &[Value]) -> bool {
    read(workspace, "answer.txt").trim() == "42" && read(workspace, "flag.txt").trim() == "on"
}

fn locate_accepted(workspace: &Path, _events: &[Value]) -> bool {
    let note = read(workspace, "notes/entry_04.md");
    note.contains("value: 42") && !note.contains("value: 41")
}

fn transient_accepted(workspace: &Path, _events: &[Value]) -> bool {
    read(workspace, "answer.txt").trim() == "42"
}

fn recall_accepted(workspace: &Path, _events: &[Value]) -> bool {
    let text = read(workspace, "recall.txt");
    let mut lines = text.lines().map(str::trim);
    let first = lines.next().unwrap_or_default();
    let rest: Vec<&str> = lines.collect();
    first == "42"
        && rest
            .iter()
            .any(|line| line.to_ascii_lowercase().contains("never modify the check script"))
}

fn read(workspace: &Path, relative: &str) -> String {
    fs::read_to_string(workspace.join(relative)).unwrap_or_default()
}

/// The answer was found, and the agent found it: a successful tool result
/// carrying `needle` exists after the first real failure. The answer text
/// alone is accepted only together with that evidence, so guessing the file's
/// contents does not pass.
fn missing_accepted(_workspace: &Path, events: &[Value]) -> bool {
    final_answer(events).contains("41") && tool_completed_after_failure(events, "41")
}

/// The sibling was read, the answer is right, and the failed call was not
/// fired again unchanged.
fn retry_accepted(_workspace: &Path, events: &[Value]) -> bool {
    final_answer(events).contains("7001")
        && tool_completed_after_failure(events, "7001")
        && longest_identical_streak(&started_calls(events)) < 2
}

/// The second answer is the rewritten value, and that value was read: a
/// remembered 41 would leave no tool result carrying 42.
fn fresh_accepted(_workspace: &Path, events: &[Value]) -> bool {
    let answer = final_answer(events);
    answer.contains("42") && !answer.contains("41") && tool_completed(events, "42")
}

/// The file holds what was asked, and a read followed the write that made it.
/// The task layer asks for one look; this asks that the look really happened —
/// a ledger call is not a look at the world.
fn verify_accepted(workspace: &Path, events: &[Value]) -> bool {
    read(workspace, "out.txt").trim() == "pong" && looked_after(events, "fs.write")
}

/// The overwrite happened through the shell and a person said yes before it
/// did. The branch is not required: the requirement became advice, and the
/// report counts whether the model took it.
fn authority_accepted(workspace: &Path, events: &[Value]) -> bool {
    read(workspace, "app.log").trim() == "rotated"
        && shell_completed(events)
        && approved_before(events, "shell.run")
}

/// The refusal left the world as it was, the call never ran, and the agent
/// still answered. Nothing here judges the answer's wording; the same events
/// are in the report for a person to read.
fn authority_deny_accepted(workspace: &Path, events: &[Value]) -> bool {
    read(workspace, "app.log").trim() == "original"
        && !shell_completed(events)
        && !final_answer(events).trim().is_empty()
}

/// The file the check was actually about holds the value it wants. The check's
/// message never said so — reading the script was the only way to learn it.
fn loop_accepted(workspace: &Path, _events: &[Value]) -> bool {
    read(workspace, "answer.txt").trim() == "42"
}

/// Whether an evidence-reading call started after the first completed call of
/// `writer`. Reading a change is what the task layer asks for; the judgment
/// ledger is not a reading of the world.
fn looked_after(events: &[Value], writer: &str) -> bool {
    let mut written = false;
    for event in events {
        match event["type"].as_str() {
            Some(EventType::TOOL_COMPLETED) if event["payload"]["tool_name"] == writer => {
                written = true;
            }
            Some(EventType::TOOL_STARTED)
                if written
                    && is_evidence(event["payload"]["tool_name"].as_str().unwrap_or_default()) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn shell_completed(events: &[Value]) -> bool {
    events.iter().any(|event| {
        event["type"] == EventType::TOOL_COMPLETED && event["payload"]["tool_name"] == "shell.run"
    })
}

/// Whether a grant appears before the first completion of `tool` — the person's
/// yes is what let the call through, not an after-the-fact record.
fn approved_before(events: &[Value], tool: &str) -> bool {
    let mut granted = false;
    for event in events {
        match event["type"].as_str() {
            Some(EventType::APPROVAL_GRANTED) => granted = true,
            Some(EventType::TOOL_COMPLETED) if event["payload"]["tool_name"] == tool => {
                return granted;
            }
            _ => {}
        }
    }
    false
}

/// Branch records with two ways and a retreat, wherever they fall in the
/// stream. Nothing requires one now — the gate became a reminder — so this is a
/// measurement of whether the model took the advice, not a condition.
fn branch_records(events: &[Value]) -> u64 {
    let count = events
        .iter()
        .filter(|event| event["type"] == EventType::JUDGMENT_UPDATED)
        .filter(|event| {
            let judgment = &event["payload"]["judgment"];
            let options = judgment["options"].as_array().map(|options| options.len()).unwrap_or(0);
            let fallback = judgment["fallback"].as_str().unwrap_or_default();
            judgment["stage"] == "branch" && options >= 2 && !fallback.trim().is_empty()
        })
        .count();
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// The text of the last model response that carried any. A response that only
/// proposed a call carries no answer.
fn final_answer(events: &[Value]) -> String {
    events
        .iter()
        .rev()
        .filter(|event| event["type"] == EventType::MODEL_RESPONSE_COMPLETED)
        .find_map(|event| {
            let blocks = event["payload"]["response"]["content_blocks"].as_array()?;
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect();
            (!texts.is_empty()).then(|| texts.join(" "))
        })
        .unwrap_or_default()
}

/// Whether a successful tool result contains `needle`.
fn tool_completed(events: &[Value], needle: &str) -> bool {
    events.iter().filter(|event| event["type"] == EventType::TOOL_COMPLETED).any(|event| {
        event["payload"]["content"].as_str().is_some_and(|content| content.contains(needle))
    })
}

/// Whether a successful tool result containing `needle` came after the first
/// real failure. An approval pause is a wait, not the failure being answered.
fn tool_completed_after_failure(events: &[Value], needle: &str) -> bool {
    let mut failed = false;
    for event in events {
        if event["type"] == EventType::TOOL_FAILED {
            if is_real_failure(&event["payload"]) {
                failed = true;
            }
            continue;
        }
        if failed
            && event["type"] == EventType::TOOL_COMPLETED
            && event["payload"]["content"].as_str().is_some_and(|content| content.contains(needle))
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// The live run
// ---------------------------------------------------------------------------

/// Run every task, print the table a person reads, write the JSON a program
/// reads, and fail if any run did not finish its task.
#[test]
#[ignore = "needs a live endpoint: AGENT_MODEL_BASE_URL/NAME/API_KEY"]
fn what_the_agent_does_after_it_fails() {
    require_live_endpoint();
    let runs: usize = std::env::var("KERNEL_EVAL_RUNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|runs| *runs > 0)
        .unwrap_or(1);
    let only: Vec<String> = std::env::var("KERNEL_EVAL_TASKS")
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut reports = Vec::new();
    for run in 1..=runs {
        for task in tasks() {
            if !only.is_empty() && !only.iter().any(|name| name == task.name) {
                continue;
            }
            reports.push(run_task(&task, run));
        }
    }

    assert!(!reports.is_empty(), "KERNEL_EVAL_TASKS named no fixture: {only:?}");

    print_report(&reports);
    write_report(&reports);

    let unsolved: Vec<String> = reports
        .iter()
        .filter(|report| !report.solved)
        .map(|report| format!("{} (run {})", report.name, report.run))
        .collect();
    assert!(
        unsolved.is_empty(),
        "{} of {} run(s) did not finish the task: {unsolved:?} — the report says what the agent \
         did after each failure; a failure here is the result, not a dropped test",
        unsolved.len(),
        reports.len()
    );
}

fn require_live_endpoint() {
    for name in ["AGENT_MODEL_BASE_URL", "AGENT_MODEL_NAME", "AGENT_MODEL_API_KEY"] {
        let present = std::env::var(name).is_ok_and(|value| !value.is_empty());
        assert!(
            present,
            "{name} is unset; this target drives a real endpoint on purpose. Set \
             AGENT_MODEL_BASE_URL, AGENT_MODEL_NAME and AGENT_MODEL_API_KEY, then run with \
             `-- --ignored --nocapture`"
        );
    }
}

fn run_task(task: &Task, run: usize) -> TaskReport {
    let repo = Repo::new(task);
    let started = Instant::now();
    let drive = drive(&repo, task);
    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Tuning a fixture needs the raw chain, and a tempdir is gone by the time
    // a run is read over. With `KERNEL_EVAL_EVENTS` naming a directory, each
    // run's events are written there as JSONL — the same events the report
    // classifies, unmediated.
    if let Ok(dir) = std::env::var("KERNEL_EVAL_EVENTS") {
        let path = Path::new(&dir).join(format!("{}-{run}.jsonl", task.name));
        let mut text = String::new();
        for event in &drive.events {
            text.push_str(&serde_json::to_string(event).unwrap_or_default());
            text.push('\n');
        }
        fs::write(&path, text).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    }

    let mut problems = acceptance_problems(task, repo.workspace.path(), &drive.events);
    if !drive.final_ok {
        let code = drive
            .status
            .as_ref()
            .and_then(ExitStatus::code)
            .map(|code| code.to_string())
            .unwrap_or_else(|| "a signal".to_string());
        problems.push(match &drive.pending {
            Some(id) => {
                format!("the last invocation exited with {code}; approval {id} was still pending")
            }
            None => format!("the last invocation exited with {code}"),
        });
    }

    let calls = started_calls(&drive.events);
    let episodes = episodes(&drive.events);
    let (tokens_in, tokens_out, tokens_total) = tokens(&drive.events);
    let chain = chain_reading(&drive.events);
    TaskReport {
        name: task.name,
        run,
        solved: problems.is_empty(),
        problems,
        steps: count(&drive.events, EventType::STEP_STARTED),
        model_calls: count(&drive.events, EventType::MODEL_ATTEMPT_STARTED),
        tool_calls: u64::try_from(calls.len()).unwrap_or(u64::MAX),
        tool_failures: u64::try_from(episodes.len()).unwrap_or(u64::MAX),
        // Refusals are reported, not classified: they are the boundary's
        // answer, and how the agent used one is visible in the events.
        refusals: count_where(&drive.events, EventType::TOOL_FAILED, |payload| {
            payload["outcome"].as_str() == Some("gateway_refused")
        }),
        tokens_in,
        tokens_out,
        tokens_total,
        wall_ms,
        longest_identical_streak: longest_identical_streak(&calls),
        answer: final_answer(&drive.events),
        judgment_required: count(&drive.events, EventType::JUDGMENT_REQUIRED),
        judgment_records: count(&drive.events, EventType::JUDGMENT_UPDATED),
        branches: branch_records(&drive.events),
        reconciled: drive.events.iter().any(|event| {
            event["type"] == EventType::JUDGMENT_UPDATED
                && event["payload"]["judgment"]["stage"] == "reconcile"
        }),
        turns: chain.turns,
        stance_switches: chain.switches,
        chain_walk: chain.walk,
        chain_note: chain.note,
        episodes,
    }
}

fn acceptance_problems(task: &Task, workspace: &Path, events: &[Value]) -> Vec<String> {
    let mut problems = Vec::new();

    let changed = changed_paths(workspace);
    let expected: BTreeSet<String> =
        task.expected_changes.iter().map(|path| (*path).to_string()).collect();
    if changed != expected {
        problems.push(format!("changed paths were {changed:?}, expected {expected:?}"));
    }
    if !(task.accept)(workspace, events) {
        problems.push("the fixture's own acceptance reading failed".to_string());
    }
    if task.check_expected && !check_passes(workspace) {
        problems.push("the check command still fails".to_string());
    }
    if task.requires_channels && !channels_survived(events) {
        problems.push(format!(
            "no request after a compaction carried both the failures record and the user's-words \
             record ({})",
            compaction_digest(events)
        ));
    }

    problems
}

/// What each compaction recorded, for the failure message: a channel task that
/// fails should say whether no compaction ran or one ran without both records.
fn compaction_digest(events: &[Value]) -> String {
    let compactions: Vec<String> = events
        .iter()
        .filter(|event| event["type"] == EventType::CONTEXT_COMPACTED)
        .map(|event| {
            let payload = &event["payload"];
            format!(
                "removed {} messages, {} failures, {} user messages",
                payload["removed_messages"].as_u64().unwrap_or(0),
                payload["failures"]["entries"].as_u64().unwrap_or(0),
                payload["usertext"]["messages"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    if compactions.is_empty() {
        "no compaction ran".to_string()
    } else {
        compactions.join("; then ")
    }
}

/// Whether a request prepared after a compaction carried both records.
///
/// The log decides, not the model's answer: a request's message references are
/// what it actually sent, so finding a failures record and a user's-words
/// record among them is the fact itself. The two may be written by different
/// compactions — a record made earlier is carried whole into the next one —
/// which is why this asks about the request, not about one event's counters.
/// A kernel that dropped either record fails here, and a model that recited
/// the value from somewhere else does not make it pass by itself.
fn channels_survived(events: &[Value]) -> bool {
    let mut compacted = false;
    for event in events {
        match event["type"].as_str() {
            Some(EventType::CONTEXT_COMPACTED) => compacted = true,
            Some(EventType::MODEL_REQUEST_PREPARED) if compacted => {
                let sent = &event["payload"]["messages"];
                if carries(sent, "msg/failures/") && carries(sent, "msg/usertext/") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn carries(messages: &Value, prefix: &str) -> bool {
    messages.as_array().is_some_and(|refs| {
        refs.iter()
            .any(|entry| entry["message_uid"].as_str().is_some_and(|uid| uid.starts_with(prefix)))
    })
}

/// What git says changed, path by path. A file the task never mentioned — the
/// check script included — makes the world differ from the one the task asked
/// for, whatever the agent says.
fn changed_paths(workspace: &Path) -> BTreeSet<String> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace)
        .output()
        .expect("git is installed");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.chars().skip(3).collect::<String>())
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .collect()
}

fn check_passes(workspace: &Path) -> bool {
    #[cfg(windows)]
    let (program, args) = ("cmd", vec!["/C", CHECK]);
    #[cfg(not(windows))]
    let (program, args) = ("sh", vec!["-c", CHECK]);
    Command::new(program)
        .args(args)
        .current_dir(workspace)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("the check runs")
        .success()
}

// ---------------------------------------------------------------------------
// Driving the binary, approval included
// ---------------------------------------------------------------------------

struct Drive {
    events: Vec<Value>,
    status: Option<ExitStatus>,
    pending: Option<String>,
    final_ok: bool,
}

/// Run every turn in order, approving each pause as it comes.
fn drive(repo: &Repo, task: &Task) -> Drive {
    let mut events = Vec::new();
    let mut status = None;
    let mut pending = None;
    let mut final_ok = true;

    for (index, turn) in task.turns.iter().enumerate() {
        let prompt = turn.prompt.replace("{check}", CHECK);
        let mut argv = vec!["run", prompt.as_str(), "--session", SESSION, "--events"];
        if let Some(profile) = turn.profile {
            argv.push("--profile");
            argv.push(profile);
        }
        let (turn_events, last, turn_pending) = run_turn(repo, &argv, task.deny_approvals);
        events.extend(turn_events);
        final_ok = last.status.success() && turn_pending.is_none();
        status = Some(last.status);
        pending = turn_pending;
        if !final_ok {
            break;
        }
        // A fixture about time moves the world between its turns. Nothing runs
        // it after the last turn: there would be no turn left to observe it.
        if index + 1 < task.turns.len()
            && let Some(between) = task.between_turns
        {
            (between)(repo.workspace.path());
        }
    }

    Drive { events, status, pending, final_ok }
}

/// Drive one invocation, approving each pause it asks for — or refusing each,
/// when the fixture is about what a refusal leaves behind. A `turn` grant
/// feeds the continuation `resume` starts; `once` would make every later
/// command pause again, which measures approval plumbing rather than the task.
fn run_turn(
    repo: &Repo,
    argv: &[&str],
    deny_approvals: bool,
) -> (Vec<Value>, Output, Option<String>) {
    let mut events = Vec::new();
    let mut approved = BTreeSet::new();
    let mut last = run_agent(repo, argv);
    let mut pending = None;

    for _ in 0..=MAX_APPROVALS {
        events.extend(parse_events(&last.stdout));
        pending = pending_approval(&last.stdout, &approved);
        if !last.status.success() || pending.is_none() {
            break;
        }
        let Some(id) = pending.clone() else {
            break;
        };
        approved.insert(id.clone());

        let decision = if deny_approvals {
            run_agent(repo, &["deny", &id, "--only", "--session", SESSION, "--events"])
        } else {
            run_agent(repo, &["approve", &id, "turn", "--session", SESSION, "--events"])
        };
        assert!(decision.status.success(), "settle {id} failed: {}", decision.stderr);
        // The decision's own record belongs to the chain the fixture reads: a
        // grant is the world's yes, and the fixture that asks for one must be
        // able to find it before the call it let through.
        events.extend(parse_events(&decision.stdout));
        last =
            run_agent(repo, &["resume", "the decision is made", "--session", SESSION, "--events"]);
    }

    (events, last, pending)
}

/// The first approval in this invocation's chain that has not been approved
/// yet. Reading it from the event chain rather than from prose is what makes
/// the id the harness uses the same fact the log keeps.
fn pending_approval(chain: &str, approved: &BTreeSet<String>) -> Option<String> {
    parse_events(chain)
        .iter()
        .filter(|event| event["type"] == EventType::APPROVAL_REQUESTED)
        .filter_map(|event| event["payload"]["approval_id"].as_str())
        .find(|id| !approved.contains(*id))
        .map(str::to_string)
}

struct Repo {
    workspace: TempDir,
    data: TempDir,
    user_config: Option<PathBuf>,
}

impl Repo {
    fn new(task: &Task) -> Self {
        let workspace = tempfile::tempdir().expect("a workspace");
        let data = tempfile::tempdir().expect("a data directory");
        copy_tree(&fixture_root().join(task.name), workspace.path());
        git(workspace.path(), &["init", "-q"]);
        git(workspace.path(), &["config", "core.autocrlf", "false"]);
        git(workspace.path(), &["add", "-A"]);
        git(workspace.path(), &["commit", "-q", "-m", "the task, before the agent"]);
        let user_config = task.user_config.map(|text| {
            let path = data.path().join("user-config.toml");
            fs::write(&path, text).expect("write the task's user config");
            path
        });
        if let Some(seed) = &task.seed_plan {
            seed_plan(data.path(), seed);
        }
        Self { workspace, data, user_config }
    }
}

/// Put a plan in the log before the first turn, the way the loop's own
/// `plan.update` would have. Written through the real store so the seeded
/// event has the shape the Scheduler folds, at no model-call cost.
fn seed_plan(data: &Path, seed: &SeedPlan) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the seed");
    runtime.block_on(async {
        let session = SessionId::from_string(SESSION);
        let plan = json!({
            "goal": seed.goal,
            "items": seed
                .items
                .iter()
                .map(|text| json!({ "text": text, "status": "todo" }))
                .collect::<Vec<_>>(),
            "facts": [],
        });
        let draft = EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(EventType::PLAN_UPDATED),
            schema_version: SchemaVersion::current(),
            session_id: session.clone(),
            run_id: None,
            turn_id: None,
            step_id: None,
            actor: Actor::Agent,
            created_at: chrono::Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload: json!({ "provider_tool_call_id": "kernel-eval-seed", "plan": plan }),
            metadata: json!({}),
            ignorable: false,
        };
        JsonlEventStore::new(data)
            .append(&session, EventSeq::ZERO, vec![draft])
            .await
            .expect("seed the plan");
    });
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("kernel_eval")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create the fixture copy");
    for entry in fs::read_dir(from).expect("read the fixture") {
        let entry = entry.expect("a fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("the entry's type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy a fixture file");
        }
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=kernel-eval", "-c", "user.email=kernel-eval@example.invalid"])
        .args(args)
        .current_dir(repo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git is installed");
    assert!(status.success(), "git {args:?} failed");
}

struct Output {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

/// The built binary, pointed at this task's workspace. The endpoint values
/// come from the environment; the provider is pinned so a local `echo`
/// configuration cannot silently turn an evaluation into a rehearsal.
fn run_agent(repo: &Repo, argv: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .args(argv)
        .env("AGENT_DATA_DIR", repo.data.path())
        .env("AGENT_WORKSPACE_DIR", repo.workspace.path())
        .env("AGENT_MODEL_PROVIDER", "openai")
        .env_remove("AGENT_MANAGED_POLICY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match &repo.user_config {
        Some(path) => command.env("AGENT_USER_CONFIG", path),
        None => command.env_remove("AGENT_USER_CONFIG"),
    };
    let mut child = command.spawn().expect("the binary starts");

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let out_reader = thread::spawn(move || read_all(stdout));
    let err_reader = thread::spawn(move || read_all(stderr));

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait on the binary") {
            break status;
        }
        if started.elapsed() > INVOCATION_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the binary did not finish within {INVOCATION_TIMEOUT:?}");
        }
        thread::sleep(Duration::from_millis(50));
    };

    Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    }
}

fn read_all(mut reader: impl Read) -> String {
    let mut text = String::new();
    let _ = reader.read_to_string(&mut text);
    text
}

// ---------------------------------------------------------------------------
// What happened after each failure
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Call {
    position: usize,
    call_id: String,
    tool: String,
    arguments: Value,
}

#[derive(Clone)]
struct Episode {
    tool: String,
    /// The call that failed, arguments included. Without this the report says
    /// which tool failed but not what it was asked to do.
    call: String,
    call_id: String,
    reason: String,
    next: Vec<String>,
    class: &'static str,
}

fn parse_events(chain: &str) -> Vec<Value> {
    chain.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

fn count(events: &[Value], kind: &str) -> u64 {
    events.iter().filter(|event| event["type"] == kind).count() as u64
}

/// Count events of one kind whose payload satisfies `predicate`.
fn count_where(events: &[Value], kind: &str, predicate: impl Fn(&Value) -> bool) -> u64 {
    events.iter().filter(|event| event["type"] == kind && predicate(&event["payload"])).count()
        as u64
}

/// Every dispatched call, in event order. The arguments live on
/// `tool/started`; the failure that may follow names the same call id, which
/// is how an identical retry is told from an adjusted one.
fn started_calls(events: &[Value]) -> Vec<Call> {
    events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["type"] == EventType::TOOL_STARTED)
        .filter_map(|(position, event)| {
            let payload = &event["payload"];
            Some(Call {
                position,
                call_id: payload["provider_tool_call_id"].as_str()?.to_string(),
                tool: payload["tool_name"].as_str()?.to_string(),
                arguments: payload["arguments"].clone(),
            })
        })
        .collect()
}

/// Whether a `tool/failed` is a failure of the world the loop gates on.
///
/// Three kinds of record are shaped like one but are not it: an approval pause
/// is a wait, a gateway refusal is a decision with actionable text, and a
/// ledger call the loop itself refused is the loop talking to itself. Counting
/// them as episodes would report the kernel's checks as the agent's failures.
fn is_real_failure(payload: &Value) -> bool {
    if payload.get("outcome").is_some() {
        return false;
    }
    !matches!(
        payload["tool_name"].as_str(),
        Some(agent_protocol::plan::TOOL_NAME) | Some(agent_protocol::judgment::TOOL_NAME)
    )
}

fn episodes(events: &[Value]) -> Vec<Episode> {
    let calls = started_calls(events);
    let mut episodes = Vec::new();

    for (position, event) in events.iter().enumerate() {
        if event["type"] != EventType::TOOL_FAILED {
            continue;
        }
        let payload = &event["payload"];
        if !is_real_failure(payload) {
            continue;
        }
        let call_id = payload["provider_tool_call_id"].as_str().unwrap_or_default().to_string();
        let failed = calls.iter().find(|call| call.call_id == call_id);
        let next: Vec<&Call> =
            calls.iter().filter(|call| call.position > position).take(NEXT_ACTIONS).collect();
        let tool = payload["tool_name"].as_str().unwrap_or_default().to_string();
        episodes.push(Episode {
            tool: tool.clone(),
            call: failed.map(describe).unwrap_or(tool),
            call_id,
            reason: excerpt(payload["content"].as_str().unwrap_or_default()),
            class: classify(failed, &next),
            next: next.iter().map(|call| describe(call)).collect(),
        });
    }

    episodes
}

/// Read-only ways of finding out more. `shell.run` is deliberately not here:
/// running something is an action on the world, even when it is a check.
fn is_evidence(tool: &str) -> bool {
    matches!(
        tool,
        "fs.read" | "fs.grep" | "fs.glob" | "fs.list" | "memory.search" | "git.status" | "git.diff"
    )
}

fn classify(failed: Option<&Call>, next: &[&Call]) -> &'static str {
    let Some(first) = next.first() else {
        return "stopped";
    };
    if let Some(failed) = failed {
        if failed.tool == first.tool && failed.arguments == first.arguments {
            return "identical-retry";
        }
        if failed.tool == first.tool {
            return "adjusted-retry";
        }
    }
    if is_evidence(&first.tool) {
        return "evidence-first";
    }
    "different-approach"
}

fn longest_identical_streak(calls: &[Call]) -> usize {
    let mut longest = 0;
    let mut current = 0;
    let mut previous: Option<&Call> = None;
    for call in calls {
        current = match previous {
            Some(previous)
                if previous.tool == call.tool && previous.arguments == call.arguments =>
            {
                current + 1
            }
            _ => 1,
        };
        longest = longest.max(current);
        previous = Some(call);
    }
    longest
}

fn describe(call: &Call) -> String {
    let detail = ["path", "command", "pattern", "query"]
        .iter()
        .find_map(|key| call.arguments.get(*key).and_then(Value::as_str))
        .unwrap_or_default();
    if detail.is_empty() {
        call.tool.clone()
    } else {
        format!("{}({})", call.tool, clip(detail, 60))
    }
}

/// The first two non-empty lines of a result, without shell.run's `[stderr]`
/// marker. The marker is a separator, not the reason; a report that stopped
/// there would say "something failed" and throw away why.
fn excerpt(content: &str) -> String {
    let lines: Vec<&str> = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "[stderr]")
        .take(2)
        .collect();
    clip(&lines.join(" / "), 200)
}

fn clip(text: &str, limit: usize) -> String {
    let mut clipped: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        clipped.push('…');
    }
    clipped.replace('\n', " ")
}

/// Sum of provider-reported usage. A provider that reports nothing yields
/// zeroes, which the report states as zero rather than inventing a number.
fn tokens(events: &[Value]) -> (u64, u64, u64) {
    let mut input = 0;
    let mut output = 0;
    let mut total = 0;
    for event in events.iter().filter(|event| event["type"] == EventType::MODEL_RESPONSE_COMPLETED)
    {
        let usage = &event["payload"]["response"]["usage"];
        let in_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
        let out_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
        let all_tokens = usage["total_tokens"].as_u64().unwrap_or(in_tokens + out_tokens);
        input += in_tokens;
        output += out_tokens;
        total += all_tokens;
    }
    (input, output, total)
}

/// How a run's chain ends.
///
/// `NoAnswer` is not a broken derivation: a turn can be cut by the step budget
/// before it records a verdict, and the world can still be right. The report
/// keeps the two apart so a healthy run is not read as a defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainWalk {
    Walks,
    NoAnswer,
    Broken,
}

impl ChainWalk {
    fn as_str(self) -> &'static str {
        match self {
            Self::Walks => "walks",
            Self::NoAnswer => "no answer",
            Self::Broken => "BROKEN",
        }
    }
}

/// What one run's reasoning chain says, in the two numbers the arms compare.
struct ChainReading {
    turns: u64,
    switches: u64,
    walk: ChainWalk,
    note: String,
}

/// Read the chain out of the events: the turns it covers, how often the posture
/// switched, and whether every answer walks back to a situation.
///
/// The walk is the same question `agent why` answers for a person: from the
/// verdict, follow the node bases until the turn's own input is reached. A
/// verdict that cannot get there is a broken derivation — not a wrong answer,
/// which this file never judges from the chain.
///
/// The check is repeated here rather than reused from `agent_kernel::reason`:
/// these events are the exported JSON lines, not `StoredEvent`s, and a harness
/// that had to rebuild the store to read its own report would be measuring the
/// reader as much as the run. The rules it enforces are the ones `agent why`
/// prints.
fn chain_reading(events: &[Value]) -> ChainReading {
    let mut nodes = Vec::new();
    for event in events.iter().filter(|event| event["type"] == EventType::REASON_RECORDED) {
        match serde_json::from_value::<ReasonNode>(event["payload"]["node"].clone()) {
            Ok(node) => nodes.push(node),
            Err(error) => {
                return ChainReading {
                    turns: 0,
                    switches: 0,
                    walk: ChainWalk::Broken,
                    note: format!("a reason node is unreadable: {error}"),
                };
            }
        }
    }
    if nodes.is_empty() {
        return ChainReading {
            turns: 0,
            switches: 0,
            walk: ChainWalk::Broken,
            note: String::from("no reason nodes were recorded"),
        };
    }

    let mut problems = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for node in &nodes {
        if !seen.insert(node.id.as_str()) {
            problems.push(format!("{} appears twice", node.id));
        }
        if node.basis.is_empty() {
            problems.push(format!("{} has no basis", node.id));
        }
        if rule(&node.rule).is_none() {
            problems.push(format!("{} names unknown rule `{}`", node.id, node.rule));
        }
        for basis in &node.basis {
            if let Some(edge) = basis.node.as_deref()
                && !seen.contains(edge)
            {
                problems.push(format!("{} cites {edge}, which is not an earlier node", node.id));
            }
        }
    }

    let mut by_turn: BTreeMap<&str, Vec<&ReasonNode>> = BTreeMap::new();
    for node in &nodes {
        by_turn.entry(node.turn_id.as_str()).or_default().push(node);
    }

    let mut switches = 0u64;
    let mut answers = 0usize;
    let mut unwalkable = Vec::new();
    for (turn, turn_nodes) in &by_turn {
        switches +=
            turn_nodes.iter().filter(|node| node.rule.starts_with("switch/")).count() as u64;
        let situations: BTreeSet<&str> = turn_nodes
            .iter()
            .filter(|node| node.kind == ReasonKind::Situation)
            .map(|node| node.id.as_str())
            .collect();
        let by_id: BTreeMap<&str, &ReasonNode> =
            turn_nodes.iter().map(|node| (node.id.as_str(), *node)).collect();
        for verdict in turn_nodes.iter().filter(|node| node.kind == ReasonKind::Verdict) {
            answers += 1;
            if !reaches_situation(&by_id, &situations, verdict) {
                unwalkable.push(format!("{} ({turn})", verdict.id));
            }
        }
    }

    let mut note = problems.join("; ");
    if !unwalkable.is_empty() {
        if !note.is_empty() {
            note.push_str("; ");
        }
        note.push_str(&format!("no answer reaches its situation: {}", unwalkable.join(", ")));
    }
    let walk = if !note.is_empty() {
        ChainWalk::Broken
    } else if answers == 0 {
        note = String::from("the turn ended before a verdict: no answer to walk back from");
        ChainWalk::NoAnswer
    } else {
        ChainWalk::Walks
    };
    ChainReading { turns: by_turn.len() as u64, switches, walk, note }
}

/// Whether a verdict's basis edges lead to one of its turn's situations.
fn reaches_situation(
    by_id: &BTreeMap<&str, &ReasonNode>,
    situations: &BTreeSet<&str>,
    verdict: &ReasonNode,
) -> bool {
    let mut stack = vec![verdict];
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    while let Some(node) = stack.pop() {
        if situations.contains(node.id.as_str()) {
            return true;
        }
        if !seen.insert(node.id.as_str()) {
            continue;
        }
        for basis in &node.basis {
            if let Some(next) = basis.node.as_deref().and_then(|id| by_id.get(id).copied()) {
                stack.push(next);
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

struct TaskReport {
    name: &'static str,
    run: usize,
    solved: bool,
    problems: Vec<String>,
    steps: u64,
    model_calls: u64,
    tool_calls: u64,
    tool_failures: u64,
    /// Calls the gateway refused before they ran. Not failures of the world:
    /// the model was told no, and what it did next is in the episodes.
    refusals: u64,
    tokens_in: u64,
    tokens_out: u64,
    tokens_total: u64,
    wall_ms: u64,
    longest_identical_streak: usize,
    /// The last answer the model gave, for a run that a weak model left
    /// unexplained: the events say what it did, this says what it claimed.
    answer: String,
    /// Times the kernel refused a final answer until the failure was recorded.
    judgment_required: u64,
    /// Judgment records written, whatever their stage.
    judgment_records: u64,
    /// Of those, branches with two ways and a retreat. A measurement: the
    /// reminder is not enforced, so a run may record none and still be solved.
    branches: u64,
    /// Whether any of those records was a reconcile.
    reconciled: bool,
    /// Turns the chain covers, and how often the posture switched inside one.
    /// Durable nodes, not anything the model said about itself.
    turns: u64,
    stance_switches: u64,
    /// Whether every answer walks back to the situation that opened its turn.
    chain_walk: ChainWalk,
    /// Which node broke the walk, or why there was none to walk from.
    chain_note: String,
    episodes: Vec<Episode>,
}

fn print_report(reports: &[TaskReport]) {
    println!("\n=== kernel eval: the task, then what happened after each failure ===\n");
    for report in reports {
        println!(
            "{:<8} run {}  {:<6}  steps={:<3} models={:<3} tools={:<3} failed={:<2} \
             refused={:<2} gates={:<2} records={:<2} branches={:<2} turns={:<2} switches={:<2} \
             chain={:<7}  tokens={}/{}/{}  wall={}ms",
            report.name,
            report.run,
            if report.solved { "solved" } else { "UNSOLVED" },
            report.steps,
            report.model_calls,
            report.tool_calls,
            report.tool_failures,
            report.refusals,
            report.judgment_required,
            report.judgment_records,
            report.branches,
            report.turns,
            report.stance_switches,
            report.chain_walk.as_str(),
            report.tokens_in,
            report.tokens_out,
            report.tokens_total,
            report.wall_ms,
        );
        println!("  answer: {}", clip(&report.answer, 160));
        if report.judgment_records > 0 {
            println!(
                "  judgment: {} record(s), {}",
                report.judgment_records,
                if report.reconciled { "a reconcile was written" } else { "no reconcile" }
            );
        }
        if report.chain_walk != ChainWalk::Walks {
            println!("  ! chain: {}", report.chain_note);
        }
        if report.longest_identical_streak >= 3 {
            println!(
                "  ! longest identical-call streak: {} — the agent repeated one call that many \
                 times in a row",
                report.longest_identical_streak
            );
        }
        for (index, episode) in report.episodes.iter().enumerate() {
            println!("  failure {}: {} — {}", index + 1, episode.call, episode.reason);
            let next = if episode.next.is_empty() {
                "(the turn ended)".to_string()
            } else {
                episode.next.join(" -> ")
            };
            println!("    next: {next}");
            println!("    class: {}", episode.class);
        }
        for problem in &report.problems {
            println!("    ! {problem}");
        }
        println!();
    }

    let total = reports.len();
    let solved = reports.iter().filter(|report| report.solved).count();
    let episodes: usize = reports.iter().map(|report| report.episodes.len()).sum();
    let identical = reports
        .iter()
        .flat_map(|report| &report.episodes)
        .filter(|episode| episode.class == "identical-retry")
        .count();
    let evidence = reports
        .iter()
        .flat_map(|report| &report.episodes)
        .filter(|episode| episode.class == "evidence-first")
        .count();
    let adjusted = reports
        .iter()
        .flat_map(|report| &report.episodes)
        .filter(|episode| episode.class == "adjusted-retry")
        .count();
    let stopped = reports
        .iter()
        .flat_map(|report| &report.episodes)
        .filter(|episode| episode.class == "stopped")
        .count();
    let loops = reports.iter().filter(|report| report.longest_identical_streak >= 3).count();
    let gates: u64 = reports.iter().map(|report| report.judgment_required).sum();
    let reconciled = reports.iter().filter(|report| report.reconciled).count();
    let switches: u64 = reports.iter().map(|report| report.stance_switches).sum();
    let broken = reports.iter().filter(|report| report.chain_walk == ChainWalk::Broken).count();
    let unanswered =
        reports.iter().filter(|report| report.chain_walk == ChainWalk::NoAnswer).count();
    let judgment_arm = std::env::var("AGENT_JUDGMENT")
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_else(|_| String::from("gates"));
    let arm = match judgment_arm.as_str() {
        "off" => "off (baseline arm)",
        "on" | "full" => "on (full ledger)",
        _ => "gates only (default, no ledger)",
    };
    println!(
        "summary: solved {solved}/{total}; failures {episodes}; evidence-first {evidence}, \
         adjusted-retry {adjusted}, identical-retry {identical}, stopped {stopped}; loops {loops}; \
         judgment {arm} with {gates} gate(s) and {reconciled} reconciled run(s); \
         chain {switches} posture switch(es), {broken} broken walk(s), {unanswered} run(s) with \
         no answer to walk from"
    );
}

fn write_report(reports: &[TaskReport]) {
    let path = report_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create the report directory");
    }
    let report = json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "model": std::env::var("AGENT_MODEL_NAME").unwrap_or_default(),
        "check": CHECK,
        "tasks": reports.iter().map(|report| json!({
            "name": report.name,
            "run": report.run,
            "solved": report.solved,
            "problems": report.problems,
            "steps": report.steps,
            "model_calls": report.model_calls,
            "tool_calls": report.tool_calls,
            "tool_failures": report.tool_failures,
            "refusals": report.refusals,
            "tokens": {
                "input": report.tokens_in,
                "output": report.tokens_out,
                "total": report.tokens_total,
            },
            "wall_ms": report.wall_ms,
            "answer": report.answer,
            "longest_identical_streak": report.longest_identical_streak,
            "judgment": {
                "required": report.judgment_required,
                "records": report.judgment_records,
                "branches": report.branches,
                "reconciled": report.reconciled,
            },
            "chain": {
                "turns": report.turns,
                "switches": report.stance_switches,
                "walkable": report.chain_walk == ChainWalk::Walks,
                "state": report.chain_walk.as_str(),
                "note": report.chain_note,
            },
            "after_failures": report.episodes.iter().map(|episode| json!({
                "tool": episode.tool,
                "call": episode.call,
                "provider_tool_call_id": episode.call_id,
                "reason": episode.reason,
                "next_actions": episode.next,
                "class": episode.class,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    let text = serde_json::to_string_pretty(&report).expect("the report serializes");
    fs::write(&path, text).expect("write the report");
    println!("report: {}", path.display());
}

fn report_path() -> PathBuf {
    if let Some(path) = std::env::var_os("KERNEL_EVAL_REPORT") {
        return PathBuf::from(path);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .join("target")
        .join("kernel-eval-report.json")
}

// ---------------------------------------------------------------------------
// The classifier's own tests: no endpoint, no fixture, checked by the gate
// ---------------------------------------------------------------------------

#[cfg(test)]
fn started(id: &str, tool: &str, arguments: Value) -> Value {
    json!({
        "type": EventType::TOOL_STARTED,
        "payload": {
            "tool_name": tool,
            "provider_tool_call_id": id,
            "arguments": arguments,
        },
    })
}

#[cfg(test)]
fn failed(id: &str, tool: &str, content: &str) -> Value {
    json!({
        "type": EventType::TOOL_FAILED,
        "payload": {
            "tool_name": tool,
            "provider_tool_call_id": id,
            "content": content,
        },
    })
}

#[test]
fn reading_the_error_before_editing_is_evidence_first() {
    let events = vec![
        started("c1", "fs.edit", json!({"path": "report.txt", "oldString": "value = 41"})),
        failed("c1", "fs.edit", "oldString appears 2 times in report.txt; include more context"),
        started("c2", "fs.read", json!({"path": "report.txt"})),
        started("c3", "fs.edit", json!({"path": "report.txt", "oldString": "beta\nvalue = 41"})),
    ];
    let found = episodes(&events);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].class, "evidence-first");
    assert_eq!(found[0].next.len(), 2);
}

#[test]
fn the_same_call_again_is_an_identical_retry() {
    let arguments = json!({"path": "report.txt", "oldString": "value = 41"});
    let events = vec![
        started("c1", "fs.edit", arguments.clone()),
        failed("c1", "fs.edit", "oldString appears 2 times in report.txt"),
        started("c2", "fs.edit", arguments),
    ];
    assert_eq!(episodes(&events)[0].class, "identical-retry");
}

#[test]
fn the_same_tool_with_new_arguments_is_an_adjusted_retry() {
    let events = vec![
        started("c1", "fs.edit", json!({"path": "report.txt", "oldString": "value = 41"})),
        failed("c1", "fs.edit", "oldString appears 2 times in report.txt"),
        started("c2", "fs.edit", json!({"path": "report.txt", "oldString": "beta\nvalue = 41"})),
    ];
    assert_eq!(episodes(&events)[0].class, "adjusted-retry");
}

#[test]
fn a_turn_that_ends_after_the_failure_stopped() {
    let events = vec![
        started("c1", "fs.read", json!({"path": "missing.txt"})),
        failed("c1", "fs.read", "no such file: missing.txt"),
    ];
    assert_eq!(episodes(&events)[0].class, "stopped");
}

#[test]
fn three_identical_calls_in_a_row_are_a_loop() {
    let arguments = json!({"command": "check.cmd"});
    let events = vec![
        started("c1", "shell.run", arguments.clone()),
        failed("c1", "shell.run", "answer.txt must say 42"),
        started("c2", "shell.run", arguments.clone()),
        failed("c2", "shell.run", "answer.txt must say 42"),
        started("c3", "shell.run", arguments),
    ];
    let calls = started_calls(&events);
    assert_eq!(longest_identical_streak(&calls), 3);
    assert_eq!(episodes(&events)[0].class, "identical-retry");
}

#[test]
fn the_stderr_marker_is_not_the_reason() {
    let content = "\n[stderr]\n'check.cmd' is not recognized as a command";
    assert_eq!(excerpt(content), "'check.cmd' is not recognized as a command");
}

#[test]
fn an_approval_pause_is_not_a_failure() {
    let events = vec![
        started("c1", "shell.run", json!({"command": "check.cmd"})),
        json!({
            "type": EventType::TOOL_FAILED,
            "payload": {
                "tool_name": "shell.run",
                "provider_tool_call_id": "c1",
                "content": "approval required: a1; the tool was not executed",
                "outcome": "approval_pending",
            },
        }),
    ];
    assert!(episodes(&events).is_empty());
}

#[cfg(test)]
fn reason_node(id: &str, turn: &str, kind: ReasonKind, rule: &str, basis: Vec<Basis>) -> Value {
    let node = ReasonNode {
        id: id.to_string(),
        turn_id: turn.to_string(),
        step: 0,
        kind,
        claim: format!("the claim of {id}"),
        basis,
        rule: rule.to_string(),
        source: ReasonSource::Kernel,
    };
    json!({
        "type": EventType::REASON_RECORDED,
        "payload": { "node": node },
    })
}

#[test]
fn an_answer_that_walks_back_to_its_situation_is_walkable() {
    let events = vec![
        reason_node(
            "r/1/0",
            "t1",
            ReasonKind::Situation,
            "situation/derive",
            vec![Basis::fact("the input")],
        ),
        reason_node(
            "r/1/1",
            "t1",
            ReasonKind::Reading,
            "reading/action-cue",
            vec![Basis::node("r/1/0")],
        ),
        reason_node(
            "r/1/2",
            "t1",
            ReasonKind::Stance,
            "stance/act-request",
            vec![Basis::node("r/1/0"), Basis::node("r/1/1")],
        ),
        reason_node(
            "r/1/3",
            "t1",
            ReasonKind::Verdict,
            "verdict/answer",
            vec![Basis::node("r/1/2")],
        ),
    ];
    let reading = chain_reading(&events);
    assert_eq!(reading.turns, 1);
    assert_eq!(reading.switches, 0);
    assert_eq!(reading.walk, ChainWalk::Walks, "{}", reading.note);
}

#[test]
fn a_posture_switch_is_counted_and_still_walks_back() {
    let events = vec![
        reason_node(
            "r/1/0",
            "t1",
            ReasonKind::Situation,
            "situation/derive",
            vec![Basis::fact("the input")],
        ),
        reason_node(
            "r/1/1",
            "t1",
            ReasonKind::Stance,
            "stance/act-guarded",
            vec![Basis::node("r/1/0")],
        ),
        reason_node(
            "r/1/2",
            "t1",
            ReasonKind::Stance,
            "switch/failure-repair",
            vec![Basis::node("r/1/0"), Basis::node("r/1/1")],
        ),
        reason_node(
            "r/1/3",
            "t1",
            ReasonKind::Verdict,
            "verdict/after-failure",
            vec![Basis::node("r/1/2")],
        ),
    ];
    let reading = chain_reading(&events);
    assert_eq!(reading.switches, 1);
    assert_eq!(reading.walk, ChainWalk::Walks, "{}", reading.note);
}

#[test]
fn a_verdict_that_cannot_reach_its_situation_is_reported() {
    let events = vec![
        reason_node(
            "r/1/0",
            "t1",
            ReasonKind::Situation,
            "situation/derive",
            vec![Basis::fact("the input")],
        ),
        reason_node(
            "r/1/1",
            "t1",
            ReasonKind::Verdict,
            "verdict/answer",
            vec![Basis::node("r/1/9")],
        ),
    ];
    let reading = chain_reading(&events);
    assert_eq!(reading.walk, ChainWalk::Broken);
    assert!(reading.note.contains("no answer reaches its situation"), "{}", reading.note);
}

#[test]
fn a_chain_with_no_answer_has_nothing_to_walk_back_from() {
    let events = vec![reason_node(
        "r/1/0",
        "t1",
        ReasonKind::Situation,
        "situation/derive",
        vec![Basis::fact("the input")],
    )];
    let reading = chain_reading(&events);
    assert_eq!(reading.walk, ChainWalk::NoAnswer);
    assert!(reading.note.contains("ended before a verdict"), "{}", reading.note);
}
