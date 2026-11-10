use super::*;

#[test]
fn the_user_layers_allowlist_reaches_the_resolution() {
    // The resolution is the one place both the profile and the app can
    // read, so the entries have to survive it unchanged: canonicalization
    // belongs to the tool, and a resolution that quietly dropped one would
    // turn a configured origin into an unlisted one.
    let layers = Layers {
        user: UserConfig::from_toml_str(
            "[network]\nallow = [\"https://api.github.com\", \"https://docs.rs\"]\n",
        )
        .unwrap(),
        ..Layers::default()
    };
    assert_eq!(
        resolve(layers).unwrap().network_allow,
        vec!["https://api.github.com".to_string(), "https://docs.rs".to_string()]
    );

    // Nothing configured is no egress, not "any origin".
    assert!(resolve(Layers::default()).unwrap().network_allow.is_empty());
}

#[test]
fn the_user_layers_secret_names_reach_the_resolution_as_written() {
    // `None` is not the same answer as `Some([])`: the first is "the user
    // layer said nothing, the host's list governs", and the second is
    // "this run resolves nothing". Collapsing them would make an absent
    // section silently turn secrets off, or an empty one silently leave
    // them on.
    assert_eq!(resolve(Layers::default()).unwrap().secret_allow, None);

    let layers = Layers {
        user: UserConfig::from_toml_str("[secrets]\nallow = [\"OPENAI_API_KEY\"]\n").unwrap(),
        ..Layers::default()
    };
    assert_eq!(resolve(layers).unwrap().secret_allow, Some(vec!["OPENAI_API_KEY".to_string()]));

    let empty = Layers {
        user: UserConfig::from_toml_str("[secrets]\nallow = []\n").unwrap(),
        ..Layers::default()
    };
    assert_eq!(resolve(empty).unwrap().secret_allow, Some(Vec::new()));
}

#[test]
fn a_task_class_applies_after_the_profile_and_is_recorded() {
    let user = UserConfig::from_toml_str(
        "[profiles.coding.budget]\n\
             max_steps = 100\n\
             \n\
             [task_budgets.video]\n\
             max_steps = 24\n\
             wall_clock_timeout = \"10m\"\n\
             max_total_tokens = 200000\n",
    )
    .unwrap();
    let layers = Layers {
        user,
        profile_override: Some("coding".to_string()),
        task_override: Some("video".to_string()),
        ..Layers::default()
    };
    let resolved = resolve(layers).unwrap();
    assert_eq!(resolved.task_class.as_deref(), Some("video"));
    assert_eq!(resolved.budget.max_steps, Some(24), "the class tightens the profile");
    assert_eq!(resolved.budget.wall_clock_ms, Some(600_000));
    assert_eq!(resolved.budget.max_total_tokens, Some(200_000));
    // The derived trigger runs after the overlay, not against the profile.
    assert_eq!(resolved.budget.finalize_at_percent, Some(DEFAULT_FINALIZE_AT_PERCENT));

    // No class is not the same run: the profile's budget stands alone.
    let plain = Layers {
        user: UserConfig::from_toml_str("[profiles.coding.budget]\nmax_steps = 100\n").unwrap(),
        profile_override: Some("coding".to_string()),
        ..Layers::default()
    };
    let resolved = resolve(plain).unwrap();
    assert_eq!(resolved.task_class, None);
    assert_eq!(resolved.budget.max_steps, Some(100));
}

#[test]
fn an_unknown_task_class_is_refused_and_names_the_configured_ones() {
    let layers = Layers {
        user: UserConfig::from_toml_str("[task_budgets.video]\nmax_steps = 24\n").unwrap(),
        task_override: Some("audio".to_string()),
        ..Layers::default()
    };
    let error = resolve(layers).unwrap_err();
    assert!(matches!(error, ConfigError::UnknownTaskClass { .. }), "{error}");
    assert!(error.to_string().contains("audio"), "{error}");
    assert!(error.to_string().contains("video"), "{error}");

    let empty = Layers { task_override: Some("video".to_string()), ..Layers::default() };
    let error = resolve(empty).unwrap_err();
    assert!(error.to_string().contains("no `[task_budgets.<name>]` table"), "{error}");
}

#[test]
fn a_task_class_is_validated_like_any_other_budget_layer() {
    let layers = Layers {
        user: UserConfig::from_toml_str("[task_budgets.video]\nwall_clock_timeout = \"0s\"\n")
            .unwrap(),
        task_override: Some("video".to_string()),
        ..Layers::default()
    };
    let error = resolve(layers).unwrap_err();
    assert!(matches!(error, ConfigError::InvalidBudget { .. }), "{error}");
    assert!(error.to_string().contains("wall_clock_timeout"), "{error}");
}

/// Resolve a user configuration that selects the `coding` profile.
fn resolve_coding(toml: &str) -> Result<ResolvedProfile> {
    let layers = Layers {
        user: UserConfig::from_toml_str(toml).unwrap(),
        profile_override: Some("coding".to_string()),
        ..Layers::default()
    };
    resolve(layers)
}

#[test]
fn a_completion_check_that_could_not_run_is_refused_where_it_is_written() {
    // Every branch here is a criterion the run would otherwise start with
    // and then fail on every step: the answer belongs beside the line, not
    // in a log of failed checks. The two capability cases deliberately use
    // different top-level grants so the refusal names the missing one
    // rather than the one the file happened to authorize.
    let cases: &[(&str, &str, &str)] = &[
        (
            "[profiles.coding.completion]\ncommand = \"make check\"\n",
            "`kind` is required",
            "a check with no kind",
        ),
        (
            "[profiles.coding.completion]\nkind = \"command\"\n",
            "needs a non-empty `command`",
            "a command check with no command",
        ),
        (
            "[profiles.coding.completion]\nkind = \"command\"\ncommand = \"make check\"\nurl = \"https://x.test\"\n",
            "`url` belongs to",
            "a command check that also names a url",
        ),
        (
            "[profiles.coding.completion]\nkind = \"command\"\ncommand = \"make check\"\n",
            "`[policy] shell",
            "a command check whose profile does not allow a shell",
        ),
        (
            "[profiles.coding.policy]\nshell = \"allow\"\n[profiles.coding.completion]\nkind = \"command\"\ncommand = \"make check\"\n",
            "process.spawn",
            "a command check with no process.spawn",
        ),
        (
            "capabilities = [\"process.spawn\"]\n[profiles.coding.policy]\nshell = \"allow\"\n[profiles.coding.completion]\nkind = \"command\"\ncommand = \"make check\"\ntimeout_ms = 0\n",
            "`timeout_ms` is 0",
            "a command check with no time to run",
        ),
        (
            "[profiles.coding.completion]\nkind = \"command\"\ncommand = \"make check\"\ninterval_steps = 0\n",
            "`interval_steps` is 0",
            "a check that runs every zero steps",
        ),
        (
            "[profiles.coding.completion]\nkind = \"http\"\n",
            "needs a non-empty `url`",
            "an http check with no url",
        ),
        (
            "[profiles.coding.completion]\nkind = \"http\"\nurl = \"ftp://x.test\"\n",
            "http or https",
            "an http check on a scheme it may not fetch",
        ),
        (
            "[profiles.coding.completion]\nkind = \"http\"\nurl = \"https://x.test\"\ncommand = \"make check\"\n",
            "`command` belongs to",
            "an http check that also names a command",
        ),
        (
            "[profiles.coding.completion]\nkind = \"http\"\nurl = \"https://x.test\"\n",
            "network = \"allow\"",
            "an http check whose profile does not allow the network",
        ),
        (
            "capabilities = [\"process.spawn\"]\n[profiles.coding.policy]\nnetwork = \"allow\"\n[profiles.coding.completion]\nkind = \"http\"\nurl = \"https://x.test\"\n",
            "`network.request` is not among",
            "an http check with no network.request",
        ),
        (
            "capabilities = [\"network.request\"]\n[profiles.coding.policy]\nnetwork = \"allow\"\n[profiles.coding.completion]\nkind = \"http\"\nurl = \"https://x.test\"\ntimeout_ms = 0\n",
            "an http check may spend",
            "an http check with no time to run",
        ),
    ];
    for (toml, expected, what) in cases {
        let error = resolve_coding(toml).unwrap_err();
        assert!(matches!(error, ConfigError::InvalidCompletion { .. }), "{what}: {error}");
        let text = error.to_string();
        assert!(text.contains(expected), "{what}: {text} did not mention {expected:?}");
    }
}

#[test]
fn an_apparatus_glob_that_cannot_be_walked_is_refused() {
    let cases: &[(&str, &str, &str)] = &[
        ("apparatus = [\"\"]", "contains an empty glob", "an empty glob"),
        ("apparatus = [\"C:/out/**\"]", "names a drive or a stream", "a drive-qualified glob"),
    ];
    for (apparatus, expected, what) in cases {
        let toml = format!(
            "[profiles.coding.policy]\nshell = \"allow\"\n[profiles.coding.completion]\n\
                 kind = \"command\"\ncommand = \"make check\"\n{apparatus}\n"
        );
        let error = resolve_coding(&toml).unwrap_err();
        assert!(matches!(error, ConfigError::InvalidCompletion { .. }), "{what}: {error}");
        assert!(error.to_string().contains(expected), "{what}: {error}");
    }

    // A glob that can be walked survives as the operator wrote it, minus
    // the surrounding whitespace TOML kept: the freeze list is their order
    // and spelling, not a normalized copy.
    let toml = "capabilities = [\"process.spawn\"]\n\
                    [profiles.coding.policy]\nshell = \"allow\"\n\
                    [profiles.coding.completion]\nkind = \"command\"\ncommand = \"make check\"\n\
                    apparatus = [\" tests/** \", \"benches/*.rs\"]\n";
    let resolved = resolve_coding(toml).unwrap();
    assert_eq!(
        resolved.completion.expect("the criterion survives").apparatus,
        vec!["tests/**".to_string(), "benches/*.rs".to_string()]
    );
}

#[test]
fn a_child_profile_overrides_its_parent_field_by_field() {
    // Merging is per field, not per section: a child that repeats a table
    // still inherits every field it did not name, and a list is appended
    // rather than replaced.
    let user = UserConfig::from_toml_str(
        "plugins = [\"vendor.tools@1.0\", \"vendor.extra@1.0\"]\n\
             \n\
             [profiles.parent.policy]\n\
             shell = \"ask\"\n\
             network = \"deny\"\n\
             workspace_write = true\n\
             \n\
             [profiles.parent.budget]\n\
             max_steps = 100\n\
             max_tool_calls = 10\n\
             wall_clock_timeout = \"30s\"\n\
             max_total_tokens = 100000\n\
             finalize_at_percent = 80\n\
             settle_at_percent = 60\n\
             stall_steps = 6\n\
             cache_hit_weight_percent = 20\n\
             input_working_set_tokens = 40000\n\
             completion_cap_tokens = 2000\n\
             compaction_headroom_tokens = 6000\n\
             retain_recent_tokens = 12000\n\
             compact_at_percent = 75\n\
             preset = \"balanced\"\n\
             \n\
             [profiles.parent.plugins]\n\
             enable = [\"vendor.tools\"]\n\
             disable = [\"vendor.blocked\"]\n\
             \n\
             [profiles.parent.decisions]\n\
             laya = true\n\
             \n\
             [profiles.child]\n\
             extends = \"parent\"\n\
             \n\
             [profiles.child.policy]\n\
             network = \"allow\"\n\
             \n\
             [profiles.child.budget]\n\
             max_steps = 200\n\
             preset = \"deep\"\n\
             \n\
             [profiles.child.plugins]\n\
             enable = [\"vendor.extra\"]\n\
             \n\
             [profiles.child.decisions]\n\
             laya = false\n",
    )
    .unwrap();
    let layers = Layers { user, profile_override: Some("child".to_string()), ..Layers::default() };
    let resolved = resolve(layers).unwrap();

    // The budget: the child's two fields win, the parent's others stand.
    assert_eq!(resolved.budget.max_steps, Some(200));
    assert_eq!(resolved.budget.preset, agent_protocol::model::BudgetPreset::Deep);
    assert_eq!(resolved.budget.max_tool_calls, Some(10));
    assert_eq!(resolved.budget.wall_clock_ms, Some(30_000));
    assert_eq!(resolved.budget.max_total_tokens, Some(100_000));
    assert_eq!(resolved.budget.finalize_at_percent, Some(80));
    assert_eq!(resolved.budget.settle_at_percent, Some(60));
    assert_eq!(resolved.budget.stall_steps, Some(6));
    assert_eq!(resolved.budget.cache_hit_weight_percent, Some(20));
    assert_eq!(resolved.budget.max_input_tokens, Some(40_000));
    assert_eq!(resolved.budget.max_output_tokens, Some(2_000));
    assert_eq!(resolved.budget.compaction_headroom_tokens, Some(6_000));
    assert_eq!(resolved.budget.retain_recent_tokens, Some(12_000));
    assert_eq!(resolved.budget.compact_at_percent, Some(75));

    // The policy: the parent's `shell` and `workspace_write` stand, and
    // the child's one line replaces `network` rather than adding a second
    // rule that would tie and lose.
    assert!(resolved.policy.overrides.contains(&Directive {
        decision: PolicyDecision::Ask,
        capability: Capability::new(Capability::PROCESS_SPAWN),
    }));
    assert!(resolved.policy.overrides.contains(&Directive {
        decision: PolicyDecision::Allow,
        capability: Capability::new(Capability::NETWORK_REQUEST),
    }));
    assert_eq!(
        resolved
            .policy
            .overrides
            .iter()
            .filter(|rule| rule.capability == Capability::new(Capability::NETWORK_REQUEST))
            .count(),
        1
    );

    // The plugins: both enable lists are on, in one map.
    let plugins: Vec<&str> = resolved.plugin_versions.keys().map(String::as_str).collect();
    assert_eq!(plugins, vec!["vendor.extra", "vendor.tools"]);

    // The child turned the sidecar off, so no backend is attached.
    assert!(resolved.laya.is_none());
}

#[test]
fn a_refusal_names_the_field_that_could_not_be_honoured() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "plugins = [\"vendor.tools@1.0\"]\n[profiles.coding.plugins]\n\
                 enable = [\"vendor.tools\"]\ndisable = [\"vendor.tools\"]\n",
            "both enables and disables plugin 'vendor.tools'",
            "a plugin both enabled and disabled",
        ),
        (
            "plugins = [\"mcp.files@1.0\"]\n[profiles.coding.plugins]\nenable = [\"mcp.files\"]\n\
                 [mcp.servers.files]\ncommand = \"node\"\ntimeout = \"soon\"\n",
            "is not a duration",
            "an MCP timeout that is not one",
        ),
        (
            "[profiles.coding]\n\n[decisions.laya]\nurl = \"   \"\n",
            "`url` is empty",
            "a Laya sidecar with no address",
        ),
    ];
    for (toml, expected, what) in cases {
        let error = resolve_coding(toml).unwrap_err();
        assert!(error.to_string().contains(expected), "{what}: {error}");
    }
}
