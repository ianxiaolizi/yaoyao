use agent_config::{Layers, UserConfig, resolve};

const PERSONAL: &str = include_str!("../../../examples/personal/config.toml");

fn resolved(profile: &str) -> agent_config::ResolvedProfile {
    let user = UserConfig::from_toml_str(PERSONAL).expect("personal config parses");
    resolve(Layers { user, profile_override: Some(profile.to_string()), ..Layers::default() })
        .expect("personal profile resolves")
}

#[test]
fn personal_profiles_resolve_with_narrow_defaults() {
    let safe = resolved("safe");
    assert!(
        !safe
            .capability_set
            .contains(&agent_protocol::permissions::Capability::new("process.spawn"))
    );

    let coding = resolved("coding");
    assert!(
        coding
            .capability_set
            .contains(&agent_protocol::permissions::Capability::new("process.spawn"))
    );
    assert_eq!(
        coding
            .policy
            .overrides
            .iter()
            .find(|d| d.capability.as_str() == "process.spawn")
            .unwrap()
            .decision,
        agent_protocol::permissions::PolicyDecision::Ask
    );

    let research = resolved("research");
    assert!(
        research
            .capability_set
            .contains(&agent_protocol::permissions::Capability::new("network.request"))
    );
}
