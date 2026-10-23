//! The command line, parsed.
//!
//! Every case here is about where a token ends up, because that is the whole
//! job: the shared flags are taken out and the rest is left in the order it was
//! typed. A command that scans for its own global flag is what this replaces.

use agent_cli::args::{ArgsError, DEFAULT_SESSION, parse};

fn argv(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_string).collect()
}

#[test]
fn a_bare_command_lands_on_the_default_session() {
    let args = parse(&argv("status")).expect("a command");
    assert_eq!(args.command, "status");
    assert_eq!(args.session.as_str(), DEFAULT_SESSION);
    assert!(!args.json && !args.events);
    assert_eq!(args.command_args(), Vec::<String>::new());
}

#[test]
fn the_session_may_come_before_or_after_the_command() {
    for line in
        ["--session other run hello", "run hello --session other", "run --session other hello"]
    {
        let args = parse(&argv(line)).unwrap_or_else(|e| panic!("{line}: {e}"));
        assert_eq!(args.session.as_str(), "other", "{line}");
        assert_eq!(args.command, "run", "{line}");
        // The flag is gone from the command's own arguments wherever it stood.
        assert_eq!(args.command_args(), ["hello"], "{line}");
    }
}

#[test]
fn the_session_accepts_the_equals_form() {
    let args = parse(&argv("run --session=other hello")).expect("a session");
    assert_eq!(args.session.as_str(), "other");
    assert_eq!(args.command_args(), ["hello"]);
}

#[test]
fn a_commands_own_options_are_left_alone() {
    let args = parse(&argv("fork --at turn-3 --reason because")).expect("a fork");
    assert_eq!(args.command, "fork");
    assert_eq!(args.option("--at").unwrap().as_deref(), Some("turn-3"));
    assert_eq!(args.option("--reason").unwrap().as_deref(), Some("because"));
    assert_eq!(args.option("--nowhere").unwrap(), None);
    // And they are still there for anyone reading the line as typed.
    assert_eq!(args.command_args(), ["--at", "turn-3", "--reason", "because"]);
}

#[test]
fn an_option_takes_the_equals_form_too() {
    let args = parse(&argv("fork --at=turn-3")).expect("a fork");
    assert_eq!(args.option("--at").unwrap().as_deref(), Some("turn-3"));
    assert_eq!(args.joined_text(), "", "a flag is not part of the sentence");
}

#[test]
fn a_commands_own_flag_is_refused_without_a_value() {
    let missing = |option: &str| ArgsError::MissingValue { option: option.to_string() };
    let args = |line: &str| parse(&argv(line)).expect("a command");

    // The flag last on the line, and the equals form with nothing after it.
    assert_eq!(args("fork --at").option("--at"), Err(missing("--at")));
    assert_eq!(args("fork --at=").option("--at"), Err(missing("--at")));
    // The next word is another flag rather than a value. Reading it as a value
    // would both fake a value and eat the flag that follows.
    assert_eq!(args("fork --at --reason why").option("--at"), Err(missing("--at")));
    assert_eq!(args("export --out --redacted").option("--out"), Err(missing("--out")));
    // Not given at all is not an error: a command may have a default.
    assert_eq!(args("fork").option("--at"), Ok(None));
    // A value that itself begins with `--` is still expressible, through `=`.
    assert_eq!(args("fork --at=--odd").option("--at"), Ok(Some("--odd".to_string())));
    // An empty *token* is a value — someone passed it on purpose. Only its
    // absence is refused.
    let empty = parse(&["fork".to_string(), "--at".to_string(), String::new()]).expect("a fork");
    assert_eq!(empty.option("--at"), Ok(Some(String::new())));
}

#[test]
fn a_sentence_survives_the_shared_flags() {
    let args = parse(&argv("resume carry the test forward --session other")).expect("a resume");
    assert_eq!(args.joined_text(), "carry the test forward");
    assert_eq!(args.session.as_str(), "other");
}

#[test]
fn two_shapes_of_one_output_are_refused() {
    assert_eq!(parse(&argv("run hi --json --events")), Err(ArgsError::ConflictingOutput));
    // In either order, and with the flag before the command.
    assert_eq!(parse(&argv("--events --json run hi")), Err(ArgsError::ConflictingOutput));
}

#[test]
fn a_flag_that_takes_a_value_is_refused_without_one() {
    let missing = |option: &str| ArgsError::MissingValue { option: option.to_string() };
    assert_eq!(parse(&argv("run hi --session")), Err(missing("--session")));
    assert_eq!(parse(&argv("run hi --session=")), Err(missing("--session")));
    // Every value flag is held to the same rule, wherever it appears.
    assert_eq!(parse(&argv("run hi --profile")), Err(missing("--profile")));
    assert_eq!(parse(&argv("run hi --model=")), Err(missing("--model")));
    // Greedy when a value follows, exactly like `--session`: the parser has no
    // way to know `run` was meant to be the command rather than the profile.
    let greedy = parse(&argv("--profile run hi")).expect("a command");
    assert_eq!(greedy.profile.as_deref(), Some("run"));
    assert_eq!(greedy.command, "hi");
}

#[test]
fn the_profile_and_model_flags_are_configuration_not_prompt_text() {
    // `run` takes its prompt from the words left over, so a `--profile` that
    // stayed in `rest` would end up read out loud as part of the sentence.
    let args = parse(&argv("run --profile coding --model big write a test")).expect("a run");
    assert_eq!(args.profile.as_deref(), Some("coding"));
    assert_eq!(args.model.as_deref(), Some("big"));
    assert_eq!(args.joined_text(), "write a test");

    let inline = parse(&argv("--profile=coding run hi")).expect("a run");
    assert_eq!(inline.profile.as_deref(), Some("coding"));
    assert_eq!(inline.joined_text(), "hi");
}

#[test]
fn an_empty_line_is_not_a_command() {
    assert_eq!(parse(&[]), Err(ArgsError::NoCommand));
    // Flags alone are still no command.
    assert_eq!(parse(&argv("--json --session other")), Err(ArgsError::NoCommand));
}

#[test]
fn an_unknown_flag_still_names_a_command() {
    // `--wat` is not a global flag, so it belongs to the command — which is
    // what makes an unknown flag the command's problem to report rather than a
    // silent no-op here.
    let args = parse(&argv("run --wat hi")).expect("a command");
    assert_eq!(args.command, "run");
    assert_eq!(args.command_args(), ["--wat", "hi"]);
}
