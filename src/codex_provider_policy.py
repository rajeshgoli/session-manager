"""Codex provider migration policy and operator-facing guidance."""

from __future__ import annotations

from typing import Any

CODEX_PROVIDER_MAPPING_PHASES = {
    "pre_cutover",
    "migration_window",
    "post_cutover",
}

DEFAULT_CODEX_PROVIDER_MAPPING_PHASE = "pre_cutover"

CODEX_APP_DEPRECATION_WARNING = (
    "provider=codex-app is deprecated; migrate to sm codex-fork "
    "(or sm codex --provider codex-fork)."
)
CODEX_APP_MIGRATION_WINDOW_REJECTION = (
    "provider=codex-app is deprecated; use sm codex-fork "
    "(recommended) or sm codex for rollback."
)
CODEX_APP_POST_CUTOVER_REJECTION = (
    "provider=codex-app has been removed; use sm codex-fork."
)

REMOVED_CODEX_SERVER_ENTRYPOINT_MESSAGE = (
    "sm codex-server has been removed; use sm codex-app during the deprecation "
    "window or sm codex-fork."
)


def normalize_provider_mapping_phase(value: Any) -> str:
    """Normalize configured provider mapping phase to a supported value."""
    if not isinstance(value, str):
        return DEFAULT_CODEX_PROVIDER_MAPPING_PHASE
    normalized = value.strip().lower()
    if normalized in CODEX_PROVIDER_MAPPING_PHASES:
        return normalized
    return DEFAULT_CODEX_PROVIDER_MAPPING_PHASE


def get_codex_app_policy(phase: Any = None) -> dict[str, Any]:
    """Return codex-app availability and migration guidance for a rollout phase."""
    normalized_phase = normalize_provider_mapping_phase(phase)
    if normalized_phase == "migration_window":
        return {
            "phase": normalized_phase,
            "allow_create": False,
            "warning": None,
            "rejection_error": CODEX_APP_MIGRATION_WINDOW_REJECTION,
        }
    if normalized_phase == "post_cutover":
        return {
            "phase": normalized_phase,
            "allow_create": False,
            "warning": None,
            "rejection_error": CODEX_APP_POST_CUTOVER_REJECTION,
        }
    return {
        "phase": "pre_cutover",
        "allow_create": True,
        "warning": CODEX_APP_DEPRECATION_WARNING,
        "rejection_error": CODEX_APP_MIGRATION_WINDOW_REJECTION,
    }
