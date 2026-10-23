//! The adapter against a real sidecar process over a real socket.
//!
//! `fake-laya-server` is this package's own `examples/` target, so the same
//! `cargo test` that builds this suite builds it: there is no separate step to
//! forget. It is an example rather than a `src/bin/` binary so that `cargo build
//! --workspace` does not put a test double where product binaries go.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use agent_decision_laya::{LayaConfig, LayaDecisionProvider};
use agent_ports::decision::{
    DecisionFamily, DecisionKind, DecisionOption, DecisionQuestion, DecisionState, DecisionValue,
    FastDecisionProvider, QuestionPack,
};
use agent_protocol::ids::{SessionId, TurnId};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// The `fake-laya-server` example Cargo built for this package's tests.
///
/// Cargo exports no `CARGO_BIN_EXE_*` for an example, so the path is worked out
/// from where this test binary sits (`<target>/<profile>/deps/`), and the
/// example is built on the spot when a narrower `--test` selection did not build
/// it — a test that skipped itself would report success for a wire it never
/// walked.
fn sidecar_bin() -> PathBuf {
    let deps = std::env::current_exe().expect("the test binary has a path");
    let profile = deps.parent().and_then(Path::parent).expect("<target>/<profile>");
    let examples = profile.join("examples");
    let path =
        examples.join(if cfg!(windows) { "fake-laya-server.exe" } else { "fake-laya-server" });
    if path.exists() {
        return path;
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    // Build into the same target directory this test binary lives under: a
    // coverage build relocates it, and a default build would not be found here.
    let mut build = std::process::Command::new(cargo);
    build
        .args(["build", "-p", "agent-decision-laya", "--example", "fake-laya-server"])
        .arg("--target-dir")
        .arg(profile.parent().expect("the target directory"))
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    if profile.ends_with("release") {
        build.arg("--release");
    }
    let built = build.status().expect("cargo is on the path");
    assert!(built.success(), "the fake-laya-server example could not be built");
    assert!(path.exists(), "{} was not built where this test looks for it", path.display());
    path
}

/// A running `fake-laya-server`, killed when the test ends.
struct FakeSidecar {
    child: Child,
    base: String,
}

impl FakeSidecar {
    async fn start() -> Self {
        let mut child = Command::new(sidecar_bin())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("the fixture binary is a member of this package");

        // The fixture prints the address it bound, so the ephemeral port needs
        // no guessing and no sleep-and-hope.
        let stdout = child.stdout.take().expect("piped");
        let mut lines = BufReader::new(stdout).lines();
        let line = lines.next_line().await.expect("read").expect("the fixture prints its address");
        let address = line.strip_prefix("listening ").expect("the address line").to_string();
        // Keep draining so the child never blocks on a full pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

        Self { child, base: format!("http://{address}") }
    }

    fn url(&self, query: &str) -> String {
        format!("{}/v1/systemone{query}", self.base)
    }

    fn provider(&self, query: &str, threshold: Option<f64>) -> LayaDecisionProvider {
        LayaDecisionProvider::new(LayaConfig {
            threshold,
            model: Some("multilingual".to_string()),
            ..LayaConfig::new(self.url(query))
        })
        .expect("the fixture endpoint is loopback")
    }
}

impl Drop for FakeSidecar {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn state() -> DecisionState {
    let mut facts = serde_json::Map::new();
    facts.insert("chosen_tier".to_string(), serde_json::json!("main"));
    facts.insert("message_count".to_string(), serde_json::json!(3));
    DecisionState::new(SessionId::from_string("s"), Some(TurnId::from_string("t")), 11, facts)
        .expect("a small state")
}

fn choice_pack() -> QuestionPack {
    // Sorted, the options are fast, main, thinking — so "the last one" is
    // "thinking", which is not what a rule baseline answering "main" would say.
    QuestionPack::new(
        DecisionFamily::new(DecisionFamily::MODEL_ROUTE),
        "v1",
        vec![DecisionQuestion::categorical(
            "tier",
            "Which tier should serve this turn?",
            vec![
                DecisionOption::new("main", "the main tier"),
                DecisionOption::new("fast", "the fast tier"),
                DecisionOption::new("thinking", "the deepest tier"),
            ],
        )],
    )
}

#[tokio::test]
async fn a_choice_answer_comes_back_as_a_categorical_value() {
    let sidecar = FakeSidecar::start().await;
    let provider = sidecar.provider("", None);
    let pack = choice_pack();

    let result = provider.decide(&pack, &state()).await.expect("the fixture answers");

    assert!(!result.abstained);
    assert_eq!(result.pack_id, pack.pack_id, "the adapter stamps the pack it was handed");
    assert_eq!(result.pack_version, pack.version);
    assert_eq!(
        result.answer_for("tier").map(|answer| answer.value.clone()),
        Some(DecisionValue::Categorical { choice: "thinking".to_string() })
    );
    assert_eq!(result.answer_for("tier").and_then(|answer| answer.confidence), Some(0.93));
    assert!(pack.validate(&result).is_ok());
    assert_eq!(provider.provider_id(), "laya");
}

#[tokio::test]
async fn a_score_is_normalized_and_a_noul_crosses_into_a_boolean() {
    let sidecar = FakeSidecar::start().await;
    let provider = sidecar.provider("", None);
    let pack = QuestionPack::new(
        DecisionFamily::new(DecisionFamily::INPUT_GUARD),
        "v1",
        vec![
            DecisionQuestion {
                question_id: "risk".to_string(),
                kind: DecisionKind::Ordinal,
                prompt: "How risky is this input?".to_string(),
                options: vec![],
            },
            DecisionQuestion {
                question_id: "needs_tools".to_string(),
                kind: DecisionKind::Binary,
                prompt: "Does this input need a tool?".to_string(),
                options: vec![],
            },
        ],
    );

    let result = provider.decide(&pack, &state()).await.expect("the fixture answers");

    // The fixture answers `score: 1.0` against the five default levels, so the
    // expected index is level 1 of 4 intervals.
    assert_eq!(
        result.answer_for("risk").map(|answer| answer.value.clone()),
        Some(DecisionValue::Ordinal { score: 0.25 })
    );
    // And `noul: 0.0236` is P(true), so it is a "no".
    assert_eq!(
        result.answer_for("needs_tools").map(|answer| answer.value.clone()),
        Some(DecisionValue::Binary { value: false })
    );
    assert!(pack.validate(&result).is_ok());
}

#[tokio::test]
async fn a_backend_that_is_unsure_of_everything_abstains() {
    let sidecar = FakeSidecar::start().await;
    let provider = sidecar.provider("?mode=abstain", None);
    let pack = choice_pack();

    let result = provider.decide(&pack, &state()).await.expect("a low-confidence answer is valid");

    assert!(result.abstained, "0.01 is below the default 0.5 threshold");
    assert!(result.answers.is_empty());
    // And the same answer at a threshold it clears is served, so abstention is
    // a threshold decision and not a broken mapping.
    let forgiving = sidecar.provider("?mode=abstain", Some(0.001));
    assert!(!forgiving.decide(&pack, &state()).await.unwrap().abstained);
}

#[tokio::test]
async fn a_sidecar_that_is_not_there_is_unavailable() {
    // A port that was just free and is now nobody's.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        listener.local_addr().expect("bound").port()
    };
    let provider =
        LayaDecisionProvider::new(LayaConfig::new(format!("http://127.0.0.1:{port}/v1/systemone")))
            .expect("loopback");

    let error = provider.decide(&choice_pack(), &state()).await.unwrap_err();

    assert_eq!(error.kind(), "unavailable");
}

#[tokio::test]
async fn an_http_error_is_a_protocol_failure_not_an_outage() {
    // `laya-serve` answers a malformed question with a 500. Reading that as
    // "the sidecar is down" would mislabel every question the server dislikes.
    let sidecar = FakeSidecar::start().await;
    let provider = sidecar.provider("?mode=fail", None);

    let error = provider.decide(&choice_pack(), &state()).await.unwrap_err();

    assert_eq!(error.kind(), "protocol");
}

#[tokio::test]
async fn a_body_that_is_not_json_is_a_protocol_failure() {
    let sidecar = FakeSidecar::start().await;
    let provider = sidecar.provider("?mode=garbage", None);

    assert_eq!(provider.decide(&choice_pack(), &state()).await.unwrap_err().kind(), "protocol");
}

#[tokio::test]
async fn a_body_cut_short_after_the_headers_is_a_protocol_failure() {
    // The response was reached: the status line and headers are delivered and
    // only the body is lost. That is a wire failure, not an outage — the
    // sidecar was up and answered.
    let sidecar = FakeSidecar::start().await;
    let provider = sidecar.provider("?mode=truncate", None);

    assert_eq!(provider.decide(&choice_pack(), &state()).await.unwrap_err().kind(), "protocol");
}

#[tokio::test]
async fn a_sidecar_that_outlasts_the_socket_bound_is_unavailable() {
    // `mode=slow` sleeps two seconds; a 50ms socket bound has to give up long
    // before that, which is the only way `DEFAULT_REQUEST_TIMEOUT`'s mechanism
    // is exercised inside this crate.
    let sidecar = FakeSidecar::start().await;
    let provider = LayaDecisionProvider::new(LayaConfig {
        request_timeout: Some(std::time::Duration::from_millis(50)),
        ..LayaConfig::new(sidecar.url("?mode=slow"))
    })
    .expect("the fixture endpoint is loopback");

    let started = std::time::Instant::now();
    let error = provider.decide(&choice_pack(), &state()).await.unwrap_err();

    assert_eq!(error.kind(), "unavailable");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "the bound cut the request, not the fixture's two-second answer"
    );
}

#[tokio::test]
async fn an_answer_shaped_for_another_question_is_a_protocol_failure() {
    let sidecar = FakeSidecar::start().await;
    let provider = sidecar.provider("?mode=wrongtype", None);

    assert_eq!(provider.decide(&choice_pack(), &state()).await.unwrap_err().kind(), "protocol");
}

#[tokio::test]
async fn a_question_left_unanswered_is_a_protocol_failure() {
    let sidecar = FakeSidecar::start().await;
    let provider = sidecar.provider("?mode=partial", None);
    let pack = QuestionPack::new(
        DecisionFamily::new(DecisionFamily::INPUT_GUARD),
        "v1",
        vec![
            DecisionQuestion {
                question_id: "a".to_string(),
                kind: DecisionKind::Binary,
                prompt: "?".to_string(),
                options: vec![],
            },
            DecisionQuestion {
                question_id: "b".to_string(),
                kind: DecisionKind::Binary,
                prompt: "?".to_string(),
                options: vec![],
            },
        ],
    );

    // One answer for two questions is not half an opinion; it is a result that
    // does not answer what was asked.
    assert_eq!(provider.decide(&pack, &state()).await.unwrap_err().kind(), "protocol");
}

#[tokio::test]
async fn a_ranking_never_reaches_the_sidecar() {
    // The request would be refused by the server; the adapter refuses first, so
    // the failure is about the pack rather than about the wire.
    let sidecar = FakeSidecar::start().await;
    let provider = sidecar.provider("", None);
    let pack = QuestionPack::new(
        DecisionFamily::new(DecisionFamily::TOOL_RANKING),
        "v1",
        vec![DecisionQuestion {
            question_id: "order".to_string(),
            kind: DecisionKind::Ranking,
            prompt: "Rank the candidates.".to_string(),
            options: vec![DecisionOption::new("a", "a")],
        }],
    );

    // Nothing was sent, so nothing can be blamed on the backend: the pack is
    // what could not be handed over.
    assert_eq!(provider.decide(&pack, &state()).await.unwrap_err().kind(), "payload");
}
