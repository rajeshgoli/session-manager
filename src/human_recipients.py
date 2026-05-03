"""File-driven human recipient registry."""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any, Optional


class HumanRecipientConfigError(ValueError):
    """Raised when the human recipient registry is ambiguous or malformed."""


@dataclass(frozen=True)
class HumanChannel:
    """One configured delivery channel for a human recipient."""

    name: str
    enabled: bool = False
    delivery: Optional[str] = None
    address: Optional[str] = None
    address_env: Optional[str] = None
    use: Optional[str] = None

    def resolved_address(self) -> Optional[str]:
        """Return the configured email address without requiring it in source config."""
        if self.address:
            return self.address
        if self.address_env:
            value = os.environ.get(self.address_env)
            if value:
                return value.strip() or None
        return None


@dataclass(frozen=True)
class HumanRecipient:
    """A canonical human/operator recipient and its aliases."""

    name: str
    display_name: str
    aliases: tuple[str, ...]
    default_channel: str
    channels: dict[str, HumanChannel]

    @property
    def available_channels(self) -> tuple[str, ...]:
        """Return enabled channel names in config order."""
        return tuple(name for name, channel in self.channels.items() if channel.enabled)

    def channel(self, name: str) -> Optional[HumanChannel]:
        """Return an enabled channel by name."""
        channel = self.channels.get(name)
        if not channel or not channel.enabled:
            return None
        return channel


class HumanRecipientRegistry:
    """Resolve configured human/operator aliases."""

    def __init__(self, recipients: dict[str, HumanRecipient]):
        self._recipients = recipients
        alias_map: dict[str, list[str]] = {}
        for recipient in recipients.values():
            for alias in recipient.aliases:
                alias_map.setdefault(alias, []).append(recipient.name)
        self._alias_map = alias_map

    @classmethod
    def from_config(cls, config: Any) -> "HumanRecipientRegistry":
        """Build a registry from a mapping containing a top-level humans key."""
        if not isinstance(config, dict):
            return cls({})
        raw_humans = config.get("humans") or {}
        if not isinstance(raw_humans, dict):
            return cls({})

        recipients: dict[str, HumanRecipient] = {}
        for raw_name, raw_spec in raw_humans.items():
            recipient = cls._normalize_human(raw_name, raw_spec)
            if recipient is not None:
                recipients[recipient.name] = recipient
        return cls(recipients)

    @staticmethod
    def _normalize_human(raw_name: Any, raw_spec: Any) -> Optional[HumanRecipient]:
        canonical = str(raw_name or "").strip().lower()
        if not canonical or not isinstance(raw_spec, dict):
            return None

        display_name = str(
            raw_spec.get("display_name") or raw_spec.get("name") or canonical
        ).strip() or canonical
        default_channel = str(raw_spec.get("default_channel") or "telegram").strip().lower()

        aliases: list[str] = [canonical]
        raw_aliases = raw_spec.get("aliases") or []
        if isinstance(raw_aliases, str):
            raw_aliases = [raw_aliases]
        aliases.extend(str(alias).strip().lower() for alias in raw_aliases if str(alias).strip())
        normalized_aliases = tuple(dict.fromkeys(alias for alias in aliases if alias))

        channels = HumanRecipientRegistry._normalize_channels(raw_spec.get("channels") or {})
        return HumanRecipient(
            name=canonical,
            display_name=display_name,
            aliases=normalized_aliases,
            default_channel=default_channel,
            channels=channels,
        )

    @staticmethod
    def _normalize_channels(raw_channels: Any) -> dict[str, HumanChannel]:
        if not isinstance(raw_channels, dict):
            return {}

        channels: dict[str, HumanChannel] = {}
        for raw_name, raw_spec in raw_channels.items():
            name = str(raw_name or "").strip().lower()
            if not name:
                continue
            if isinstance(raw_spec, bool):
                channels[name] = HumanChannel(name=name, enabled=raw_spec)
                continue
            if not isinstance(raw_spec, dict):
                continue
            channels[name] = HumanChannel(
                name=name,
                enabled=bool(raw_spec.get("enabled", False)),
                delivery=str(raw_spec.get("delivery") or "").strip().lower() or None,
                address=str(raw_spec.get("address") or "").strip() or None,
                address_env=str(raw_spec.get("address_env") or "").strip() or None,
                use=str(raw_spec.get("use") or "").strip().lower() or None,
            )
        return channels

    def lookup(self, identifier: str) -> Optional[HumanRecipient]:
        """Resolve one alias to a human recipient."""
        needle = str(identifier or "").strip().lower()
        if not needle:
            return None
        matches = self._alias_map.get(needle, [])
        if len(matches) > 1:
            raise HumanRecipientConfigError(
                f'Human recipient alias "{identifier}" is configured for multiple humans: '
                + ", ".join(sorted(matches))
            )
        if not matches:
            return None
        return self._recipients.get(matches[0])

    def list_recipients(self) -> tuple[HumanRecipient, ...]:
        """Return all configured human recipients in stable display order."""
        return tuple(self._recipients[name] for name in sorted(self._recipients))

    def reserved_names(self) -> set[str]:
        """Return every canonical name and alias reserved by humans."""
        return set(self._alias_map)
