//! Who answers: the route's model, said in the request.
//!
//! The harness chose the model and the model cannot discover it: left to
//! itself it answers "who are you?" from training data, which is a different
//! model than the one running. These tests pin the sentence, its position and
//! the agreement between it and the route the request carries.

use super::*;

/// The route's two names are said in the request, so a report of "which model
/// produced this" is the route rather than the training data's guess.
#[tokio::test]
async fn the_model_identity_names_the_routed_model() {
    let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
    let loop_ = DefaultAgentLoop::new(
        model.clone(),
        Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
        no_context(),
        Arc::new(FixedModelRouter::new(labelled_route())),
    );

    run_turn(&loop_, &ctx()).await;

    let messages = &model.requests()[0].messages;
    let identity = messages
        .iter()
        .find(|m| m.message_uid.as_str().starts_with("msg/identity/"))
        .expect("the identity layer");
    let text = identity.text_content();
    assert!(text.contains("`routed-model`"), "{text}");
    assert!(text.contains("`routed-provider`"), "{text}");
    assert!(text.contains("that is the name to give"), "{text}");
    // The route the request carried and the sentence agree: two names for one
    // model would be two things a report could contradict.
    assert_eq!(
        model.requests()[0].model_route.as_ref().map(|route| route.model.as_str()),
        Some("routed-model")
    );

    // A route that names no model gets no identity sentence rather than an
    // invented one.
    assert_eq!(identity_text(&ModelRoute::new(ModelTier::Main, "p", "  ")), None);
}
