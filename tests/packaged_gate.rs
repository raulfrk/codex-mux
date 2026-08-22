pub fn missing_binary_is_allowed(required: bool) -> bool {
    assert!(
        !required,
        "CODEX_MUX_E2E_BINARY must be set when packaged E2E is required"
    );
    true
}

#[test]
fn required_packaged_gate_rejects_a_missing_candidate() {
    let rejected = std::panic::catch_unwind(|| missing_binary_is_allowed(true)).is_err();
    assert!(rejected);
}
