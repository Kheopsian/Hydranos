use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

/// An install with a THIRD engine whose ID is not its role. Two engines
/// prove nothing here: every role-as-ID fault is invisible until an engine
/// is spelt differently from the behaviour it carries.
fn three_engines(tag: &str) -> TestState {
    state_from(
        tag,
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\n\
             [race]\nlisten_port = 16371\n\n\
             [hoard]\nlisten_port = 16372\n\n\
             [[engine]]\nengine_id = \"vpn1\"\nrole = \"race\"\n\
             [engine.session]\nlisten_port = 26991\n"
        ),
    )
}

/// ⚠ The fixture is the first thing under test. A config that quietly fails
/// to start `vpn1` would make every assertion below pass for the wrong
/// reason -- the engine would be absent because it never existed, not
/// because the lookup missed it.
#[tokio::test]
async fn the_fixture_really_starts_a_third_engine() {
    let s = three_engines("fixture-3");
    let ids: Vec<&str> = s
        .state
        .engines
        .engines()
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    assert!(
        ids.contains(&"vpn1"),
        "the fixture must start vpn1, got {ids:?}"
    );
    let vpn1 = s.state.engines.get("vpn1").expect("vpn1 present");
    assert_eq!(vpn1.role, "race", "vpn1 carries the race behaviour");
}

/// ⭐⭐ ID first, role second, and the order is the assertion. An ID
/// resolves to itself even when some other engine's role is spelt the same.
#[tokio::test]
async fn an_id_resolves_before_a_role() {
    let s = three_engines("resolve-order");
    assert_eq!(
        resolve_engine(&s.state, "vpn1").as_deref(),
        Some("vpn1"),
        "an ID resolves to itself"
    );
    assert_eq!(
        resolve_engine(&s.state, "race").as_deref(),
        Some("race"),
        "the stock engine is found by its own ID, not by the role fallback"
    );
    assert_eq!(
        resolve_engine(&s.state, "no-such-engine"),
        None,
        "an unknown selector resolves to nothing, rather than to a default"
    );
}

/// ⭐⭐ Why the resolver needs no role fallback, pinned as a fact about the
/// model rather than a choice: `race` and `hoard` are pushed by
/// `local_engines` before any `[[engine]]` block is read, so NO config can
/// produce an install without them. This test failed when it asserted the
/// opposite, which is how the fact was found.
#[tokio::test]
async fn the_stock_engine_ids_exist_even_when_the_config_omits_them() {
    let s = state_from(
        "stock-ids",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\n\
             [[engine]]\nengine_id = \"vpn1\"\nrole = \"race\"\n\
             [engine.session]\nlisten_port = 26991\n"
        ),
    );
    let ids: Vec<&str> = s
        .state
        .engines
        .engines()
        .iter()
        .map(|e| e.id.as_str())
        .collect();
    assert!(
        ids.contains(&"race") && ids.contains(&"hoard"),
        "race and hoard exist without being configured, got {ids:?}"
    );
    // So the deprecated spelling resolves as an ID, with no role lookup.
    assert_eq!(resolve_engine(&s.state, "race").as_deref(), Some("race"));
    assert_eq!(resolve_engine(&s.state, "hoard").as_deref(), Some("hoard"));
}

/// A role is NOT a selector. `vpn1` carries the race behaviour, and asking
/// for `race` must still mean the engine called race -- never "some engine
/// that behaves like one", which would differ between installs.
#[tokio::test]
async fn a_role_is_not_a_selector() {
    let s = three_engines("role-not-selector");
    assert_eq!(
        resolve_engine(&s.state, "race").as_deref(),
        Some("race"),
        "race means the engine called race, not whichever engine races"
    );
    let vpn1 = s.state.engines.get("vpn1").expect("vpn1 present");
    assert_eq!(vpn1.role, "race", "even though vpn1 races too");
}
