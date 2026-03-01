from src.codex_provider_policy import (
    CODEX_APP_DEPRECATION_WARNING,
    CODEX_APP_MIGRATION_WINDOW_REJECTION,
    CODEX_APP_POST_CUTOVER_REJECTION,
    get_codex_app_policy,
    normalize_provider_mapping_phase,
)


def test_normalize_provider_mapping_phase_defaults_to_pre_cutover():
    assert normalize_provider_mapping_phase(None) == "pre_cutover"
    assert normalize_provider_mapping_phase("invalid-phase") == "pre_cutover"


def test_pre_cutover_policy_allows_codex_app_with_warning():
    policy = get_codex_app_policy("pre_cutover")
    assert policy["phase"] == "pre_cutover"
    assert policy["allow_create"] is True
    assert policy["warning"] == CODEX_APP_DEPRECATION_WARNING
    assert policy["rejection_error"] == CODEX_APP_MIGRATION_WINDOW_REJECTION


def test_migration_window_policy_rejects_codex_app_creation():
    policy = get_codex_app_policy("migration_window")
    assert policy["phase"] == "migration_window"
    assert policy["allow_create"] is False
    assert policy["warning"] is None
    assert policy["rejection_error"] == CODEX_APP_MIGRATION_WINDOW_REJECTION


def test_post_cutover_policy_rejects_codex_app_creation():
    policy = get_codex_app_policy("post_cutover")
    assert policy["phase"] == "post_cutover"
    assert policy["allow_create"] is False
    assert policy["warning"] is None
    assert policy["rejection_error"] == CODEX_APP_POST_CUTOVER_REJECTION

