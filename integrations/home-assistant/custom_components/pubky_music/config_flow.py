"""Config flow: the node's address and API token, typed in or found by zeroconf."""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

import voluptuous as vol

from homeassistant.config_entries import ConfigFlow, ConfigFlowResult
from homeassistant.const import CONF_HOST, CONF_PORT, CONF_TOKEN
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.service_info.zeroconf import ZeroconfServiceInfo

from .api import AuthError, NodeError, PubkyMusicApi, base_url
from .const import DEFAULT_PORT, DOMAIN


class PubkyMusicConfigFlow(ConfigFlow, domain=DOMAIN):
    """Add a Pubky Music node."""

    VERSION = 1

    def __init__(self) -> None:
        self._host: str | None = None
        self._port: int = DEFAULT_PORT
        self._name: str | None = None
        self._token_required = True

    async def _check(self, host: str, port: int, token: str) -> dict[str, Any]:
        api = PubkyMusicApi(async_get_clientsession(self.hass), base_url(host, port), token)
        info = await api.get("/info")
        # /info answers without a token on some nodes; zones never do
        await api.get("/zones")
        return info

    async def async_step_user(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            host = user_input[CONF_HOST].strip()
            port = user_input[CONF_PORT]
            token = user_input.get(CONF_TOKEN, "").strip()
            try:
                info = await self._check(host, port, token)
            except AuthError:
                errors["base"] = "invalid_auth"
            except NodeError:
                errors["base"] = "cannot_connect"
            else:
                await self.async_set_unique_id(info["id"])
                self._abort_if_unique_id_configured(updates={CONF_HOST: host, CONF_PORT: port, CONF_TOKEN: token})
                return self.async_create_entry(
                    title=info.get("name") or "Pubky Music",
                    data={CONF_HOST: host, CONF_PORT: port, CONF_TOKEN: token},
                )
        return self.async_show_form(
            step_id="user",
            data_schema=vol.Schema(
                {
                    vol.Required(CONF_HOST, default=(user_input or {}).get(CONF_HOST, self._host or "")): str,
                    vol.Required(CONF_PORT, default=(user_input or {}).get(CONF_PORT, self._port)): int,
                    vol.Optional(CONF_TOKEN, default=""): str,
                }
            ),
            errors=errors,
        )

    async def async_step_zeroconf(self, discovery_info: ZeroconfServiceInfo) -> ConfigFlowResult:
        props = discovery_info.properties
        if props.get("role") != "node" or not props.get("id"):
            return self.async_abort(reason="not_a_node")
        await self.async_set_unique_id(props["id"])
        host = str(discovery_info.ip_address)
        port = discovery_info.port or DEFAULT_PORT
        self._abort_if_unique_id_configured(updates={CONF_HOST: host, CONF_PORT: port})
        self._host, self._port = host, port
        self._name = props.get("name") or "Pubky Music"
        self._token_required = props.get("auth", "token") != "none"
        self.context["title_placeholders"] = {"name": self._name}
        return await self.async_step_zeroconf_confirm()

    async def async_step_zeroconf_confirm(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            token = user_input.get(CONF_TOKEN, "").strip()
            try:
                info = await self._check(self._host, self._port, token)
            except AuthError:
                errors["base"] = "invalid_auth"
            except NodeError:
                errors["base"] = "cannot_connect"
            else:
                return self.async_create_entry(
                    title=info.get("name") or self._name,
                    data={CONF_HOST: self._host, CONF_PORT: self._port, CONF_TOKEN: token},
                )
        schema = vol.Schema(
            {vol.Required(CONF_TOKEN): str} if self._token_required else {vol.Optional(CONF_TOKEN, default=""): str}
        )
        return self.async_show_form(
            step_id="zeroconf_confirm",
            data_schema=schema,
            description_placeholders={"name": self._name, "host": f"{self._host}:{self._port}"},
            errors=errors,
        )

    async def async_step_reauth(self, entry_data: Mapping[str, Any]) -> ConfigFlowResult:
        self._host, self._port = entry_data[CONF_HOST], entry_data[CONF_PORT]
        return await self.async_step_reauth_confirm()

    async def async_step_reauth_confirm(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            token = user_input[CONF_TOKEN].strip()
            try:
                await self._check(self._host, self._port, token)
            except AuthError:
                errors["base"] = "invalid_auth"
            except NodeError:
                errors["base"] = "cannot_connect"
            else:
                return self.async_update_reload_and_abort(self._get_reauth_entry(), data_updates={CONF_TOKEN: token})
        return self.async_show_form(
            step_id="reauth_confirm", data_schema=vol.Schema({vol.Required(CONF_TOKEN): str}), errors=errors
        )
