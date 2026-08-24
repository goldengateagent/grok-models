#!/usr/bin/env python3
"""Sandbox tests: --add-provider model ids must match GET {base_url}/models."""

from __future__ import annotations

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("grok_models", ROOT / "grok-models.py")
gm = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(gm)


def _sandbox_home() -> Path:
    home = Path(tempfile.mkdtemp(prefix="gm-test-home-"))
    os.environ["GROK_HOME"] = str(home)
    gm.GROK_HOME = home
    gm.PROVIDERS_PATH = home / "providers.json"
    gm.CONFIG_TOML_PATH = home / "config.toml"
    return home


def _assert_live_ids_match_stored(provider_id: str) -> None:
    _sandbox_home()
    gm._models_dev_api = None
    gm.USE_PROVIDER_MODELS_ENDPOINT = True
    rc = gm.cmd_add_provider(provider_id)
    assert rc == 0, f"add-provider {provider_id!r} exited {rc}"
    doc = gm.load_providers()
    provider = next(
        (p for p in doc["providers"] if isinstance(p, dict) and p.get("id") == provider_id),
        None,
    )
    assert provider is not None, f"provider {provider_id!r} missing from providers.json"
    base_url = provider.get("base_url")
    assert isinstance(base_url, str) and base_url, f"no base_url for {provider_id!r}"
    live = gm.try_fetch_provider_models(base_url)
    assert live is not None, f"GET {gm.provider_models_url(base_url)} failed"
    live_ids = [mid for mid, _ in live]
    stored = provider.get("models") if isinstance(provider.get("models"), dict) else {}
    stored_ids = list(stored.keys())
    extra = set(stored_ids) - set(live_ids)
    missing = set(live_ids) - set(stored_ids)
    assert len(stored_ids) == len(live_ids) and not extra and not missing, (
        f"{provider_id}: stored ids != live /models ids\n"
        f"  extra in stored: {extra}\n"
        f"  missing from stored: {missing}\n"
        f"  counts stored={len(stored_ids)} live={len(live_ids)}"
    )


class ProviderModelsEndpointTests(unittest.TestCase):
    def test_opencode_add_provider_matches_live_models(self) -> None:
        _assert_live_ids_match_stored("opencode")

    def test_openrouter_add_provider_matches_live_models(self) -> None:
        _assert_live_ids_match_stored("openrouter")


if __name__ == "__main__":
    unittest.main()
