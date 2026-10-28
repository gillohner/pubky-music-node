"""A small client for the Pubky Music node's HTTP API and its `/v1/events` websocket."""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Callable
from typing import Any
from urllib.parse import urlsplit

import aiohttp

_LOGGER = logging.getLogger(__name__)


class NodeError(Exception):
    """The node answered with an error, or could not be reached."""


class AuthError(NodeError):
    """The node refused the API token."""


def base_url(host: str, port: int) -> str:
    """`http://host:port`, with brackets for an IPv6 address."""
    if ":" in host and not host.startswith("["):
        host = f"[{host}]"
    return f"http://{host}:{port}"


def _origin(url: str) -> tuple[str, str, int] | None:
    """`(scheme, host, port)` of an http(s) URL without user info; `None` for anything else."""
    try:
        u = urlsplit(url)
        port = u.port
    except ValueError:
        return None
    scheme = u.scheme.lower()
    if scheme not in ("http", "https") or not u.hostname or "@" in u.netloc:
        return None
    return scheme, u.hostname.lower(), port or (443 if scheme == "https" else 80)


def same_origin(url: str, base: str) -> bool:
    """Whether `url` is on the node at `base`: scheme, host and port compared exactly, after
    parsing. A prefix check let a URL through whose user info was the node's address, so the
    host was another one (security review 2026-10-27, #4); URLs with user info never count as
    the node's."""
    a = _origin(url)
    return a is not None and a == _origin(base)


class PubkyMusicApi:
    """The node's REST API (`/v1/...`), authenticated with the API token."""

    def __init__(self, session: aiohttp.ClientSession, base: str, token: str) -> None:
        self._session = session
        self.base = base.rstrip("/")
        self._token = token

    @property
    def headers(self) -> dict[str, str]:
        return {"Authorization": f"Bearer {self._token}"} if self._token else {}

    async def request(self, method: str, path: str, *, json: Any = None, params: dict | None = None) -> Any:
        url = path if path.startswith("http") else f"{self.base}/v1{path}"
        try:
            async with self._session.request(
                method,
                url,
                json=json,
                params=params,
                headers=self.headers if same_origin(url, self.base) else {},
                timeout=aiohttp.ClientTimeout(total=30),
            ) as resp:
                if resp.status in (401, 403):
                    raise AuthError("the node refused the API token")
                if resp.status >= 400:
                    try:
                        detail = (await resp.json()).get("error")
                    except (aiohttp.ContentTypeError, ValueError):
                        detail = await resp.text()
                    raise NodeError(f"{method} {path}: {resp.status} {detail}")
                if resp.status == 204 or resp.content_length == 0:
                    return None
                if resp.content_type == "application/json":
                    return await resp.json()
                return await resp.read()
        except (aiohttp.ClientError, TimeoutError) as err:
            raise NodeError(f"can't reach the node: {err}") from err

    async def get(self, path: str, **params: Any) -> Any:
        return await self.request("GET", path, params={k: v for k, v in params.items() if v is not None} or None)

    async def post(self, path: str, body: Any = None) -> Any:
        return await self.request("POST", path, json=body if body is not None else {})

    async def put(self, path: str, body: Any) -> Any:
        return await self.request("PUT", path, json=body)

    async def delete(self, path: str) -> Any:
        return await self.request("DELETE", path)

    async def image(self, path: str) -> tuple[bytes | None, str | None]:
        """An image from the node (covers need the token) or from anywhere else."""
        url = path if path.startswith("http") else f"{self.base}{path}"
        headers = self.headers if same_origin(url, self.base) else {}
        try:
            async with self._session.get(url, headers=headers, timeout=aiohttp.ClientTimeout(total=20)) as resp:
                if resp.status != 200:
                    return None, None
                return await resp.read(), resp.content_type
        except (aiohttp.ClientError, TimeoutError):
            return None, None


class NodeEvents:
    """Follows `/v1/events`: a snapshot of every zone, then one message per change. Reconnects with
    a back-off and asks for a fresh snapshot (`resync`) when it missed a message."""

    def __init__(
        self,
        api: PubkyMusicApi,
        session: aiohttp.ClientSession,
        on_message: Callable[[dict], None],
        on_connected: Callable[[bool], None],
    ) -> None:
        self._api = api
        self._session = session
        self._on_message = on_message
        self._on_connected = on_connected
        self._task: asyncio.Task | None = None
        self._stop = False

    def start(self) -> None:
        self._task = asyncio.get_running_loop().create_task(self._run())

    async def stop(self) -> None:
        self._stop = True
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    async def _run(self) -> None:
        delay = 1
        ws_url = self._api.base.replace("http", "ws", 1) + "/v1/events"
        while not self._stop:
            try:
                async with self._session.ws_connect(ws_url, headers=self._api.headers, heartbeat=30) as ws:
                    delay = 1
                    self._on_connected(True)
                    seq = None
                    async for msg in ws:
                        if msg.type != aiohttp.WSMsgType.TEXT:
                            if msg.type in (aiohttp.WSMsgType.CLOSED, aiohttp.WSMsgType.ERROR):
                                break
                            continue
                        data = msg.json()
                        s = data.get("seq")
                        if s is not None and seq is not None and data.get("type") != "snapshot" and s != seq + 1:
                            await ws.send_json({"type": "resync"})
                        if s is not None:
                            seq = s
                        self._on_message(data)
            except asyncio.CancelledError:
                raise
            except (aiohttp.ClientError, TimeoutError, ValueError) as err:
                _LOGGER.debug("events socket: %s", err)
            self._on_connected(False)
            if self._stop:
                return
            await asyncio.sleep(delay)
            delay = min(delay * 2, 60)
