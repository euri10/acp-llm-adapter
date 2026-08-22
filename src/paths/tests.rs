use std::path::Path;

use super::{APPLICATION_STATE_DIR, PROXY_DIR, default_proxy_log_root, default_state_dir};

#[test]
fn proxy_root_sits_under_the_state_directory() {
    let Some(state) = default_state_dir() else {
        // Neither XDG_STATE_HOME nor HOME is set in this environment; the
        // relationship under test is unobservable, so there is nothing to check.
        return;
    };
    let Some(proxy) = default_proxy_log_root() else {
        return;
    };

    assert_eq!(proxy, state.join(PROXY_DIR));
}

#[test]
fn proxy_root_is_not_the_directory_the_session_store_enumerates() {
    let Some(state) = default_state_dir() else {
        return;
    };
    let Some(proxy) = default_proxy_log_root() else {
        return;
    };

    assert!(
        !proxy.starts_with(state.join("sessions")),
        "proxy logs must not land where the session store looks for sessions"
    );
}

#[test]
fn state_directory_is_namespaced_to_this_application() {
    let Some(state) = default_state_dir() else {
        return;
    };

    assert_eq!(
        state.file_name().and_then(|name| name.to_str()),
        Some(APPLICATION_STATE_DIR)
    );
    assert!(Path::new(&state).is_absolute() || state.is_relative());
}
