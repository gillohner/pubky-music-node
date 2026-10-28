"""Zeroconf may not move a configured node to another address without the person (security review
2026-10-27, #3): the node's id is announced in the clear, so anyone on the network can claim it, and
moving the entry sends its saved token to the new address.

Runs without Home Assistant: the few Home Assistant classes the flow uses are stood in for below,
with the behaviour the flow relies on (`_abort_if_unique_id_configured(updates=...)` changes the
entry and reloads it, as Home Assistant does). Needs aiohttp and voluptuous.
"""

from __future__ import annotations

import asyncio
import importlib
import sys
import types
import unittest
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

PKG_DIR = Path(__file__).resolve().parents[1] / "custom_components" / "pubky_music"
TOKEN = "saved" + "-token"  # built at run time, so it never reads as a secret


# ---- stand-ins for the Home Assistant API the flow uses ----
class AbortFlow(Exception):
    def __init__(self, reason: str) -> None:
        super().__init__(reason)
        self.reason = reason


@dataclass
class ConfigEntry:
    unique_id: str
    title: str
    data: dict[str, Any]
    entry_id: str = "e1"
    reloads: int = 0
    runtime_data: Any = None


@dataclass
class Entries:
    entries: list[ConfigEntry] = field(default_factory=list)

    def async_entry_for_domain_unique_id(self, domain: str, unique_id: str) -> ConfigEntry | None:
        return next((e for e in self.entries if e.unique_id == unique_id), None)

    def update(self, entry: ConfigEntry, data: dict[str, Any]) -> None:
        entry.data = {**entry.data, **data}
        entry.reloads += 1


class ConfigFlow:
    def __init_subclass__(cls, domain: str | None = None, **kw: Any) -> None:
        super().__init_subclass__(**kw)

    @property
    def unique_id(self) -> str | None:
        return self.context.get("unique_id")

    async def async_set_unique_id(self, unique_id: str) -> None:
        self.context["unique_id"] = unique_id

    def _abort_if_unique_id_configured(self, updates: dict[str, Any] | None = None, **kw: Any) -> None:
        entry = self.hass.config_entries.async_entry_for_domain_unique_id("pubky_music", self.unique_id)
        if entry is None:
            return
        if updates and any(entry.data.get(k) != v for k, v in updates.items()):
            self.hass.config_entries.update(entry, updates)
        raise AbortFlow("already_configured")

    def async_abort(self, reason: str) -> dict:
        return {"type": "abort", "reason": reason}

    def async_show_form(self, step_id: str, data_schema: Any = None, errors: dict | None = None, **kw: Any) -> dict:
        return {"type": "form", "step_id": step_id, "errors": errors or {}, **kw}

    def async_create_entry(self, title: str, data: dict[str, Any]) -> dict:
        return {"type": "create_entry", "title": title, "data": data}

    def async_update_reload_and_abort(self, entry: ConfigEntry, data_updates: dict[str, Any], reason: str) -> dict:
        self.hass.config_entries.update(entry, data_updates)
        return {"type": "abort", "reason": reason}


@dataclass
class ZeroconfServiceInfo:
    ip_address: str
    port: int
    properties: dict[str, str]


def _install_stubs() -> None:
    def mod(name: str, **attrs: Any) -> None:
        m = types.ModuleType(name)
        m.__dict__.update(attrs)
        sys.modules[name] = m

    mod("homeassistant")
    mod("homeassistant.config_entries", ConfigEntry=ConfigEntry, ConfigFlow=ConfigFlow, ConfigFlowResult=dict)
    mod("homeassistant.const", CONF_HOST="host", CONF_PORT="port", CONF_TOKEN="token")
    mod("homeassistant.helpers")
    mod("homeassistant.helpers.aiohttp_client", async_get_clientsession=lambda hass: None)
    mod("homeassistant.helpers.service_info")
    mod("homeassistant.helpers.service_info.zeroconf", ZeroconfServiceInfo=ZeroconfServiceInfo)
    # The package without its __init__ (which sets up the whole integration)
    pkg = types.ModuleType("pubky_music")
    pkg.__path__ = [str(PKG_DIR)]
    sys.modules["pubky_music"] = pkg


_install_stubs()
config_flow = importlib.import_module("pubky_music.config_flow")


class FakeApi:
    """Stands in for the node client: records where the token went, answers as `nodes` says."""

    calls: list[tuple[str, str]] = []
    nodes: dict[str, dict] = {}

    def __init__(self, session: Any, base: str, token: str) -> None:
        self.base, self.token = base, token

    async def get(self, path: str, **kw: Any) -> dict:
        FakeApi.calls.append((self.base, self.token))
        if self.base not in FakeApi.nodes:
            raise config_flow.NodeError("nothing there")
        return FakeApi.nodes[self.base]


def step(coro) -> dict:
    try:
        return asyncio.run(coro)
    except AbortFlow as e:
        return {"type": "abort", "reason": e.reason}


class MovedNode(unittest.TestCase):
    def setUp(self) -> None:
        FakeApi.calls, FakeApi.nodes = [], {}
        config_flow.PubkyMusicApi = FakeApi
        self.entry = ConfigEntry("node-1", "Living room", {"host": "10.0.0.2", "port": 7788, "token": TOKEN})
        self.hass = types.SimpleNamespace(config_entries=Entries([self.entry]))

    def flow(self):
        f = config_flow.PubkyMusicConfigFlow()
        f.hass, f.context = self.hass, {"source": "zeroconf"}  # set by Home Assistant's flow manager
        return f

    def announce(self, f, ip: str, port: int = 7788, node_id: str = "node-1") -> dict:
        return step(f.async_step_zeroconf(ZeroconfServiceInfo(ip, port, {"role": "node", "id": node_id})))

    def test_a_claim_from_another_address_asks_first(self):
        r = self.announce(self.flow(), "10.0.0.66")
        self.assertEqual(r["type"], "form", r)
        self.assertEqual(r["step_id"], "zeroconf_moved")
        self.assertEqual(r["description_placeholders"]["new"], "10.0.0.66:7788")
        # Nothing moved, nothing reloaded, the token went nowhere
        self.assertEqual(self.entry.data["host"], "10.0.0.2")
        self.assertEqual(self.entry.reloads, 0)
        self.assertEqual(FakeApi.calls, [])

    def test_yes_moves_the_entry_once_the_node_there_is_this_one(self):
        f = self.flow()
        self.announce(f, "10.0.0.66", 7790)
        FakeApi.nodes["http://10.0.0.66:7790"] = {"id": "node-1", "name": "Living room"}
        r = step(f.async_step_zeroconf_moved({}))
        self.assertEqual(r, {"type": "abort", "reason": "moved"})
        self.assertEqual((self.entry.data["host"], self.entry.data["port"]), ("10.0.0.66", 7790))
        self.assertEqual(self.entry.reloads, 1)

    def test_yes_to_a_different_node_keeps_the_entry(self):
        f = self.flow()
        self.announce(f, "10.0.0.66")
        FakeApi.nodes["http://10.0.0.66:7788"] = {"id": "someone-else"}
        r = step(f.async_step_zeroconf_moved({}))
        self.assertEqual(r["errors"], {"base": "not_this_node"})
        self.assertEqual(self.entry.data["host"], "10.0.0.2")

    def test_the_same_address_or_a_connected_node_is_left_alone(self):
        self.assertEqual(self.announce(self.flow(), "10.0.0.2"), {"type": "abort", "reason": "already_configured"})
        self.entry.runtime_data = types.SimpleNamespace(connected=True)
        self.assertEqual(self.announce(self.flow(), "10.0.0.66"), {"type": "abort", "reason": "already_configured"})
        self.assertEqual(self.entry.data["host"], "10.0.0.2")
        self.assertEqual(FakeApi.calls, [])

    def test_a_new_node_is_still_offered(self):
        r = self.announce(self.flow(), "10.0.0.9", node_id="node-2")
        self.assertEqual((r["type"], r["step_id"]), ("form", "zeroconf_confirm"))


if __name__ == "__main__":
    unittest.main()
