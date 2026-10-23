"""Pubky Music: the node's zones as media players, its library in the media browser."""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from typing import Any

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import CONF_HOST, CONF_PORT, CONF_TOKEN, Platform
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import ConfigEntryAuthFailed, ConfigEntryNotReady
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.dispatcher import async_dispatcher_send

from .api import AuthError, NodeError, NodeEvents, PubkyMusicApi, base_url
from .const import DOMAIN, SIGNAL_CONNECTION, SIGNAL_NEW_ZONE, SIGNAL_ZONE

_LOGGER = logging.getLogger(__name__)
PLATFORMS = [Platform.MEDIA_PLAYER]


@dataclass
class PubkyMusicNode:
    """One node: its API, its zones as the events socket last described them, its outputs."""

    hass: HomeAssistant
    entry: ConfigEntry
    api: PubkyMusicApi
    info: dict[str, Any]
    zones: dict[str, dict] = field(default_factory=dict)
    outputs: dict[str, dict] = field(default_factory=dict)
    connected: bool = False
    events: NodeEvents | None = None

    @property
    def node_id(self) -> str:
        return self.info["id"]

    async def refresh_outputs(self) -> None:
        self.outputs = {o["id"]: o for o in await self.api.get("/outputs")}

    @callback
    def handle(self, msg: dict) -> None:
        kind = msg.get("type")
        if kind == "snapshot":
            fresh = {z["id"]: z for z in msg.get("zones", [])}
            for zid, z in fresh.items():
                self._zone(zid, z)
            for zid in set(self.zones) - set(fresh):
                self.zones.pop(zid, None)
                self._signal(zid)
        elif kind == "zone":
            z = msg["state"]
            self._zone(z["id"], z)
        elif kind == "zone_removed":
            zid = msg["zone"]
            self.zones.pop(zid, None)
            self._signal(zid)

    def _zone(self, zid: str, state: dict) -> None:
        new = zid not in self.zones
        self.zones[zid] = state
        if new:
            if state.get("output") not in self.outputs:
                self.hass.async_create_task(self._refresh_then_add(zid))
            else:
                async_dispatcher_send(self.hass, SIGNAL_NEW_ZONE.format(entry=self.entry.entry_id), zid)
        else:
            self._signal(zid)

    async def _refresh_then_add(self, zid: str) -> None:
        try:
            await self.refresh_outputs()
        except NodeError as err:
            _LOGGER.debug("outputs: %s", err)
        async_dispatcher_send(self.hass, SIGNAL_NEW_ZONE.format(entry=self.entry.entry_id), zid)

    def _signal(self, zid: str) -> None:
        async_dispatcher_send(self.hass, SIGNAL_ZONE.format(entry=self.entry.entry_id, zone=zid))

    @callback
    def set_connected(self, up: bool) -> None:
        if up != self.connected:
            self.connected = up
            async_dispatcher_send(self.hass, SIGNAL_CONNECTION.format(entry=self.entry.entry_id))


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    session = async_get_clientsession(hass)
    api = PubkyMusicApi(session, base_url(entry.data[CONF_HOST], entry.data[CONF_PORT]), entry.data.get(CONF_TOKEN, ""))
    try:
        info = await api.get("/info")
        zones = await api.get("/zones")
        node = PubkyMusicNode(hass, entry, api, info)
        await node.refresh_outputs()
    except AuthError as err:
        raise ConfigEntryAuthFailed(str(err)) from err
    except NodeError as err:
        raise ConfigEntryNotReady(str(err)) from err
    node.zones = {z["id"]: z for z in zones}
    dr.async_get(hass).async_get_or_create(
        config_entry_id=entry.entry_id,
        identifiers={(DOMAIN, node.node_id)},
        manufacturer="Pubky Music",
        model="Node",
        name=info.get("name") or "Pubky Music",
        sw_version=info.get("version"),
        configuration_url=api.base,
    )
    entry.runtime_data = node
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    node.events = NodeEvents(api, session, node.handle, node.set_connected)
    node.events.start()
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    node: PubkyMusicNode = entry.runtime_data
    if node.events:
        await node.events.stop()
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
