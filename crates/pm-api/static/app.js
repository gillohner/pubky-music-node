/* Pubky Music web UI. Talks to /v1 (bearer token), listens on /v1/events,
   and plays either through a node zone or through this browser's <audio>. */
(() => {
  'use strict';
  const $ = (s, root = document) => root.querySelector(s);
  const $$ = (s, root = document) => Array.from(root.querySelectorAll(s));
  const BROWSER = 'browser';

  const state = {
    token: localStorage.getItem('pm.token') || '',
    zone: localStorage.getItem('pm.zone') || '',
    zones: {},
    lyrics: new Map(),
    formats: null,
    focus: '',
    sleepAt: {},
    browserSleepTimer: null,
    pubkyUser: undefined,
    view: 'home',
    viewArg: null,
    albums: null,
    artists: null,
    playlists: [],
    queue: [],
    info: null,
    ws: null,
    favUris: new Set(),
    trackCache: new Map(),
    browserQueue: (() => { try { const q = JSON.parse(localStorage.getItem('pm.browserQueue') || '[]'); return Array.isArray(q) ? q.filter((it) => it && it.uri && !it.stream) : []; } catch { return []; } })(),
    browserIndex: -1,
    history: [],
  };

  // ---- helpers -------------------------------------------------------------
  const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const icon = (name, cls = 'ic') => `<svg class="${cls}" aria-hidden="true"><use href="#i-${name}"/></svg>`;
  const fmt = (ms) => {
    const s = Math.floor((ms || 0) / 1000);
    const m = Math.floor(s / 60);
    const h = Math.floor(m / 60);
    const pad = (n) => String(n).padStart(2, '0');
    return h ? `${h}:${pad(m % 60)}:${pad(s % 60)}` : `${m}:${pad(s % 60)}`;
  };
  const fmtLong = (ms) => {
    const m = Math.round((ms || 0) / 60000);
    if (m < 1) return `${Math.round((ms || 0) / 1000)} s`;
    if (m < 60) return `${m} min`;
    const h = Math.floor(m / 60);
    return `${h} h ${m % 60} min`;
  };
  const ago = (unix) => {
    const s = Math.max(0, Math.floor(Date.now() / 1000) - unix);
    if (s < 60) return 'just now';
    if (s < 3600) return `${Math.floor(s / 60)} min ago`;
    if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
    return `${Math.floor(s / 86400)} d ago`;
  };
  const fmtFormat = (f) => !f ? '' : f.kind === 'dsd' ? `DSD${Math.round(f.rate_hz / 44100)}` : `${f.bits}/${(f.rate_hz / 1000).toFixed(1).replace('.0', '')}k`;
  const artists = (t) => (t.artists || []).map((a) => a.name).join(', ');
  const hue = (s) => { let h = 0; for (const c of String(s || '')) h = (h * 31 + c.charCodeAt(0)) >>> 0; return h % 360; };
  const initials = (s) => (s || '?').trim().split(/\s+/).slice(0, 2).map((w) => w[0]).join('').toUpperCase();
  const tokenQ = () => (state.token ? '?token=' + encodeURIComponent(state.token) : '');
  const art = (albumId, title, cls = '') => {
    const h = hue(title);
    const bg = `background:linear-gradient(135deg, hsl(${h} 38% 30%), hsl(${(h + 40) % 360} 42% 18%))`;
    const img = albumId ? `<img loading="lazy" alt="" src="/v1/library/albums/${albumId}/cover${tokenQ()}" onerror="this.remove()">` : '';
    return `<div class="art ${cls}" style="${bg}"><span>${esc(initials(title))}</span>${img}</div>`;
  };
  // Art for a queue item: a streaming source's image, else the library album cover.
  const itemArt = (it, cls = '') => {
    if (it && it.image) { const h = hue(it.title || ''); return `<div class="art ${cls}" style="background:hsl(${h} 30% 22%)"><img loading="lazy" alt="" src="${esc(it.image)}" onerror="this.remove()"></div>`; }
    return art(albumIdOf(it), (it && (it.title || it.uri)) || '', cls);
  };
  const toast = (msg, kind = '') => {
    const t = $('#toast');
    t.textContent = msg;
    t.className = `toast show ${kind}`;
    clearTimeout(toast.h);
    toast.h = setTimeout(() => t.classList.remove('show'), 3800);
  };
  const setFill = (el) => { const min = +el.min || 0; const max = +el.max || 1; el.style.setProperty('--fill', `${((el.value - min) / (max - min)) * 100}%`); };

  const api = async (method, path, body) => {
    // A change made while the connection is down waits a few seconds for it, then is refused (nothing is sent).
    if (method !== 'GET') await whenConnected();
    let r;
    try {
      r = await fetch('/v1' + path, {
        method,
        headers: { 'content-type': 'application/json', ...(state.token ? { authorization: 'Bearer ' + state.token } : {}) },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
    } catch { linkHint(); throw new Error("Can't reach the node."); }
    if (r.status === 401) { login(); throw new Error('The node wants an API token.'); }
    const text = await r.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { json = { error: text }; }
    if (!r.ok) throw new Error(json && json.error ? json.error : `HTTP ${r.status}`);
    return json;
  };

  // A small dialog with a few choices; resolves to the picked value, or null (Esc, Cancel).
  const choiceDialog = ({ title, text, choices }) => new Promise((resolve) => {
    const d = document.createElement('dialog'); d.className = 'dialog';
    d.innerHTML = `<h2>${esc(title)}</h2>${text ? `<p class="muted">${esc(text)}</p>` : ''}<div class="choices">${choices.map((c, i) => `<button class="btn choice ${c.primary ? 'primary' : ''}" data-ci="${i}"><span class="choice-label">${esc(c.label)}</span>${c.note ? `<span class="choice-note">${esc(c.note)}</span>` : ''}</button>`).join('')}</div><div class="dialog-actions"><button class="btn ghost" data-ci="-1">Cancel</button></div>`;
    document.body.appendChild(d);
    const done = (v) => { d.close(); d.remove(); resolve(v); };
    $$('[data-ci]', d).forEach((b) => b.onclick = () => done(Number(b.dataset.ci) < 0 ? null : choices[Number(b.dataset.ci)].value));
    d.addEventListener('cancel', (e) => { e.preventDefault(); done(null); });
    d.showModal(); const first = $('.choice', d); if (first) first.focus();
  });
  // Importing a provider playlist asks once: keep that provider as each song's source, or Auto.
  const importPlaylist = async (b, service, id) => {
    const name = SERVICE_NAMES[service] || service;
    let order = 'your play order'; try { const o = await api('GET', '/play-order'); order = o.order.map(srcName).join(' › '); } catch { /* keep the words */ }
    const how = await choiceDialog({ title: 'Import this playlist', text: 'Where should its songs play from?', choices: [
      { label: `Keep ${name}`, note: `Each song is pinned to its ${name} track.`, value: 'keep', primary: true },
      { label: 'Auto', note: `Each song plays from the first source that has it (${order}).`, value: 'auto' },
    ] });
    if (!how) return;
    b.disabled = true;
    try { const r = await api('POST', `/sources/${service}/playlists/${encodeURIComponent(id)}/import${how === 'keep' ? '?keep_source=true' : ''}`); state.playlists = await api('GET', '/playlists'); toast(`Imported “${r.playlist.summary.name}”: ${r.imported} songs${r.skipped ? `, ${r.skipped} skipped` : ''}${how === 'keep' ? `, pinned to ${name}` : ''}`); }
    catch (e) { toast(e.message, 'error'); }
    b.disabled = false;
  };
  const login = () => { const d = $('#login'); if (!d.open) { $('#tok').value = state.token; d.showModal(); } };
  // Pubky Ring sign-in: the node starts a flow, we show the link (and a QR when the device has a camera elsewhere), poll until the owner approves, and keep the token it hands out.
  let ringPoll = null;
  $('#ringstart').onclick = async () => {
    const out = $('#ringout'); $('#ringstart').disabled = true; out.innerHTML = 'Starting…';
    try {
      const r = await fetch('/v1/auth/pubky/start', { method: 'POST' }); const j = await r.json(); if (!r.ok) throw new Error(j.error || `HTTP ${r.status}`);
      out.innerHTML = `<div class="qrbox">${j.qr_svg ? `<div class="qr">${j.qr_svg}</div>` : ''}<div><p>Scan with Pubky Ring, or <a href="${esc(j.url)}">open it in Ring on this device</a>.</p><p>Waiting for approval from <code>${esc(j.owner.slice(0, 12))}…</code></p></div></div>`;
      clearInterval(ringPoll);
      ringPoll = setInterval(async () => {
        try {
          const p = await (await fetch(`/v1/auth/pubky/poll?id=${encodeURIComponent(j.id)}`)).json();
          if (p.status === 'pending') return;
          clearInterval(ringPoll); $('#ringstart').disabled = false;
          if (p.status === 'ok') { state.token = p.token || ''; localStorage.setItem('pm.token', state.token); $('#login').close(); toast('Connected with Pubky Ring'); connect(); }
          else if (p.status === 'denied') out.innerHTML = `<p class="warn">That was a different Pubky identity (${esc((p.user || '').slice(0, 12))}…). Only the node's owner can connect.</p>`;
          else out.innerHTML = '<p class="warn">The sign-in expired. Try again.</p>';
        } catch (e) { clearInterval(ringPoll); $('#ringstart').disabled = false; out.innerHTML = `<p class="warn">${esc(e.message)}</p>`; }
      }, 2000);
    } catch (e) { $('#ringstart').disabled = false; out.innerHTML = `<p class="warn">${esc(e.message)}</p>`; }
  };
  $('#login form').addEventListener('submit', () => {
    state.token = $('#tok').value.trim();
    localStorage.setItem('pm.token', state.token);
    connect();
  });

  // ---- zones & playback ----------------------------------------------------
  const audio = $('#browser');
  const isBrowser = () => state.zone === BROWSER;
  const browserState = () => {
    const it = state.browserQueue[state.browserIndex];
    return {
      id: BROWSER, name: 'This browser', output: 'browser',
      transport: audio.paused ? (audio.currentTime > 0 && !audio.ended ? 'paused' : 'stopped') : 'playing',
      queue_index: state.browserIndex,
      now_playing: it ? { item: it, position_ms: audio.currentTime * 1000, duration_ms: (it.duration_ms && (!Number.isFinite(audio.duration) || it.stream)) ? it.duration_ms : (audio.duration || 0) * 1000, signal_path: null } : null,
    };
  };
  const currentZone = () => (isBrowser() ? browserState() : state.zones[state.zone]);
  const zoneList = () => [browserState(), ...Object.values(state.zones)];
  // The browser's queue survives a reload (library tracks only; radio streams are not kept).
  let lastSavedQueue = '';
  setInterval(() => { try { const j = JSON.stringify(state.browserQueue.filter((it) => !it.stream)); if (j !== lastSavedQueue) { lastSavedQueue = j; localStorage.setItem('pm.browserQueue', j); } } catch { /* storage unavailable */ } }, 2000);
  const ensureZone = () => { if (!isBrowser() && !state.zones[state.zone]) state.zone = BROWSER; };
  const renderZoneName = () => { const z = currentZone(); $('#zonename').textContent = z ? z.name : 'Zone'; };
  // A queue URI the browser can play directly: TIDAL tracks go through the node's proxy.
  const streamUrlFor = (uri) => { const m = /^(tidal|deezer):track:([A-Za-z0-9]+)$/.exec(uri || ''); if (m) return `/v1/sources/${m[1]}/stream/${m[2]}${tokenQ()}`; return /^https?:/.test(uri || '') ? uri : null; };
  // Source tracks (TIDAL, archive…) as queue items for the current zone.
  const playSourceItems = async (qitems, replace = true, at = 'end') => {
    if (!qitems.length) return;
    if (isBrowser()) {
      const bitems = qitems.map((q, k) => ({ id: Date.now() + k, uri: q.uri, stream: streamUrlFor(q.uri) || q.uri, title: q.title, artist: q.artist, album: q.album, duration_ms: q.duration_ms, image: q.image || null }));
      if (replace) { state.browserQueue = bitems; browserPlay(0); } else if (at === 'next') state.browserQueue.splice(state.browserIndex + 1, 0, ...bitems); else state.browserQueue.push(...bitems);
      if (!replace) toast(bitems.length === 1 ? 'Added to queue' : `${bitems.length} tracks added`);
      return;
    }
    try { await api('POST', `/zones/${encodeURIComponent(state.zone)}/queue`, { items: qitems, at, replace, play: replace }); if (!replace) toast(qitems.length === 1 ? (at === 'next' ? 'Playing next' : 'Added to queue') : `${qitems.length} tracks added`); refreshQueue(); } catch (e) { toast(e.message, 'error'); }
  };
  // Play on a Spotify Connect device: the active one, else a menu of the devices online. Spotify streams it; the node only drives it.
  const spotifyPlay = async (uris, anchor) => {
    try {
      const devs = await api('GET', '/sources/spotify/devices');
      const go = async (device_id) => { try { await api('POST', '/sources/spotify/play', { uris, device_id }); toast('Playing on Spotify'); } catch (e) { toast(e.message, 'error'); } };
      if (!devs.length) return toast('No Spotify device online: open Spotify on a device first', 'error');
      const active = devs.find((d) => d.is_active);
      if (devs.length === 1 || (active && !anchor)) return go((active || devs[0]).id);
      showMenu(anchor || document.body, [{ title: 'Play on Spotify device' }, ...devs.map((d) => ({ label: `${d.name} (${d.type}${d.is_active ? ', active' : ''})`, icon: 'zone', run: () => go(d.id) }))]);
    } catch (e) { toast(e.message, 'error'); }
  };
  // TIDAL's official embed player in a panel: full-length playback for subscribers, inside this UI, no tier needed.
  const tidalEmbed = (kind, id, title) => { const p = $('#embedpanel'); $('#embedtitle').textContent = title || 'TIDAL player'; $('#embedframe').src = `https://embed.tidal.com/${kind}/${encodeURIComponent(id)}?layout=gridify`; p.hidden = false; };
  $('#embedclose').onclick = () => { const p = $('#embedpanel'); p.hidden = true; $('#embedframe').src = 'about:blank'; };
  { const sp = new URLSearchParams(location.search); if (sp.get('spotify') === 'ok') { toast('Spotify signed in'); history.replaceState(null, '', location.pathname); } else if (sp.get('spotify') === 'error') { toast('Spotify: ' + (sp.get('message') || 'sign-in failed'), 'error'); history.replaceState(null, '', location.pathname); } }
  const hitItem = (h, album) => ({ uri: h.uri || `${h.source || 'tidal'}:track:${h.id.replace(/^track:/, '')}`, title: h.title, artist: h.creator || '', album: album || '', duration_ms: h.duration_ms || null, image: h.image || null });
  const browserPlay = (i) => {
    const it = state.browserQueue[i];
    if (!it) { audio.pause(); state.browserIndex = -1; renderBar(); return; }
    state.browserIndex = i;
    audio.src = it.stream ? it.stream : streamUrlFor(it.uri) || `/v1/library/tracks/${it.track_id}/file${tokenQ()}`;
    audio.play().catch((e) => toast('Browser playback: ' + e.message, 'error'));
    if (it.track_id != null) api('POST', '/library/history', { track_id: it.track_id, zone: 'browser' }).catch(() => {});
    mediaSessionUpdate(it);
    renderBar();
    if (state.view === 'queue') render();
  };
  // Hardware media keys and lock-screen controls for browser playback.
  const hasMediaSession = typeof navigator !== 'undefined' && 'mediaSession' in navigator && typeof MediaMetadata === 'function';
  const mediaSessionUpdate = (it) => {
    if (!hasMediaSession) return;
    try {
      const albumId = albumIdOf(it);
      navigator.mediaSession.metadata = new MediaMetadata({ title: it.title || it.uri, artist: it.artist || '', album: it.album || '', artwork: it.image ? [{ src: it.image, sizes: '640x640' }] : albumId ? [{ src: `${location.origin}/v1/library/albums/${albumId}/cover${tokenQ()}`, sizes: '512x512' }] : [] });
    } catch { /* metadata is a nicety */ }
  };
  if (hasMediaSession) {
    const handlers = { play: () => audio.play().catch(() => {}), pause: () => audio.pause(), nexttrack: () => browserPlay(state.browserIndex + 1), previoustrack: () => browserPlay(Math.max(0, state.browserIndex - 1)), seekto: (d) => { if (d && typeof d.seekTime === 'number') audio.currentTime = d.seekTime; } };
    for (const [k, f] of Object.entries(handlers)) { try { navigator.mediaSession.setActionHandler(k, f); } catch { /* unsupported action */ } }
    audio.addEventListener('play', () => { try { navigator.mediaSession.playbackState = 'playing'; } catch { /* ignore */ } });
    audio.addEventListener('pause', () => { try { navigator.mediaSession.playbackState = 'paused'; } catch { /* ignore */ } });
  }
  audio.addEventListener('ended', () => browserPlay(state.browserIndex + 1));
  audio.addEventListener('timeupdate', () => { if (isBrowser()) renderBar(); });
  audio.addEventListener('play', () => renderBar());
  audio.addEventListener('pause', () => renderBar());

  const rememberTracks = (tracks) => { for (const t of tracks || []) if (t && t.id != null) state.trackCache.set('track:' + t.id, t); };
  const playTracks = async (tracks, replace = true, at = 'end') => {
    tracks = (tracks || []).filter((t) => t && t.id != null);
    if (!tracks.length) return;
    rememberTracks(tracks);
    if (isBrowser()) {
      const items = tracks.map((t) => ({ id: t.id, uri: 'track:' + t.id, track_id: t.id, title: t.title, artist: artists(t), album: t.album ? t.album.title : '', duration_ms: t.duration_ms }));
      if (replace) { state.browserQueue = items; browserPlay(0); }
      else if (at === 'next') state.browserQueue.splice(state.browserIndex + 1, 0, ...items);
      else state.browserQueue.push(...items);
      if (!replace) toast(items.length === 1 ? (at === 'next' ? 'Playing next' : 'Added to queue') : `${items.length} tracks added`);
      if (state.view === 'queue') render();
      return;
    }
    try {
      await api('POST', `/zones/${state.zone}/queue`, { track_ids: tracks.map((t) => t.id), replace, play: replace, at });
      if (!replace) toast(tracks.length === 1 ? (at === 'next' ? 'Playing next' : 'Added to queue') : `${tracks.length} tracks added`);
      refreshQueue();
    } catch (e) { toast(e.message, 'error'); }
  };
  const transport = async (action) => {
    if (isBrowser()) {
      if (action.action === 'toggle') { if (audio.paused) audio.play().catch(() => {}); else audio.pause(); }
      else if (action.action === 'next') browserPlay(state.browserIndex + 1);
      else if (action.action === 'previous') { if (audio.currentTime > 3) audio.currentTime = 0; else browserPlay(Math.max(0, state.browserIndex - 1)); }
      else if (action.action === 'seek') audio.currentTime = action.position_ms / 1000;
      else if (action.action === 'play' && action.index != null) browserPlay(action.index);
      renderBar();
      return;
    }
    if (!state.zones[state.zone]) return;
    try {
      const st = await api('POST', `/zones/${state.zone}/transport`, action);
      state.zones[st.id] = st;
      renderBar();
    } catch (e) { toast(e.message, 'error'); }
  };
  const playStation = async (st) => {
    if (isBrowser()) {
      if (location.protocol === 'https:' && /^http:\/\//i.test(st.url)) { toast('This page is served over HTTPS, so the browser will not play an http:// stream. Pick a node zone, or a station with an https stream.', 'error'); return; }
      state.browserQueue = [{ id: Date.now(), uri: st.url, stream: st.url, title: st.name, artist: [st.country, (st.tags || []).slice(0, 3).join(', ')].filter(Boolean).join(' · '), album: 'Internet radio', duration_ms: 0 }];
      browserPlay(0); toast(`Tuning to ${st.name}`); return;
    }
    try { await api('POST', '/radio/play', { zone: state.zone, station: st }); toast(`Tuning to ${st.name}`); refreshQueue(); } catch (e) { toast(e.message, 'error'); }
  };
  const playPlaylist = async (id) => {
    try {
      if (isBrowser()) { const d = await api('GET', `/playlists/${id}`); playTracks(d.tracks.filter(Boolean), true); return; }
      const r = await api('POST', `/playlists/${id}/play`, { zone: state.zone });
      toast(`Queued ${r.queued} song${r.queued === 1 ? '' : 's'}${r.missing.length ? `; ${r.missing.length} not playable here` : ''}`);
    } catch (e) { toast(e.message, 'error'); }
  };
  const refreshQueue = async (rerender = true) => {
    if (!isBrowser()) {
      try { state.queue = await api('GET', `/zones/${state.zone}/queue`); } catch { state.queue = []; }
    }
    if (rerender && state.view === 'queue') render();
    if (!$('#nowplaying').hidden) renderNowPlaying();
  };

  $('#toggle').onclick = () => transport({ action: 'toggle' });
  $('#next').onclick = () => transport({ action: 'next' });
  $('#prev').onclick = () => transport({ action: 'previous' });
  const seek = $('#seek');
  seek.addEventListener('input', () => { seek.dataset.dragging = '1'; setFill(seek); });
  seek.addEventListener('change', () => {
    delete seek.dataset.dragging;
    const z = currentZone(); const d = z && z.now_playing ? z.now_playing.duration_ms : 0;
    transport({ action: 'seek', position_ms: Math.round((seek.value / 1000) * d) });
  });
  const vol = $('#vol');
  setFill(vol);
  vol.addEventListener('input', () => setFill(vol));
  vol.addEventListener('change', () => {
    if (isBrowser()) { audio.volume = Math.pow(10, vol.value / 20); return; }
    api('POST', `/zones/${state.zone}/volume`, { volume: { db: Number(vol.value) } }).catch((e) => toast(e.message, 'error'));
  });

  // Zone picker.
  const zonePop = $('#zonepop');
  const closePop = () => { zonePop.hidden = true; $('#scrim').hidden = true; };
  $('#zonebtn').onclick = () => {
    if (!zonePop.hidden) { closePop(); return; }
    zonePop.innerHTML = `<div class="pop-title">Play to</div>` + zoneList().map((z) => `
      <button class="pop-item ${z.id === state.zone ? 'on' : ''}" data-zone="${esc(z.id)}">
        ${icon('zone')}<span><span class="name">${esc(z.name)}</span><span class="sub">${esc(z.id === BROWSER ? 'Streams the files to this browser' : z.output)}</span></span>
        <span class="state">${z.transport === 'playing' ? `${icon('play')} playing` : ''}</span>
      </button>`).join('');
    $$('[data-zone]', zonePop).forEach((b) => b.onclick = () => {
      state.zone = b.dataset.zone; localStorage.setItem('pm.zone', state.zone);
      closePop(); renderZoneName(); renderBar(); refreshQueue();
    });
    zonePop.hidden = false; $('#scrim').hidden = false;
  };
  $('#scrim').onclick = () => { closePop(); if ($('#sidebar').classList.contains('open')) closeSheet(); };
  // The menu button: phones open the sidebar as a sheet (closes on back, outside tap, Esc or a pick);
  // wider screens collapse it to icons (or expand it on tablets), remembered per browser.
  const shell = $('.shell');
  const mq = (q) => !!(window.matchMedia && window.matchMedia(q).matches);
  const phone = () => mq('(max-width: 720px)');
  const tablet = () => mq('(max-width: 1100px)');
  const applySide = () => { let m = 'auto'; try { m = localStorage.getItem('pm.sidebar') || 'auto'; } catch { /* no storage */ } shell.classList.toggle('collapsed', m === 'icons'); shell.classList.toggle('expanded', m === 'names'); };
  applySide();
  // Icon-only nav items keep their names as tooltips and accessible labels.
  $$('.nav-item').forEach((b) => { const t = (b.querySelector('span') || {}).textContent; if (t) { b.title = t; b.setAttribute('aria-label', t); } });
  const sheetOpen = () => $('#sidebar').classList.contains('open');
  let sheetBack = false;
  const closeSheet = (fromBack) => { if (!sheetOpen()) return; sheetBack = !!fromBack; $('#sidebar').classList.remove('open'); $('#scrim').hidden = true; $('#menu').setAttribute('aria-expanded', 'false'); if (!fromBack && history.state && history.state.sheet) history.back(); };
  $('#menu').setAttribute('aria-expanded', 'false');
  $('#menu').onclick = () => {
    if (phone()) {
      if (sheetOpen()) { closeSheet(); return; }
      $('#sidebar').classList.add('open'); $('#scrim').hidden = false; $('#menu').setAttribute('aria-expanded', 'true');
      // A transient history entry, so back closes the sheet instead of leaving the page.
      try { history.pushState({ ...(history.state || {}), sheet: true }, '', location.pathname + location.search); } catch { /* sandboxed */ }
      return;
    }
    const collapsedNow = shell.classList.contains('collapsed') || (tablet() && !shell.classList.contains('expanded'));
    const next = collapsedNow ? (tablet() ? 'names' : 'auto') : 'icons';
    try { localStorage.setItem('pm.sidebar', next); } catch { /* no storage */ }
    applySide();
    $('#menu').setAttribute('aria-expanded', String(next !== 'icons' && !(tablet() && next === 'auto')));
  };
  window.addEventListener('popstate', () => { if (sheetOpen()) closeSheet(true); });
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && sheetOpen()) closeSheet(); });

  // ---- player bar ----------------------------------------------------------
  const ORDER = { bit_perfect: 0, enhanced: 1, lossless: 2, lossy: 3 };
  const LABEL = { bit_perfect: 'Bit-perfect', enhanced: 'Enhanced', lossless: 'Lossless', lossy: 'Lossy' };
  const headline = (p) => p.stages.reduce((w, s) => (ORDER[s.quality] > ORDER[w] ? s.quality : w), 'bit_perfect');
  const albumIdOf = (it) => { const t = state.trackCache.get(it.uri); return t && t.album ? t.album.id : null; };
  const renderBar = () => {
    const z = currentZone(); const np = z && z.now_playing;
    const playing = z && z.transport === 'playing';
    $('#toggle').innerHTML = icon(playing ? 'pause' : 'play');
    $('#toggle').setAttribute('aria-label', playing ? 'Pause' : 'Play');
    const q = $('#quality');
    if (!np) {
      $('#np').innerHTML = `<div class="np-art placeholder"></div><div class="np-text"><div class="np-title muted">Nothing playing</div><div class="np-sub">${esc(z ? z.name : '')}</div></div>`;
      $('#pos').textContent = '0:00'; $('#dur').textContent = '0:00'; seek.value = 0; setFill(seek);
      q.className = 'quality'; $('.qtext', q).textContent = '—'; q.title = 'Signal path';
      return;
    }
    const it = np.item;
    const live = np.stream_title;
    $('#np').innerHTML = `<div class="np-art">${itemArt(it, 'sm')}</div><div class="np-text"><div class="np-title">${esc(live || it.title || it.uri)}</div><div class="np-sub">${esc(live ? [it.title, it.album].filter(Boolean).join(' · ') : [it.artist, it.album].filter(Boolean).join(' · '))}</div></div>`;
    $('#pos').textContent = fmt(np.position_ms); $('#dur').textContent = fmt(np.duration_ms);
    if (!seek.dataset.dragging) { seek.value = np.duration_ms ? Math.round((np.position_ms / np.duration_ms) * 1000) : 0; setFill(seek); }
    const h = np.signal_path ? headline(np.signal_path) : null;
    q.className = 'quality ' + (h ? 'q-' + h : '');
    $('.qtext', q).textContent = h ? LABEL[h] : 'Browser decode';
    q.title = np.signal_path ? np.signal_path.stages.map((s) => `${s.name}: ${fmtFormat(s.in_format)} → ${fmtFormat(s.out_format)} (${s.quality})${s.note ? ' · ' + s.note : ''}`).join('\n') : 'This browser decodes the original file itself; the node makes no signal-path claim.';
  };
  $('#np').onclick = () => toggleNowPlaying();
  $('#np').onkeydown = (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggleNowPlaying(); } };
  $('#quality').onclick = () => toggleNowPlaying(true);

  // ---- Now Playing overlay -------------------------------------------------
  const overlay = $('#nowplaying');
  const toggleNowPlaying = (force) => {
    const open = force === true ? true : overlay.hidden;
    overlay.hidden = !open;
    if (open) renderNowPlaying();
  };
  const renderNowPlaying = () => {
    const z = currentZone(); const np = z && z.now_playing;
    if (!np) {
      overlay.innerHTML = `<div class="np-page"><div class="np-top"><button class="btn ghost" id="npclose">${icon('back')} Back</button></div><div class="empty">${icon('notes')}<h3>Nothing playing on ${esc(z ? z.name : 'this zone')}</h3><p>Pick an album or a track and press play.</p></div></div>`;
      $('#npclose').onclick = () => toggleNowPlaying();
      return;
    }
    const it = np.item;
    const albumId = albumIdOf(it);
    const q = (isBrowser() ? state.browserQueue : state.queue) || [];
    const idx = q.findIndex((x) => x.id === it.id);
    const next = idx >= 0 ? q.slice(idx + 1, idx + 7) : [];
    const h = np.signal_path ? headline(np.signal_path) : null;
    const colour = { bit_perfect: 'var(--q-bit)', enhanced: 'var(--q-enh)', lossless: 'var(--q-lossless)', lossy: 'var(--q-lossy)' };
    overlay.innerHTML = `
      <div class="np-page">
        <div class="np-top"><button class="btn ghost" id="npclose">${icon('back')} Back</button><span class="muted">Now playing on ${esc(z.name)}</span><span class="spacer"></span><button class="btn ghost" id="npsleep" title="Pause this zone after a while">${icon('moon')} ${sleepLabel()}</button>${isBrowser() ? '' : `<button class="btn ghost" id="npdsp">DSP</button>`}<button class="btn ghost" id="npq">${icon('queue')} Queue</button></div>
        <div class="np-hero">
          <div class="np-big">${itemArt(it)}</div>
          <div class="np-meta">
            <div class="eyebrow">${esc(np.stream_title ? 'Live · ' + (it.title || '') : it.album ? 'From ' + it.album : 'Track')}</div>
            <h1>${esc(np.stream_title || it.title || it.uri)}</h1>
            <div class="artist">${esc(np.stream_title ? (it.artist || '') : (it.artist || ''))}</div>
            <div class="np-progress"><input type="range" id="npseek" min="0" max="1000" value="${np.duration_ms ? Math.round((np.position_ms / np.duration_ms) * 1000) : 0}" aria-label="Seek"><div class="muted small" style="display:flex;justify-content:space-between"><span id="nppos">${fmt(np.position_ms)}</span><span>${fmt(np.duration_ms)}</span></div></div>
            <div class="path">
              <div class="path-head"><span class="qdot" style="width:10px;height:10px;border-radius:50%;background:${h ? colour[h] : 'var(--muted)'}"></span>${h ? LABEL[h] : 'Browser decode'}<span class="muted" style="font-weight:400;margin-left:auto">${np.signal_path ? esc(fmtFormat(np.source_format) + ' → ' + fmtFormat(np.output_format)) : ''}</span></div>
              ${np.signal_path ? np.signal_path.stages.map((s) => `<div class="path-row"><span class="pd" style="background:${colour[s.quality]}"></span><span class="pn">${esc(s.name)}</span><span class="pf">${esc(fmtFormat(s.in_format))} → ${esc(fmtFormat(s.out_format))}</span>${s.note ? `<span class="pnote">${esc(s.note)}</span>` : ''}</div>`).join('') : `<div class="path-row"><span class="pd" style="background:var(--muted)"></span><span class="pn">browser</span><span class="pf">the original file, decoded by this browser</span></div>`}
            </div>
            <div class="lyrics" id="nplyrics" hidden></div>
            ${next.length ? `<div class="upnext"><h3>Up next</h3><div class="list">${next.map((n) => `<div class="list-row"><div class="grow"><div class="name">${esc(n.title || n.uri)}</div><div class="sub">${esc(n.artist || '')}</div></div><span class="muted small">${fmt(n.duration_ms)}</span></div>`).join('')}</div></div>` : ''}
          </div>
        </div>
      </div>`;
    const s = $('#npseek'); setFill(s);
    s.addEventListener('input', () => { s.dataset.dragging = '1'; setFill(s); $('#nppos').textContent = fmt((s.value / 1000) * (np.duration_ms || 0)); });
    s.addEventListener('change', () => { delete s.dataset.dragging; transport({ action: 'seek', position_ms: Math.round((s.value / 1000) * (np.duration_ms || 0)) }); });
    $('#npclose').onclick = () => toggleNowPlaying();
    $('#npq').onclick = () => { toggleNowPlaying(); setView('queue'); };
    const npd = $('#npdsp'); if (npd) npd.onclick = () => { toggleNowPlaying(); state.dspOpen = state.zone; setView('settings'); };
    const nps = $('#npsleep'); if (nps) nps.onclick = () => showMenu(nps, [{ title: 'Sleep timer' }, ...[15, 30, 45, 60, 90].map((m) => ({ label: `${m} minutes`, icon: 'moon', run: () => setSleep(m * 60) })), { label: 'Off', icon: 'close', run: () => setSleep(null) }]);
    loadLyrics(it);
    refreshSleep();
  };
  // Sleep timer per zone (node zones through the API, the browser zone locally).
  const sleepLabel = () => { const at = state.sleepAt[state.zone]; if (!at || at <= Date.now()) return 'Sleep'; const m = Math.ceil((at - Date.now()) / 60000); return `Sleep in ${m} min`; };
  const setSleep = async (seconds) => {
    if (isBrowser()) {
      clearTimeout(state.browserSleepTimer);
      if (seconds) { state.sleepAt[BROWSER] = Date.now() + seconds * 1000; state.browserSleepTimer = setTimeout(() => { audio.pause(); delete state.sleepAt[BROWSER]; renderNowPlaying(); }, seconds * 1000); } else delete state.sleepAt[BROWSER];
      toast(seconds ? `Sleeping in ${Math.round(seconds / 60)} minutes` : 'Sleep timer off'); renderNowPlaying(); return;
    }
    try { const v = await api('PUT', `/zones/${encodeURIComponent(state.zone)}/sleep`, { seconds }); if (v.ends_at_ms) state.sleepAt[state.zone] = v.ends_at_ms; else delete state.sleepAt[state.zone]; toast(seconds ? `Sleeping in ${Math.round(seconds / 60)} minutes` : 'Sleep timer off'); renderNowPlaying(); } catch (e) { toast(e.message, 'error'); }
  };
  const refreshSleep = async () => {
    if (isBrowser()) return;
    try { const v = await api('GET', `/zones/${encodeURIComponent(state.zone)}/sleep`); if (v.ends_at_ms) state.sleepAt[state.zone] = v.ends_at_ms; else delete state.sleepAt[state.zone]; const b = $('#npsleep'); if (b) b.innerHTML = `${icon('moon')} ${sleepLabel()}`; } catch { /* keep */ }
  };
  // Lyrics for the playing library track (tags or a sidecar .lrc); synced lines follow the position.
  const trackIdOf = (it) => { const m = /^track:(\d+)$/.exec(it && it.uri || ''); return m ? Number(m[1]) : null; };
  const renderLyrics = (l) => {
    const box = $('#nplyrics'); if (!box) return;
    if (!l) { box.hidden = true; box.innerHTML = ''; return; }
    box.hidden = false;
    box.innerHTML = `<h3>Lyrics${l.synced ? ' <span class="muted small">synced</span>' : ''}</h3><div class="lyric-lines">${l.lines.map((ln) => `<p class="lyric-line" ${ln.ms != null ? `data-ms="${ln.ms}"` : ''}>${esc(ln.text) || icon('notes')}</p>`).join('')}</div>`;
  };
  const loadLyrics = async (it) => {
    const id = trackIdOf(it);
    if (id == null) { renderLyrics(null); return; }
    if (state.lyrics.has(id)) { renderLyrics(state.lyrics.get(id)); return; }
    let l = null;
    try { l = await api('GET', `/library/tracks/${id}/lyrics`); } catch { l = null; }
    state.lyrics.set(id, l);
    if (trackIdOf(currentNowPlayingItem()) === id) renderLyrics(l);
  };
  const currentNowPlayingItem = () => { const z = currentZone(); return z && z.now_playing ? z.now_playing.item : null; };
  const followLyrics = (positionMs) => {
    const lines = $$('#nplyrics .lyric-line[data-ms]'); if (!lines.length) return;
    let cur = null; for (const ln of lines) { if (Number(ln.dataset.ms) <= positionMs) cur = ln; else break; }
    for (const ln of lines) ln.classList.toggle('on', ln === cur);
    if (cur && !cur.dataset.seen) { lines.forEach((ln) => delete ln.dataset.seen); cur.dataset.seen = '1'; try { cur.scrollIntoView({ block: 'center', behavior: 'smooth' }); } catch { /* jsdom */ } }
  };
  // Keep the overlay's progress moving without rebuilding it on every tick.
  setInterval(() => {
    if (overlay.hidden) return;
    const z = currentZone(); const np = z && z.now_playing; const s = $('#npseek');
    if (!np || !s || s.dataset.dragging) return;
    if (np.item.id !== (renderNowPlaying.lastItem || {}).id || np.stream_title !== renderNowPlaying.lastTitle) { renderNowPlaying.lastTitle = np.stream_title; renderNowPlaying(); }
    s.value = np.duration_ms ? Math.round((np.position_ms / np.duration_ms) * 1000) : 0; setFill(s);
    const p = $('#nppos'); if (p) p.textContent = fmt(np.position_ms);
    followLyrics(np.position_ms);
    renderNowPlaying.lastItem = np.item;
  }, 500);

  // ---- views ---------------------------------------------------------------
  const main = $('#main');
  // ---- paths and back (navigation brief): real URLs, back/forward restore view and scroll ----
  const NAV_VIEWS = ['home', 'favourites', 'albums', 'artists', 'genres', 'tracks', 'playlists', 'radio', 'sources', 'queue', 'zones', 'settings', 'identify'];
  const VIEW_LABELS = { home: 'Home', favourites: 'Favourites', albums: 'Albums', artists: 'Artists', genres: 'Genres', tracks: 'Tracks', playlists: 'Playlists', radio: 'Radio', sources: 'Sources', discover: 'Discover', queue: 'Queue', zones: 'Zones', settings: 'Settings', identify: 'Confirm matches', album: 'Album', artist: 'Artist', genre: 'Genre', playlist: 'Playlist' };
  const b64u = (x) => btoa(unescape(encodeURIComponent(x))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  const unb64u = (x) => decodeURIComponent(escape(atob(x.replace(/-/g, '+').replace(/_/g, '/'))));
  const itemSeg = (kind) => (kind === 'artist' ? 'artist' : kind === 'album' ? 'album' : 'song');
  // A page id is the canonical identity in short form: mb-<mbid>, isrc-<ISRC>, else u-<base64url of the link>.
  const idOf = (uri) => { let m = uri.match(/^https:\/\/musicbrainz\.org\/(recording|artist|release-group)\/([0-9a-f-]{36})$/); if (m) return 'mb-' + m[2]; m = uri.match(/^https:\/\/musicbrainz\.org\/isrc\/([A-Za-z0-9]{12})$/); if (m) return 'isrc-' + m[1].toUpperCase(); return 'u-' + b64u(uri); };
  const uriOf = (seg, id) => { if (id.startsWith('mb-')) return `https://musicbrainz.org/${seg === 'artist' ? 'artist' : seg === 'album' ? 'release-group' : 'recording'}/${id.slice(3)}`; if (id.startsWith('isrc-')) return `https://musicbrainz.org/isrc/${id.slice(5)}`; if (id.startsWith('u-')) { try { return unb64u(id.slice(2)); } catch { return null; } } return null; };
  const pathFor = (v, arg) => {
    if (v === 'item' && arg && arg.kind && arg.id) return `/${arg.kind}/${arg.id}`;
    if (v === 'home-section' && arg && arg.section) return `/home/${arg.section}${arg.window && arg.window !== '7d' ? `?window=${arg.window}` : ''}`;
    if (v === 'tag' && arg) return `/tag/${String(arg).split(',').map(encodeURIComponent).join(',')}`;
    if (v === 'playlist' && arg != null) return `/playlist/${encodeURIComponent(arg)}`;
    if (v === 'genre' && arg != null) return `/genre/${encodeURIComponent(arg)}`;
    if (v === 'sources' && arg && arg.source) return `/source/${encodeURIComponent(arg.source)}`;
    if (v === 'source-item' || v === 'source-artist') return null;
    if (v === 'discover-item' && arg && arg.uri) return `/${itemSeg(arg.kind)}/${idOf(arg.uri)}`;
    if (v === 'search') return arg && arg.q ? `/search?q=${encodeURIComponent(arg.q)}${arg.in && arg.in !== 'everywhere' ? `&in=${encodeURIComponent(arg.in)}` : ''}` : '/search';
    if (v === 'home') return '/';
    return NAV_VIEWS.includes(v) ? '/' + v : null;
  };
  const routeFromPath = () => {
    const p = location.pathname.replace(/\/+$/, '') || '/';
    let m = p.match(/^\/(song|album|artist)\/([^/]+)$/);
    if (m && (m[2].startsWith('lib-') || uriOf(m[1], m[2]))) return { v: 'item', arg: { kind: m[1], id: m[2] } };
    m = p.match(/^\/home\/([a-z-]+)$/);
    if (m) { const w = new URLSearchParams(location.search).get('window'); return { v: 'home-section', arg: w ? { section: m[1], window: w } : { section: m[1] } }; }
    m = p.match(/^\/(?:tag|discover\/tag)\/([^/]+)$/);
    if (m) return { v: 'tag', arg: m[1].split(',').map(decodeURIComponent).join(',') };
    if (p === '/discover' || p.startsWith('/discover/')) return { v: 'home' };
    m = p.match(/^\/(playlist|genre|source)\/([^/]+)$/);
    if (m) { const a = decodeURIComponent(m[2]); return m[1] === 'playlist' ? { v: 'playlist', arg: a } : m[1] === 'genre' ? { v: 'genre', arg: /^\d+$/.test(a) ? Number(a) : a } : { v: 'sources', arg: { source: a } }; }
    if (p === '/search') { const sp = new URLSearchParams(location.search); const q = sp.get('q') || ''; const sc = sp.get('in'); return { v: 'search', arg: q ? (sc ? { q, in: sc } : { q }) : null }; }
    const v = p.slice(1);
    return NAV_VIEWS.includes(v) ? { v } : null;
  };
  // The same thing gets the same page (docs/ux/08-item-page.md): old detail views open the item page.
  const SERVICE_PAGE = { tidal: (k, id) => `https://tidal.com/${k}/${id}`, deezer: (k, id) => `https://www.deezer.com/${k}/${id}`, spotify: (k, id) => `https://open.spotify.com/${k}/${id}` };
  const toItem = (v, arg) => {
    if (v === 'discover-item' && arg && arg.uri) return { kind: itemSeg(arg.kind), id: idOf(arg.uri) };
    if (v === 'album' && typeof arg === 'number') return { kind: 'album', id: 'lib-' + arg };
    if (v === 'artist' && typeof arg === 'number') return { kind: 'artist', id: 'lib-' + arg };
    if ((v === 'source-item' || v === 'source-artist') && arg && SERVICE_PAGE[arg.source]) {
      const [k, id] = String(arg.id).includes(':') ? String(arg.id).split(':') : [v === 'source-artist' ? 'artist' : 'album', String(arg.id)];
      return { kind: k === 'track' ? 'song' : k, id: idOf(SERVICE_PAGE[arg.source](k, id)) };
    }
    return null;
  };
  // What the back arrow says on the next page.
  const hereLabel = () => state.view === 'item' ? (state.itemTitle || 'Back') : state.view === 'tag' ? `#${String(state.viewArg).split(',').join(' #')}` : state.view === 'home-section' ? (HOME_TITLES[state.viewArg && state.viewArg.section] || 'Home') : state.view === 'playlist' ? (state.itemTitle || 'Playlist') : state.view === 'search' ? `Results for “${(state.viewArg && state.viewArg.q) || ''}”` : (VIEW_LABELS[state.view] || 'Back');
  // History stores only what restores a view (search keeps its query, not its results).
  const histArg = (v, arg) => (v === 'search' ? (arg && arg.q ? (arg.in && arg.in !== 'everywhere' ? { q: arg.q, in: arg.in } : { q: arg.q }) : null) : (arg && typeof arg === 'object' && !Array.isArray(arg) ? JSON.parse(JSON.stringify(arg)) : arg ?? null));
  let navDepth = 0;
  // For tests (tools/ui-item-pages.js): every view with a path round-trips through routeFromPath.
  window.pmRoutes = {
    check: () => {
      const cases = [['home'], ['albums'], ['artists'], ['tracks'], ['playlists'], ['queue'], ['zones'], ['settings'], ['favourites'], ['genres'], ['radio'], ['identify'],
        ['item', { kind: 'song', id: 'lib-3' }], ['item', { kind: 'album', id: 'lib-1' }], ['item', { kind: 'artist', id: 'mb-0383dadf-2a4e-4d10-a46a-e9e041da8eb3' }],
        ['item', { kind: 'album', id: idOf('https://tidal.com/album/199574241') }], ['item', { kind: 'song', id: 'isrc-GBUM71029604' }],
        ['home-section', { section: 'trending-songs' }], ['home-section', { section: 'trending-albums', window: '30d' }], ['tag', 'hip-hop,german'],
        ['playlist', 'abc123'], ['genre', 4], ['sources', { source: 'tidal' }], ['search', { q: 'queen' }], ['search', { q: 'queen', in: 'tidal' }]];
      const saved = location.pathname + location.search;
      const out = cases.map(([v, arg]) => {
        const path = pathFor(v, arg); if (!path) return `FAIL ${v}: no path`;
        history.replaceState(history.state, '', path); const r = routeFromPath() || { v: path === '/' ? 'home' : null };
        const same = r.v === v && JSON.stringify(r.arg ?? null) === JSON.stringify(arg ?? null);
        return `${same ? 'ok' : 'FAIL'} ${path}`;
      });
      history.replaceState(history.state, '', saved);
      return out;
    },
  };
  const setView = (v, arg, how = 'push') => {
    { const t = toItem(v, arg); if (t) { v = 'item'; arg = t; } }
    if (v === 'discover') { if (arg && arg.tag) { v = 'tag'; arg = arg.tag; } else { v = 'home'; arg = undefined; } }
    if (how !== 'pop') {
      const path = pathFor(v, arg) || location.pathname + location.search;
      const cur = history.state || {};
      try { history.replaceState({ ...cur, scroll: main.scrollTop }, '', location.pathname + location.search); } catch { /* sandboxed */ }
      // Changing a query or tab inside the same page replaces; opening another page pushes.
      const replace = how === 'replace' || !!cur.sheet || (v === state.view && (v === 'search' || v === 'discover' || path === location.pathname + location.search)) || !state.view;
      const entry = { sheet: false, v, arg: histArg(v, arg), backLabel: replace ? cur.backLabel : hereLabel(), depth: replace ? (cur.depth || 0) : (cur.depth || 0) + 1, scroll: 0 };
      try { if (replace) history.replaceState(entry, '', path); else history.pushState(entry, '', path); } catch { /* sandboxed */ }
      navDepth = entry.depth;
    }
    state.view = v; state.viewArg = arg;
    $$('.nav-item').forEach((b) => { const on = b.dataset.view === v || (v === 'item' && b.dataset.view === (state.itemSection || (arg && arg.kind === 'artist' ? 'artists' : 'albums'))) || (v === 'album' && b.dataset.view === 'albums') || (v === 'artist' && b.dataset.view === 'artists') || (v === 'genre' && b.dataset.view === 'genres') || (v === 'playlist' && b.dataset.view === 'playlists') || (v === 'source-item' && b.dataset.view === 'sources'); b.classList.toggle('on', on); if (on) b.setAttribute('aria-current', 'page'); else b.removeAttribute('aria-current'); });
    if ($('#sidebar').classList.contains('open')) { $('#sidebar').classList.remove('open'); $('#menu').setAttribute('aria-expanded', 'false'); } $('#scrim').hidden = true; closePop();
    main.scrollTop = 0;
    return render();
  };
  window.addEventListener('popstate', async (e) => {
    if (sheetBack) { sheetBack = false; return; } // back only closed the sidebar sheet
    const st = e.state || routeFromPath() || { v: 'home' };
    navDepth = st.depth || 0;
    if (st.v === 'search' && st.arg && st.arg.q) { search.value = st.arg.q; startSearch(st.arg.q, st.arg.in || 'everywhere'); await setView('search', st.arg, 'pop'); }
    else await setView(st.v, st.arg, 'pop');
    if (st.scroll) main.scrollTop = st.scroll;
  });
  // The back arrow: one step back when there is in-app history, else the parent page.
  const goBack = (fallback) => { if (navDepth > 0) history.back(); else setView(fallback || 'home'); };
  $$('.nav-item').forEach((b) => b.onclick = () => setView(b.dataset.view));
  // Back from TIDAL's sign-in page.
  { const p = new URLSearchParams(location.search); const t = p.get('tidal'); if (t) { history.replaceState(null, '', location.pathname); setTimeout(() => { if (t === 'signed_in') { toast('TIDAL signed in'); setView('sources', { source: 'tidal' }); } else toast('TIDAL sign-in failed: ' + (p.get('detail') || t), 'error'); }, 300); } }

  const search = $('#search');
  let searchTimer = null;
  // ---- one search, scoped, streamed per source (STEWARD-UX-HOME.md "Search") ----
  const searchRuns = new Map(); // `${q}|${scope}` → { q, scope, sources, answers, startedAt, firstAt, doneAt, ctrl }
  const SEARCH_TTL = 5 * 60 * 1000;
  const startSearch = (q, scope) => {
    const key = `${q.toLowerCase()}|${scope}`;
    const old = searchRuns.get(key);
    if (old && (old.pending || Date.now() - old.startedAt < SEARCH_TTL)) return old;
    for (const r of searchRuns.values()) if (r.pending && r !== old) { try { r.ctrl.abort(); } catch { /* */ } r.pending = false; }
    const run = { q, scope, sources: [], answers: {}, startedAt: Date.now(), firstAt: null, doneAt: null, ctrl: new AbortController(), pending: true, slow: new Set() };
    searchRuns.set(key, run);
    (async () => {
      try {
        const r = await fetch(`/v1/search?${new URLSearchParams({ q, in: scope })}`, { headers: state.token ? { authorization: 'Bearer ' + state.token } : {}, signal: run.ctrl.signal });
        run.sources = (r.headers.get('x-pm-sources') || '').split(',').filter(Boolean);
        searchChanged(run);
        setTimeout(() => { for (const src of run.sources) if (!run.answers[src]) run.slow.add(src); searchChanged(run); }, 5000);
        const reader = r.body.getReader(); const dec = new TextDecoder(); let buf = '';
        for (;;) {
          const { value, done } = await reader.read(); if (done) break;
          buf += dec.decode(value, { stream: true });
          let nl;
          while ((nl = buf.indexOf('\n')) >= 0) {
            const line = buf.slice(0, nl); buf = buf.slice(nl + 1);
            if (!line.trim()) continue;
            const a = JSON.parse(line); run.answers[a.source] = a; run.slow.delete(a.source);
            if (!run.firstAt) run.firstAt = Date.now();
            searchChanged(run);
          }
        }
      } catch (e) { if (e.name !== 'AbortError') toast(e.message, 'error'); }
      run.pending = false; run.doneAt = Date.now();
      window.pmLastSearch = { q, scope, first_ms: run.firstAt && run.firstAt - run.startedAt, all_ms: run.doneAt - run.startedAt, sources: Object.fromEntries(Object.values(run.answers).map((a) => [a.source, a.ms])) };
      searchChanged(run);
    })();
    return run;
  };
  const searchChanged = (run) => {
    const prog = $('#sprog'); const total = Math.max(run.sources.length, 1); const got = Object.keys(run.answers).length;
    if (prog) { prog.hidden = !run.pending; prog.firstElementChild.style.width = `${Math.max(8, (got / total) * 100)}%`; }
    if (state.view === 'search' && state.viewArg && state.viewArg.q === run.q && (state.viewArg.in || 'everywhere') === run.scope) renderSearchBody(run);
  };
  const isLink = (q) => /^https?:\/\/\S+$/i.test(q);
  const linkKind = (u) => (/\/(artist|artists)\//.test(u) ? 'artist' : /\/(album|albums|release|release-group)\//.test(u) ? 'album' : 'song');
  const runSearch = async (how) => {
    const q = search.value.trim();
    if (!q) { if (state.view === 'search') setView('home'); return; }
    // A pasted link skips searching: its item page (by-uri, then resolve; "Reading this link…").
    if (isLink(q)) { setView('item', { kind: linkKind(q), id: idOf(q) }); return; }
    const scope = (state.viewArg && state.view === 'search' && state.viewArg.in) || state.searchScope || 'everywhere';
    startSearch(q, scope);
    setView('search', { q, in: scope }, how === 'pop' ? 'pop' : 'push');
  };
  search.addEventListener('input', () => { clearTimeout(searchTimer); searchTimer = setTimeout(runSearch, 250); });
  search.addEventListener('keydown', (e) => { if (e.key === 'Enter') { clearTimeout(searchTimer); runSearch(); } if (e.key === 'Escape') { search.value = ''; search.blur(); if (state.view === 'search') setView('home'); } });

  document.addEventListener('keydown', (e) => {
    const typing = ['INPUT', 'SELECT', 'TEXTAREA'].includes(e.target.tagName) || e.target.isContentEditable;
    if (e.key === 'Escape') { if (!overlay.hidden) toggleNowPlaying(); closePop(); closeMenu(); return; }
    if (typing) return;
    if (e.key === '/') { e.preventDefault(); search.focus(); search.select(); return; }
    if (e.code === 'Space') { e.preventDefault(); transport({ action: 'toggle' }); }
    if (e.key === 'ArrowRight' && !e.metaKey && !e.ctrlKey) transport({ action: 'next' });
    if (e.key === 'ArrowLeft' && !e.metaKey && !e.ctrlKey) transport({ action: 'previous' });
    if (e.key.toLowerCase() === 'n') toggleNowPlaying();
  });

  // ---- templates -----------------------------------------------------------
  const isFav = (t) => { const c = t.canonical; const uri = Array.isArray(c) ? c[0] : c; const url = uri && typeof uri === 'object' ? canonicalUrl(uri) : null; return url ? state.favUris.has(url) : false; };
  const canonicalUrl = (u) => {
    if (!u) return null;
    if (u.recording) return 'https://musicbrainz.org/recording/' + (u.recording.mbid || u.recording);
    if (u.release) return 'https://musicbrainz.org/release/' + (u.release.mbid || u.release);
    if (u.release_group) return 'https://musicbrainz.org/release-group/' + (u.release_group.mbid || u.release_group);
    if (u.artist) return 'https://musicbrainz.org/artist/' + (u.artist.mbid || u.artist);
    if (u.isrc) return 'https://musicbrainz.org/isrc/' + (u.isrc.isrc || u.isrc);
    if (u.service) return u.service.url || null;
    return null;
  };
  // A library track's page id: its canonical identity when known, else lib-<id>.
  const trackPageId = (t) => { const c = Array.isArray(t.canonical) ? t.canonical[0] : t.canonical; const u = canonicalUrl(c); return u ? idOf(u) : 'lib-' + t.id; };
  const trackHasId = (t) => !!canonicalUrl(Array.isArray(t.canonical) ? t.canonical[0] : t.canonical);
  const trackRows = (tracks, { showAlbum = true, numbers = 'index', key = 'tr' } = {}) => {
    rememberTracks(tracks);
    const z = currentZone(); const np = z && z.now_playing;
    return `<div class="tracks" data-list="${key}">${tracks.map((t, i) => {
      const cur = np && np.item.uri === 'track:' + t.id;
      const fav = isFav(t); const can = trackHasId(t);
      return `<div class="track ${showAlbum ? '' : 'compact'} ${cur ? 'cur' : ''}" data-i="${i}">
        <div class="no"><span class="num">${numbers === 'track' ? (t.track_no ?? '') : i + 1}</span><button class="rowplay" data-act="play" title="Play from here">${icon('play')}</button></div>
        <div class="main-col"><div class="title">${can ? '' : `<button class="idmark" data-act="identify" title="Not identified yet: identify it so tags, playlists and Pubky can name it">?</button>`}<a class="lnk" data-act="song">${esc(t.title)}</a></div><div class="by">${t.artists && t.artists[0] && t.artists[0].id != null ? `<a class="lnk" data-act="tartist">${esc(artists(t))}</a>` : esc(artists(t))}</div></div>
        ${showAlbum ? `<div class="album-col">${t.album ? `<button class="lnk" data-act="album" data-album="${t.album.id}">${esc(t.album.title)}</button>` : ''}</div>` : ''}
        <div><span class="fmt">${esc(fmtFormat(t.file && t.file.format))}</span></div>
        <div class="dur">${fmt(t.duration_ms)}</div>
        <div class="row-actions">
          <button class="icon-btn heart ${fav ? 'on' : ''}" data-act="fav" ${can ? '' : 'disabled'} title="${can ? (fav ? 'Remove from favourites' : 'Add to favourites') : 'No MusicBrainz id or ISRC yet'}">${icon(fav ? 'heart-fill' : 'heart')}</button>
          <button class="icon-btn" data-act="next" title="Play next">${icon('playnext')}</button>
          <button class="icon-btn" data-act="add" title="Add to queue">${icon('plus')}</button>
          <button class="icon-btn" data-act="more" title="More">${icon('more')}</button>
        </div>
      </div>`;
    }).join('')}</div>`;
  };
  const bindTrackRows = (container, lists) => {
    container.addEventListener('click', async (e) => {
      const btn = e.target.closest('[data-act]'); if (!btn) return;
      const row = btn.closest('.track'); const list = btn.closest('[data-list]');
      const tracks = lists[list ? list.dataset.list : 'tr'] || [];
      const i = Number(row ? row.dataset.i : -1);
      const act = btn.dataset.act;
      if (act === 'album') { setView('album', Number(btn.dataset.album)); return; }
      if (act === 'identify') { setView('identify'); return; }
      if (act === 'song' || act === 'tartist') {
        const t0 = (lists[list ? list.dataset.list : 'tr'] || [])[Number(btn.closest('.track').dataset.i)]; if (!t0) return;
        if (act === 'song') setView('item', { kind: 'song', id: trackPageId(t0) }); else setView('item', { kind: 'artist', id: 'lib-' + t0.artists[0].id });
        return;
      }
      if (i < 0) return;
      if (act === 'play') playTracks(tracks.slice(i), true);
      else if (act === 'next') playTracks([tracks[i]], false, 'next');
      else if (act === 'add') playTracks([tracks[i]], false, 'end');
      else if (act === 'more') trackMenu(btn, [tracks[i]]);
      else if (act === 'fav') {
        const on = btn.classList.contains('on');
        try { await setFavourite({ track_id: tracks[i].id }, !on); btn.classList.toggle('on', !on); btn.innerHTML = icon(!on ? 'heart-fill' : 'heart'); btn.title = !on ? 'Remove from favourites' : 'Add to favourites'; }
        catch (er) { toast(er.message, 'error'); }
      }
    }, { once: false });
  };
  const fmtOf = (id) => (state.formats && state.formats.get(id)) || null;
  const fmtBadge = (id) => { const f = fmtOf(id); return f ? `<span class="fmt ${f.dsd || f.hi_res ? 'hi' : f.lossless ? '' : 'lossy'}" title="Best file on this album">${esc(f.label)}</span>` : ''; };
  const tile = (a) => `<button class="tile" data-album="${a.id}" title="${esc(a.title)}">${art(a.id, a.title)}<span class="tile-play" data-tileplay="${a.id}" role="button" title="Play album">${icon('play')}</span><div class="t">${esc(a.title)}</div><div class="a">${esc(a.album_artist || '')}${a.year ? ` · ${a.year}` : ''}${fmtBadge(a.id)}</div></button>`;
  const tagRow = (tags, ref) => `<div class="tagrow" data-tagref='${esc(JSON.stringify(ref))}'><span class="eyebrow" title="Public on Pubky">Tags</span>${tags.map((t) => `<span class="chip tag"><button class="chiplink" data-tag-open="${esc(t.label)}" title="Open in Discover">#${esc(t.label)}</button><button class="chipx" data-tag-del="${esc(t.label)}" title="${t.synced ? 'Public on your homeserver · remove' : 'Not pushed yet · remove'}">×</button></span>`).join('')}<input class="taginput" id="tagadd" placeholder="Add a tag…" maxlength="20"></div>`;
  const bindTagRow = (ref) => {
    const add = $('#tagadd'); if (!add) return;
    add.onkeydown = async (e) => { if (e.key !== 'Enter' || !add.value.trim()) return; try { await api('PUT', '/tags', { ...ref, label: add.value.trim() }); toast(`Tagged #${add.value.trim().toLowerCase()}`); render(); } catch (er) { toast(er.message, 'error'); } };
    $$('[data-tag-del]', main).forEach((b) => b.onclick = async () => { try { await api('DELETE', '/tags?' + new URLSearchParams({ ...Object.fromEntries(Object.entries(ref).map(([k, v]) => [k, String(v)])), label: b.dataset.tagDel }).toString()); render(); } catch (er) { toast(er.message, 'error'); } });
    $$('[data-tag-open]', main).forEach((b) => b.onclick = () => setView('discover', { tag: b.dataset.tagOpen }));
  };
  // Ratings are 1–10: five stars in halves (7 shows three and a half).
  const starsText = (v) => (v ? `${Math.floor(v / 2) || ''}${v % 2 ? '½' : ''}` : '');
  const starFill = (v, n) => (!v ? 0 : v >= 2 * n ? 2 : v === 2 * n - 1 ? 1 : 0);
  const reviewRow = (r) => { const v = r ? r.rating : null; return `<div class="reviewrow"><span class="eyebrow">My rating</span><span class="stars" id="stars" role="group" aria-label="My rating, in half stars">${[1, 2, 3, 4, 5].map((n) => `<span class="star f${starFill(v, n)}"><span class="glyph" aria-hidden="true">★</span><button class="half l" data-rate="${2 * n - 1}" aria-pressed="${v === 2 * n - 1}" aria-label="${n - 1 ? `${n - 1} and a half stars` : 'Half a star'}" title="${starsText(2 * n - 1)}"></button><button class="half r" data-rate="${2 * n}" aria-pressed="${v === 2 * n}" aria-label="${n} star${n === 1 ? '' : 's'}" title="${n}"></button></span>`).join('')}</span><span class="ratingval" id="ratingval">${v ? `${starsText(v)} of 5` : ''}</span><button class="btn sm ghost" id="notesbtn">${r && r.text ? 'Edit notes' : 'Add notes'}</button>${r && r.text ? `<div class="notes" id="notes">${esc(r.text)}</div>` : ''}<div class="notes-edit" id="notesedit" hidden><textarea class="input" id="notestext" rows="3" placeholder="Notes, kept privately on your homeserver">${esc(r ? r.text : '')}</textarea><div class="actions" style="margin-top:8px"><button class="btn primary sm" id="notessave">Save</button><button class="btn sm ghost" id="notescancel">Cancel</button></div></div></div>`; }; // lint-allow-glyph: the stars are the rating component's own styled glyphs
  const bindReviewRow = (ref, r) => {
    const stars = $('#stars'); if (!stars) return;
    const save = async (rating, text) => { try { if (rating == null && !text) await api('DELETE', '/reviews?' + new URLSearchParams(Object.entries(ref).map(([k, v]) => [k, String(v)])).toString()); else await api('PUT', '/reviews', { ...ref, rating, text }); toast('Review saved'); render(); } catch (e) { toast(e.message, 'error'); } };
    const cur = r ? r.rating : null;
    const paint = (v) => $$('.star', stars).forEach((s, i) => { s.className = `star f${starFill(v, i + 1)}`; });
    $$('[data-rate]', stars).forEach((b) => {
      b.onmouseenter = () => paint(Number(b.dataset.rate));
      b.onfocus = () => paint(Number(b.dataset.rate));
      // The same value again clears the rating
      b.onclick = () => { const v = Number(b.dataset.rate); save(cur === v ? null : v, r ? r.text : ''); };
    });
    stars.onmouseleave = () => paint(cur);
    stars.addEventListener('focusout', () => paint(cur));
    $('#notesbtn').onclick = () => { $('#notesedit').hidden = false; $('#notesbtn').hidden = true; if ($('#notes')) $('#notes').hidden = true; $('#notestext').focus(); };
    $('#notescancel').onclick = () => { $('#notesedit').hidden = true; $('#notesbtn').hidden = false; if ($('#notes')) $('#notes').hidden = false; };
    $('#notessave').onclick = () => save(r ? r.rating : null, $('#notestext').value.trim());
  };
  // A small anchored menu (track ⋯, album actions). One at a time; Esc or the scrim closes it.
  const menuEl = document.createElement('div'); menuEl.className = 'popover menu'; menuEl.hidden = true; document.body.appendChild(menuEl);
  const closeMenu = () => { menuEl.hidden = true; };
  const showMenu = (anchor, items) => {
    menuEl.innerHTML = items.map((it, i) => it.title ? `<div class="pop-title">${esc(it.title)}</div>` : `<button class="pop-item ${it.on ? 'on' : ''}" data-mi="${i}" ${it.disabled ? 'disabled' : ''} ${it.on != null ? `aria-checked="${!!it.on}" role="menuitemradio"` : ''}>${it.svc ? svcBadge(it.svc) : it.icon ? icon(it.icon) : ''}<span>${esc(it.label)}${it.note ? `<span class="pop-note">${esc(it.note)}</span>` : ''}</span>${it.on ? icon('check', 'ic pop-check') : ''}</button>`).join('');
    $$('[data-mi]', menuEl).forEach((b) => b.onclick = () => { closeMenu(); items[Number(b.dataset.mi)].run(); });
    menuEl.hidden = false;
    const r = anchor.getBoundingClientRect(); const mw = 280;
    menuEl.style.left = Math.max(8, Math.min(window.innerWidth - mw - 8, r.right - mw)) + 'px';
    menuEl.style.top = Math.min(window.innerHeight - menuEl.offsetHeight - 8, r.bottom + 6) + 'px';
    menuEl.style.bottom = 'auto'; menuEl.style.right = 'auto'; menuEl.style.width = mw + 'px';
    setTimeout(() => document.addEventListener('click', (e) => { if (!menuEl.contains(e.target)) closeMenu(); }, { once: true }), 0);
  };
  const addToPlaylist = async (playlistId, tracks, uris) => {
    try { await api('POST', `/playlists/${encodeURIComponent(playlistId)}/items`, uris ? { uris } : { track_ids: tracks.map((t) => t.id) }); const n = uris ? uris.length : tracks.length; toast(n === 1 ? 'Added to playlist' : `${n} tracks added to playlist`); }
    catch (e) { toast(e.message, 'error'); }
  };
  // "Add to playlist" for canonical URLs (Internet Archive files): the same menu, by uris.
  const uriMenu = async (anchor, uris, name) => {
    if (!state.playlists.length) { try { state.playlists = await api('GET', '/playlists'); } catch { /* keep */ } }
    showMenu(anchor, [
      { title: 'Add to playlist' },
      ...state.playlists.map((p) => ({ label: p.name, icon: 'playlist', run: () => addToPlaylist(p.id, [], uris) })),
      { label: 'New playlist…', icon: 'plus', run: async () => { try { await api('POST', '/playlists', { name, uris }); state.playlists = await api('GET', '/playlists'); toast(`Created “${name}”`); } catch (e) { toast(e.message, 'error'); } } },
    ]);
  };
  const trackMenu = async (anchor, tracks) => {
    if (!state.playlists.length) { try { state.playlists = await api('GET', '/playlists'); } catch { /* keep */ } }
    await pubkyUser();
    const single = tracks.length === 1 ? tracks[0] : null;
    const favItem = single && trackHasId(single) ? [{ label: isFav(single) ? 'Remove from favourites' : 'Add to favourites', icon: isFav(single) ? 'heart-fill' : 'heart', run: async () => { try { await setFavourite({ track_id: single.id }, !isFav(single)); render(); } catch (e) { toast(e.message, 'error'); } } }] : [];
    // Start radio: from the single track, or from a random track of a set (album page menu).
    const seeds = tracks.filter((t) => t && trackHasId(t));
    let versionItems = [];
    if (single && single.id != null && single.isrc) {
      try { const vs = await api('GET', `/library/tracks/${single.id}/versions`); versionItems = vs.filter((v) => v.uri).map((v) => ({ label: `Stream from ${v.source.toUpperCase()} (${v.label.replace(/^TIDAL · /, '')})`, icon: 'zone', run: () => playSourceItems([{ uri: v.uri, title: single.title, artist: artists(single), album: single.album ? single.album.title : '', duration_ms: single.duration_ms }], true) })); const tv = vs.find((v) => v.source === 'tidal' && /^track:/.test(v.id)); if (tv) versionItems.push({ label: 'Full length in TIDAL\'s player (here)', icon: 'external', run: () => tidalEmbed('tracks', tv.id.replace(/^track:/, ''), single.title) }); } catch { /* no versions */ }
    }
    const radioItem = seeds.length && !isBrowser() ? [{ label: 'Start radio', icon: 'radio', run: async () => { const seed = single || seeds[Math.floor(Math.random() * seeds.length)]; try { await playTracks([seed], true); await api('PUT', `/zones/${encodeURIComponent(state.zone)}/radio`, { on: true }); const zs = await api('GET', `/zones/${encodeURIComponent(state.zone)}`); state.zones[zs.id] = zs; toast(`Radio from “${seed.title}”: similar music follows`); if (state.view === 'queue') render(); } catch (e) { toast(e.message, 'error'); } } }] : [];
    const nav = single ? [
      ...(single.artists && single.artists[0] && single.artists[0].id != null ? [{ label: 'Go to artist', icon: 'artist', run: () => setView('item', { kind: 'artist', id: 'lib-' + single.artists[0].id }) }] : []),
      ...(single.album && single.album.id != null ? [{ label: 'Go to album', icon: 'disc', run: () => setView('item', { kind: 'album', id: 'lib-' + single.album.id }) }] : []),
      { label: 'Song page', icon: 'compass', run: () => setView('item', { kind: 'song', id: trackPageId(single) }) },
      { label: 'Copy link', icon: 'compass', run: () => copyLink(`/song/${trackPageId(single)}`) },
    ] : [];
    const items = [
      { label: 'Play next', icon: 'playnext', run: () => playTracks(tracks, false, 'next') },
      { label: 'Add to queue', icon: 'plus', run: () => playTracks(tracks, false, 'end') },
      ...favItem,
      ...radioItem,
      ...versionItems,
      ...nav,
      ...(tracks.length && tracks.every((t) => t && trackHasId(t)) && state.pubkyUser ? [{ label: tracks.length === 1 ? 'Upload to homeserver' : `Upload ${tracks.length} tracks to homeserver`, icon: 'download', run: async () => { let n = 0; for (const t of tracks) { try { await api('POST', '/pubky/files', { track_id: t.id }); n++; } catch (e) { toast(`${t.title}: ${e.message}`, 'error'); break; } } if (n) toast(n === 1 ? 'Uploaded to your homeserver (private)' : `${n} tracks uploaded to your homeserver (private)`); } }] : []),
      { title: 'Add to playlist' },
      ...state.playlists.map((p) => ({ label: p.name, icon: 'playlist', run: () => addToPlaylist(p.id, tracks) })),
      { label: 'New playlist…', icon: 'plus', run: async () => { const name = (tracks.length === 1 ? tracks[0].title : (tracks[0].album ? tracks[0].album.title : 'New playlist')) || 'New playlist'; try { const d = await api('POST', '/playlists', { name, track_ids: tracks.map((t) => t.id) }); state.playlists = await api('GET', '/playlists'); toast(`Created “${d.summary ? d.summary.name : name}”`); } catch (e) { toast(e.message, 'error'); } } },
    ];
    showMenu(anchor, items);
  };
  const BAND_KINDS = ['peaking', 'low_shelf', 'high_shelf', 'low_pass', 'high_pass'];
  const dspEditor = (zone, cfg) => `<div class="card dsp" style="margin-top:12px"><div class="section-head"><h3>DSP · ${esc(zone.name)}</h3><span class="muted small">Every stage shows in the signal path; defaults keep the path bit-perfect.</span></div>
    <div class="dsp-grid">
      <label><span>Headroom (dB)</span><input class="input" id="d-headroom" type="number" step="0.5" min="-30" max="0" value="${cfg.headroom_db ?? ''}" placeholder="off"></label>
      <label><span>Software volume (dB)</span><input class="input" id="d-volume" type="number" step="0.5" min="-60" max="0" value="${cfg.volume_db ?? ''}" placeholder="off"></label>
      <label><span>Volume limit (dB)</span><input class="input" id="d-maxvol" type="number" step="0.5" min="-90" max="0" value="${cfg.max_volume_db ?? ''}" placeholder="none" title="Every volume request for this zone, hardware or software, is capped here"></label>
      <label><span>Loudness (ReplayGain)</span><select class="input" id="d-loudness">${['off', 'track', 'album'].map((v) => `<option value="${v}" ${cfg.loudness === v ? 'selected' : ''}>${v}</option>`).join('')}</select></label>
      <label><span>DSD</span><select class="input" id="d-dsd">${['auto', 'native', 'do_p', 'to_pcm'].map((v) => `<option value="${v}" ${cfg.dsd_mode === v ? 'selected' : ''}>${v === 'do_p' ? 'DoP' : v}</option>`).join('')}</select></label>
      <label class="check"><input type="checkbox" id="d-dither" ${cfg.dither ? 'checked' : ''}> TPDF dither when reducing bit depth</label>
      <label class="check"><input type="checkbox" id="d-resample" ${cfg.resample ? 'checked' : ''}> Resample when the output cannot take the rate (shown as Lossy)</label>
      <label class="check"><input type="checkbox" id="d-cf" ${cfg.crossfeed && cfg.crossfeed.enabled !== false ? 'checked' : ''}> Headphone crossfeed</label>
      <div class="cf-fields"><label><span>Level (dB)</span><input class="input" id="d-cf-level" type="number" step="0.5" value="${cfg.crossfeed ? cfg.crossfeed.level_db : -6}"></label><label><span>Cutoff (Hz)</span><input class="input" id="d-cf-cutoff" type="number" step="50" value="${cfg.crossfeed ? cfg.crossfeed.cutoff_hz : 700}"></label><label><span>Delay (µs)</span><input class="input" id="d-cf-delay" type="number" step="10" value="${cfg.crossfeed ? cfg.crossfeed.delay_us : 300}"></label></div>
    </div>
    <h3 style="margin-top:14px">Parametric EQ</h3>
    <div class="eq" id="eq">${(cfg.eq || []).map((b, i) => eqRow(b, i)).join('')}</div>
    <div class="actions" style="margin-top:8px"><button class="btn sm" id="eq-add">${icon('plus')} Add band</button></div>
    <div class="actions" style="margin-top:14px"><button class="btn primary" id="d-save">Save</button><button class="btn" id="d-reset">Reset to bit-perfect</button><button class="btn ghost" id="d-close">Close</button></div></div>`;
  const eqRow = (b, i) => `<div class="eq-row" data-i="${i}"><select class="input eq-kind">${BAND_KINDS.map((k) => `<option value="${k}" ${b.kind === k ? 'selected' : ''}>${k.replace('_', ' ')}</option>`).join('')}</select><input class="input eq-freq" type="number" min="10" max="22000" step="1" value="${b.freq_hz}" title="Hz"><input class="input eq-gain" type="number" min="-24" max="24" step="0.5" value="${b.gain_db ?? 0}" title="dB"><input class="input eq-q" type="number" min="0.1" max="20" step="0.1" value="${b.q ?? 0.7071}" title="Q"><label class="check small"><input type="checkbox" class="eq-on" ${b.enabled !== false ? 'checked' : ''}> on</label><button class="icon-btn eq-del" title="Remove band">${icon('close')}</button></div>`;
  const bindDspEditor = (zoneId, cfg) => {
    const num = (id) => { const v = $(id).value.trim(); return v === '' ? null : Number(v); };
    const collect = () => ({
      ...cfg,
      headroom_db: num('#d-headroom'), volume_db: num('#d-volume'), max_volume_db: num('#d-maxvol'), loudness: $('#d-loudness').value, dsd_mode: $('#d-dsd').value,
      dither: $('#d-dither').checked, resample: $('#d-resample').checked,
      crossfeed: $('#d-cf').checked ? { level_db: Number($('#d-cf-level').value), cutoff_hz: Number($('#d-cf-cutoff').value), delay_us: Number($('#d-cf-delay').value), enabled: true } : null,
      eq: $$('#eq .eq-row').map((r) => ({ kind: $('.eq-kind', r).value, freq_hz: Number($('.eq-freq', r).value), gain_db: Number($('.eq-gain', r).value), q: Number($('.eq-q', r).value), enabled: $('.eq-on', r).checked })),
    });
    const bindRows = () => $$('#eq .eq-del').forEach((b) => b.onclick = () => { b.closest('.eq-row').remove(); });
    bindRows();
    $('#eq-add').onclick = () => { $('#eq').insertAdjacentHTML('beforeend', eqRow({ kind: 'peaking', freq_hz: 1000, gain_db: 0, q: 1.0, enabled: true }, $$('#eq .eq-row').length)); bindRows(); };
    $('#d-save').onclick = async () => { try { await api('PUT', `/zones/${encodeURIComponent(zoneId)}/dsp`, collect()); toast('DSP saved; applies from the next track'); } catch (e) { toast(e.message, 'error'); } };
    $('#d-reset').onclick = async () => { try { await api('DELETE', `/zones/${encodeURIComponent(zoneId)}/dsp`); toast('DSP reset'); render(); } catch (e) { toast(e.message, 'error'); } };
    $('#d-close').onclick = () => { state.dspOpen = null; render(); };
  };
  // "New releases from your artists": release groups MusicBrainz lists for tagged artists.
  const newReleasesSection = (nr) => {
    if (!nr || !nr.artists_total) return '';
    const sub = nr.refreshing ? `checking ${nr.artists_checked} of ${nr.artists_total} artists…` : `${nr.artists_checked} of ${nr.artists_total} tagged artists checked`;
    const rows = nr.items.slice(0, 12).map((r) => `<div class="list-row"><div class="grow"><div class="name">${esc(r.title)} <span class="muted small">${esc(r.kind)}</span></div><div class="sub">${esc(r.artist)} · ${esc(r.first_release_date)}</div></div>${r.in_library ? `<button class="btn ghost small" data-album="${r.in_library.id}">In library</button>` : `<a class="btn ghost small" href="${esc(r.url)}" target="_blank" rel="noopener">MusicBrainz</a>`}</div>`).join('');
    return `<section class="section"><div class="section-head"><h3>New releases from your artists</h3><span class="muted small">${sub}</span>${nr.refreshing ? '' : `<button class="link" id="nrrefresh">Refresh</button>`}</div>${rows ? `<div class="list">${rows}</div>` : `<p class="muted">Nothing from the last year yet${nr.artists_checked < nr.artists_total ? ' (still checking)' : ''}.</p>`}</section>`;
  };
  const albumGrid = (albums) => `<div class="grid">${albums.map(tile).join('')}</div>`;
  const strip = (title, albums, more) => (!albums || !albums.length) ? '' : `<section class="section"><div class="section-head"><h3>${esc(title)}</h3>${more ? `<button class="link" data-more="${more}">See all</button>` : ''}</div><div class="strip">${albums.map(tile).join('')}</div></section>`;
  const bindTiles = () => {
    $$('[data-tileplay]', main).forEach((p) => p.onclick = async (e) => {
      e.stopPropagation();
      try { const r = await api('GET', `/library/albums/${p.dataset.tileplay}/tracks`); playTracks(r.tracks, true); } catch (er) { toast(er.message, 'error'); }
    });
    $$('.tile[data-album]', main).forEach((el) => el.onclick = () => setView('album', Number(el.dataset.album)));
    $$('[data-more]', main).forEach((b) => b.onclick = () => setView(b.dataset.more));
  };
  // Discover rows: a Pinakes card joined to the library. Play what you own, else the service page the node can stream, else "Play on…".
  const discoverRows = (items) => `<div class="list">${items.map((it, i) => { return `<div class="list-row clickable" data-ditem="${i}">${it.image ? `<img class="thumb" src="${esc(it.image)}" alt="" loading="lazy">` : `<div class="avatar" style="background:hsl(${hue(it.title || it.uri)} 35% 28%)">${esc(initials(it.title || 'M'))}</div>`}<div class="grow"><div class="name">${esc(itemName(it))} ${it.site ? `<span class="src-badge">${esc(it.site.toUpperCase())}</span>` : ''}${it.in_library ? '<span class="pill ok">in library</span>' : ''}</div><div class="sub">${esc([it.artist, it.kind !== 'track' ? it.kind : null, it.group_size ? `on ${it.group_size} services` : null, it.taggers ? `${it.taggers} tagger${it.taggers === 1 ? '' : 's'}` : null].filter(Boolean).join(' · '))}</div></div>${playsChip(it)}<span class="row-actions"><button class="icon-btn" data-dplay="${i}" title="Play">${icon('play')}</button><button class="icon-btn" data-dmore="${i}" title="More">${icon('more')}</button></span></div>`; }).join('')}</div>`;
  // Charts and search as artwork tiles (Roon-style), with the same play/menu actions as the rows.
  // Service marks for badges: simple monochrome glyphs in the spirit of each
  // service's logo (no trademarked artwork copied), a note for the rest.
  const SERVICE_GLYPHS = {
    tidal: '<path d="M4 8l4-4 4 4-4 4zm8 0l4-4 4 4-4 4zm-4 8l4-4 4 4-4 4z"/>',
    spotify: '<path d="M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm4.4 14.4a.7.7 0 0 1-1 .3c-2.6-1.6-5.9-2-9.8-1.1a.7.7 0 0 1-.3-1.4c4.2-1 7.9-.5 10.8 1.3.3.2.4.6.3.9zm1.2-2.8a.9.9 0 0 1-1.2.3c-3-1.8-7.5-2.4-11-1.3a.9.9 0 0 1-.5-1.7c4-1.2 9-.6 12.4 1.5.4.3.5.8.3 1.2zm.1-2.9C14.1 8.6 8.3 8.4 4.9 9.4a1 1 0 0 1-.6-2c3.9-1.2 10.4-1 14.5 1.5a1 1 0 0 1-1.1 1.8z"/>',
    deezer: '<path d="M2 15h4v3H2zm5 0h4v3H7zm5 0h4v3h-4zm5 0h4v3h-4zM7 11h4v3H7zm5 0h4v3h-4zm5 0h4v3h-4zm-5-4h4v3h-4zm5 0h4v3h-4zm0-4h4v3h-4z"/>',
    youtube: '<path d="M21.6 7.2a2.5 2.5 0 0 0-1.8-1.8C18.3 5 12 5 12 5s-6.3 0-7.8.4A2.5 2.5 0 0 0 2.4 7.2 26 26 0 0 0 2 12a26 26 0 0 0 .4 4.8 2.5 2.5 0 0 0 1.8 1.8C5.7 19 12 19 12 19s6.3 0 7.8-.4a2.5 2.5 0 0 0 1.8-1.8A26 26 0 0 0 22 12a26 26 0 0 0-.4-4.8zM10 15V9l5.2 3z"/>',
    soundcloud: '<path d="M2 13h1.5v4H2zm2.5-2H6v6H4.5zm2.5-2h1.5v8H7zm2.5-1H11v9H9.5zm2.5-2h1.5v11H12zm3-1h3a4 4 0 0 1 0 8h-3z"/>',
    bandcamp: '<path d="M2 18l6-12h14l-6 12z"/>',
    amazonmusic: '<path d="M3 13c5 3.5 13 3.5 18 0l-1 2.5c-5 3-11 3-16 0zM19 15l2-1 0 3zM9 4h2v8H9zm4 0h2v8h-2z"/>',
    applemusic: '<path d="M16 3v11.5a3.5 3.5 0 1 1-2-3.2V7.2l-5 1.4v7.9A3.5 3.5 0 1 1 7 13.3V5.3z"/>',
    audius: '<path d="M12 3l9 17h-4.5L12 11l-4.5 9H3z"/>',
    archive: '<path d="M3 20h18v2H3zM4 9h2v9H4zm4 0h2v9H8zm4 0h2v9h-2zm4 0h2v9h-2zm4 0h1v9h-1zM2 7l10-5 10 5v1H2z"/>',
    qobuz: '<path d="M12 3a8 8 0 1 0 4.9 14.3l2.4 2.4 1.4-1.4-2.4-2.4A8 8 0 0 0 12 3zm0 2.5a5.5 5.5 0 1 1 0 11 5.5 5.5 0 0 1 0-11z"/>',
    musicbrainz: '<path d="M12 2l9 5v10l-9 5-9-5V7zm-1 4.5L6 9v6l5 2.5zm2 0v11l5-2.5V9z"/>',
    library: '<path d="M4 4h16v3H4zm0 5h16v3H4zm0 5h10v3H4zm12 0h4v3h-4z"/>',
  };
  const serviceKey = (site) => String(site || '').toLowerCase().replace(/[^a-z]/g, '').replace(/^youtubemusic$/, 'youtube').replace(/^internetarchive$/, 'archive');
  const svcIcon = (site) => { const g = SERVICE_GLYPHS[serviceKey(site)]; return `<svg viewBox="0 0 24 24" aria-hidden="true">${g || '<path d="M9 3v12.3A3.5 3.5 0 1 0 11 18.5V7h6V3z"/>'}</svg>`; };
  const svcBadge = (site, extra = '') => site ? `<span class="src-badge ico${extra ? ' ' + extra : ''}" title="${esc(site)}">${svcIcon(site)}</span>` : '';
  const discoverTiles = (items) => `<div class="grid">${items.map((it, i) => { const h = hue(it.title || it.uri); const playable = true; return `<button class="tile" data-ditem="${i}" title="${esc(itemName(it))}"><div class="art" style="background:linear-gradient(135deg, hsl(${h} 38% 30%), hsl(${(h + 40) % 360} 42% 18%))"><span>${esc(initials(it.title || 'M'))}</span>${it.image ? `<img loading="lazy" alt="" src="${esc(it.image)}" onerror="this.remove()">` : ''}${svcBadge(it.site)}${playable ? `<span class="tile-play" data-dplay="${i}" role="button" title="Play">${icon('play')}</span>` : ''}</div><div class="t">${esc(itemName(it))}${it.in_library ? ' <span class="pill ok">owned</span>' : ''}</div><div class="a">${esc([it.artist, it.group_size ? `${it.group_size} services` : null].filter(Boolean).join(' · '))}</div></button>`; }).join('')}</div>`;
  // A row's name: the title, never a URL ("Untitled on TIDAL" when unknown).
  const itemName = (it) => it.title || `Untitled on ${it.site || 'another service'}`;
  // Describe music URLs that are not in the library (favourites, playlists, history).
  const describe = async (uris) => { if (!uris.length) return []; try { return await api('POST', '/discover/describe', { uris }); } catch { return uris.map((u) => ({ uri: u, page: u, title: null, site: null, also_on: [], kind: 'other' })); } };
  const SERVICE_NAMES = { tidal: 'TIDAL', deezer: 'Deezer', spotify: 'Spotify', library: 'your library' };
  // ---- where playlist items play from (STEWARD-UX-BRIEF §3) ----
  // A row's source chip: where it plays from, a pin mark when the owner chose it.
  const sourceChip = (t, k) => t.pending ? '<span class="chip srcchip wait" aria-label="Finding where this plays"><span class="spin"></span></span>' : t.unavailable
    ? `<button class="chip srcchip none" data-psrc="${k}" title="${esc(t.unavailable)}">${icon('close')}<span>Unavailable</span></button>`
    : t.plays ? `<button class="chip srcchip${t.pinned ? ' pinned' : ''}" data-psrc="${k}" title="${esc(t.plays.label)}${t.pinned ? ' · pinned' : t.pinLost ? ' · pinned source not available here' : ''}">${svcBadge(t.plays.service)}<span>${esc(srcName(t.plays.service))}</span>${t.pinned ? icon('pin') : ''}</button>` : '';
  // The "Play from" order: a short list to drag (or Alt+↑/↓, or the arrow buttons).
  const orderList = (o) => `<ol class="porder" aria-label="Play from, in order">${o.order.map((id, k) => { const sv = o.services.find((x) => x.id === id) || { id, name: id }; return `<li class="porder-row${sv.connected === false ? ' off' : ''}" data-oi="${k}"><span class="porder-grip" data-ogrip="${k}" role="button" tabindex="0" aria-label="Move ${esc(sv.name)} (Alt+arrow keys)" title="Drag, or Alt+↑/↓">${icon('menu')}</span><span class="num muted small">${k + 1}</span>${svcBadge(id)}<span class="grow">${esc(sv.name)}${sv.note ? ` <span class="muted small">· ${esc(sv.note)}</span>` : ''}</span><button class="icon-btn" data-oup="${k}" title="Earlier" ${k === 0 ? 'disabled' : ''}>${icon('chevron', 'ic up')}</button><button class="icon-btn" data-odown="${k}" title="Later" ${k === o.order.length - 1 ? 'disabled' : ''}>${icon('chevron')}</button></li>`; }).join('')}</ol>`;
  const bindOrderList = (root, order, save) => {
    const move = (i, j) => { if (j < 0 || j >= order.length || i === j) return; const o = [...order]; const [x] = o.splice(i, 1); o.splice(j, 0, x); save(o); };
    $$('[data-oup]', root).forEach((b) => b.onclick = () => move(Number(b.dataset.oup), Number(b.dataset.oup) - 1));
    $$('[data-odown]', root).forEach((b) => b.onclick = () => move(Number(b.dataset.odown), Number(b.dataset.odown) + 1));
    $$('[data-ogrip]', root).forEach((g) => {
      const from = Number(g.dataset.ogrip);
      g.onkeydown = (e) => { if (e.altKey && (e.key === 'ArrowUp' || e.key === 'ArrowDown')) { e.preventDefault(); move(from, from + (e.key === 'ArrowUp' ? -1 : 1)); } };
      g.onpointerdown = (e) => {
        if (e.button !== 0) return; e.preventDefault(); g.setPointerCapture(e.pointerId);
        const rows = $$('.porder-row', root); const row = rows[from]; let to = from; const startY = e.clientY;
        row.classList.add('dragging');
        g.onpointermove = (ev) => {
          row.style.transform = `translateY(${ev.clientY - startY}px)`;
          to = from; rows.forEach((r, k) => { if (k === from) return; const b = r.getBoundingClientRect(); const mid = b.top + b.height / 2; if (k < from && ev.clientY < mid) to = Math.min(to, k); if (k > from && ev.clientY > mid) to = Math.max(to, k); });
          rows.forEach((r, k) => { if (k === from) return; const h = row.offsetHeight; r.style.transform = to > from && k > from && k <= to ? `translateY(${-h}px)` : to < from && k >= to && k < from ? `translateY(${h}px)` : ''; });
        };
        const end = (commit) => { g.onpointermove = null; g.onpointerup = null; g.onpointercancel = null; rows.forEach((r) => { r.style.transform = ''; }); row.classList.remove('dragging'); if (commit) move(from, to); };
        g.onpointerup = () => end(true); g.onpointercancel = () => end(false);
      };
    });
  };
  // The "Plays from" chip: the resolver's answer for an item (rows, tiles, the item page).
  const playsChip = (it, long = false) => {
    const pf = it.plays_from;
    if (pf) return `<span class="plays" title="${esc(pf.label)}">${svcIcon(pf.service)}<span>${esc(long ? pf.label : (pf.service === 'library' ? 'Library' : SERVICE_NAMES[pf.service] || pf.service))}</span></span>`;
    if (it.connect && it.connect.length) return `<span class="plays none" title="Not on your services">${icon('zone')}<span>${long ? 'Not on your services · ' : ''}<a data-view="sources">Connect ${esc(it.connect.map((c) => SERVICE_NAMES[c] || c).join(' or '))}</a></span></span>`;
    return '';
  };
  // Play through the node's connected services: the library, else the best service link
  // (`GET /v1/discover/play`); nothing connected offers Sources and a preview.
  // ---- one row, one tile, one menu (docs/ux/08-item-page.md) ----
  const imgSrc = (u) => (u && u.startsWith('/v1/') ? u + tokenQ() : u);
  const qOf = (t, fallback = {}) => ({ uri: t.uri, title: t.title, artist: t.artist || fallback.artist || '', album: t.album || fallback.album || '', duration_ms: t.duration_ms || null, image: imgSrc(t.image || fallback.image) || null });
  // Play a list of item songs from index k: library tracks, streamed items, or Spotify Connect.
  const playSongs = async (songs, k = 0, how = 'replace', pf, anchorEl, fallback) => {
    const first = songs[k];
    if (first && first.track_id == null && !first.uri && first.page) {
      // A song only known by its link: the resolver finds where it plays.
      return discoverPlay({ uri: first.page, page: first.page, title: first.title, artist: first.artist, image: first.image }, null, anchorEl);
    }
    const list = songs.slice(k).filter((t) => t.track_id != null || t.uri);
    if (!list.length) return toast('Nothing here plays on your services', 'error');
    if (pf && pf.mode === 'connect') return spotifyPlay(list.map((t) => t.uri).filter(Boolean), anchorEl);
    if (list.every((t) => t.track_id != null)) { try { const tr = await Promise.all(list.map((t) => api('GET', `/library/tracks/${t.track_id}`))); return how === 'replace' ? playTracks(tr, true) : playTracks(tr, false, how); } catch (e) { return toast(e.message, 'error'); } }
    const items = list.filter((t) => t.uri).map((t) => qOf(t, fallback));
    return how === 'replace' ? playSourceItems(items, true) : playSourceItems(items, false, how);
  };
  const copyLink = async (path) => { const u = location.origin + path; try { await navigator.clipboard.writeText(u); toast('Link copied'); } catch { toast(u); } };
  // The ⋯ menu, the same on a page's hero and on every song row.
  const itemMenu = async (anchorEl, o) => {
    if (!state.playlists.length) { try { state.playlists = await api('GET', '/playlists'); } catch { /* keep */ } }
    const uris = (o.songs || []).map((t) => t.page || (t.id && (uriOf('song', t.id) || null))).filter(Boolean);
    const trackIds = (o.songs || []).filter((t) => t.track_id != null).map((t) => t.track_id);
    const addTo = (pl) => (trackIds.length === (o.songs || []).length && trackIds.length ? addToPlaylist(pl.id, trackIds.map((id) => ({ id })), null) : addToPlaylist(pl.id, [], uris.length ? uris : o.uri ? [o.uri] : []));
    showMenu(anchorEl, [{ title: o.title || 'Item' },
      { label: 'Play next', icon: 'playnext', run: () => playSongs(o.songs || [], 0, 'next', o.plays_from, anchorEl, o.fallback) },
      { label: 'Add to queue', icon: 'plus', run: () => playSongs(o.songs || [], 0, 'end', o.plays_from, anchorEl, o.fallback) },
      ...state.playlists.slice(0, 6).map((pl) => ({ label: `Add to “${pl.name}”`, icon: 'playlist', run: () => addTo(pl) })),
      ...(o.artistId ? [{ label: 'Go to artist', icon: 'artist', run: () => setView('item', { kind: 'artist', id: o.artistId }) }] : o.artistName ? [{ label: `Search “${o.artistName}”`, icon: 'artist', run: () => { search.value = o.artistName; runSearch(); } }] : []),
      ...(o.albumId ? [{ label: 'Go to album', icon: 'disc', run: () => setView('item', { kind: 'album', id: o.albumId }) }] : []),
      ...(o.openOn || []).slice(0, 4).map((v) => ({ label: `Open on ${SERVICE_NAMES[v.source] || v.label.split(' · ')[0]}`, icon: 'external', run: () => window.open(v.url, '_blank', 'noopener') })),
      ...(o.path ? [{ label: 'Copy link', icon: 'compass', run: () => copyLink(o.path) }] : []),
    ]);
  };
  // One song row: title → song page, artist → artist page, album → album page, ⋯ = the item menu.
  const songRows = (songs, opts = {}) => `<div class="list">${songs.map((t, k) => `<div class="list-row clickable srow${opts.rowClass ? ' ' + opts.rowClass(t, k) : ''}" data-srow="${k}"><span class="num muted small">${opts.numbers === 'index' ? k + 1 : (t.no ?? k + 1)}</span><button class="icon-btn" data-splay="${k}" title="Play from here">${icon('play')}</button>${t.image && opts.covers !== false ? `<img class="thumb" src="${esc(imgSrc(t.image))}" alt="" loading="lazy" onerror="this.style.visibility='hidden'">` : ''}<div class="grow"><div class="name">${t.id ? `<a class="lnk" data-ssong="${k}">${esc(t.title)}</a>` : esc(t.title)}</div><div class="sub">${[t.artist ? (t.artist_id ? `<a class="lnk" data-sartist="${k}">${esc(t.artist)}</a>` : `<a class="lnk" data-sartistq="${k}">${esc(t.artist)}</a>`) : '', t.album && opts.showAlbum !== false ? (t.album_id ? `<a class="lnk" data-salbum="${k}">${esc(t.album)}</a>` : esc(t.album)) : ''].filter(Boolean).join(' · ')}</div></div><span class="muted small">${t.duration_ms ? fmt(t.duration_ms) : ''}</span>${opts.extra ? opts.extra(t, k) : ''}<button class="icon-btn" data-smore="${k}" title="More">${icon('more')}</button></div>`).join('')}</div>`;
  const bindSongRows = (box, songs, ctx = {}) => {
    $$('[data-splay]', box).forEach((b) => b.onclick = (e) => { e.stopPropagation(); playSongs(songs, Number(b.dataset.splay), 'replace', ctx.plays_from, b, ctx.fallback); });
    $$('[data-srow]', box).forEach((r) => r.onclick = (e) => { if (e.target.closest('a,button')) return; playSongs(songs, Number(r.dataset.srow), 'replace', ctx.plays_from, r, ctx.fallback); });
    $$('[data-ssong]', box).forEach((a) => a.onclick = (e) => { e.stopPropagation(); setView('item', { kind: 'song', id: songs[Number(a.dataset.ssong)].id }); });
    $$('[data-sartist]', box).forEach((a) => a.onclick = (e) => { e.stopPropagation(); setView('item', { kind: 'artist', id: songs[Number(a.dataset.sartist)].artist_id }); });
    $$('[data-sartistq]', box).forEach((a) => a.onclick = (e) => { e.stopPropagation(); search.value = songs[Number(a.dataset.sartistq)].artist; runSearch(); });
    $$('[data-salbum]', box).forEach((a) => a.onclick = (e) => { e.stopPropagation(); setView('item', { kind: 'album', id: songs[Number(a.dataset.salbum)].album_id }); });
    $$('[data-smore]', box).forEach((b) => b.onclick = (e) => { e.stopPropagation(); const t = songs[Number(b.dataset.smore)]; itemMenu(b, { title: t.title, songs: [t], plays_from: ctx.plays_from, fallback: ctx.fallback, artistId: t.artist_id, artistName: t.artist, albumId: t.album_id, openOn: t.page ? [{ source: (t.uri || '').split(':')[0], url: t.page, label: '' }] : [], path: t.id ? `/song/${t.id}` : null }); });
  };
  // One tile for albums and artists: click opens the item page, the hover Play plays it.
  const itemTileGrid = (tiles, kind) => `<div class="grid">${tiles.map((a, k) => `<button class="tile" data-tile="${k}" title="${esc(a.title)}"><div class="art" style="background:linear-gradient(135deg, hsl(${hue(a.title)} 38% 30%), hsl(${(hue(a.title) + 40) % 360} 42% 18%))"><span>${esc(initials(a.title))}</span>${a.image ? `<img loading="lazy" alt="" src="${esc(imgSrc(a.image))}" onerror="this.remove()">` : ''}<span class="tile-play" data-tileplay="${k}" role="button" title="Play">${icon('play')}</span></div><div class="t">${esc(a.title)}</div><div class="a">${a.year || ''}</div></button>`).join('')}</div>`;
  const bindItemTiles = (box, tiles, kind) => {
    $$('[data-tile]', box).forEach((b) => b.onclick = (e) => { if (e.target.closest('[data-tileplay]')) return; setView('item', { kind, id: tiles[Number(b.dataset.tile)].id }); });
    $$('[data-tileplay]', box).forEach((b) => b.onclick = async (e) => { e.stopPropagation(); try { const pg = await api('GET', `/item/${kind}/${tiles[Number(b.dataset.tileplay)].id}`); playSongs(pg.songs, 0, 'replace', pg.plays_from, b, { artist: pg.artist && pg.artist.name, album: pg.title, image: pg.image }); } catch (er) { toast(er.message, 'error'); } });
  };

  const SOURCE_RANK = ['library', 'tidal', 'deezer', 'spotify', 'archive', 'audius', 'pubky'];
  const SEARCH_KINDS = [['song', 'Songs'], ['album', 'Albums'], ['artist', 'Artists'], ['playlist', 'Playlists'], ['podcast', 'Podcasts'], ['concert', 'Concerts']];
  const srcName = (src) => (src === 'library' ? 'Library' : src === 'pubky' ? 'Pubky' : SERVICE_NAMES[src] || (state.sources || []).find((x) => x.id === src)?.name || src);
  // Merge hits across sources: one row per song (Pinakes group, ISRC, UPC, MBID), best source first.
  const mergeSearch = (run) => {
    const byKind = Object.fromEntries(SEARCH_KINDS.map(([k]) => [k, []]));
    const index = new Map();
    const answers = Object.values(run.answers).filter((x) => x.status === 'ok').sort((x, y) => SOURCE_RANK.indexOf(x.source) - SOURCE_RANK.indexOf(y.source));
    for (const ans of answers) for (const h of ans.hits) {
      if (!byKind[h.kind]) continue;
      const found = (h.keys || []).map((k) => index.get(h.kind + '|' + k)).find(Boolean);
      if (found) { if (!found.sources.includes(ans.source)) found.sources.push(ans.source); for (const k of h.keys || []) index.set(h.kind + '|' + k, found); continue; }
      const m = { ...h, sources: [ans.source] };
      byKind[h.kind].push(m);
      for (const k of h.keys || []) index.set(h.kind + '|' + k, m);
    }
    return byKind;
  };
  const renderSearchBody = (run) => {
    const chips = $('#scopechips'); const box = $('#sresults'); if (!chips || !box) return;
    const scope = run.scope;
    const services = (state.sources || []).filter((x) => !/^not configured/.test(x.status));
    const scopes = [['everywhere', 'Everywhere'], ['library', 'Library'], ['pubky', 'Pubky'], ...services.map((x) => [x.id, x.name])];
    const stat = (src) => { const ans = run.answers[src]; if (ans) return ans.status === 'ok' ? `<span class="cnt">${ans.hits.length}</span>` : `<span class="bad" title="${esc(ans.error || '')}">!</span>`; return run.sources.includes(src) && run.pending ? '<span class="spin"></span>' : ''; };
    chips.innerHTML = scopes.map(([id, name]) => `<button class="chip scope ${scope === id ? 'on' : ''}" data-scope-in="${id}">${id === 'everywhere' || id === 'library' || id === 'pubky' ? '' : svcBadge(id)}${esc(name)} ${scope === 'everywhere' || scope === id ? stat(id === 'everywhere' ? '' : id) : ''}</button>`).join('');
    $$('[data-scope-in]', chips).forEach((b) => b.onclick = () => { state.searchScope = b.dataset.scopeIn; startSearch(run.q, b.dataset.scopeIn); setView('search', { q: run.q, in: b.dataset.scopeIn }, 'replace'); });
    const byKind = mergeSearch(run);
    const problems = run.sources.filter((src) => (run.answers[src] && run.answers[src].status !== 'ok') || (!run.answers[src] && run.slow.has(src)));
    const pendingAny = run.pending && run.sources.some((src) => !run.answers[src]);
    const skel = (n, tiles) => (tiles ? `<div class="sk-grid">${'<div class="skeleton"></div>'.repeat(n)}</div>` : `<div class="skeleton" style="height:${n * 54}px"></div>`);
    const sections = [];
    const counts = {}; for (const ans of Object.values(run.answers)) for (const [k, c] of ans.counts || []) counts[k] = Math.max(counts[k] || 0, c);
    const top = byKind.artist.find((x) => x.sources.includes('library')) || byKind.song.find((x) => x.sources.includes('library')) || byKind.artist[0] || byKind.album[0] || byKind.song[0];
    if (top) sections.push(`<section class="section fadein" id="stop"><div class="section-head"><h3>Top result</h3></div><div class="card toprow"><div class="art ${top.kind === 'artist' ? 'round' : ''}">${top.image ? `<img src="${esc(imgSrc(top.image))}" alt="" onerror="this.remove()">` : ''}</div><div class="grow"><div class="name big"><a class="lnk" data-sopen="top">${esc(top.title)}</a></div><div class="sub">${esc([{ song: 'Song', album: 'Album', artist: 'Artist', playlist: 'Playlist', podcast: 'Podcast', concert: 'Concert' }[top.kind], top.subtitle].filter(Boolean).join(' · '))} <span class="src-badges">${top.sources.map((x) => svcBadge(x === 'library' ? 'library' : x)).join('')}</span></div></div><button class="btn primary" data-splaytop>${icon('play')} Play</button></div></section>`);
    for (const [kind, label] of SEARCH_KINDS) {
      const items = byKind[kind];
      if (!items.length && !pendingAny) continue;
      const head = `<div class="section-head"><h3>${label}</h3><span class="muted small">${items.length ? items.length : ''}${counts[kind] && counts[kind] > items.length ? ` of ${counts[kind]} on Pubky` : ''}</span></div>`;
      let body;
      if (!items.length) body = skel(kind === 'song' ? 4 : 1, kind === 'album' || kind === 'artist');
      else if (kind === 'song') body = songRows(items.slice(0, 12).map((h) => ({ ...h, artist: h.subtitle, id: h.id })), { showAlbum: true, numbers: 'index', extra: (t) => `<span class="src-badges">${t.sources.map((x) => svcBadge(x === 'library' ? 'library' : x)).join('')}</span>` });
      else if (kind === 'album' || kind === 'artist') body = itemTileGrid(items.slice(0, 12).map((h) => ({ ...h, year: h.year || h.subtitle })), kind);
      else body = `<div class="list">${items.slice(0, 10).map((h, k) => `<div class="list-row clickable" data-sother="${kind}:${k}">${svcBadge(h.sources[0])}<div class="grow"><div class="name">${esc(h.title)}</div><div class="sub">${esc(h.subtitle || '')}</div></div>${icon('external')}</div>`).join('')}</div>`;
      sections.push(`<section class="section ${items.length ? 'fadein' : ''}" data-skind="${kind}">${head}${body}</section>`);
    }
    const short = (e) => { const m = String(e || '').replace(/^[a-z]+: /, '').split(/(?<=\.)\s/)[0]; return m.length > 110 ? m.slice(0, 107) + '…' : m; };
    const notes = problems.length ? `<div class="snotes">${problems.map((src) => `<span class="snote" title="${esc((run.answers[src] && run.answers[src].error) || 'still waiting')}">${svcBadge(src)} ${esc(srcName(src))} didn't answer${run.answers[src] && run.answers[src].error ? `: ${esc(short(run.answers[src].error))}` : ''} · <a data-sretry="${src}">Retry</a></span>`).join('')}</div>` : '';
    const nothing = !pendingAny && !SEARCH_KINDS.some(([k]) => byKind[k].length);
    box.innerHTML = notes + (nothing ? emptyState('search', 'Nothing matched', scope === 'everywhere' ? 'Not in your library, your services or on Pubky.' : `Nothing on ${srcName(scope)}. Try Everywhere.`) : sections.join(''));
    // Bind: rows and tiles use the shared components.
    const songsSec = $('[data-skind="song"]', box);
    if (songsSec && byKind.song.length) bindSongRows(songsSec, byKind.song.slice(0, 12).map((h) => ({ ...h, artist: h.subtitle })), {});
    for (const kind of ['album', 'artist']) { const sec = $(`[data-skind="${kind}"]`, box); if (sec && byKind[kind].length) bindItemTiles(sec, byKind[kind].slice(0, 12), kind); }
    $$('[data-sother]', box).forEach((r) => r.onclick = () => { const [k, n] = r.dataset.sother.split(':'); const h = byKind[k][Number(n)]; if (h.page) window.open(h.page, '_blank', 'noopener'); });
    const to = $('[data-sopen="top"]', box); if (to) to.onclick = () => setView('item', { kind: top.kind === 'song' ? 'song' : top.kind, id: top.id });
    const tp = $('[data-splaytop]', box); if (tp) tp.onclick = async (e) => { if (top.kind === 'song') return playSongs([{ ...top, artist: top.subtitle }], 0, 'replace', null, e.currentTarget); try { const pg = await api('GET', `/item/${top.kind}/${top.id}`); playSongs(pg.songs || [], 0, 'replace', pg.plays_from, e.currentTarget, { artist: pg.artist && pg.artist.name, album: pg.title, image: pg.image }); } catch (er) { toast(er.message, 'error'); } };
    $$('[data-sretry]', box).forEach((b) => b.onclick = () => { searchRuns.delete(`${run.q.toLowerCase()}|${run.scope}`); startSearch(run.q, run.scope); render(); });
  };
  // ---- Home (STEWARD-UX-HOME.md): everything Pinakes knows about music, one section per call ----
  const HOME_TITLES = { 'trending-songs': 'Trending songs', 'trending-albums': 'Trending albums', 'trending-artists': 'Trending artists', following: 'From people you follow', tags: 'Tags people use for music', concerts: 'Concerts near you', podcasts: 'Podcasts', yours: 'Yours on Pubky', new: 'New on Pubky' };
  const WINDOWS = [['24h', 'Today'], ['7d', 'This week'], ['30d', 'This month']];
  const homeKind = (it) => (it.kind === 'track' ? 'song' : it.kind);
  // A Discover item as a shared song row or tile.
  const asRow = (it) => ({ title: itemName(it), artist: it.artist, id: idOf(it.page || it.uri), page: it.page || it.uri, uri: it.in_library ? null : it.playable, track_id: it.in_library ? it.in_library.id : null, image: it.image });
  const asTile = (it) => ({ id: idOf(it.page || it.uri), title: itemName(it), image: it.image, year: it.artist || '' });
  const homeBody = (sec, name, full) => {
    if (sec.error) return `<p class="muted small">Pubky did not answer: ${esc(sec.error)}</p>`;
    if (sec.needs === 'pubky') return `<div class="card small">${esc(sec.note)} <button class="btn sm primary" data-home-signin>Sign in with Pubky Ring</button></div>`;
    if (sec.needs === 'location') return `<div class="card small">${esc(sec.note)} <span class="row" style="gap:6px;display:inline-flex"><input class="input" data-city placeholder="Your city" style="width:180px"><button class="btn sm primary" data-city-save>Save</button></span></div>`;
    if (sec.needs === 'contact') return `<p class="muted small">${esc(sec.note)}</p>`;
    if (name === 'tags') return sec.tags.length ? `<div class="chips">${sec.tags.map(([l, c]) => `<button class="chip" data-tag-open="${esc(l)}">#${esc(l)} <span class="muted">${c}</span></button>`).join('')}</div>` : `<p class="muted small">${esc(sec.note || '')}</p>`;
    if (!sec.items.length) return `<p class="muted small">${esc(sec.note || 'Nothing yet.')}</p>`;
    const songs = sec.items.filter((it) => it.kind === 'track');
    const tiles = sec.items.filter((it) => it.kind === 'album' || it.kind === 'artist' || it.kind === 'podcast' || it.kind === 'playlist');
    const events = sec.items.filter((it) => it.kind === 'concert' || it.kind === 'other');
    return (songs.length ? songRows(songs.slice(0, full ? 60 : 6).map(asRow), { numbers: 'index' }) : '')
      + (tiles.length ? itemTileGrid(tiles.slice(0, full ? 60 : 12).map(asTile), 'album') : '')
      + (events.length ? `<div class="list">${events.slice(0, full ? 60 : 6).map((it, k) => `<div class="list-row clickable" data-hopen="${k}"><div class="grow"><div class="name">${esc(itemName(it))}</div><div class="sub">${esc([it.description, it.site].filter(Boolean).join(' · '))}</div></div>${icon('external')}</div>`).join('')}</div>` : '');
  };
  const bindHome = (el, sec) => {
    const songs = sec.items.filter((it) => it.kind === 'track'); const tiles = sec.items.filter((it) => ['album', 'artist', 'podcast', 'playlist'].includes(it.kind)); const events = sec.items.filter((it) => it.kind === 'concert' || it.kind === 'other');
    if (songs.length) bindSongRows(el, songs.map(asRow), {});
    $$('[data-tile]', el).forEach((b) => b.onclick = (e) => { if (e.target.closest('[data-tileplay]')) return; const it = tiles[Number(b.dataset.tile)]; if (it.kind === 'podcast') return window.open(it.page || it.uri, '_blank', 'noopener'); setView('item', { kind: homeKind(it), id: idOf(it.page || it.uri) }); });
    $$('[data-tileplay]', el).forEach((b) => b.onclick = (e) => { e.stopPropagation(); const it = tiles[Number(b.dataset.tileplay)]; discoverPlay(it, null, b); });
    $$('[data-hopen]', el).forEach((r) => r.onclick = () => { const it = events[Number(r.dataset.hopen)]; window.open(it.page || it.uri, '_blank', 'noopener'); });
    $$('[data-tag-open]', el).forEach((b) => b.onclick = () => setView('tag', b.dataset.tagOpen));
    $$('[data-home-signin]', el).forEach((b) => b.onclick = () => setView('settings'));
    const cs = $('[data-city-save]', el); if (cs) cs.onclick = async () => { const city = $('[data-city]', el).value.trim(); if (!city) return; try { const l = await api('PUT', '/home/location', { city }); toast(`Concerts near ${l.label}`); render(); } catch (e) { toast(e.message, 'error'); } };
  };
  const homeCache = new Map(); // section|window → { at, sec }
  const loadHomeSection = async (name, window, limit) => {
    const key = `${name}|${window || ''}|${limit || ''}`; const c = homeCache.get(key);
    if (c && Date.now() - c.at < 60000) return c.sec;
    const sec = await api('GET', `/home/${name}?${new URLSearchParams({ ...(window ? { window } : {}), ...(limit ? { limit } : {}) })}`).catch((e) => ({ section: name, items: [], tags: [], error: e.message }));
    sec.items = sec.items || []; sec.tags = sec.tags || [];
    if (!sec.error) homeCache.set(key, { at: Date.now(), sec });
    return sec;
  };
  const mins = (ms) => { const m = Math.round((ms || 0) / 60000); return m >= 60 ? `${Math.floor(m / 60)} h ${m % 60} min` : `${m} min`; };
  const discoverPlay = async (it, v, anchor) => {
    if (v) return playSourceItems([{ uri: v.uri, title: it.title || '', artist: it.artist || '', album: '', duration_ms: null, image: it.image || null }], true);
    let plan;
    try { plan = await api('GET', `/discover/play?uri=${encodeURIComponent(it.page || it.uri)}`); } catch (e) { return toast(e.message, 'error'); }
    const pf = plan.plays_from;
    if (pf && pf.service === 'library') {
      try { const tracks = await Promise.all(plan.track_ids.map((id) => api('GET', `/library/tracks/${id}`))); return playTracks(tracks, true); } catch (e) { return toast(e.message, 'error'); }
    }
    if (pf && pf.mode === 'connect') return spotifyPlay(plan.spotify_uris, anchor);
    if (pf && plan.items.length) { toast(`Playing from ${pf.label}`); return playSourceItems(plan.items, true); }
    const options = [{ title: 'Not on your services' }, ...(plan.connect || []).map((c) => ({ label: `Connect ${SERVICE_NAMES[c] || c}`, icon: 'zone', run: () => setView('sources') }))];
    if (plan.preview) options.push({ label: `Play a 30 s preview (${SERVICE_NAMES[plan.preview.service] || plan.preview.service})`, icon: 'play', run: () => playSourceItems([{ uri: plan.preview.id.startsWith('track:') ? `${plan.preview.service}:${plan.preview.id}` : plan.preview.id, title: it.title || '', artist: it.artist || '', album: '', duration_ms: null, image: it.image || null }], true) });
    if (anchor) showMenu(anchor, options); else toast('Not on your services: connect one in Sources', 'error');
  };
  const bindDiscoverRows = (items) => {
    $$('[data-ditem]', main).forEach((row) => row.onclick = (e) => { if (e.target.closest('[data-dplay],[data-dmore]')) return; { const it = items[Number(row.dataset.ditem)]; setView('discover-item', { uri: it.uri, kind: it.kind }); } });
    $$('[data-dplay]', main).forEach((b) => b.onclick = (e) => { e.stopPropagation(); discoverPlay(items[Number(b.dataset.dplay)], null, b); });
    $$('[data-view="sources"]', main).forEach((a) => a.onclick = (e) => { e.stopPropagation(); setView('sources'); });
    $$('[data-dmore]', main).forEach((b) => b.onclick = (e) => { e.stopPropagation(); const it = items[Number(b.dataset.dmore)]; showMenu(b, [{ title: it.title || it.uri }, { label: 'Details, tags and notes', icon: 'compass', run: () => setView('discover-item', { uri: it.uri, kind: it.kind }) }, ...(it.in_library ? [{ label: 'Play from library', icon: 'play', run: () => playTracks([it.in_library], true) }, { label: 'Open album', icon: 'disc', run: () => it.in_library.album && it.in_library.album.id ? setView('album', { id: it.in_library.album.id }) : toast('No album for this track') }] : []), ...(it.playable ? [{ label: `Play on ${it.site || 'service'}`, icon: 'zone', run: () => discoverPlay(it) }] : []), ...it.also_on.filter((v) => v.uri).map((v) => ({ label: `Play on ${v.source.toUpperCase()}`, icon: 'zone', run: () => discoverPlay(it, v) })), { label: `Open ${it.site ? 'on ' + it.site : 'link'}`, icon: 'external', run: () => window.open(it.uri, '_blank', 'noopener') }]); });
  };
  const emptyState = (iconName, title, text, action) => `<div class="empty">${icon(iconName)}<h3>${esc(title)}</h3><p>${esc(text)}</p>${action ? `<button class="btn primary" data-empty-action="${action.view}">${esc(action.label)}</button>` : ''}</div>`;
  const pageHead = (title, count, actions = '') => `<div class="page-head"><h2>${esc(title)}${count != null ? `<span class="count">${esc(count)}</span>` : ''}</h2><div class="actions">${actions}</div></div>`;
  // One way back (08-item-page.md): history, labelled with the previous page; on a deep link the page's section.
  const backBtn = (view, label) => `<button class="back" data-back="${view}">${icon('back')} ${esc((navDepth > 0 && history.state && history.state.backLabel) || label)}</button>`;
  const loading = () => { main.innerHTML = `<div class="skeleton" style="height:28px;width:220px"></div><div class="sk-grid">${'<div class="skeleton"></div>'.repeat(8)}</div>`; };

  const loadFavourites = async () => { const f = await api('GET', '/favourites'); state.favUris = new Set(f.uris || []); return f; };
  const setFavourite = async (ref, on) => {
    if (on) await api('PUT', '/favourites', ref);
    else await api('DELETE', '/favourites?' + new URLSearchParams(Object.entries(ref).map(([k, v]) => [k, String(v)])).toString());
    await loadFavourites();
    toast(on ? 'Added to favourites' : 'Removed from favourites');
  };
  // Whether the node holds a Pubky session (menus offer homeserver uploads then).
  const pubkyUser = async () => { if (state.pubkyUser === undefined) { try { const pk = await api('GET', '/pubky'); state.pubkyUser = pk.user || null; state.pubkyStatus = pk; } catch { state.pubkyUser = null; } } return state.pubkyUser; };
  // A session approved for fewer folders than the node uses: one plain line with the way out.
  const RESIGN_TEXT = 'This node needs permission for new folders on your homeserver. Sign in again with Pubky Ring to approve them.';
  const resignNotice = (pk = state.pubkyStatus) => pk && pk.needs_resign ? `<div class="notice warn resign" role="status"><span>${esc(pk.notice || RESIGN_TEXT)}</span><button class="btn sm primary" data-resign>Sign in again</button></div>` : '';
  const bindResign = (root) => $$('[data-resign]', root).forEach((b) => b.onclick = () => { state.autoResign = true; if (state.view === 'settings') render(); else setView('settings'); });
  // A sync's answer as one plain line: the re-sign notice instead of a raw 403.
  const syncToast = (r) => { const resign = (r.errors || []).find((e) => e === RESIGN_TEXT); if (resign) { state.pubkyUser = undefined; return toast(RESIGN_TEXT, 'error'); } toast(`Synced: ${r.pushed} pushed, ${r.pulled} pulled${r.errors.length ? `, ${r.errors.length} errors` : ''}${(r.notes || []).length ? `. ${r.notes.join(' ')}` : ''}`); };
  // The API serves at most 1000 rows per call; follow the `after` cursor to the end (capped at 50k rows).
  const pageAll = async (path, idOf, unwrap = (r) => r) => {
    const out = []; let after = null;
    for (let i = 0; i < 50; i++) {
      const rows = unwrap(await api('GET', `${path}?limit=1000${after != null ? `&after=${after}` : ''}`));
      out.push(...rows);
      if (rows.length < 1000) break;
      after = idOf(rows[rows.length - 1]); if (after == null) break;
    }
    return out;
  };
  // A client-side filter box for long lists: hides rows/tiles whose text does not contain the query.
  const filterBox = (placeholder) => `<input class="input filter" id="filter" type="search" placeholder="${esc(placeholder)}" aria-label="${esc(placeholder)}">`;
  const bindFilter = (selector) => {
    const f = $('#filter'); if (!f) return;
    f.oninput = () => { const q = f.value.trim().toLowerCase(); let n = 0; $$(selector, main).forEach((el) => { const show = !q || el.textContent.toLowerCase().includes(q); el.hidden = !show; if (show) n++; }); const c = $('.page-head .count'); if (c) c.dataset.filtered = q ? `${n} shown` : ''; };
    f.onkeydown = (e) => { if (e.key === 'Escape') { f.value = ''; f.oninput(); f.blur(); } };
  };
  const albumsAll = async () => {
    if (!state.albums) {
      const [albums, formats] = await Promise.all([pageAll('/library/albums', (a) => a.id), api('GET', '/library/albums/formats').catch(() => [])]);
      state.albums = albums; state.formats = new Map(formats.map((f) => [f.album_id, f]));
    }
    return state.albums;
  };
  // Roon-style Focus: which albums have a file of this quality.
  const FOCUS = [['', 'All'], ['hi_res', 'Hi-res'], ['dsd', 'DSD'], ['cd', 'CD quality'], ['lossless', 'Lossless'], ['lossy', 'Lossy']];
  const inFocus = (a, focus) => {
    if (!focus) return true;
    const f = fmtOf(a.id); if (!f) return false;
    switch (focus) { case 'hi_res': return f.hi_res; case 'dsd': return f.dsd; case 'cd': return f.lossless && !f.hi_res && !f.dsd; case 'lossless': return f.lossless; case 'lossy': return !f.lossless; default: return true; }
  };
  const focusChips = (albums) => `<div class="chips" role="tablist" aria-label="Focus">${FOCUS.map(([k, label]) => { const n = albums.filter((a) => inFocus(a, k)).length; return (k && !n) ? '' : `<button class="chip ${state.focus === k ? 'on' : ''}" data-focus="${k}" role="tab" aria-selected="${state.focus === k}">${label}${k ? ` <span class="count">${n}</span>` : ''}</button>`; }).join('')}</div>`;

  // ---- render --------------------------------------------------------------
  const render = async () => {
    const v = state.view;
    const lists = {};
    try {
      switch (v) {
        case 'home': {
          if (!state.sources) { try { state.sources = await api('GET', '/sources'); } catch { state.sources = []; } }
          const win = state.homeWindow || '7d';
          const sec = (name, title, opts = {}) => `<section class="section" id="h-${name}"><div class="section-head"><h3>${title}</h3>${opts.windows ? `<span class="chips win">${WINDOWS.map(([w, l]) => `<button class="chip ${w === win ? 'on' : ''}" data-win="${w}">${l}</button>`).join('')}</span>` : ''}<button class="link" data-seeall="${name}">See all</button></div><div class="hbody"><div class="skeleton" style="height:${opts.h || 160}px"></div></div></section>`;
          await pubkyUser();
          main.innerHTML = pageHead('Home') + resignNotice()
            + `<section class="section" id="h-jump"><div class="section-head"><h3>Jump back in</h3></div><div class="hbody"><div class="skeleton" style="height:150px"></div></div></section>`
            + sec('trending-songs', 'Trending songs', { windows: true, h: 300 }) + sec('trending-albums', 'Trending albums', { windows: true }) + sec('trending-artists', 'Trending artists', { windows: true })
            + sec('following', 'From people you follow', { h: 120 }) + sec('tags', 'Tags people use for music', { h: 60 }) + sec('concerts', 'Concerts near you', { h: 80 })
            + sec('podcasts', 'Podcasts') + sec('yours', 'Yours on Pubky', { h: 120 }) + sec('new', 'New on Pubky', { h: 120 });
          $$('[data-win]', main).forEach((b) => b.onclick = () => { state.homeWindow = b.dataset.win; render(); });
          $$('[data-seeall]', main).forEach((b) => b.onclick = () => setView('home-section', { section: b.dataset.seeall, window: state.homeWindow || '7d' }));
          // Local first: recently played and added.
          api('GET', '/library/home?limit=12').then((h) => {
            const el = $('#h-jump .hbody'); if (!el || state.view !== 'home') return;
            const albums = [...(h.recently_played || []), ...(h.recently_added || [])].filter((a, k, all) => a.id != null && all.findIndex((x) => x.id === a.id) === k).slice(0, 12);
            el.innerHTML = albums.length ? itemTileGrid(albums.map((a) => ({ id: 'lib-' + a.id, title: a.title, image: `/v1/library/albums/${a.id}/cover`, year: a.album_artist || a.year || '' })), 'album') : '<p class="muted small">Play or add some music and it shows up here.</p>';
            el.parentElement.classList.add('fadein');
            if (albums.length) bindItemTiles(el, albums.map((a) => ({ id: 'lib-' + a.id, title: a.title })), 'album');
          }).catch(() => { const el = $('#h-jump .hbody'); if (el) el.innerHTML = '<p class="muted small">The library did not answer.</p>'; });
          // Every Pubky section in parallel, each rendered as it arrives.
          for (const name of ['trending-songs', 'trending-albums', 'trending-artists', 'following', 'tags', 'concerts', 'podcasts', 'yours', 'new']) {
            loadHomeSection(name, name.startsWith('trending') ? win : null).then((sc) => {
              const el = $(`#h-${name} .hbody`); if (!el || state.view !== 'home') return;
              el.innerHTML = homeBody(sc, name, false); el.parentElement.classList.add('fadein'); bindHome(el, sc);
              const see = $(`#h-${name} [data-seeall]`); if (see) see.hidden = !(sc.items.length > 6 || sc.tags.length > 20);
            });
          }
          break;
        }
        case 'home-section': {
          const { section, window } = state.viewArg || {};
          main.innerHTML = backBtn('home', 'Home') + pageHead(HOME_TITLES[section] || 'Home') + (section && section.startsWith('trending') ? `<div class="chips win">${WINDOWS.map(([w, l]) => `<button class="chip ${w === (window || '7d') ? 'on' : ''}" data-win="${w}">${l}</button>`).join('')}</div>` : '') + '<div id="hsfull"><div class="skeleton" style="height:400px"></div></div>';
          $('[data-back]', main).onclick = () => goBack('home');
          $$('[data-win]', main).forEach((b) => b.onclick = () => setView('home-section', { section, window: b.dataset.win }, 'replace'));
          const sc = await loadHomeSection(section, section.startsWith('trending') ? window || '7d' : null, 60);
          if (state.view !== 'home-section') return;
          const el = $('#hsfull'); el.innerHTML = homeBody(sc, section, true); el.classList.add('fadein'); bindHome(el, sc);
          break;
        }
        case 'tag': {
          const labels = String(state.viewArg || '').split(',').filter(Boolean);
          main.innerHTML = backBtn('home', 'Home') + pageHead(labels.map((l) => '#' + l).join(' + ')) + '<div id="tagcotags"></div><div id="tagbody"><div class="skeleton" style="height:400px"></div></div>';
          $('[data-back]', main).onclick = () => goBack('home');
          const sc = await api('GET', `/home/tag?${new URLSearchParams({ labels: labels.join(','), limit: 60 })}`).catch((e) => ({ items: [], tags: [], error: e.message }));
          if (state.view !== 'tag') return;
          sc.items = sc.items || []; sc.tags = sc.tags || [];
          const co = sc.tags.filter(([l]) => !labels.includes(l)).slice(0, 24);
          $('#tagcotags').innerHTML = co.length ? `<div class="chips" style="margin-bottom:14px"><span class="muted small">Also tagged:</span>${co.map(([l, c]) => `<button class="chip" data-tag-add="${esc(l)}" title="Narrow to both tags">+ #${esc(l)} <span class="muted">${c}</span></button>`).join('')}${labels.length > 1 ? labels.map((l) => `<button class="chip on" data-tag-rm="${esc(l)}" title="Remove this tag">#${esc(l)} ×</button>`).join('') : ''}</div>` : '';
          $$('[data-tag-add]', main).forEach((b) => b.onclick = () => setView('tag', [...labels, b.dataset.tagAdd].join(',')));
          $$('[data-tag-rm]', main).forEach((b) => b.onclick = () => setView('tag', labels.filter((l) => l !== b.dataset.tagRm).join(','), 'replace'));
          const el = $('#tagbody'); el.innerHTML = homeBody(sc, 'tag', true); el.classList.add('fadein'); bindHome(el, sc);
          break;
        }
        case 'favourites': {
          loading();
          const f = await loadFavourites();
          if (state.view !== 'favourites') return;
          const none = !f.albums.length && !f.tracks.length && !f.artists.length && !f.other.length;
          main.innerHTML = pageHead('Favourites', `${f.albums.length} albums · ${f.tracks.length} tracks · ${f.artists.length} artists`, f.tracks.length ? `<button class="btn primary" id="favplay">${icon('play')} Play tracks</button>` : '')
            + (none ? emptyState('heart', 'Nothing hearted yet', 'Press the heart on a track, album or artist. Hearts are v1 Pubky bookmarks kept in your private homeserver folder.') : '')
            + (f.albums.length ? `<section class="section"><div class="section-head"><h3>Albums</h3></div>${albumGrid(f.albums)}</section>` : '')
            + (f.artists.length ? `<section class="section"><div class="section-head"><h3>Artists</h3></div><div class="chips">${f.artists.map((a) => `<button class="chip" data-artist="${a.id}">${esc(a.name)}</button>`).join('')}</div></section>` : '')
            + (f.tracks.length ? `<section class="section"><div class="section-head"><h3>Tracks</h3></div>${trackRows(f.tracks, { key: 'fav' })}</section>` : '')
            + (f.other.length ? `<section class="section"><div class="section-head"><h3>From other services</h3></div><div id="favother"><div class="skeleton" style="height:56px"></div></div></section>` : '');
          lists.fav = f.tracks;
          const fp = $('#favplay'); if (fp) fp.onclick = () => playTracks(f.tracks, true);
          if (f.other.length) { const items = await describe(f.other); const box = $('#favother'); if (box && state.view === 'favourites') { box.innerHTML = discoverRows(items); bindDiscoverRows(items); } }
          break;
        }
        case 'albums': {
          loading();
          const albums = await albumsAll();
          if (state.view !== 'albums') return;
          const shown = albums.filter((a) => inFocus(a, state.focus));
          main.innerHTML = pageHead('Albums', state.focus ? `${shown.length} of ${albums.length}` : albums.length, albums.length > 12 ? filterBox('Filter albums') : '') + (albums.length ? focusChips(albums) : '') + (shown.length ? albumGrid(shown) : albums.length ? emptyState('disc', 'Nothing in this focus', 'No album has a file of that quality.') : emptyState('disc', 'No albums yet', 'Scan a folder from Settings to fill the library.', { view: 'settings', label: 'Open settings' }));
          $$('[data-focus]', main).forEach((b) => b.onclick = () => { state.focus = b.dataset.focus; render(); });
          bindFilter('.grid .tile');
          break;
        }
        case 'artists': {
          loading();
          state.artists = await api('GET', '/library/artists');
          if (state.view !== 'artists') return;
          main.innerHTML = pageHead('Artists', state.artists.length, state.artists.length > 12 ? filterBox('Filter artists') : '') + (state.artists.length ? `<div class="list">${state.artists.map((a) => `<div class="list-row clickable" data-artist="${a.id}"><div class="avatar" style="background:hsl(${hue(a.name)} 35% 28%)">${esc(initials(a.name))}</div><div class="grow"><div class="name">${esc(a.name)}</div><div class="sub">${a.albums} album${a.albums === 1 ? '' : 's'} · ${a.tracks} track${a.tracks === 1 ? '' : 's'}</div></div>${icon('chevron', 'ic chev')}</div>`).join('')}</div>` : emptyState('artist', 'No artists yet', 'Artists come from the tags of scanned files.'));
          bindFilter('.list .list-row');
          break;
        }
        case 'identify': {
          const sug = await api('GET', '/library/suggestions');
          if (state.view !== 'identify') return;
          const byTrack = new Map(); for (const x of sug) { if (!byTrack.has(x.track_id)) byTrack.set(x.track_id, { track: x.track, options: [] }); byTrack.get(x.track_id).options.push(x); }
          const byFolder = new Map(); for (const g of byTrack.values()) { const f = (g.track.file.path || '').replace(/[^/]*$/, ''); if (!byFolder.has(f)) byFolder.set(f, []); byFolder.get(f).push(g); }
          // The release most suggestions in a folder point at (the folder as an album).
          const releaseOf = (groups) => { const n = new Map(); groups.forEach((g) => g.options.forEach((o) => { if (o.release_mbid) n.set(o.release_mbid, (n.get(o.release_mbid) || 0) + 1); })); let best = null; n.forEach((c, r) => { if (!best || c > best[1]) best = [r, c]; }); return best && best[0]; };
          main.innerHTML = pageHead('Confirm matches', byTrack.size) + (byTrack.size ? `<p class="muted small" style="margin-top:-6px">Each file on the left; what MusicBrainz knows on the right. A confirmed match becomes the file's identity (stored in the node, your files stay as they are).</p>` + [...byFolder.entries()].map(([folder, groups]) => `<section class="section" data-folder="${esc(folder)}"><div class="section-head"><h3>${esc(folder.split('/').filter(Boolean).slice(-2).join(' / ') || 'Library')}</h3><span class="muted small">${groups.length} file${groups.length === 1 ? '' : 's'}</span>${releaseOf(groups) ? `<button class="btn sm" data-align="${esc(folder)}">Compare with the album</button>` : ''}</div><div class="alignbox" hidden></div><div class="list">${groups.map((g) => `<div class="idrow"><div class="idfile"><div class="name">${g.track.track_no ? `<span class="muted">${g.track.track_no}.</span> ` : ''}${esc(g.track.title)}</div><div class="sub">${esc([artists(g.track), fmt(g.track.duration_ms), (g.track.file.path || '').split('/').pop()].filter(Boolean).join(' · '))}</div></div><div class="idopts">${g.options.map((o) => `<div class="idopt"><div class="grow"><div class="name">${esc(o.title || 'Recording')} <span class="muted small">${esc(o.artist || '')}</span></div><div class="sub">${o.release_title ? esc(o.release_title) + ' · ' : ''}${o.source === 'fingerprint' ? 'audio fingerprint' : 'search by tags'} · ${Math.round(o.score * 100)}% · <a href="https://musicbrainz.org/recording/${esc(o.recording_mbid)}" target="_blank" rel="noopener">MusicBrainz ${icon('external')}</a></div></div><button class="btn primary sm" data-accept="${o.id}">This one</button><button class="btn ghost sm" data-reject="${o.id}">Not this</button></div>`).join('')}</div></div>`).join('')}</div></section>`).join('') : emptyState('search', 'Nothing to confirm', 'Unsure matches from Identify library show up here.', { view: 'settings', label: 'Open Settings' }));
          // Side by side: the folder's files and the MusicBrainz release's tracklist, aligned by position.
          $$('[data-align]', main).forEach((b) => b.onclick = async () => {
            const sec = b.closest('[data-folder]'); const box = $('.alignbox', sec); const groups = byFolder.get(b.dataset.align) || [];
            if (!box.hidden) { box.hidden = true; return; }
            const rel = releaseOf(groups); box.hidden = false; box.innerHTML = '<p class="muted small">Loading the album from MusicBrainz…</p>';
            let r; try { const res = await fetch(`/mb/ws/2/release/${encodeURIComponent(rel)}?inc=recordings+artist-credits&fmt=json`); if (!res.ok) throw new Error(`MusicBrainz answered ${res.status}`); r = await res.json(); } catch (e) { box.innerHTML = `<div class="notice">${esc(e.message)}</div>`; return; }
            const tracks = (r.media || []).flatMap((m) => (m.tracks || []).map((t) => ({ disc: m.position || 1, pos: t.position, title: t.title, len: t.length, rec: t.recording && t.recording.id })));
            const fileAt = (disc, pos) => groups.find((g) => (g.track.track_no === pos) && ((g.track.disc_no || 1) === disc));
            const matches = [];
            const rows = tracks.map((t) => { const g = fileAt(t.disc, t.pos); const o = g && g.options.find((x) => x.recording_mbid === t.rec); if (o) matches.push(o.id); const dur = g && g.track.duration_ms && t.len ? Math.abs(g.track.duration_ms - t.len) <= 3000 : null; return `<div class="alignrow ${o ? 'ok' : g ? 'diff' : 'none'}"><span class="muted small">${t.disc > 1 ? t.disc + '.' : ''}${t.pos}</span><div class="aside">${g ? `${esc(g.track.title)} <span class="muted small">${fmt(g.track.duration_ms)}</span>` : '<span class="muted small">no file at this position</span>'}</div><span class="amark">${o ? icon('check') : g ? '≠' : ''}</span><div class="aside">${esc(t.title)} <span class="muted small">${t.len ? fmt(t.len) : ''}${dur === false ? ' · length differs' : ''}</span></div></div>`; }).join('');
            box.innerHTML = `<div class="alignhead"><span></span><b>Your folder</b><span></span><b>${esc(r.title)} <span class="muted small">${esc((r['artist-credit'] || []).map((a) => a.name).join(', '))} · ${esc(r.date || '')} · ${tracks.length} tracks</span></b></div>${rows}<div class="actions" style="margin-top:10px">${matches.length ? `<button class="btn primary sm" data-acceptall="${matches.join(',')}">Accept all ${matches.length} matching</button>` : ''}<a class="btn ghost sm" href="https://musicbrainz.org/release/${esc(rel)}" target="_blank" rel="noopener">On MusicBrainz ${icon('external')}</a></div>`;
            const all = $('[data-acceptall]', box); if (all) all.onclick = async () => { all.disabled = true; let ok = 0; for (const id of all.dataset.acceptall.split(',')) { try { await api('POST', `/library/suggestions/${id}/accept`); ok++; } catch { /* next */ } } toast(`${ok} file${ok === 1 ? '' : 's'} identified`); render(); };
          });
          $$('[data-accept]', main).forEach((b) => b.onclick = async () => { try { await api('POST', `/library/suggestions/${b.dataset.accept}/accept`); toast('Identified'); render(); } catch (e) { toast(e.message, 'error'); } });
          $$('[data-reject]', main).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/library/suggestions/${b.dataset.reject}`); render(); } catch (e) { toast(e.message, 'error'); } });
          break;
        }
        case 'genres': {
          loading();
          const genres = await api('GET', '/library/genres');
          if (state.view !== 'genres') return;
          main.innerHTML = pageHead('Genres', genres.length) + (genres.length ? `<div class="cards">${genres.map((g) => `<button class="card genre" data-genre="${g.id}" style="text-align:left;cursor:pointer;background:linear-gradient(135deg, hsl(${hue(g.name)} 32% 22%), var(--surface))"><h3 style="margin-bottom:4px">${esc(g.name)}</h3><div class="muted small">${g.albums} album${g.albums === 1 ? '' : 's'} · ${g.tracks} track${g.tracks === 1 ? '' : 's'}</div></button>`).join('')}</div>` : emptyState('genre', 'No genres yet', 'Genres come from the files\' tags; tag your music and rescan.'));
          $$('[data-genre]', main).forEach((b) => b.onclick = () => setView('genre', Number(b.dataset.genre)));
          break;
        }
        case 'genre': {
          const d = await api('GET', `/library/genres/${state.viewArg}`);
          if (state.view !== 'genre') return;
          main.innerHTML = backBtn('genres', 'Genres') + `<div class="page-head"><div><div class="eyebrow">Genre</div><h1>${esc(d.genre.name)}</h1><div class="muted">${d.genre.albums} albums · ${d.genre.tracks} tracks</div></div><div class="actions"><button class="btn primary" id="gplay">${icon('play')} Play all</button></div></div>`
            + (d.albums.length ? `<section class="section"><div class="section-head"><h3>Albums</h3></div>${albumGrid(d.albums)}</section>` : '')
            + `<section class="section"><div class="section-head"><h3>Tracks</h3></div>${trackRows(d.tracks, { key: 'genre' })}</section>`;
          lists.genre = d.tracks;
          $('#gplay').onclick = () => playTracks(d.tracks, true);
          break;
        }
        case 'tracks': {
          loading();
          const tracks = await pageAll('/library/tracks', (t) => t.id, (r) => r.tracks);
          if (state.view !== 'tracks') return;
          // Big libraries: render 500 rows at a time; the filter searches all of them.
          const PAGE = 500;
          const matches = (t, q) => !q || [t.title, artists(t), t.album && t.album.title].filter(Boolean).some((x) => x.toLowerCase().includes(q));
          main.innerHTML = pageHead('Tracks', tracks.length, (tracks.length > 12 ? filterBox('Filter tracks') : '') + (tracks.length ? `<button class="btn" id="shuffle">Play all</button>` : '')) + (tracks.length ? `<div id="tracklist"></div>` : emptyState('notes', 'No tracks yet', 'Scan a folder from Settings to fill the library.', { view: 'settings', label: 'Open settings' }));
          const renderList = (subset, all) => {
            const box = $('#tracklist'); if (!box) return;
            const shown = all ? subset : subset.slice(0, PAGE);
            lists.all = shown;
            box.innerHTML = trackRows(shown, { key: 'all' }) + (shown.length < subset.length ? `<div class="actions" style="margin-top:12px"><button class="btn" id="showall">Show all ${subset.length}</button><span class="muted small">${shown.length} of ${subset.length} shown</span></div>` : '');
            const sa = $('#showall'); if (sa) sa.onclick = () => renderList(subset, true);
          };
          renderList(tracks, false);
          const f = $('#filter'); if (f) { f.oninput = () => { const q = f.value.trim().toLowerCase(); renderList(q ? tracks.filter((t) => matches(t, q)) : tracks, false); }; f.onkeydown = (e) => { if (e.key === 'Escape') { f.value = ''; f.oninput(); f.blur(); } }; }
          const sh = $('#shuffle'); if (sh) sh.onclick = () => playTracks(tracks, true);
          break;
        }
        case 'search': {
          const a = state.viewArg || {};
          if (!a.q) { main.innerHTML = emptyState('search', 'Search', 'Type in the box above: your library, your services and Pubky at once. Paste a link to open it.'); break; }
          const scope = a.in || 'everywhere'; state.searchScope = scope;
          if (search.value.trim() !== a.q) search.value = a.q;
          const run = startSearch(a.q, scope);
          main.innerHTML = `<div class="page-head"><h2>Results for “${esc(a.q)}”</h2></div><div class="scopechips" id="scopechips"></div><div id="sresults"></div>`;
          renderSearchBody(run);
          break;
        }
        case 'playlists': {
          state.playlists = await api('GET', '/playlists');
          if (state.view !== 'playlists') return;
          main.innerHTML = pageHead('Playlists', state.playlists.length) + `
            <div class="toolbar"><input class="input" id="plname" placeholder="New playlist name"><button class="btn" id="plcreate">${icon('plus')} Create</button><label class="btn file-btn">${icon('upload')} Import M3U, CSV or Roon export<input type="file" id="plimport" accept=".m3u,.m3u8,.csv,.xlsx"></label></div>`
            + (state.playlists.length ? `<div class="list">${state.playlists.map((p) => `<div class="list-row clickable" data-pl="${esc(p.id)}"><div class="avatar" style="background:hsl(${hue(p.name)} 35% 28%)">${icon('playlist')}</div><div class="grow"><div class="name">${esc(p.name)}</div><div class="sub">${p.items} item${p.items === 1 ? '' : 's'}${p.description ? ' · ' + esc(p.description) : ''}</div></div><span class="pill ${p.synced ? 'ok' : ''}">${p.synced ? 'synced' : 'local'}</span><button class="icon-btn" data-plplay="${esc(p.id)}" title="Play">${icon('play')}</button></div>`).join('')}</div>` : emptyState('playlist', 'No playlists yet', 'Create one, import a file from another player, or add tracks from any list.'));
          $('#plcreate').onclick = async () => { const n = $('#plname').value.trim(); if (!n) { $('#plname').focus(); return; } try { await api('POST', '/playlists', { name: n }); toast(`Created “${n}”`); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#plname').onkeydown = (e) => { if (e.key === 'Enter') $('#plcreate').click(); };
          $('#plimport').onchange = async (e) => {
            const f = e.target.files[0]; if (!f) return;
            try {
              const r = await fetch('/v1/playlists/import/file?file=' + encodeURIComponent(f.name), { method: 'POST', headers: { 'content-type': 'application/octet-stream', ...(state.token ? { authorization: 'Bearer ' + state.token } : {}) }, body: await f.arrayBuffer() });
              const j = await r.json(); if (!r.ok) throw new Error(j.error || r.status);
              toast(`Imported “${j.name}”: ${j.imported} of ${j.parsed} matched${j.unresolved.length ? ` · unmatched: ${j.unresolved.slice(0, 4).map((d) => d.title).join(', ')}` : ''}`);
              render();
            } catch (er) { toast(er.message, 'error'); }
          };
          $$('[data-pl]', main).forEach((el) => el.onclick = (e) => { if (e.target.closest('[data-plplay]')) return; setView('playlist', el.dataset.pl); });
          $$('[data-plplay]', main).forEach((b) => b.onclick = () => playPlaylist(b.dataset.plplay));
          break;
        }
        case 'playlist': {
          loading();
          const plId = state.viewArg;
          // Where songs play from can take a while the first time (each service asked by ISRC):
          // the songs show at once, the chips follow.
          const cachedPf = state.plPf && state.plPf.id === plId && Date.now() - state.plPf.at < 30000 ? state.plPf.data : null;
          const pfP = cachedPf ? Promise.resolve(cachedPf) : api('GET', `/playlists/${encodeURIComponent(plId)}/play-from`).then((x) => { state.plPf = { id: plId, at: Date.now(), data: x }; return x; }).catch(() => null);
          const d = await api('GET', `/playlists/${encodeURIComponent(plId)}`);
          let pf = cachedPf || await Promise.race([pfP, new Promise((r) => setTimeout(() => r(undefined), 1200))]);
          if (state.view !== 'playlist') return;
          const pfPending = pf === undefined;
          if (pfPending) { pf = null; pfP.then((x) => { if (x && state.view === 'playlist' && state.viewArg === plId) render(); }); }
          state.itemTitle = d.summary.name;
          // Each song as it plays here: the library file, a service, or unavailable (play_from.rs).
          const songs = d.playlist.items.map((it, k) => {
            const t = d.tracks[k]; const r = pf && pf.items[k];
            const base = r ? { plays: r.plays_from, pinned: !!(r.pin && !r.pin_unavailable), pinLost: !!r.pin_unavailable, unavailable: r.plays_from ? null : (r.reason || 'Not on your connected services'), sources: r.sources, connect: r.connect } : {};
            const lib = r && r.plays_from && r.plays_from.service === 'library';
            if (t && pfPending) return { pending: true, no: k + 1, title: t.title, artist: artists(t), duration_ms: t.duration_ms, track_id: t.id, id: trackPageId(t) };
            if (t && (!r || lib)) return { ...base, no: k + 1, title: t.title, artist: artists(t), artist_id: t.artists && t.artists[0] && t.artists[0].id != null ? 'lib-' + t.artists[0].id : null, album: t.album && t.album.title, album_id: t.album && t.album.id != null ? 'lib-' + t.album.id : null, duration_ms: t.duration_ms, track_id: t.id, id: trackPageId(t) };
            if (r) return { ...base, no: k + 1, title: itemName({ title: r.title, site: r.site }), artist: r.artist, image: r.image, duration_ms: r.duration_ms, track_id: lib ? r.track_id : null, uri: r.stream_uri || null, page: r.page || it.uri, id: idOf(it.uri) };
            return { no: k + 1, title: pfPending ? 'Loading…' : itemName({ title: null, site: null }), pending: pfPending, page: it.uri, id: idOf(it.uri) };
          });
          const total = songs.reduce((a, t) => a + (t.duration_ms || 0), 0);
          const empty = (t) => `<p class="muted small">${t}</p>`;
          // Summary bar: "12 songs · 8 Library · 3 TIDAL · 1 unavailable", each part a filter.
          const counts = new Map(); songs.forEach((t) => { const key = t.unavailable ? 'none' : t.plays ? t.plays.service : null; if (key) counts.set(key, (counts.get(key) || 0) + 1); });
          const filt = state.plFilter && state.plFilter.id === plId ? state.plFilter.key : null;
          const summaryBar = pf && songs.length ? `<div class="chips plsummary" role="group" aria-label="Filter by source"><button class="chip ${filt ? '' : 'on'}" data-pfilter="" aria-pressed="${!filt}">${songs.length} song${songs.length === 1 ? '' : 's'}</button>${[...counts.entries()].sort((a, b) => (a[0] === 'none') - (b[0] === 'none') || b[1] - a[1]).map(([key, n]) => `<button class="chip ${filt === key ? 'on' : ''} ${key === 'none' ? 'warnchip' : ''}" data-pfilter="${esc(key)}" aria-pressed="${filt === key}">${key === 'none' ? '' : svcBadge(key)}${n} ${key === 'none' ? 'unavailable' : esc(srcName(key))}</button>`).join('')}</div>` : '';
          const ownOrder = pf && pf.own_order;
          const orderText = pf ? pf.order.order.map(srcName).join(' › ') : '';
          main.innerHTML = backBtn('playlists', 'Playlists') + `<div class="hero"><div class="art placeholder" style="background:hsl(${hue(d.summary.name)} 35% 28%);border-radius:12px"></div><div><div class="eyebrow">Playlist · ${esc(d.summary.visibility)} · ${d.summary.synced ? 'synced to your homeserver' : 'on this node'}</div><h1>${esc(d.summary.name)}</h1><p class="sub">${songs.length} song${songs.length === 1 ? '' : 's'}${total ? ' · ' + mins(total) : ''}</p>
              <div class="actions" style="margin-top:12px"><button class="btn primary" id="plplay">${icon('play')} Play</button><button class="btn" id="plshuffle">${icon('shuffle')} Shuffle</button><button class="btn" id="plzone">${icon('zone')} Play on…</button><button class="btn ghost" id="plmore" title="More">${icon('more')}</button><button class="btn danger" id="pldel">${icon('trash')} Delete</button></div></div></div>`
            + `<section class="section" id="plsongs"><div class="section-head"><h3>Songs</h3>${pf ? `<button class="link" id="plorderopen" title="Change where songs play from">Play from: ${esc(orderText)}</button>` : ''}</div>${summaryBar}${songs.length ? songRows(songs, { numbers: 'index', rowClass: (t) => `${t.unavailable ? 'unavail' : ''}${filt && (t.unavailable ? 'none' : t.plays && t.plays.service) !== filt ? ' filtered' : ''}`, extra: (t, k) => `${sourceChip(t, k)}<span class="row-actions q-actions"><button class="icon-btn" data-pup="${k}" title="Move up" ${k === 0 ? 'disabled' : ''}>${icon('chevron', 'ic up')}</button><button class="icon-btn" data-pdown="${k}" title="Move down" ${k === songs.length - 1 ? 'disabled' : ''}>${icon('chevron')}</button><button class="icon-btn" data-rm="${k}" title="Remove">${icon('close')}</button></span>` }) : emptyState('playlist', 'This playlist is empty', 'Add songs from any ⋯ menu, or import a file.')}</section>`
            + (pf ? `<section class="section" id="plorder"><div class="section-head"><h3>Play from</h3><span class="muted small">${ownOrder ? 'this playlist’s own order' : 'your node’s order (Settings)'}</span></div><p class="muted small">Each song plays from the first source here that has it. A pinned song keeps its own source.</p>${orderList(pf.order)}<label class="check small" style="margin-top:10px"><input type="checkbox" id="plownorder" ${ownOrder ? '' : 'checked'}> Use my node’s order</label></section>` : '')
            + `<section class="section"><div class="section-head"><h3>On Pubky</h3></div>${empty(d.summary.synced ? 'Saved privately on your homeserver; not shared yet.' : 'Not shared; it lives on this node.')}</section>`
            + `<section class="section"><div class="section-head"><h3>Yours</h3></div>${d.playlist.description ? `<p>${esc(d.playlist.description)}</p>` : empty('No description.')}</section>`
            + `<section class="section"><div class="section-head"><h3>About</h3></div><dl class="kv"><dt>Songs</dt><dd>${songs.length} · ${d.tracks.filter(Boolean).length} in this library</dd><dt>Length</dt><dd>${total ? mins(total) : '—'}</dd><dt>Stored</dt><dd>${d.summary.synced ? 'your homeserver (private)' : 'this node'}</dd></dl></section>`;
          $('[data-back]', main).onclick = () => goBack('playlists');
          bindSongRows($('#plsongs'), songs, {});
          const playable = (list) => list.filter((t) => !t.unavailable);
          const playAll = (shuffle, anchorEl) => { const list = playable(songs); return playSongs(shuffle ? [...list].sort(() => Math.random() - 0.5) : list, 0, 'replace', null, anchorEl); };
          $('#plplay').onclick = (e) => (isBrowser() ? playAll(false, e.currentTarget) : playPlaylist(d.summary.id));
          $('#plshuffle').onclick = (e) => playAll(true, e.currentTarget);
          $('#plzone').onclick = (e) => showMenu(e.currentTarget, [{ title: 'Play on' }, ...zoneList().map((z) => ({ label: z.name, icon: 'zone', run: () => { state.zone = z.id; renderZoneName(); renderBar(); if (isBrowser()) playAll(false); else playPlaylist(d.summary.id); } }))]);
          $('#plmore').onclick = (e) => itemMenu(e.currentTarget, { title: d.summary.name, songs, path: `/playlist/${encodeURIComponent(d.summary.id)}` });
          $('#pldel').onclick = async () => { if (!$('#pldel').dataset.armed) { $('#pldel').dataset.armed = '1'; $('#pldel').innerHTML = `${icon('trash')} Really delete?`; setTimeout(() => { if ($('#pldel')) { delete $('#pldel').dataset.armed; $('#pldel').innerHTML = `${icon('trash')} Delete`; } }, 3000); return; } try { await api('DELETE', `/playlists/${d.summary.id}`); toast('Playlist deleted'); setView('playlists'); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-rm]', main).forEach((b) => b.onclick = async (e) => { e.stopPropagation(); try { await api('DELETE', `/playlists/${d.summary.id}/items/${b.dataset.rm}`); state.plPf = null; render(); } catch (er) { toast(er.message, 'error'); } });
          const pmove = async (i2, j2) => { const uris = d.playlist.items.map((it) => it.uri); if (j2 < 0 || j2 >= uris.length) return; const [u] = uris.splice(i2, 1); uris.splice(j2, 0, u); try { await api('PUT', `/playlists/${d.summary.id}`, { uris }); state.plPf = null; render(); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-pup]', main).forEach((b) => b.onclick = (e) => { e.stopPropagation(); pmove(Number(b.dataset.pup), Number(b.dataset.pup) - 1); });
          $$('[data-pdown]', main).forEach((b) => b.onclick = (e) => { e.stopPropagation(); pmove(Number(b.dataset.pdown), Number(b.dataset.pdown) + 1); });
          // Source chip → every source this song has here, plus Auto; picking one pins it.
          const pin = async (k, body, msg) => { try { await api('PUT', `/playlists/${d.summary.id}/items/${k}/source`, body); state.plPf = null; toast(msg); render(); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-psrc]', main).forEach((b) => b.onclick = (e) => {
            e.stopPropagation(); const k = Number(b.dataset.psrc); const t = songs[k]; const pinnedNow = t.pinned || t.pinLost;
            const opts = (t.sources || []).map((x) => ({ label: x.name, note: x.available ? x.label.replace(/^[^·]*·\s*/, '') || null : x.mode === 'preview' ? '30 s previews only' : 'not connected', svc: x.service, on: t.pinned && x.chosen, disabled: !x.available, run: () => pin(k, { service: x.service, uri: x.url || null }, `Pinned to ${x.name}`) }));
            showMenu(b, [{ title: t.unavailable ? t.unavailable : 'Play this song from' }, { label: 'Auto (follow the playlist)', icon: 'shuffle', on: !pinnedNow, note: t.plays && !pinnedNow ? `now: ${srcName(t.plays.service)}` : null, run: () => pin(k, { service: null }, 'Follows the playlist again') }, ...opts,
              ...(t.connect && t.connect.length ? [{ label: `Connect ${t.connect.map((c) => SERVICE_NAMES[c] || c).join(' or ')}`, icon: 'plus', run: () => setView('sources') }] : []),
              ...(t.id ? [{ label: 'Find another version', icon: 'disc', run: () => setView('item', { kind: 'song', id: t.id }) }] : [])]);
          });
          $$('[data-pfilter]', main).forEach((b) => b.onclick = () => { state.plFilter = b.dataset.pfilter ? { id: plId, key: b.dataset.pfilter } : null; render(); });
          const ol = $('#plorderopen'); if (ol) ol.onclick = () => $('#plorder').scrollIntoView({ behavior: 'smooth', block: 'center' });
          if (pf) {
            const saveOrder = async (order) => { try { await api('PUT', `/playlists/${d.summary.id}/order`, { order }); state.plPf = null; render(); } catch (e) { toast(e.message, 'error'); } };
            bindOrderList($('#plorder'), pf.order.order, saveOrder);
            $('#plownorder').onchange = (e) => saveOrder(e.target.checked ? null : pf.order.order);
          }
          break;
        }
        case 'sources': {
          if (!state.sources) { try { state.sources = await api('GET', '/sources'); } catch (e) { state.sources = []; } }
          const srcs = state.sources;
          if (!state.sourceId || !srcs.some((x) => x.id === state.sourceId)) state.sourceId = srcs[0] ? srcs[0].id : 'archive';
          if (state.viewArg && state.viewArg.source && srcs.some((x) => x.id === state.viewArg.source)) state.sourceId = state.viewArg.source;
          const cur = srcs.find((x) => x.id === state.sourceId) || { id: state.sourceId, name: state.sourceId, description: '' };
          const wanted = state.viewArg && state.viewArg.q;
          if (wanted && (!state.sourceResults || state.sourceResults.q !== wanted || state.sourceResults.source !== cur.id)) {
            loading();
            try { const r = await api('GET', `/sources/${encodeURIComponent(cur.id)}/search?limit=40&q=` + encodeURIComponent(wanted)); state.sourceResults = { source: cur.id, q: wanted, items: r.items }; } catch (e) { toast(e.message, 'error'); state.sourceResults = { source: cur.id, q: wanted, items: [] }; }
            if (state.view !== 'sources') return;
          }
          const results = state.sourceResults && state.sourceResults.source === cur.id ? state.sourceResults : { q: '', items: [] };
          const tabs = `<div class="chips tabs">${srcs.map((x) => `<button class="chip ${x.id === cur.id ? 'on' : ''}" data-src="${esc(x.id)}">${esc(x.name)}</button>`).join('')}</div>`;
          let tidal = null; if (cur.id === 'tidal') { try { tidal = await api('GET', '/sources/tidal/status'); } catch { tidal = null; } if (state.view !== 'sources') return; }
          let spot = null; if (cur.id === 'spotify') { try { spot = await api('GET', '/sources/spotify/status'); } catch { spot = null; } if (state.view !== 'sources') return; }
          const spotAccount = cur.id !== 'spotify' ? '' : !spot || !spot.configured ? `<div class="notice">Spotify needs a developer app's client id and secret in the node config (<code>[spotify]</code>) or the <code>PM_SPOTIFY_*</code> variables in Docker.</div>`
            : spot.signed_in ? `<div class="toolbar"><span class="pill ok">Signed in${spot.display_name ? ' · ' + esc(spot.display_name) : spot.user_id ? ' · ' + esc(spot.user_id) : ''}${spot.country ? ' · ' + esc(spot.country) : ''}${spot.product ? ' · ' + esc(spot.product) : ''}</span><button class="btn ghost" id="spotout">Sign out</button></div><p class="muted small">Spotify hands out no audio (DRM): tracks play on your Spotify devices (Premium), chosen under “Play on Spotify”. Your library and playlists import by ISRC.</p>`
            : `<div class="toolbar"><button class="btn primary" id="spotin">${icon('zone')} Connect Spotify</button><span class="muted small">Opens Spotify's sign-in; the app must list <code>${esc(spot.redirect_uri)}</code> as a redirect URI and, while in development mode, your Spotify account under its user management.</span></div>`;
          const account = cur.id === 'spotify' ? spotAccount : cur.id !== 'tidal' ? '' : !tidal || !tidal.configured ? `<div class="notice">TIDAL needs the developer app's client id and secret in the node config (<code>[tidal]</code>) or the <code>PM_TIDAL_*</code> variables in Docker.</div>`
            : tidal.signed_in ? `<div class="toolbar"><span class="pill ok">Signed in${tidal.user_id ? ' · user ' + esc(tidal.user_id) : ''}${tidal.country ? ' · ' + esc(tidal.country) : ''}${tidal.access_tier ? ' · ' + esc(tidal.access_tier) : ''}</span>${tidal.playback_limit ? `<span class="pill warn" title="TIDAL's answer to the last stream manifest">previews only · ${esc(tidal.playback_limit)}</span>` : ''}<button class="btn ghost" id="tidalout">Sign out</button></div>${tidal.playback_limit ? `<p class="muted small">TIDAL serves 30-second previews to apps on the ${esc(tidal.access_tier || 'THIRD_PARTY')} developer tier. Full tracks need a higher access tier granted by TIDAL for this app; search, albums and the collection and playlist imports are unaffected.</p>` : ''}`
            : `<div class="toolbar"><button class="btn primary" id="tidalin">${icon('zone')} Connect TIDAL</button><span class="muted small">Opens TIDAL's sign-in; the app must list <code>${esc(tidal.redirect_uri)}</code> as a redirect URI. Search works without signing in; playback and your collection need it. The app must also have these scopes enabled: <code>${esc(tidal.requested_scopes || '')}</code> (change them with <code>[tidal] scopes</code> in the node config).</span></div>`;
          main.innerHTML = pageHead('Sources', cur.id === 'tidal' || cur.id === 'spotify' ? 'your subscription, one account on this node · also answers in the main search' : 'streaming without an account') + tabs
            + `<p class="muted small">${esc(cur.description)}</p>` + account
            + `<div class="toolbar"><button class="btn primary" id="ssearchall">${icon('search')} Search ${esc(cur.name)}</button><span class="muted small">Opens the one search, scoped to ${esc(cur.name)}.</span></div>`
            + ''
          $$('[data-src]', main).forEach((b) => b.onclick = () => { state.sourceResults = null; setView('sources', { source: b.dataset.src }); });
          const ti = $('#tidalin'); if (ti) ti.onclick = async () => { try { const r = await api('GET', '/sources/tidal/login'); location.href = r.url; let tries = 0; const t = setInterval(async () => { tries++; try { const st = await api('GET', '/sources/tidal/status'); if (st.signed_in) { clearInterval(t); toast('TIDAL signed in'); render(); } } catch { /* keep polling */ } if (tries > 60) clearInterval(t); }, 3000); } catch (e) { toast(e.message, 'error'); } };
          const to = $('#tidalout'); if (to) to.onclick = async () => { try { await api('DELETE', '/sources/tidal/session'); toast('TIDAL signed out'); render(); } catch (e) { toast(e.message, 'error'); } };
          const si = $('#spotin'); if (si) si.onclick = async () => { try { const r = await api('GET', '/sources/spotify/login'); location.href = r.url; } catch (e) { toast(e.message, 'error'); } };
          const so = $('#spotout'); if (so) so.onclick = async () => { try { await api('DELETE', '/sources/spotify/session'); toast('Spotify signed out'); render(); } catch (e) { toast(e.message, 'error'); } };
          if (spot && spot.signed_in && !results.q) {
            const box = document.createElement('div'); box.innerHTML = '<p class="muted small">Loading your Spotify library…</p>'; main.appendChild(box);
            (async () => {
              try {
                const [col, pls, devs] = await Promise.all([api('GET', '/sources/spotify/collection'), api('GET', '/sources/spotify/playlists'), api('GET', '/sources/spotify/devices').catch(() => [])]);
                if (state.view !== 'sources' || state.sourceId !== 'spotify') return;
                const albums = col.items.filter((h) => h.kind === 'album'); const tracks = col.items.filter((h) => h.kind === 'track');
                const hit = (h) => `<div class="list-row clickable" data-sitem="${esc(h.id)}">${h.image ? `<img class="thumb" src="${esc(h.image)}" alt="" loading="lazy">` : '<div class="thumb"></div>'}<div class="grow"><div class="name">${esc(h.title)}${h.library_track_id || h.library_album_id ? ' <span class="pill ok">in library</span>' : ''}</div><div class="sub">${esc([h.creator, h.year].filter(Boolean).join(' · '))}</div></div><button class="icon-btn" data-splay="${esc(h.id)}" title="Play on Spotify">${icon('play')}</button></div>`;
                box.innerHTML = `<section class="section"><div class="section-head"><h3>Your devices</h3><span class="muted small">Spotify Connect</span></div>${devs.length ? `<div class="chips">${devs.map((d) => `<span class="chip ${d.is_active ? 'on' : ''}">${esc(d.name)} <span class="muted">${esc(d.type)}${d.is_active ? ' · active' : ''}</span></span>`).join('')}</div>` : '<p class="muted small">No Spotify device online. Open Spotify on a phone, computer or speaker and it appears here.</p>'}</section>
                  <section class="section"><div class="section-head"><h3>Your library</h3><span class="muted small">${tracks.length} tracks · ${albums.length} albums</span><button class="btn sm" id="spotfav" title="Favourite them here too, by ISRC where Spotify has one">${icon('heart')} Import as favourites</button></div>${albums.length || tracks.length ? `<div class="list">${albums.slice(0, 20).map(hit).join('')}${tracks.slice(0, 50).map(hit).join('')}</div>` : '<p class="muted">Nothing saved on Spotify yet.</p>'}</section>
                  <section class="section"><div class="section-head"><h3>Your playlists</h3><span class="muted small">import one as a private Pubky playlist</span></div>${pls.length ? `<div class="list">${pls.map((p) => `<div class="list-row">${p.image ? `<img class="thumb" src="${esc(p.image)}" alt="" loading="lazy">` : ''}<div class="grow"><div class="name">${esc(p.name)}</div><div class="sub">${p.items} tracks${p.description ? ' · ' + esc(p.description) : ''}</div></div><button class="btn sm" data-spl="${esc(p.id)}">${icon('download')} Import</button></div>`).join('')}</div>` : '<p class="muted">No playlists on Spotify.</p>'}</section>`;
                $$('[data-sitem]', box).forEach((b) => b.onclick = () => setView('source-item', { source: 'spotify', id: b.dataset.sitem }));
                $$('[data-splay]', box).forEach((b) => b.onclick = (e) => { e.stopPropagation(); spotifyPlay([b.dataset.splay], b); });
                $('#spotfav').onclick = async () => { try { const r = await api('POST', '/sources/spotify/collection/import'); toast(`${r.imported} favourites imported${r.skipped ? `, ${r.skipped} skipped` : ''}`); loadFavourites().catch(() => {}); } catch (e) { toast(e.message, 'error'); } };
                $$('[data-spl]', box).forEach((b) => b.onclick = () => importPlaylist(b, 'spotify', b.dataset.spl));
              } catch (e) { box.innerHTML = `<p class="muted small">Spotify library: ${esc(e.message)}</p>`; }
            })();
          }
          if (tidal && tidal.signed_in && !results.q) {
            // Your collection and playlists, with imports into the node's favourites and Pubky playlists.
            const box = document.createElement('div'); box.id = 'tidalmine'; box.innerHTML = '<p class="muted small">Loading your TIDAL collection…</p>'; main.appendChild(box);
            (async () => {
              try {
                const [col, pls] = await Promise.all([api('GET', '/sources/tidal/collection'), api('GET', '/sources/tidal/playlists')]);
                if (state.view !== 'sources' || state.sourceId !== 'tidal') return;
                const hit = (h) => `<div class="list-row clickable" data-sitem="${esc(h.id)}">${h.image ? `<img class="thumb" src="${esc(h.image)}" alt="" loading="lazy">` : '<div class="thumb"></div>'}<div class="grow"><div class="name">${esc(h.title)}</div><div class="sub">${esc([h.creator, h.year].filter(Boolean).join(' · '))}</div></div><span class="muted small">${esc(h.kind)}</span></div>`;
                box.innerHTML = `<section class="section"><div class="section-head"><h3>Your collection</h3><span class="muted small">${col.tracks.length} tracks · ${col.albums.length} albums</span><button class="btn sm" id="tidalfav" title="Favourite them here too: by ISRC where TIDAL has one, else by the TIDAL link">${icon('heart')} Import as favourites</button></div>${col.albums.length || col.tracks.length ? `<div class="list">${col.albums.slice(0, 20).map(hit).join('')}${col.tracks.slice(0, 50).map(hit).join('')}</div>` : '<p class="muted">Nothing in your TIDAL collection yet.</p>'}</section>
                  <section class="section"><div class="section-head"><h3>Your playlists</h3><span class="muted small">import one as a private Pubky playlist</span></div>${pls.length ? `<div class="list">${pls.map((p) => `<div class="list-row"><div class="grow"><div class="name">${esc(p.name)}</div><div class="sub">${p.items} tracks${p.description ? ' · ' + esc(p.description) : ''}</div></div><button class="btn sm" data-tpl="${esc(p.id)}">${icon('download')} Import</button></div>`).join('')}</div>` : '<p class="muted">No playlists of your own on TIDAL.</p>'}</section>`;
                $$('[data-sitem]', box).forEach((b) => b.onclick = () => setView('source-item', { source: 'tidal', id: b.dataset.sitem }));
                $('#tidalfav').onclick = async () => { try { const r = await api('POST', '/sources/tidal/collection/import'); toast(`${r.imported} favourites imported${r.skipped ? `, ${r.skipped} skipped` : ''}`); loadFavourites().catch(() => {}); } catch (e) { toast(e.message, 'error'); } };
                $$('[data-tpl]', box).forEach((b) => b.onclick = () => importPlaylist(b, 'tidal', b.dataset.tpl));
              } catch (e) { box.innerHTML = `<p class="muted small">TIDAL collection: ${esc(e.message)}</p>`; }
            })();
          }
          $('#ssearchall').onclick = () => { state.searchScope = cur.id; search.focus(); if (search.value.trim()) runSearch(); };
          $$('[data-sitem]', main).forEach((b) => b.onclick = () => setView('source-item', { source: cur.id, id: b.dataset.sitem }));
          break;
        }
        case 'source-artist': {
          loading();
          const { source, id } = state.viewArg;
          const a = await api('GET', `/sources/${encodeURIComponent(source)}/artists/${encodeURIComponent(id)}`);
          if (state.view !== 'source-artist') return;
          main.innerHTML = backBtn('search', 'Back') + `<div class="artist-hero"><div class="portrait" style="background:hsl(${hue(a.name)} 35% 28%)">${a.image ? `<img src="${esc(a.image)}" alt="" onerror="this.remove()">` : esc(initials(a.name))}</div><div class="grow"><div class="page-head"><div><div class="eyebrow">Artist · ${esc(source.toUpperCase())}</div><h1>${esc(a.name)}</h1><div class="muted">${a.albums.length} albums · <a href="${esc(a.url)}" target="_blank" rel="noopener">open</a>${a.library_artist_id ? ` · <button class="link" data-artist="${a.library_artist_id}">in your library</button>` : ''}</div></div></div></div></div>` + (a.albums.length ? `<div class="grid">${a.albums.map((h) => `<button class="tile" data-sitem="${esc(h.id)}" title="${esc(h.title)}"><div class="art">${h.image ? `<img loading="lazy" alt="" src="${esc(h.image)}">` : ''}${h.library_album_id ? '<span class="src-badge lib">in library</span>' : ''}</div><div class="t">${esc(h.title)}</div><div class="a">${esc([h.year].filter(Boolean).join(' · '))}</div></button>`).join('')}</div>` : '<p class="muted">No albums listed.</p>');
          $$('[data-sitem]', main).forEach((b) => b.onclick = () => setView('source-item', { source, id: b.dataset.sitem }));
          $$('[data-artist]', main).forEach((b) => b.onclick = () => setView('artist', Number(b.dataset.artist)));
          break;
        }
        case 'source-item': {
          loading();
          const { source, id } = state.viewArg;
          const it = await api('GET', `/sources/${encodeURIComponent(source)}/items/${encodeURIComponent(id)}`);
          if (state.view !== 'source-item') return;
          const srcName = ((state.sources || []).find((x) => x.id === source) || { name: source }).name;
          const qitems = it.tracks.map((t) => ({ uri: t.url, title: t.title, artist: t.artist || it.creator || '', album: it.title, duration_ms: t.duration_ms, image: it.image || null }));
          const playFrom = async (i, replace = true) => {
            const items = qitems.slice(i);
            if (isBrowser()) {
              const bitems = items.map((q, k) => ({ id: Date.now() + k, uri: q.uri, stream: streamUrlFor(q.uri) || q.uri, title: q.title, artist: q.artist, album: q.album, duration_ms: q.duration_ms, image: q.image || null }));
              if (replace) { state.browserQueue = bitems; browserPlay(0); } else state.browserQueue.push(...bitems);
              return;
            }
            try { await api('POST', `/zones/${state.zone}/queue`, { items, replace, play: replace, at: 'end' }); toast(replace ? `Playing from ${srcName}` : 'Added to queue'); refreshQueue(); } catch (e) { toast(e.message, 'error'); }
          };
          const canHeart = !!(await (async () => { try { await api('GET', '/favourites?uri=' + encodeURIComponent(it.url)); return true; } catch { return false; } })());
          main.innerHTML = backBtn('sources', srcName) + `<div class="artist-hero"><div class="portrait" style="border-radius:12px;background:hsl(${hue(it.title)} 35% 28%)">${it.image ? `<img src="${esc(it.image)}" alt="" onerror="this.remove()">` : esc(initials(it.title))}</div><div class="grow"><div class="page-head"><div><div class="eyebrow">${esc(srcName)}</div><h1>${esc(it.title)}</h1><div class="muted">${esc([it.creator, it.year, it.tracks.length + (it.tracks.length === 1 ? ' track' : ' tracks')].filter(Boolean).join(' · '))} · <a href="${esc(it.url)}" target="_blank" rel="noopener">open</a>${it.licence ? (/^https?:/.test(it.licence) ? ` · <a href="${esc(it.licence)}" target="_blank" rel="noopener">licence</a>` : ` · ${esc(it.licence)}`) : ''}</div></div><div class="actions"><button class="btn primary" id="aplay">${icon('play')} Play</button><button class="btn" id="aadd">${icon('plus')} Add to queue</button><button class="btn" id="amore" title="Add to playlist">${icon('playlist')} Playlist</button>${it.library_album_id ? `<button class="btn ghost" id="ainlib" title="You already have this album">${icon('disc')} In your library</button>` : ''}${canHeart ? `<button class="btn heart ${state.favUris.has(it.url) ? 'on' : ''}" id="aheart">${icon(state.favUris.has(it.url) ? 'heart-fill' : 'heart')} ${state.favUris.has(it.url) ? 'Favourited' : 'Favourite'}</button>` : ''}</div></div>${it.description ? `<p class="bio">${esc(it.description.slice(0, 600))}${it.description.length > 600 ? '…' : ''}</p>` : ''}</div></div>`
            + `<div class="tracks">${it.tracks.map((t, i) => `<div class="track compact" data-i="${i}"><div class="no"><span class="num">${t.track_no ?? i + 1}</span><button class="rowplay" data-aplay="${i}" title="Play from here">${icon('play')}</button></div><div class="main-col"><div class="title">${esc(t.title)}</div><div class="by">${esc(t.artist || it.creator || '')}</div></div><div><span class="fmt">${esc(t.format)}</span></div><div class="dur">${fmt(t.duration_ms)}</div><div class="row-actions"><button class="icon-btn" data-aadd1="${i}" title="Add to queue">${icon('plus')}</button></div></div>`).join('')}</div>`;
          $('#aplay').onclick = () => playFrom(0, true); $('#aadd').onclick = () => playFrom(0, false);
          $('#amore').onclick = (e) => { e.stopPropagation(); uriMenu($('#amore'), it.tracks.map((t) => t.url), it.title); };
          const ah = $('#aheart'); if (ah) ah.onclick = async () => { const on = state.favUris.has(it.url); try { await setFavourite({ uri: it.url }, !on); render(); } catch (er) { toast(er.message, 'error'); } };
          $$('[data-aplay]', main).forEach((b) => b.onclick = () => playFrom(Number(b.dataset.aplay), true));
          const ainlib = $('#ainlib'); if (ainlib) ainlib.onclick = () => setView('album', it.library_album_id);
          $$('[data-aadd1]', main).forEach((b) => b.onclick = async () => { const i = Number(b.dataset.aadd1); if (isBrowser()) { const q = qitems[i]; state.browserQueue.push({ id: Date.now(), uri: q.uri, stream: streamUrlFor(q.uri) || q.uri, title: q.title, artist: q.artist, album: q.album, duration_ms: q.duration_ms, image: q.image || null }); toast('Added to queue'); return; } try { await api('POST', `/zones/${state.zone}/queue`, { items: [qitems[i]], at: 'end' }); toast('Added to queue'); refreshQueue(); } catch (e) { toast(e.message, 'error'); } });
          break;
        }
        case 'radio': {
          const wantedR = state.viewArg && state.viewArg.q;
          if (wantedR && (!state.radioResults || state.radioResults.q !== wantedR)) {
            try { const r = await api('GET', '/radio/search?limit=40&q=' + encodeURIComponent(wantedR)); state.radioResults = { q: wantedR, stations: r.stations }; } catch (e) { toast(e.message, 'error'); state.radioResults = { q: wantedR, stations: [] }; }
            if (state.view !== 'radio') return;
          }
          const saved = (await api('GET', '/radio/stations')).stations;
          const row = (st, isSaved) => `<div class="list-row"><button class="icon-btn" data-st-play="${esc(st.id)}" title="Play">${icon('play')}</button><div class="avatar" style="background:hsl(${hue(st.name)} 35% 28%);overflow:hidden">${st.favicon ? `<img src="${esc(st.favicon)}" alt="" style="width:100%;height:100%;object-fit:cover" onerror="this.remove()">` : esc(initials(st.name))}</div><div class="grow"><div class="name">${esc(st.name)}</div><div class="sub">${esc([st.country, st.codec ? st.codec + (st.bitrate ? ' ' + st.bitrate + ' kbps' : '') : '', (st.tags || []).slice(0, 4).join(', ')].filter(Boolean).join(' · '))}</div></div>${isSaved ? `<button class="btn sm ghost" data-st-del="${esc(st.id)}">Remove</button>` : saved.some((x) => x.url === st.url) ? '<span class="pill ok">saved</span>' : `<button class="btn sm" data-st-save="${esc(st.id)}">Save</button>`}${st.homepage ? `<a class="icon-btn" href="${esc(st.homepage)}" target="_blank" rel="noopener" title="Homepage">${icon('external')}</a>` : ''}</div>`;
          const results = state.radioResults || { q: '', stations: [] };
          main.innerHTML = pageHead('Radio', 'internet radio via radio-browser.info')
            + `<div class="toolbar"><input class="input" id="rq" placeholder="Search stations by name (jazz, FIP, BBC 6…)" value="${esc(results.q)}" style="width:360px"><button class="btn primary" id="rsearch">${icon('search')} Search</button><span class="muted small">Streams play on any zone; lossy codecs show as Lossy in the signal path.</span></div>`
            + (saved.length ? `<section class="section"><div class="section-head"><h3>Saved stations</h3></div><div class="list">${saved.map((st) => row(st, true)).join('')}</div></section>` : '')
            + (results.q ? `<section class="section"><div class="section-head"><h3>Results for “${esc(results.q)}”</h3></div>${results.stations.length ? `<div class="list">${results.stations.map((st) => row(st, false)).join('')}</div>` : '<p class="muted">No stations matched.</p>'}</section>` : (saved.length ? '' : emptyState('radio', 'No stations yet', 'Search the directory above and save the ones you like.')));
          const all = [...saved, ...results.stations];
          const byId = (id) => all.find((x) => x.id === id);
          const doSearch = async () => { const q = $('#rq').value.trim(); if (!q) return; $('#rsearch').disabled = true; try { const r = await api('GET', '/radio/search?limit=40&q=' + encodeURIComponent(q)); state.radioResults = { q, stations: r.stations }; render(); } catch (e) { toast(e.message, 'error'); $('#rsearch').disabled = false; } };
          $('#rsearch').onclick = doSearch; $('#rq').onkeydown = (e) => { if (e.key === 'Enter') doSearch(); };
          $$('[data-st-play]', main).forEach((b) => b.onclick = () => playStation(byId(b.dataset.stPlay)));
          $$('[data-st-save]', main).forEach((b) => b.onclick = async () => { try { await api('PUT', '/radio/stations', byId(b.dataset.stSave)); toast('Station saved'); render(); } catch (e) { toast(e.message, 'error'); } });
          $$('[data-st-del]', main).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/radio/stations/${encodeURIComponent(b.dataset.stDel)}`); render(); } catch (e) { toast(e.message, 'error'); } });
          break;
        }
        case 'item': {
          await pubkyUser();
          const { kind, id } = state.viewArg;
          loading();
          let pg;
          try { pg = await api('GET', `/item/${kind}/${encodeURIComponent(id)}`); }
          catch (e) { main.innerHTML = backBtn(kind === 'artist' ? 'artists' : 'albums', kind === 'artist' ? 'Artists' : 'Albums') + `<div class="notice">${esc(e.message)}</div>`; break; }
          if (state.view !== 'item' || state.viewArg.id !== id) return;
          // The node leaves empty lists out of its answer.
          for (const k of ['songs', 'albums', 'versions', 'about', 'open_on', 'connect']) pg[k] = pg[k] || [];
          state.itemTitle = pg.title || `Untitled on ${pg.source}`; state.itemSection = pg.section;
          $$('.nav-item').forEach((b) => b.classList.toggle('on', b.dataset.view === pg.section));
          const fallback = { artist: pg.artist && pg.artist.name, album: kind === 'album' ? pg.title : '', image: pg.image };
          const songs = pg.songs || [];
          const target = pg.target;
          const qs = new URLSearchParams(Object.entries(target).map(([k, v]) => [k, String(v)])).toString();
          const [myTags, myReview] = await Promise.all([
            api('GET', `/tags?${qs}`).then((r) => r.tags).catch(() => null),
            api('GET', `/reviews?${qs}`).then((r) => (r.reviews || [])[0] || null).catch(() => undefined),
          ]);
          if (state.view !== 'item' || state.viewArg.id !== id) return;
          const favKey = pg.uri; const fav = !!(favKey && state.favUris && state.favUris.has(favKey));
          const sectionName = { albums: 'Albums', artists: 'Artists', tracks: 'Tracks', home: 'Home', playlists: 'Playlists' }[pg.section] || 'Home';
          const pf = pg.plays_from;
          const where = pf ? (pf.service === 'library' ? 'your library' : (SERVICE_NAMES[pf.service] || pf.service) + (pf.mode === 'preview' ? ' (previews)' : '')) : null;
          const eyebrow = [{ song: 'Song', album: 'Album', artist: 'Artist' }[kind], where ? `plays from ${where}` : pg.connect.length ? `on ${pg.connect.map((c) => SERVICE_NAMES[c] || c).join(', ')}` : pg.source].filter(Boolean).join(' · ');
          const by = [pg.artist ? (pg.artist.id ? `<a class="lnk" id="iartist">${esc(pg.artist.name)}</a>` : `<a class="lnk" id="iartistq">${esc(pg.artist.name)}</a>`) : '', pg.album ? (pg.album.id ? `<a class="lnk" id="ialbum">${esc(pg.album.name)}</a>` : esc(pg.album.name)) : '', pg.year].filter(Boolean).join(' · ');
          const empty = (t) => `<p class="muted small">${t}</p>`;
          const songsTitle = kind === 'artist' ? 'Top songs' : kind === 'song' ? 'Song' : 'Songs';
          const songsInfo = songs.length ? `${songs.length} song${songs.length === 1 ? '' : 's'}${pg.total_ms ? ' · ' + mins(pg.total_ms) : ''}` : '';
          const versionChip = (v, k) => `<span class="chip">${svcBadge(v.source === 'library' ? 'library' : v.source)} ${esc(v.label)}${v.uri || /^(tidal|deezer)$/.test(v.source) || v.source === 'library' ? ` <button class="chiplink" data-vplay="${k}" title="Play this version">${icon('play')}</button>` : ''}${v.url ? ` <a class="chiplink" href="${esc(v.url)}" target="_blank" rel="noopener" title="Open">${icon('external')}</a>` : ''}</span>`;
          main.innerHTML = backBtn(pg.section, sectionName)
            + (pg.reading ? `<p class="muted small reading">Reading this link on Pinakes… versions and tags appear in a moment.</p>` : '')
            + `<div class="hero">${pg.image ? `<div class="art"><img src="${esc(imgSrc(pg.image))}" alt="" style="width:100%;border-radius:${kind === 'artist' ? '50%' : '12px'}" onerror="this.remove()"></div>` : `<div class="art placeholder" style="background:hsl(${hue(state.itemTitle)} 35% 28%);border-radius:${kind === 'artist' ? '50%' : '12px'}"></div>`}<div><div class="eyebrow">${esc(eyebrow)}</div><h1>${esc(state.itemTitle)}</h1><p class="sub">${by}</p>
              <div class="actions" style="margin-top:12px"><button class="btn primary" id="iplay">${icon('play')} Play</button>${kind !== 'song' ? `<button class="btn" id="ishuffle">${icon('shuffle')} Shuffle</button>` : ''}<button class="btn" id="izone">${icon('zone')} Play on…</button><button class="btn icon-only ${fav ? 'on' : ''}" id="ifav" title="${fav ? 'Remove from favourites' : 'Add to favourites'}" ${favKey || target.track_id || target.album_id || target.artist_id ? '' : 'disabled'}>${icon(fav ? 'heart-fill' : 'heart')}</button><button class="btn ghost" id="imore" title="More">${icon('more')}</button></div>
              <div class="playsfrom">${playsChip({ plays_from: pf, connect: pg.connect }, true) || ''}</div></div></div>`
            + `<section class="section" id="isongs"><div class="section-head"><h3>${songsTitle}</h3><span class="muted small">${esc(songsInfo)}</span></div>${songs.length ? songRows(songs, { showAlbum: kind !== 'album', covers: kind !== 'album' }) : empty(pg.connect.length ? `Not on your services · <a data-view="sources">Connect ${esc(pg.connect.map((c) => SERVICE_NAMES[c] || c).join(' or '))}</a>` : 'No connected service lists songs for this yet.')}</section>`
            + (kind === 'artist' || kind === 'song' ? `<section class="section" id="ialbums"><div class="section-head"><h3>${kind === 'song' ? 'On the album' : 'Albums'}</h3><span class="muted small">${pg.albums.length || ''}</span></div>${pg.albums.length ? itemTileGrid(pg.albums, 'album') : empty(kind === 'song' ? 'The album is not known for this link yet.' : 'No albums found on your services yet.')}</section>` : '')
            + (pg.concerts && pg.concerts.length ? `<section class="section" id="iconcerts"><div class="section-head"><h3>Upcoming concerts</h3></div><div class="list">${pg.concerts.map((c, k) => `<div class="list-row clickable" data-iconcert="${k}"><div class="grow"><div class="name">${esc(itemName(c))}</div><div class="sub">${esc([c.description, c.site].filter(Boolean).join(' · '))}</div></div>${icon('external')}</div>`).join('')}</div></section>` : '')
            + `<section class="section" id="iversions"><div class="section-head"><h3>Versions</h3><span class="muted small">your library and every service that has it</span>${kind !== 'song' || pg.versions.length <= 2 ? `<button class="btn sm ghost" id="ifind" title="Search your connected services for the same ${kind}">Find on other services</button>` : ''}</div><div id="ifindbox"></div>${pg.versions.length ? `<div class="chips">${pg.versions.map(versionChip).join('')}</div>` : empty(pg.connect.length ? 'Only on ' + esc(pg.connect.map((c) => SERVICE_NAMES[c] || c).join(', ')) + ' so far.' : `Only on ${esc(pg.source)} so far.`)}</section>`
            + `<section class="section" id="ipubky"><div class="section-head"><h3>On Pubky</h3><span class="chips scopeswitch" role="group" aria-label="Whose tags"><button class="chip scope on" data-scope="all" aria-pressed="true" title="Tags on every version of this">${icon('compass')} Everywhere</button><button class="chip scope" data-scope="this" aria-pressed="false" tabindex="-1" title="Tags on this exact link">${icon('disc')} This version</button></span></div><div id="ipubkybody">${pg.on_pubky && pg.on_pubky.tags.length ? `<p class="muted small">${pg.on_pubky.taggers} ${pg.on_pubky.taggers === 1 ? 'person' : 'people'} tagged this.</p><div class="chips">${pg.on_pubky.tags.map(([l, c]) => `<button class="chip" data-tag-open="${esc(l)}">#${esc(l)} <span class="muted">${c}</span></button>`).join('')}</div>` : empty('No one has tagged this on Pubky yet.')}</div></section>`
            + `<section class="section" id="iyours"><div class="section-head"><h3>Yours</h3></div>${resignNotice()}${myTags ? tagRow(myTags, target) : empty('Tags need an identity for this item first.')}${myReview !== undefined ? reviewRow(myReview) : ''}</section>`
            + (pg.related && pg.related.length ? `<section class="section" id="irelated"><div class="section-head"><h3>People who tagged this also tagged</h3></div>${itemTileGrid(pg.related.map(asTile), 'album')}</section>` : '')
            + `<section class="section" id="iabout"><div class="section-head"><h3>About</h3></div>${pg.blurb ? `<p class="blurb">${esc(pg.blurb)}</p>` : ''}${pg.about.length ? `<dl class="kv">${pg.about.map((f) => `<dt>${esc(f.label)}</dt><dd>${f.href ? `<a href="${esc(f.href)}" target="_blank" rel="noopener">${esc(f.value)}</a>` : esc(f.value)}</dd>`).join('')}</dl>` : empty('Nothing more is known yet.')}</section>`;
          $('[data-back]', main).onclick = () => goBack(pg.section);
          bindSongRows($('#isongs'), songs, { plays_from: pf, fallback });
          if (pg.albums.length) bindItemTiles($('#ialbums'), pg.albums, 'album');
          if (pg.related && pg.related.length) bindHome($('#irelated'), { items: pg.related, tags: [] });
          $$('[data-iconcert]', main).forEach((r) => r.onclick = () => { const c = pg.concerts[Number(r.dataset.iconcert)]; window.open(c.page || c.uri, '_blank', 'noopener'); });
          if (myTags) bindTagRow(target);
          if (myReview !== undefined) bindReviewRow(target, myReview);
          $$('[data-view="sources"]', main).forEach((a) => a.onclick = () => setView('sources'));
          const ia = $('#iartist'); if (ia) ia.onclick = () => setView('item', { kind: 'artist', id: pg.artist.id });
          const iq = $('#iartistq'); if (iq) iq.onclick = () => { search.value = pg.artist.name; runSearch(); };
          const ial = $('#ialbum'); if (ial) ial.onclick = () => setView('item', { kind: 'album', id: pg.album.id });
          const playAll = (shuffle, how = 'replace', anchorEl) => { if (!songs.length) return discoverPlay({ uri: pg.uri || '', page: pg.open_on[0] ? pg.open_on[0].url : pg.uri, title: pg.title, artist: fallback.artist, image: pg.image }, null, anchorEl); const list = shuffle ? [...songs].sort(() => Math.random() - 0.5) : songs; return playSongs(list, 0, how, pf, anchorEl, fallback); };
          $('#iplay').onclick = (e) => playAll(false, 'replace', e.currentTarget);
          const sh = $('#ishuffle'); if (sh) sh.onclick = (e) => playAll(true, 'replace', e.currentTarget);
          $('#izone').onclick = (e) => showMenu(e.currentTarget, [{ title: 'Play on' }, ...zoneList().map((z) => ({ label: z.name, icon: 'zone', run: () => { state.zone = z.id; try { localStorage.setItem('pm.zone', z.id); } catch { /* */ } renderZoneName(); renderBar(); playAll(false); } }))]);
          $('#ifav').onclick = async () => { try { await setFavourite(target, !fav); render(); } catch (er) { toast(er.message, 'error'); } };
          $('#imore').onclick = (e) => itemMenu(e.currentTarget, { title: pg.title, songs, plays_from: pf, fallback, artistId: pg.artist && pg.artist.id, artistName: pg.artist && pg.artist.name, albumId: pg.album && pg.album.id, openOn: pg.open_on, path: `/${kind}/${id}`, uri: pg.uri });
          $$('[data-vplay]', main).forEach((b) => b.onclick = async () => { const v = pg.versions[Number(b.dataset.vplay)]; if (v.source === 'library') { if (v.uri && v.uri.startsWith('track:')) { const t = await api('GET', `/library/tracks/${v.uri.slice(6)}`); return playTracks([t], true); } return setView('item', { kind: 'album', id: 'lib-' + v.id }); } if (v.uri) return playSourceItems([qOf({ uri: v.uri, title: pg.title }, fallback)], true); return discoverPlay({ uri: v.url, page: v.url, title: pg.title, artist: fallback.artist }, null, b); });
          $$('[data-tag-open]', $('#ipubky')).forEach((b) => b.onclick = () => setView('discover', { tag: b.dataset.tagOpen }));
          // "Find on other services": the same title on the connected services, "possibly the same", which the owner confirms.
          const fb = $('#ifind'); if (fb) fb.onclick = async () => {
            const box = $('#ifindbox'); box.innerHTML = '<p class="muted small">Searching your services…</p>'; fb.disabled = true;
            try {
              const c = await api('GET', `/item/candidates?${new URLSearchParams({ uri: pg.uri || '', kind, title: pg.title || '', artist: (pg.artist && pg.artist.name) || '' })}`);
              box.innerHTML = c.length ? `<p class="muted small">Possibly the same ${kind}. Confirm the ones that are; this stays on your node.</p><div class="list">${c.map((v, k) => `<div class="list-row">${svcBadge(v.source)}<div class="grow"><div class="name">${esc(v.label)}</div><div class="sub"><a href="${esc(v.url)}" target="_blank" rel="noopener">${esc(v.url)}</a></div></div><button class="btn sm primary" data-same="${k}">Same ${kind}</button></div>`).join('')}</div>` : '<p class="muted small">Nothing with this title on your services.</p>';
              $$('[data-same]', box).forEach((b) => b.onclick = async () => { try { await api('POST', '/item/matches', { uri: pg.uri, version: c[Number(b.dataset.same)] }); toast('Matched'); render(); } catch (e) { toast(e.message, 'error'); } });
            } catch (e) { box.innerHTML = `<p class="muted small">${esc(e.message)}</p>`; }
            fb.disabled = false;
          };
          const scopes = $$('[data-scope]', main);
          // A segmented switch: arrow keys move and pick, one tab stop.
          scopes.forEach((b, k) => b.onkeydown = (e) => { const d = e.key === 'ArrowRight' || e.key === 'ArrowDown' ? 1 : e.key === 'ArrowLeft' || e.key === 'ArrowUp' ? -1 : 0; if (!d) return; e.preventDefault(); const nb = scopes[(k + d + scopes.length) % scopes.length]; nb.focus(); nb.click(); });
          scopes.forEach((b) => b.onclick = async () => {
            scopes.forEach((x) => { const on = x === b; x.classList.toggle('on', on); x.setAttribute('aria-pressed', String(on)); x.tabIndex = on ? 0 : -1; });
            const body = $('#ipubkybody'); const link = b.dataset.scope === 'this' ? (pg.open_on[0] && pg.open_on[0].url) : pg.uri;
            if (!link) { body.innerHTML = empty('This version has no public link.'); return; }
            try { const v = await api('GET', `/discover/uri?uri=${encodeURIComponent(link)}`); body.innerHTML = v && v.tags && v.tags.length ? `<p class="muted small">${v.taggers} ${v.taggers === 1 ? 'person' : 'people'} · ${esc(link)}</p><div class="chips">${v.tags.map(([l, c]) => `<span class="chip">#${esc(l)} <span class="muted">${c}</span></span>`).join('')}</div>` : empty(`No one has tagged ${b.dataset.scope === 'this' ? 'this version' : 'this'} on Pubky yet.`); } catch { body.innerHTML = empty('Pubky did not answer.'); }
          });
          if (pg.reading) { const tries = (state.viewArg.tries || 0) + 1; if (tries <= 8) setTimeout(() => { if (state.view === 'item' && state.viewArg.id === id) { state.viewArg.tries = tries; render(); } }, 4000); }
          break;
        }
        case 'queue': {
          await refreshQueue(false);
          if (state.view !== 'queue') return;
          const z = currentZone(); const q = isBrowser() ? state.browserQueue : state.queue;
          main.innerHTML = pageHead('Queue', z ? z.name : '', (isBrowser() ? '' : `<button class="btn ${z && z.radio ? 'primary' : ''}" id="qradio" title="When the queue ends, keep playing similar music from the library">${icon('radio')} Radio ${z && z.radio ? 'on' : 'off'}</button>`) + (q.length > 1 ? `<button class="btn" id="qshuffle" title="Shuffle what comes next; the playing track stays">${icon('shuffle')} Shuffle</button>` : '') + (q.length && !isBrowser() && zoneList().filter((o) => o.id !== state.zone && o.id !== BROWSER).length ? `<button class="btn" id="qtransfer" title="Move this queue and position to another zone">${icon('zone')} Transfer…</button>` : '') + (q.length ? `<button class="btn" id="qsave" title="A new private playlist from this queue">${icon('playlist')} Save as playlist</button>` : '') + (q.length && !isBrowser() ? `<button class="btn" id="qclear">${icon('trash')} Clear</button>` : ''))
            + (q.length ? `<div class="list queue">${q.map((it, i) => `<div class="list-row ${z && z.queue_index === i ? 'cur' : ''}" data-qrow="${i}"><span class="qgrip" data-qgrip="${i}" role="button" tabindex="0" aria-label="Drag to reorder (or Alt+↑/↓)" title="Drag to reorder">⋮⋮</span><button class="icon-btn" data-qi="${i}" title="Play from here">${z && z.queue_index === i ? icon('play') : `<span class="num">${i + 1}</span>`}</button><div class="grow"><div class="name" ${z && z.queue_index === i ? 'style="color:var(--accent-strong)"' : ''}>${esc(itemName(it))}</div><div class="sub">${esc([it.artist, it.album].filter(Boolean).join(' · '))}</div></div><span class="muted small">${fmt(it.duration_ms)}</span><span class="row-actions q-actions"><button class="icon-btn" data-qup="${i}" title="Move up" ${i === 0 ? 'disabled' : ''}>${icon('chevron', 'ic up')}</button><button class="icon-btn" data-qdown="${i}" title="Move down" ${i === q.length - 1 ? 'disabled' : ''}>${icon('chevron')}</button><button class="icon-btn" data-qrm="${i}" title="Remove">${icon('close')}</button></span></div>`).join('')}</div>` : emptyState('queue', 'The queue is empty', 'Play an album or add tracks with the row actions.'));
          $$('[data-qi]', main).forEach((b) => b.onclick = () => transport({ action: 'play', index: Number(b.dataset.qi) }));
          const qedit = async (i, op, to) => {
            if (isBrowser()) {
              const bq = state.browserQueue; const j = op === 'up' ? i - 1 : op === 'down' ? i + 1 : op === 'to' ? to : null;
              if (op === 'rm') { bq.splice(i, 1); if (state.browserIndex === i) browserPlay(Math.min(i, bq.length - 1)); else if (state.browserIndex > i) state.browserIndex--; }
              else if (j != null && j >= 0 && j < bq.length && j !== i) {
                const [it] = bq.splice(i, 1); bq.splice(j, 0, it);
                // The playing item keeps playing wherever it moves to.
                const c = state.browserIndex;
                if (c === i) state.browserIndex = j; else if (i < c && c <= j) state.browserIndex = c - 1; else if (j <= c && c < i) state.browserIndex = c + 1;
              }
              render(); return;
            }
            const it = q[i]; if (!it) return;
            try {
              if (op === 'rm') await api('DELETE', `/zones/${encodeURIComponent(state.zone)}/queue/${it.id}`);
              else { const j = op === 'to' ? to : op === 'up' ? i - 1 : i + 1; if (j === i) return; await api('PUT', `/zones/${encodeURIComponent(state.zone)}/queue/${it.id}`, { to_index: j }); }
              refreshQueue();
            } catch (e) { toast(e.message, 'error'); }
          };
          $$('[data-qup]', main).forEach((b) => b.onclick = () => qedit(Number(b.dataset.qup), 'up'));
          // Drag to reorder: pointer events, so mouse, touch and pen all work. The row follows the pointer,
          // a line shows where it lands, and the list scrolls near its edges.
          $$('[data-qgrip]', main).forEach((g) => {
            g.onkeydown = (e) => { const i = Number(g.dataset.qgrip); if (e.altKey && e.key === 'ArrowUp' && i > 0) { e.preventDefault(); qedit(i, 'to', i - 1); } if (e.altKey && e.key === 'ArrowDown' && i < q.length - 1) { e.preventDefault(); qedit(i, 'to', i + 1); } };
            g.onpointerdown = (e) => {
              if (e.button !== 0) return;
              e.preventDefault();
              const from = Number(g.dataset.qgrip);
              const rows = $$('.list.queue .list-row', main); const row = rows[from]; if (!row) return;
              g.setPointerCapture(e.pointerId);
              const startY = e.clientY; const startScroll = main.scrollTop; let to = from; let lastY = e.clientY; let raf = null;
              const rest = rows.map((r) => { const b = r.getBoundingClientRect(); return b.top + b.height / 2; });
              row.classList.add('qdragging'); document.body.classList.add('qdrag-active');
              const place = () => {
                const dy = lastY - startY + (main.scrollTop - startScroll);
                row.style.transform = `translateY(${dy}px)`;
                const mid = row.getBoundingClientRect().top + row.offsetHeight / 2;
                to = from;
                rows.forEach((r, k) => { if (k === from) return; const c = rest[k] - (main.scrollTop - startScroll); if (k < from && mid < c) to = Math.min(to, k); if (k > from && mid > c) to = Math.max(to, k); });
                // The rows in between slide aside, so the gap shows where the row lands.
                const h = row.offsetHeight;
                rows.forEach((r, k) => { if (k === from) return; const shift = to > from && k > from && k <= to ? -h : to < from && k >= to && k < from ? h : 0; r.style.transform = shift ? `translateY(${shift}px)` : ''; });
              };
              const scrollLoop = () => { const b = main.getBoundingClientRect(); const edge = 48; if (lastY < b.top + edge) main.scrollTop -= 10; else if (lastY > b.bottom - edge) main.scrollTop += 10; place(); raf = requestAnimationFrame(scrollLoop); };
              raf = requestAnimationFrame(scrollLoop);
              g.onpointermove = (ev) => { lastY = ev.clientY; place(); };
              const end = (commit) => {
                cancelAnimationFrame(raf); g.onpointermove = null; g.onpointerup = null; g.onpointercancel = null;
                row.classList.remove('qdragging'); row.style.transform = ''; document.body.classList.remove('qdrag-active');
                rows.forEach((r) => { r.style.transform = ''; });
                if (commit && to !== from) qedit(from, 'to', to);
              };
              g.onpointerup = () => end(true);
              g.onpointercancel = () => end(false);
            };
          });
          $$('[data-qdown]', main).forEach((b) => b.onclick = () => qedit(Number(b.dataset.qdown), 'down'));
          $$('[data-qrm]', main).forEach((b) => b.onclick = () => qedit(Number(b.dataset.qrm), 'rm'));
          const qr = $('#qradio'); if (qr) qr.onclick = async () => { try { await api('PUT', `/zones/${encodeURIComponent(state.zone)}/radio`, { on: !(z && z.radio) }); toast(z && z.radio ? 'Radio off' : 'Radio on: similar music follows the queue'); const zs = await api('GET', `/zones/${encodeURIComponent(state.zone)}`); state.zones[zs.id] = zs; render(); } catch (e) { toast(e.message, 'error'); } };
          const qsh = $('#qshuffle'); if (qsh) qsh.onclick = async () => {
            if (isBrowser()) {
              const bq = state.browserQueue; const start = state.browserIndex == null ? 0 : state.browserIndex + 1;
              for (let i = bq.length - 1; i > start; i--) { const j = start + Math.floor(Math.random() * (i - start + 1)); [bq[i], bq[j]] = [bq[j], bq[i]]; }
              render(); return;
            }
            try { await api('POST', `/zones/${encodeURIComponent(state.zone)}/shuffle`); toast('Upcoming tracks shuffled'); refreshQueue(); } catch (e) { toast(e.message, 'error'); }
          };
          const qt = $('#qtransfer'); if (qt) qt.onclick = () => showMenu(qt, [{ title: 'Transfer to' }, ...zoneList().filter((o) => o.id !== state.zone && o.id !== BROWSER).map((o) => ({ label: o.name, icon: 'zone', run: async () => {
            try { const zs = await api('POST', `/zones/${encodeURIComponent(state.zone)}/transfer`, { to: o.id }); state.zones[zs.id] = zs; state.zone = zs.id; localStorage.setItem('pm.zone', state.zone); toast(`Moved playback to ${zs.name}`); renderZoneName(); renderBar(); refreshQueue(); } catch (e) { toast(e.message, 'error'); }
          } }))]);
          const qsv = $('#qsave'); if (qsv) qsv.onclick = async () => {
            const body = isBrowser() ? { uris: state.browserQueue.map((it) => it.uri), name: 'Queue · this browser' } : { zone: state.zone };
            try { const r = await api('POST', '/playlists/from-queue', body); state.playlists = await api('GET', '/playlists'); toast(r.skipped.length ? `Saved “${r.playlist.summary.name}”; ${r.skipped.length} not identified yet were left out (Settings › Identify library)` : `Saved “${r.playlist.summary.name}”`); } catch (e) { toast(e.message, 'error'); }
          };
          const qc = $('#qclear'); if (qc) qc.onclick = async () => { try { await api('DELETE', `/zones/${state.zone}/queue`); refreshQueue(); } catch (e) { toast(e.message, 'error'); } };
          break;
        }
        case 'zones': {
          const outs = state.outputs || (state.outputs = await api('GET', '/outputs').catch(() => []));
          if (state.view !== 'zones') return;
          const capsOf = (z) => (outs.find((o) => o.id === z.output) || {}).caps || null;
          const zs = zoneList();
          main.innerHTML = pageHead('Zones', `${zs.length} · multi-room overview`, `<button class="btn" id="zgroup">${icon('plus')} Group outputs…</button>`) + `<div class="cards">${zs.map((z) => {
            const np = z.now_playing; const it = np && np.item; const caps = capsOf(z); const hv = caps && caps.hw_volume && caps.hw_volume.kind === 'db' ? caps.hw_volume : null;
            const members = np && np.members && np.members.length ? `<span class="pill">${np.members.length} outputs</span>` : '';
            return `<div class="card zone-card ${z.id === state.zone ? 'on' : ''}" data-zcard="${esc(z.id)}">
              <div class="zc-head">${icon('zone')}<h3>${esc(z.name)}</h3>${members}<span class="pill ${z.transport === 'playing' ? 'ok' : ''}">${esc(z.transport)}</span></div>
              <div class="zc-now">${it ? itemArt(it, 'sm') : '<div class="np-art placeholder"></div>'}<div class="zc-text"><div class="name">${it ? esc(np.stream_title || it.title || it.uri) : '<span class="muted">Nothing playing</span>'}</div><div class="sub">${it ? esc([it.artist, it.album].filter(Boolean).join(' · ')) : esc(z.id === BROWSER ? 'Streams the files to this browser' : z.output)}</div></div></div>
              <div class="zc-controls">
                <button class="icon-btn" data-zt="previous" title="Previous">${icon('prev')}</button>
                <button class="icon-btn big" data-zt="toggle" title="${z.transport === 'playing' ? 'Pause' : 'Play'}">${icon(z.transport === 'playing' ? 'pause' : 'play')}</button>
                <button class="icon-btn" data-zt="next" title="Next">${icon('next')}</button>
                ${hv ? `<input type="range" data-zvol="${esc(z.id)}" min="${hv.min}" max="${hv.max}" step="${hv.step || 1}" value="${z.volume_db != null ? z.volume_db : hv.max}" aria-label="Volume of ${esc(z.name)}">` : z.id === BROWSER ? `<input type="range" data-zvol="${BROWSER}" min="-60" max="0" value="${Math.round(20 * Math.log10(audio.volume || 1))}" aria-label="Browser volume">` : '<span class="muted small">fixed volume</span>'}
                ${z.id === state.zone ? '<span class="pill ok">selected</span>' : `<button class="btn sm" data-zsel="${esc(z.id)}">Play to</button>`}
                ${String(z.id).startsWith('group:') ? `<button class="btn sm ghost" data-zone-del="${esc(z.id)}">Ungroup</button>` : ''}
              </div></div>`;
          }).join('')}</div>`;
          const zoneTransport = async (zid, action) => {
            if (zid === BROWSER) { const keep = state.zone; state.zone = BROWSER; try { await transport({ action }); } finally { state.zone = keep; } return; }
            try { const zs2 = await api('POST', `/zones/${encodeURIComponent(zid)}/transport`, { action }); state.zones[zs2.id] = zs2; render(); } catch (e) { toast(e.message, 'error'); }
          };
          $$('[data-zcard]', main).forEach((card) => {
            const zid = card.dataset.zcard;
            $$('[data-zt]', card).forEach((b) => b.onclick = () => zoneTransport(zid, b.dataset.zt));
            const v = $('[data-zvol]', card); if (v) { setFill(v); v.addEventListener('input', () => setFill(v)); v.addEventListener('change', () => { if (zid === BROWSER) { audio.volume = Math.pow(10, v.value / 20); return; } api('POST', `/zones/${encodeURIComponent(zid)}/volume`, { volume: { db: Number(v.value) } }).catch((e) => toast(e.message, 'error')); }); }
          });
          $$('[data-zsel]', main).forEach((b) => b.onclick = () => { state.zone = b.dataset.zsel; localStorage.setItem('pm.zone', state.zone); renderZoneName(); renderBar(); refreshQueue(); render(); });
          $$('[data-zone-del]', main).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/zones/${encodeURIComponent(b.dataset.zoneDel)}`); toast('Group removed'); if (state.zone === b.dataset.zoneDel) { ensureZone(); renderZoneName(); } render(); } catch (e) { toast(e.message, 'error'); } });
          $('#zgroup').onclick = () => setView('settings');
          break;
        }
        case 'settings': {
          const info = await api('GET', '/info'); state.info = info;
          const outs = await api('GET', '/outputs');
          const folders = await api('GET', '/library/folders').catch(() => []);
          const ns = await api('GET', '/node/settings').catch(() => ({ contact: '', discovery: false }));
          let pk = null; try { pk = await api('GET', '/pubky'); state.pubkyStatus = pk; state.pubkyUser = pk.user || null; } catch { /* no pubky config */ }
          if (state.view !== 'settings') return;
          main.innerHTML = pageHead('Settings') + `
            <div class="cards">
              <div class="card"><h3>Node</h3><dl class="kv"><dt>Name</dt><dd>${esc(info.name)}</dd><dt>Version</dt><dd>${esc(info.version)}</dd><dt>Library</dt><dd>${info.library.tracks} tracks · ${info.library.albums} albums · ${info.library.artists} artists</dd><dt>Files</dt><dd>${info.library.files}${info.library.missing_files ? ` · <span style="color:var(--warn)">${info.library.missing_files} missing</span>` : ''}</dd></dl>
                <div class="actions" style="margin-top:14px"><button class="btn" id="scan">Scan library</button><button class="btn" id="rescan">Rescan outputs</button><a class="btn" href="/v1/backup${tokenQ()}" download title="A snapshot of the library database: playlists, favourites, tags, history, settings. Restore with pm-node restore">${icon('download')} Download backup</a><button class="btn ghost" id="logout">Change token</button></div></div>
              <div class="card" id="idcard"><h3>Identify library</h3><p class="muted small">Gives every file a MusicBrainz identity so Pinakes, tags and playlists know it: IDs in the tags first, then an audio fingerprint (AcoustID), with the folder as the album. Unsure matches wait for you.</p><div id="idbox" class="muted small">Loading…</div></div>
              <div class="card" id="loccard"><h3>Concerts near you</h3><p class="muted small">Your city, for Home's concerts. It is looked up once on OpenStreetMap and stays on this node.</p><div class="toolbar"><input class="input" id="loccity" placeholder="City" style="width:220px"><button class="btn primary" id="locsave">Save</button><button class="btn ghost" id="locclear">Clear</button><span class="muted small" id="loccur"></span></div></div>
              <div class="card"><h3>Upstream contact</h3><p class="muted small">An email or URL sent in the node's User-Agent. Pinakes (Discover, On Pubky), MusicBrainz and Wikipedia ask for one.</p><div class="toolbar"><input class="input" id="contact" placeholder="you@example.org or https://your.site" value="${esc(ns.contact)}" style="width:280px"><button class="btn primary" id="contactsave">Save</button><span class="pill ${ns.discovery ? 'ok' : 'warn'}">${ns.discovery ? 'Discover on' : 'Discover off'}</span></div>
                <h3 style="margin-top:16px">Subsonic jukebox</h3><p class="muted small">The zone Subsonic clients (Symfonium, Feishin…) drive with their jukebox control.</p><select class="input" id="jukeboxzone"><option value="" ${ns.jukebox_zone ? '' : 'selected'}>First zone by name</option>${zoneList().filter((z) => z.id !== BROWSER).map((z) => `<option value="${esc(z.id)}" ${ns.jukebox_zone === z.id ? 'selected' : ''}>${esc(z.name)}</option>`).join('')}</select></div>
              <div class="card" id="gordercard"><h3>Play from</h3><p class="muted small">Songs play from the first source here that has them. A playlist can have its own order; a pinned song keeps its source.</p><div id="gorder"><div class="skeleton" style="height:120px"></div></div></div><div class="card"><h3>Pubky</h3>${pk ? `<p class="muted">${pk.user ? 'Signed in as <code>' + esc(pk.user) + '</code>' : 'No session on the node yet'} · ${pk.unsynced} unsynced object${pk.unsynced === 1 ? '' : 's'}</p>${pk.needs_resign ? `<div class="notice warn resign" role="status" id="pkresign"><span>${esc(pk.notice || RESIGN_TEXT)}</span></div>` : ''}<div class="actions" style="margin-top:14px">${pk.user ? `<button class="btn ${pk.needs_resign ? 'primary' : ''}" id="pklogin" title="Approve this node again in Pubky Ring (replaces the current session)">Sign in again</button>` : `<button class="btn primary" id="pklogin">Sign in with Pubky Ring</button>`}<button class="btn" id="sync" ${pk.needs_resign ? `disabled title="${esc(pk.notice || RESIGN_TEXT)}"` : ''}>Sync with Pubky</button>${pk.user ? `<button class="btn" id="pkmirror" title="Download files from the homeserver that this node does not have yet, then scan them">${icon('download')} Mirror files to this node</button>` : ''}</div><div id="pkurl"></div><div class="row" style="gap:8px;margin-top:12px;align-items:center"><input class="input" id="bulklabel" value="library" style="width:140px" aria-label="Tag label"><button class="btn sm" id="bulktag" title="Put one public tag on every track that has a MusicBrainz id or ISRC, so Pinakes matches your songs across services (pushed by the next sync)">${icon('plus')} Tag my whole library on Pubky</button></div><div id="bulkout" class="muted small"></div>${pk.user ? `<div id="pkfiles" class="muted small" style="margin-top:12px">Loading homeserver files…</div>` : ''}` : '<p class="muted">Pubky is not configured on this node.</p>'}</div>
            </div>
            <section class="section"><div class="section-head"><h3>Library folders</h3></div><p class="muted small">Paths on the node. New folders are watched and scanned; removing one forgets its tracks.</p>
              ${folders.length ? `<div class="list">${folders.map(([p, present]) => `<div class="list-row"><div class="grow"><div class="name">${esc(p)}</div></div>${present ? '' : '<span class="pill warn">missing</span>'}<button class="btn sm ghost" data-folder-del="${esc(p)}">Remove</button></div>`).join('')}</div>` : '<p class="muted">No folders yet.</p>'}
              <div class="toolbar" style="margin-top:10px"><input class="input" id="folderadd" placeholder="/path/on/the/node/Music" style="width:360px"><button class="btn primary" id="folderbtn">${icon('plus')} Add folder</button></div></section>
            <section class="section"><div class="section-head"><h3>Outputs</h3><button class="btn sm" id="netscan" title="Ask every UPnP and Cast device on the network and explain what answers">${icon('search')} Scan the network</button></div><div id="netscanout"></div><div class="row" style="gap:8px;margin:6px 0 10px"><input id="rendaddr" placeholder="Renderer IP, description URL, or for a Linn: IP MAC serial" style="flex:1;min-width:200px"><button class="btn sm" id="rendadd">${icon('plus')} Add renderer</button></div><div id="rendsaved" class="muted small"></div>${outs.length ? `<div class="list">${outs.map((o) => `<div class="list-row"><div class="grow"><div class="name">${esc(o.name)}</div><div class="sub">${esc(o.id)}</div></div><span class="muted small">${o.caps.max_pcm_bits}-bit · ${o.caps.pcm_rates.map((r) => (r / 1000).toFixed(1).replace('.0', '') + 'k').join(' ')}${o.caps.dsd ? ' · DSD' : ''}</span></div>`).join('')}</div>` : '<p class="muted">No outputs found.</p>'}</section>
            <section class="section"><div class="section-head"><h3>Zones</h3></div><div class="list">${zoneList().map((z) => `<div class="list-row"><div class="grow"><div class="name">${esc(z.name)}</div><div class="sub">${esc(z.output || '')}</div></div><span class="pill ${z.transport === 'playing' ? 'ok' : ''}">${esc(z.transport)}</span>${z.id !== BROWSER ? `<button class="btn sm ${state.dspOpen === z.id ? 'primary' : ''}" data-dsp="${esc(z.id)}">DSP</button>` : ''}${String(z.id).startsWith('group:') ? `<button class="btn sm ghost" data-zone-del="${esc(z.id)}">Remove</button>` : ''}</div>`).join('')}</div>
              <div id="dspbox"></div>
              <div class="card" style="margin-top:12px"><h3>Group outputs</h3><p class="muted small">Pick two or more outputs to play as one zone; the first checked is the leader. Members are kept in time by single-sample slips (shown as a <code>sync</code> stage).</p><div class="chips" id="groupouts">${outs.filter((o) => o.caps.groupable !== false).map((o) => `<label class="chip"><input type="checkbox" value="${esc(o.id)}" style="margin:0 6px 0 0"> ${esc(o.name)}</label>`).join('')}</div><div class="toolbar" style="margin-top:10px"><input class="input" id="groupname" placeholder="Group name (Everywhere)" style="width:240px"><button class="btn primary" id="groupmake">${icon('plus')} Create group</button></div></div>
            </section>
            <p class="muted small">Shortcuts: <kbd>/</kbd> search · <kbd>Space</kbd> play/pause · <kbd>←</kbd> <kbd>→</kbd> previous/next · <kbd>N</kbd> now playing · <kbd>Esc</kbd> close</p>`;
          $('#scan').onclick = async () => { try { await api('POST', '/library/scan', {}); toast('Scan started'); watchScan({ scanning: true }); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-zone-del]', main).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/zones/${encodeURIComponent(b.dataset.zoneDel)}`); toast('Group removed'); if (state.zone === b.dataset.zoneDel) state.zone = BROWSER; render(); } catch (e) { toast(e.message, 'error'); } });
          $('#groupmake').onclick = async () => { const ids = $$('#groupouts input:checked').map((i) => i.value); const name = $('#groupname').value.trim() || 'Group'; if (ids.length < 2) { toast('Pick at least two outputs', 'error'); return; } try { const z = await api('POST', '/zones', { name, outputs: ids }); state.zones[z.id] = z; toast(`Group “${name}” created`); render(); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-dsp]', main).forEach((b) => b.onclick = () => { state.dspOpen = state.dspOpen === b.dataset.dsp ? null : b.dataset.dsp; render(); });
          if (state.dspOpen && state.zones[state.dspOpen]) {
            try { const cfg = await api('GET', `/zones/${encodeURIComponent(state.dspOpen)}/dsp`); $('#dspbox').innerHTML = dspEditor(state.zones[state.dspOpen], cfg); bindDspEditor(state.dspOpen, cfg); } catch (e) { $('#dspbox').innerHTML = `<div class="notice warn">${esc(e.message)}</div>`; }
          }
          const saveContact = async () => { const c = $('#contact').value.trim(); if (!c) return; try { await api('PUT', '/node/settings', { contact: c }); state.discoveryOff = false; toast('Contact saved; Discover is on'); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#contactsave').onclick = saveContact; $('#contact').onkeydown = (e) => { if (e.key === 'Enter') saveContact(); };
          const addFolder = async () => { const p = $('#folderadd').value.trim(); if (!p) return; try { const r = await api('POST', '/library/folders', { path: p }); toast(r.scan_started ? 'Folder added; scanning' : 'Folder added; scanned after the current scan'); watchScan({ scanning: true }); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#folderbtn').onclick = addFolder; $('#folderadd').onkeydown = (e) => { if (e.key === 'Enter') addFolder(); };
          $$('[data-folder-del]', main).forEach((b) => b.onclick = async () => { if (!b.dataset.armed) { b.dataset.armed = '1'; b.textContent = 'Really remove?'; setTimeout(() => { if (b.isConnected) { delete b.dataset.armed; b.textContent = 'Remove'; } }, 3000); return; } try { const r = await api('DELETE', '/library/folders?path=' + encodeURIComponent(b.dataset.folderDel)); toast(`Folder removed · ${r.removed_tracks} tracks gone`); render(); } catch (e) { toast(e.message, 'error'); } });
          renderIdentify();
          api('GET', '/home/location').then((l) => { const c = $('#loccur'); if (c) c.textContent = l ? `Now: ${l.label || `${l.lat.toFixed(2)}, ${l.lon.toFixed(2)}`}` : 'Not set'; }).catch(() => {});
          $('#locsave').onclick = async () => { const city = $('#loccity').value.trim(); if (!city) return; try { const l = await api('PUT', '/home/location', { city }); toast(`Concerts near ${l.label}`); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#locclear').onclick = async () => { try { await api('PUT', '/home/location', {}); toast('City cleared'); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#rescan').onclick = async () => { try { await api('POST', '/outputs/rescan'); toast('Outputs rescanned'); render(); } catch (e) { toast(e.message, 'error'); } };
          const showSaved = async () => { try { const r = await api('GET', '/outputs/discovery?quick=1'); const box = $('#rendsaved'); if (!box) return; box.innerHTML = r.saved_addresses.length ? 'Added by address: ' + r.saved_addresses.map((a) => `<code>${esc(a)}</code> <button class="link" data-rendrm="${esc(a)}">forget</button>`).join(' · ') : ''; $$('[data-rendrm]', box).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/outputs/upnp/${encodeURIComponent(b.dataset.rendrm)}`); toast('Address forgotten (its zone stays until restart)'); showSaved(); } catch (e) { toast(e.message, 'error'); } }); } catch { /* older node */ } };
          showSaved();
          const ra = $('#rendadd'); if (ra) ra.onclick = async () => {
            const address = $('#rendaddr').value.trim(); if (!address) return; ra.disabled = true;
            try { const outs = await api('POST', '/outputs/upnp', { address }); toast(`Renderer added · ${outs.length} outputs`); $('#rendaddr').value = ''; render(); } catch (e) { toast(e.message, 'error'); }
            ra.disabled = false;
          };
          const bt = $('#bulktag'); if (bt) bt.onclick = async () => {
            const label = $('#bulklabel').value.trim(); if (!label) return; bt.disabled = true;
            try { const r = await api('POST', '/tags/bulk', { label }); $('#bulkout').textContent = `#${r.label}: ${r.tagged} tracks tagged, ${r.already} already had it, ${r.skipped_no_identity} skipped (no MusicBrainz id or ISRC). Sync with Pubky to publish.`; }
            catch (e) { toast(e.message, 'error'); }
            bt.disabled = false;
          };
          const sc = $('#netscan'); if (sc) sc.onclick = async () => {
            const box = $('#netscanout'); sc.disabled = true; box.innerHTML = '<p class="muted small">Asking the network (about 5 s)…</p>';
            try {
              const r = await api('GET', '/outputs/discovery');
              box.innerHTML = (r.notes.length ? r.notes.map((n) => `<div class="notice">${esc(n)}</div>`).join('') : '')
                + `<p class="muted small">Scanned from <strong>${esc(r.node || '')}</strong>${r.local_address ? ` at <code>${esc(r.local_address)}</code>` : ''} · audio is fetched from <code>${esc(r.public_url)}</code></p>`
                + (r.upnp.length ? `<div class="list">${r.upnp.map((d) => `<div class="list-row"><div class="grow"><div class="name">${esc(d.friendly_name || d.location)}</div><div class="sub">${esc(d.from)}${d.server ? ' · ' + esc(d.server) : ''}</div></div><span class="pill ${d.renderer ? 'ok' : ''}">${d.renderer ? (d.output ? 'renderer · output' : 'renderer') : 'not a renderer'}</span></div>`).join('')}</div>` : (r.upnp_enabled ? '' : '<p class="muted small">UPnP discovery is off.</p>'))
                + (r.cast.length ? `<div class="list">${r.cast.map((d) => `<div class="list-row"><div class="grow"><div class="name">${esc(d.friendly_name)}</div><div class="sub">${esc(d.addr)}</div></div><span class="pill ok">Cast</span></div>`).join('')}</div>` : '');
            } catch (e) { box.innerHTML = `<p class="muted small">${esc(e.message)}</p>`; }
            sc.disabled = false;
          };
          $('#logout').onclick = () => login();
          const pkf = $('#pkfiles'); if (pkf) {
            try {
              const files = await api('GET', '/pubky/files');
              const total = files.reduce((n, f) => n + (f.size || 0), 0);
              pkf.innerHTML = files.length ? `<div class="eyebrow">Private files on the homeserver · ${files.length} · ${(total / 1e6).toFixed(1)} MB</div><div class="list compact">${files.slice(0, 50).map((f) => `<div class="list-row"><div class="grow"><div class="name">${esc(f.path.split('/').pop())}</div></div><span class="muted small">${f.size != null ? (f.size / 1e6).toFixed(1) + ' MB' : ''}</span></div>`).join('')}${files.length > 50 ? `<div class="muted small">… and ${files.length - 50} more</div>` : ''}</div>` : '<span>No private files on the homeserver yet. Upload tracks from their ⋯ menu; audio only ever goes under /priv/.</span>';
            } catch (e) { pkf.textContent = 'Homeserver files: ' + e.message; }
          }
          const mi = $('#pkmirror'); if (mi) mi.onclick = async () => { mi.disabled = true; try { const r = await api('POST', '/pubky/files/mirror'); toast(`Mirrored ${r.fetched} file${r.fetched === 1 ? '' : 's'} (${r.present} already here${r.errors.length ? `, ${r.errors.length} errors` : ''})`); } catch (e) { toast(e.message, 'error'); } mi.disabled = false; };
          const jz = $('#jukeboxzone'); if (jz) jz.onchange = async () => { try { await api('PUT', '/node/settings', { jukebox_zone: jz.value }); toast(jz.value ? 'Jukebox zone saved' : 'Jukebox follows the first zone'); } catch (e) { toast(e.message, 'error'); } };
          const loadOrder = async () => { const box = $('#gorder'); if (!box) return; try { const o = await api('GET', '/play-order'); box.innerHTML = orderList(o); bindOrderList(box, o.order, async (order) => { try { await api('PUT', '/play-order', { order }); state.plPf = null; loadOrder(); } catch (e) { toast(e.message, 'error'); } }); } catch (e) { box.innerHTML = `<p class="muted small">${esc(e.message)}</p>`; } };
          loadOrder();
          const sy = $('#sync'); if (sy) sy.onclick = async () => { try { const r = await api('POST', '/pubky/sync'); syncToast(r); render(); } catch (e) { toast(e.message, 'error'); } };
          const pl = $('#pklogin'); if (pl) pl.onclick = async () => {
            try {
              const r = await api('POST', '/pubky/login');
              $('#pkurl').innerHTML = `<div class="notice qrbox" style="margin-top:12px">${r.qr_svg ? `<div class="qr">${r.qr_svg}</div>` : ''}<div><p>Scan with Pubky Ring, or open the link on this device and approve. This page updates by itself.</p><a class="btn sm" href="${esc(r.url)}">Open in Pubky Ring</a> <button class="btn sm ghost" id="pkcopy">Copy link</button></div></div>`;
              const pc = $('#pkcopy'); if (pc) pc.onclick = () => { navigator.clipboard && navigator.clipboard.writeText(r.url).then(() => toast('Link copied')).catch(() => toast(r.url)); };
              const t = setInterval(async () => { try { const st = await api('GET', '/pubky/login'); if (st.status === 'signed_in') { clearInterval(t); state.pubkyUser = undefined; toast('Signed in as ' + st.user); render(); } } catch (e) { clearInterval(t); toast(e.message, 'error'); } }, 2000);
            } catch (e) { toast(e.message, 'error'); }
          };
          if (state.autoResign && pl) { state.autoResign = false; pl.click(); pl.scrollIntoView({ block: 'center' }); }
          break;
        }
        default:
          main.innerHTML = emptyState('compass', 'Unknown view', '');
      }
      bindTiles();
      $$('[data-artist]', main).forEach((el) => el.onclick = () => setView('artist', Number(el.dataset.artist)));
      $$('[data-back]', main).forEach((b) => b.onclick = () => goBack(b.dataset.back));
      $$('[data-empty-action]', main).forEach((b) => b.onclick = () => setView(b.dataset.emptyAction));
      bindResign(main);
      if (Object.keys(lists).length) bindTrackRows(main, lists);
    } catch (e) {
      main.innerHTML = `<div class="notice warn">${esc(e.message)}</div>`;
    }
  };

  // ---- connection + events -------------------------------------------------
  // The connection to the node (docs/CONNECTION.md): one WebSocket on /v1/events that reconnects with
  // backoff and jitter, resyncs from the snapshot (seq, boot, library_rev) without a reload, pings for the
  // round-trip time, and holds changes made while it is down for a few seconds.
  const link = { ws: null, up: false, ever: false, attempt: 0, retry: null, retryAt: 0, tick: null, ping: null, pingId: 0, pingSent: 0, awaiting: false, missed: 0, rtt: null, pongs: false, loaded: false, seq: null, boot: null, libRev: null, downSince: 0, waiters: [] };
  const HOLD_MS = 8000;
  const nodeLabel = () => (state.info ? `${state.info.name} · ${state.info.library.tracks} tracks` : 'Connected');
  // kind: 'ok' (connected) | 'wait' (reconnecting) | 'bad' (offline); true/false for ok/bad.
  const setConn = (kind, text) => {
    if (kind === true) kind = 'ok'; else if (kind === false) kind = 'bad';
    const c = $('#conn');
    c.className = 'node-status ' + kind;
    c.dataset.state = kind === 'ok' ? 'connected' : kind === 'wait' ? 'reconnecting' : 'offline';
    const ms = kind === 'ok' && link.rtt != null ? ` · ${link.rtt} ms` : '';
    $('.node-text', c).textContent = text + ms;
    c.title = kind === 'ok' ? (link.rtt != null ? `${text}. Round trip ${link.rtt} ms.` : text) : `${text}. Click to retry now.`;
    const bar = $('#connbar');
    if (bar) { bar.hidden = kind === 'ok'; bar.className = 'connbar ' + kind; $('.connbar-text', bar).textContent = text; }
  };
  const whenConnected = () => {
    if (!link.ever || link.up) return Promise.resolve();
    linkHint();
    return new Promise((resolve, reject) => {
      const w = { resolve, timer: setTimeout(() => { link.waiters = link.waiters.filter((x) => x !== w); reject(new Error('Not connected to the node, so nothing was changed. Try again when it is back.')); }, HOLD_MS) };
      link.waiters.push(w);
    });
  };
  const flushWaiters = () => { const ws = link.waiters; link.waiters = []; ws.forEach((w) => { clearTimeout(w.timer); w.resolve(); }); };
  // Someone is waiting on the node: retry now rather than at the end of a long backoff.
  const linkHint = () => { if (link.ever && !link.up && link.retry && link.retryAt - Date.now() > 1000) retryNow(); };
  const showDown = () => {
    clearInterval(link.tick);
    const paint = () => {
      if (link.up) { clearInterval(link.tick); return; }
      const secs = Math.max(0, Math.ceil((link.retryAt - Date.now()) / 1000));
      if (!navigator.onLine) setConn('bad', 'Offline: no network');
      else if (link.ever && Date.now() - link.downSince < 20000) setConn('wait', 'Reconnecting…');
      else setConn('bad', secs ? `Node offline · retrying in ${secs} s` : 'Node offline · retrying…');
    };
    paint(); link.tick = setInterval(paint, 1000);
  };
  const scheduleRetry = () => {
    clearTimeout(link.retry);
    const d = Math.min(30000, 500 * 2 ** Math.min(link.attempt, 10));
    const wait = Math.round(d / 2 + Math.random() * d / 2);
    link.attempt++; link.retryAt = Date.now() + wait;
    link.retry = setTimeout(() => { link.retry = null; reconnect(); }, wait);
  };
  const goneDown = () => {
    link.up = false; clearInterval(link.ping); link.awaiting = false;
    if (!link.downSince) link.downSince = Date.now();
    scheduleRetry(); showDown();
  };
  // Drop the socket without waiting for its close handshake (a dead peer may never answer).
  const dropSocket = () => { const ws = link.ws; link.ws = null; if (ws) { ws.onclose = ws.onmessage = ws.onerror = null; try { ws.close(); } catch {} } };
  const retryNow = () => { clearTimeout(link.retry); link.retry = null; link.attempt = 0; reconnect(); };
  const reconnect = async () => {
    if (!link.loaded) return connect();
    try { state.info = await api('GET', '/info'); } catch { goneDown(); return; }
    openEvents();
  };
  const sendPing = () => {
    const ws = link.ws; if (!ws || ws.readyState !== 1) return;
    if (link.awaiting && link.pongs && ++link.missed >= 2) { dropSocket(); goneDown(); return; }
    link.pingId++; link.pingSent = performance.now(); link.awaiting = true;
    ws.send(JSON.stringify({ type: 'ping', id: link.pingId }));
  };
  const onLibraryChanged = () => {
    state.albums = null;
    api('GET', '/info').then((i) => { state.info = i; watchScan(i); if (link.up) setConn('ok', nodeLabel()); }).catch(() => {});
    if (['albums', 'tracks', 'artists', 'home', 'favourites', 'album', 'artist', 'playlists', 'playlist'].includes(state.view)) render();
  };
  const applySnapshot = (ev) => {
    const first = link.boot == null;
    const restarted = !first && ev.boot != null && ev.boot !== link.boot;
    const libChanged = !first && (restarted || (ev.library_rev != null && ev.library_rev !== link.libRev));
    const downFor = link.downSince ? Date.now() - link.downSince : 0;
    link.seq = ev.seq ?? null; link.boot = ev.boot ?? null; link.libRev = ev.library_rev ?? null;
    state.zones = Object.fromEntries(ev.zones.map((z) => [z.id, z])); ensureZone(); renderZoneName();
    const wasUp = link.up;
    link.up = true; link.ever = true; link.attempt = 0; link.downSince = 0; clearInterval(link.tick);
    setConn('ok', nodeLabel());
    if (!wasUp) { clearInterval(link.ping); link.missed = 0; link.awaiting = false; sendPing(); link.ping = setInterval(sendPing, 10000); }
    flushWaiters();
    if (!first) {
      refreshQueue(false);
      if (!overlay.hidden) renderNowPlaying();
      if (state.view === 'zones') render();
      if (libChanged) onLibraryChanged();
      if (downFor > 5000) toast(restarted ? 'Reconnected: the node restarted' : 'Reconnected');
    }
  };
  // Scan indicator: while the node says it is scanning, poll /info until it is done.
  let scanTimer = null;
  const showScanning = (on) => { $('#status').innerHTML = on ? `<span class="scanning"><span class="spin"></span> Scanning library…</span>` : ''; };
  // ---- identify library (Settings card and the review view) ----
  let idTimer = null;
  const renderIdentify = async () => {
    const box = $('#idbox'); if (!box) return;
    let st; try { st = await api('GET', '/library/identify'); } catch (e) { box.textContent = e.message; return; }
    const c = st.counts; const p = st.progress;
    const pct = c.total ? Math.round((c.identified / c.total) * 100) : 0;
    box.innerHTML = `<div class="idbar"><span style="width:${pct}%"></span></div>
      <p class="small">${c.identified} of ${c.total} identified${c.suggested ? ` · <a data-view="identify">${c.suggested} to confirm</a>` : ''}${c.unknown ? ` · ${c.unknown} not found` : ''}${c.unchecked ? ` · ${c.unchecked} not checked yet` : ''}</p>
      ${p.running ? `<p class="small">Working: ${p.done} of ${p.total}…</p>` : (p.finished_at ? `<p class="small muted">Last run: ${p.identified} identified, ${p.suggested} to confirm, ${p.unknown} not found${p.errors ? `, ${p.errors} errors` : ''}.</p>` : '')}
      ${st.acoustid ? '' : '<p class="small" style="color:var(--warn)">No AcoustID key on this node: only tags and MusicBrainz search are used. Add <code>[acoustid] api_key</code> or PM_ACOUSTID_KEY.</p>'}
      <label class="row small" style="gap:8px; margin:8px 0"><input type="checkbox" id="idwrite" ${st.write_ids ? 'checked' : ''}> Write IDs into my files <span class="muted">(MusicBrainz recording and release IDs; each file's tags are backed up first; off by default)</span></label>
      <div class="actions"><button class="btn primary" id="idrun" ${p.running ? 'disabled' : ''}>${c.unchecked ? 'Identify' : 'Identify again'}</button>${c.suggested ? '<button class="btn" data-view="identify">Review matches</button>' : ''}${st.write_ids ? '<button class="btn" id="idwritenow" title="Write the IDs found so far into the files">Write now</button>' : ''}</div>`;
    $$('[data-view="identify"]', box).forEach((b) => b.onclick = () => setView('identify'));
    const wr = $('#idwrite'); if (wr) wr.onchange = async () => { try { await api('PUT', '/library/identify/settings', { write_ids: wr.checked }); toast(wr.checked ? 'IDs will be written into your files' : 'Your files stay as they are'); renderIdentify(); } catch (e) { toast(e.message, 'error'); wr.checked = !wr.checked; } };
    const wn = $('#idwritenow'); if (wn) wn.onclick = async () => { wn.disabled = true; try { const r = await api('POST', '/library/identify/write'); toast(`${r.written} file${r.written === 1 ? '' : 's'} updated${r.skipped.length ? `, ${r.skipped.length} left as they were (${r.skipped[0].reason})` : ''}`); } catch (e) { toast(e.message, 'error'); } wn.disabled = false; };
    const run = $('#idrun'); if (run) run.onclick = async () => { try { await api('POST', '/library/identify', { retry: !c.unchecked }); renderIdentify(); } catch (e) { toast(e.message, 'error'); } };
    clearTimeout(idTimer);
    if (p.running) idTimer = setTimeout(renderIdentify, 1500);
  };
  const watchScan = (info) => {
    showScanning(!!(info && info.scanning));
    clearTimeout(scanTimer);
    if (info && info.scanning) scanTimer = setTimeout(async () => { try { const i = await api('GET', '/info'); state.info = i; setConn(true, `${i.name} · ${i.library.tracks} tracks`); watchScan(i); } catch { showScanning(false); } }, 4000);
  };
  const connect = async () => {
    try {
      state.info = await api('GET', '/info');
      setConn(true, `${state.info.name} · ${state.info.library.tracks} tracks`);
      watchScan(state.info);
      loadFavourites().catch(() => {});
      const zones = await api('GET', '/zones');
      state.zones = Object.fromEntries(zones.map((z) => [z.id, z]));
      ensureZone(); renderZoneName(); renderBar(); refreshQueue(false);
      { const r = routeFromPath(); if (r && r.v === 'search' && r.arg && r.arg.q) { search.value = r.arg.q; history.replaceState({ v: 'search', arg: r.arg, depth: 0 }, '', location.pathname + location.search); runSearch('pop'); } else if (r) setView(r.v, r.arg, 'replace'); else setView(state.view, undefined, 'replace'); }
      link.loaded = true;
      openEvents();
    } catch (e) {
      main.innerHTML = `<div class="notice warn">Cannot reach the node: ${esc(e.message)} It retries by itself.</div>`;
      if (!link.downSince) link.downSince = Date.now();
      scheduleRetry(); showDown();
    }
  };
  const openEvents = () => {
    clearTimeout(link.retry); link.retry = null;
    dropSocket();
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${proto}://${location.host}/v1/events${tokenQ()}`);
    link.ws = ws;
    ws.onclose = () => { if (link.ws === ws) { link.ws = null; goneDown(); } };
    ws.onmessage = (m) => {
      let ev; try { ev = JSON.parse(m.data); } catch { return; }
      if (ev.type === 'pong') {
        if (ev.id === link.pingId) { link.rtt = Math.max(1, Math.round(performance.now() - link.pingSent)); link.awaiting = false; link.missed = 0; link.pongs = true; setConn('ok', nodeLabel()); }
        return;
      }
      if (ev.type === 'snapshot') { applySnapshot(ev); renderBar(); return; }
      // Numbered events: a gap means something was missed, so ask for a fresh snapshot.
      if (typeof ev.seq === 'number' && link.seq != null) {
        if (ev.seq > link.seq + 1) ws.send(JSON.stringify({ type: 'resync' }));
        if (ev.seq > link.seq) link.seq = ev.seq;
      }
      if (ev.type === 'zone') {
        const old = state.zones[ev.state.id]; state.zones[ev.state.id] = ev.state;
        if (!old) renderZoneName();
        if (ev.state.id === state.zone && (!old || old.queue_version !== ev.state.queue_version)) refreshQueue();
        if (ev.state.id === state.zone && old && old.now_playing && ev.state.now_playing && old.now_playing.item.id !== ev.state.now_playing.item.id && !overlay.hidden) renderNowPlaying();
        if (state.view === 'zones' && (!old || old.transport !== ev.state.transport || (old.now_playing && old.now_playing.item.id) !== (ev.state.now_playing && ev.state.now_playing.item.id))) render();
      }
      else if (ev.type === 'zone_removed') { delete state.zones[ev.zone]; ensureZone(); renderZoneName(); }
      else if (ev.type === 'library_changed') { if (link.libRev != null) link.libRev++; onLibraryChanged(); }
      renderBar();
    };
  };
  $('#conn').addEventListener('click', () => { if (!link.up) retryNow(); });
  $('#connretry').addEventListener('click', () => retryNow());
  window.addEventListener('online', () => { if (!link.up) retryNow(); });
  window.addEventListener('offline', () => { if (link.up) sendPing(); else showDown(); });
  // Phones and laptops suspend sockets in the background: check at once when the page is back.
  document.addEventListener('visibilitychange', () => { if (document.visibilityState !== 'visible') return; if (link.up) { link.awaiting = false; sendPing(); } else if (link.ever || link.retry) retryNow(); });
  connect();
})();
