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
    browserQueue: (() => { try { const q = JSON.parse(localStorage.getItem('pm.browserQueue') || '[]'); return Array.isArray(q) ? q.filter((it) => it && it.uri) : []; } catch { return []; } })(),
    browserIndex: -1,
    history: [],
  };

  // ---- helpers -------------------------------------------------------------
  const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  // A link from data (a homepage, a station's site, a pasted link) only when it is http or https: esc keeps
  // it inside the attribute, but a javascript: one would still run (security review 2026-10-27, #7).
  const webUrl = (u) => { if (!u) return ''; try { const x = new URL(String(u), location.href); return x.protocol === 'http:' || x.protocol === 'https:' ? x.href : ''; } catch { return ''; } };
  // Pubky Ring's deep links (pubkyauth:, pubkyring:) besides web ones
  const ringUrl = (u) => (/^pubky(auth|ring):\/\//i.test(String(u || '')) ? String(u) : webUrl(u));
  const openWeb = (u) => { const x = webUrl(u); if (x) window.open(x, '_blank', 'noopener'); };
  // Images that fail to load are removed, or hidden where they hold their place. One listener instead of
  // inline onerror handlers, which the page's Content-Security-Policy does not run.
  document.addEventListener('error', (e) => { const t = e.target; const how = t && t.dataset && t.dataset.err; if (how === 'remove') t.remove(); else if (how === 'hide') t.style.visibility = 'hidden'; }, true);
  const icon = (name, cls = 'ic') => `<svg class="${cls}" aria-hidden="true"><use href="#i-${name}"/></svg>`;
  const fmt = (ms) => {
    if (ms === Infinity) return 'Live'; // a radio stream has no length
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
  // A lossy file shows its codec, not the PCM it decodes to ("MP3", not "24/44.1k")
  const LOSSY = { mp3: 'MP3', aac: 'AAC', vorbis: 'Vorbis', opus: 'Opus' };
  const fileFormat = (file) => (!file ? '' : LOSSY[file.container] || fmtFormat(file.format));
  // "1 track", "2 tracks" (n may be a formatted number)
  const countOf = (n, many) => `${n} ${String(n).replace(/[^0-9]/g, '') === '1' ? many.replace(/s$/, '') : many}`;
  // Which service a page link belongs to ("Open on Audius", not "Open on")
  const SERVICE_HOSTS = { 'audius.co': 'audius', 'archive.org': 'archive', 'deezer.com': 'deezer', 'tidal.com': 'tidal', 'listen.tidal.com': 'tidal', 'open.spotify.com': 'spotify', 'jamendo.com': 'jamendo', 'musicbrainz.org': 'musicbrainz' };
  const serviceOfUrl = (u) => { try { return SERVICE_HOSTS[new URL(u).hostname.replace(/^www\./, '')] || ''; } catch { return ''; } };
  // A Pubky key as people read it: no "pubky" prefix, shortened in the middle; the copy button
  // copies all of it (owner, 2026-10-22)
  const bareKey = (k) => String(k || '').replace(/^pubky:?\/*/i, '').replace(/^pk:/i, '');
  const shortKey = (k) => { const b = bareKey(k); return b.length > 20 ? `${b.slice(0, 6)}…${b.slice(-10)}` : b; };
  const keyHtml = (k) => `<span class="pkey"><code title="${esc(bareKey(k))}">${esc(shortKey(k))}</code><button class="icon-btn sm" data-copykey="${esc(bareKey(k))}" title="Copy the whole key" aria-label="Copy the whole key">${icon('copy')}</button></span>`;
  document.addEventListener('click', (e) => { const b = e.target.closest && e.target.closest('[data-copylink]'); if (!b) return; e.preventDefault(); const v = b.dataset.copylink; (navigator.clipboard ? navigator.clipboard.writeText(v) : Promise.reject()).then(() => toast('Copied')).catch(() => toast(v)); }, true);
  document.addEventListener('click', (e) => { const b = e.target.closest && e.target.closest('[data-addmusic]'); if (!b) return; e.preventDefault(); setView('add-music'); });
  document.addEventListener('click', (e) => { const b = e.target.closest && e.target.closest('[data-copykey]'); if (!b) return; e.preventDefault(); e.stopPropagation(); const k = b.dataset.copykey; (navigator.clipboard ? navigator.clipboard.writeText(k) : Promise.reject()).then(() => toast('Key copied')).catch(() => toast(k)); }, true);
  const artists = (t) => (t.artists || []).map((a) => a.name).join(', ');
  const hue = (s) => { let h = 0; for (const c of String(s || '')) h = (h * 31 + c.charCodeAt(0)) >>> 0; return h % 360; };
  // The credit Wikipedia's text (CC BY-SA) and a Commons picture need: source, licence and a link to each
  const creditLink = (href, text) => (webUrl(href) ? `<a href="${esc(webUrl(href))}" target="_blank" rel="noopener">${esc(text)}</a>` : esc(text));
  const aboutCredit = (pg) => {
    const t = pg.about_text; const c = pg.image_credit; const lines = [];
    if (t && t.source) lines.push(`Text: ${t.url ? creditLink(t.url, t.source === 'Wikipedia' ? 'Read more on Wikipedia' : t.source) : esc(t.source)}${t.licence ? ` · ${creditLink(t.licence_url, t.licence)}` : ''}`);
    if (c) lines.push(`Picture: ${c.author ? `${esc(c.author)} · ` : ''}${c.licence ? `${creditLink(c.licence_url, c.licence)} · ` : ''}${creditLink(c.file_page, c.source || 'Wikimedia Commons')}`);
    return lines.length ? `<p class="muted small credit">${lines.join('<br>')}</p>` : '';
  };
  // Two letters for an artwork placeholder: the first word and the next real word ("I'm in Love…"
  // gives "IL", not "II", which reads as a pause button)
  const initials = (s) => {
    const w = (s || '?').trim().split(/\s+/).filter((x) => /[\p{L}\p{N}]/u.test(x));
    if (!w.length) return '?';
    const second = w.slice(1).find((x) => x.replace(/[^\p{L}\p{N}]/gu, '').length > 2) || w[1];
    const letter = (x) => (x.match(/[\p{L}\p{N}]/u) || [''])[0];
    const out = (letter(w[0]) + (second ? letter(second) : '')).toUpperCase();
    return out === 'II' ? w[0].slice(0, 2).toUpperCase() : out;
  };
  const tokenQ = () => (state.token ? '?token=' + encodeURIComponent(state.token) : '');
  // `svc`: where it plays from, as a badge in the cover's corner (coverBadge); none when null.
  // `quiet`: the badge is decoration (the art sits in a button that names the source itself).
  const art = (albumId, title, cls = '', svc = null, quiet = false) => {
    const h = hue(title);
    const bg = `background:linear-gradient(135deg, hsl(${h} 38% 30%), hsl(${(h + 40) % 360} 42% 18%))`;
    const img = albumId ? `<img loading="lazy" alt="" src="/v1/library/albums/${albumId}/cover${tokenQ()}" data-err="remove">` : '';
    return `<div class="art ${cls}" style="${bg}"><span>${esc(initials(title))}</span>${img}${coverBadge(svc, quiet)}</div>`;
  };
  // A playlist's art (UX review #8): four album covers as a 2×2 mosaic, one cover, or its initials
  // on a gradient; never an empty square.
  const mosaic = (covers, name, cls = '') => {
    const u = [...new Set((covers || []).filter(Boolean))].map(imgSrc);
    if (u.length >= 4) return `<div class="art mosaic ${cls}" aria-hidden="true">${u.slice(0, 4).map((x) => `<img loading="lazy" alt="" src="${esc(x)}" data-err="hide">`).join('')}</div>`;
    const h = hue(name);
    return `<div class="art ${cls}" aria-hidden="true" style="background:linear-gradient(135deg, hsl(${h} 38% 30%), hsl(${(h + 40) % 360} 42% 18%))"><span>${esc(initials(name))}</span>${u[0] ? `<img loading="lazy" alt="" src="${esc(u[0])}" data-err="remove">` : ''}</div>`;
  };
  // Smart playlists: the rules people can pick from, in words
  const SMART_FIELDS = {
    genre: { label: 'Genre', ops: [['contains', 'contains'], ['is', 'is']], input: 'text', hint: 'techno' },
    artist: { label: 'Artist', ops: [['contains', 'contains']], input: 'text', hint: 'name' },
    year: { label: 'Year', ops: [['between', 'between']], input: 'range' },
    favourite: { label: 'Favourite', ops: [['is', 'is']], input: 'yesno' },
    plays: { label: 'Times played', ops: [['at_least', 'at least'], ['at_most', 'at most']], input: 'number' },
    last_played: { label: 'Last played', ops: [['within_days', 'in the last'], ['not_within_days', 'not in the last']], input: 'days' },
    tag: { label: 'Your Pubky tag', ops: [['is', 'is']], input: 'text', hint: 'chill' },
  };
  const SMART_SORTS = [['random', 'Random (new each time)'], ['most_played', 'Most played'], ['least_recent', 'Longest unplayed'], ['newest', 'Newest'], ['title', 'Title']];
  const smartRuleRow = (r, i) => {
    const f = SMART_FIELDS[r.field] || SMART_FIELDS.genre;
    const val = r.value == null ? '' : r.value;
    const input = f.input === 'yesno' ? `<select class="input sm" data-k="value" aria-label="Favourite"><option value="yes" ${val !== false ? 'selected' : ''}>yes</option><option value="no" ${val === false ? 'selected' : ''}>no</option></select>`
      : f.input === 'range' ? `<input class="input sm" data-k="value" type="number" placeholder="from" value="${esc(val || '')}" aria-label="From year" style="width:90px"> and <input class="input sm" data-k="to" type="number" placeholder="to" value="${esc(r.to || '')}" aria-label="To year" style="width:90px">`
      : f.input === 'number' ? `<input class="input sm" data-k="value" type="number" min="0" value="${esc(val || 0)}" aria-label="Times" style="width:90px"> times`
      : f.input === 'days' ? `<input class="input sm" data-k="value" type="number" min="1" value="${esc(val || 30)}" aria-label="Days" style="width:90px"> days`
      : `<input class="input sm" data-k="value" value="${esc(val)}" placeholder="${esc(f.hint || '')}" aria-label="${esc(f.label)}">`;
    return `<div class="smrule" data-smrule="${i}"><select class="input sm" data-k="field" aria-label="What">${Object.entries(SMART_FIELDS).map(([k, x]) => `<option value="${k}" ${r.field === k ? 'selected' : ''}>${x.label}</option>`).join('')}</select><select class="input sm" data-k="op" aria-label="How">${f.ops.map(([k, l]) => `<option value="${k}" ${r.op === k ? 'selected' : ''}>${l}</option>`).join('')}</select>${input}<button class="icon-btn" data-smrm="${i}" aria-label="Remove this rule" title="Remove this rule">${icon('close')}</button></div>`;
  };
  const smartSummary = (x) => { const seeds = (x.seeds || []).map((sd) => sd.label || sd.kind); const rs = [...(seeds.length ? [`like ${seeds.join(', ')}`] : []), ...(x.rules || []).map((r) => { const f = SMART_FIELDS[r.field]; if (!f) return r.field; const op = (f.ops.find(([k]) => k === r.op) || f.ops[0])[1]; if (r.field === 'favourite') return r.value === false ? 'not a favourite' : 'a favourite'; if (r.field === 'year') return `year ${r.value || '…'}–${r.to || '…'}`; return `${f.label.toLowerCase()} ${op} ${r.value}${r.field === 'last_played' ? ' days' : ''}`; })]; return rs.join(x.matches === 'any' ? ' or ' : ' and ') + (x.minutes ? ` · ${x.minutes} min` : ''); };
  // Art for a queue item: a streaming source's image, else the library album cover; a badge for
  // where it plays from (`svc`; null for none), quiet when the art sits in a button.
  const itemArt = (it, cls = '', svc = null, quiet = false) => {
    if (it && it.image) { const h = hue(it.title || ''); return `<div class="art ${cls}" style="background:hsl(${h} 30% 22%)"><img loading="lazy" alt="" src="${esc(it.image)}" data-err="remove">${coverBadge(svc, quiet)}</div>`; }
    return art(albumIdOf(it), (it && (it.title || it.uri)) || '', cls, svc, quiet);
  };
  const toast = (msg, kind = '') => {
    const t = $('#toast');
    t.textContent = msg;
    t.className = `toast show ${kind}`;
    clearTimeout(toast.h);
    toast.h = setTimeout(() => { t.classList.remove('show'); toast.h = setTimeout(() => { t.textContent = ''; }, 400); }, kind === 'error' ? 6000 : 3800);
  };
  const setFill = (el) => { const min = +el.min || 0; const max = +el.max || 1; el.style.setProperty('--fill', `${((el.value - min) / (max - min)) * 100}%`); };

  // Errors in people's words: what happened and what to do, in one line. The node's own wording
  // passes through; raw service answers ("…answered 403 Forbidden: …") are translated, and the raw
  // text stays on the error as `raw` (UX review: wording leaks internals).
  const SVC_WORDS = { tidal: 'TIDAL', qobuz: 'Qobuz', soundcloud: 'SoundCloud', bandcamp: 'Bandcamp', spotify: 'Spotify', deezer: 'Deezer', jamendo: 'Jamendo', subsonic: 'Subsonic', jellyfin: 'Jellyfin', archive: 'The Internet Archive', audius: 'Audius', radio: 'The radio directory', pubky: 'Your homeserver', pinakes: 'Pinakes', listenbrainz: 'ListenBrainz' };
  const humanError = (raw, path = '') => {
    const text = String(raw || '');
    const svcKey = (/^\/sources\/([a-z]+)/.exec(path) || /^([a-z]+): /.exec(text) || [])[1];
    const who = SVC_WORDS[svcKey] || (svcKey ? svcKey[0].toUpperCase() + svcKey.slice(1) : 'The service');
    const where = svcKey && SVC_WORDS[svcKey] && svcKey !== 'pubky' && svcKey !== 'pinakes' ? ' · Sources' : '';
    if (/deserializ|missing field|invalid type|unknown variant|at line \d+ column/i.test(text)) return "The node didn't understand that request. Reload the page and try again.";
    if (/premium/i.test(text)) return `${who} needs a Premium plan for this${where}.`;
    if (/answered 401|unauthori[sz]ed|invalid[_ ]token|token (has )?expired/i.test(text)) return `${who} wants you to sign in again${where}.`;
    if (/answered 403|forbidden/i.test(text)) return `${who} refused this request (your plan or region may not include it)${where}.`;
    if (/answered 404|not found/i.test(text) && /answered/.test(text)) return `${who} doesn't have this any more.`;
    if (/answered 429|rate.?limit|too many requests/i.test(text)) return `${who} is busy; try again in a minute.`;
    if (/answered 5\d\d|bad gateway|service unavailable/i.test(text)) return `${who} isn't answering right now; try again later.`;
    if (/timed? ?out|deadline/i.test(text)) return `${who} took too long to answer; try again.`;
    if (/error sending request|connection refused|dns error|could not resolve/i.test(text)) return `Couldn't reach ${who === 'The service' ? 'the service' : who}; check the node's internet connection.`;
    return text;
  };
  const api = async (method, path, body) => {
    // A change made while the connection is down waits a few seconds for it, then is refused (nothing is sent).
    if (method !== 'GET') await whenConnected();
    // Switching a service on or off, signing in or out, or a new play order changes the ranking
    if (method !== 'GET' && path.startsWith('/me/prefs')) { state.prefsCache = null; state.prefsGen = (state.prefsGen || 0) + 1; }
    if (method !== 'GET' && /^\/(sources|play-order)\b/.test(path)) { state.svcRank = null; state.svcRankLoading = null; state.svcRankGen = (state.svcRankGen || 0) + 1; }
    let r;
    try {
      r = await fetch('/v1' + path, {
        method,
        headers: { 'content-type': 'application/json', ...(state.token ? { authorization: 'Bearer ' + state.token } : {}) },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
    } catch { linkHint(); throw new Error("Can't reach the node."); }
    // and again once the change has landed: a read that started meanwhile isn't kept either
    if (method !== 'GET' && path.startsWith('/me/prefs')) { state.prefsCache = null; state.prefsGen = (state.prefsGen || 0) + 1; }
    if (r.status === 401) { login(); throw new Error('The node wants an API token.'); }
    const text = await r.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { json = { error: text }; }
    if (!r.ok) { const raw = json && json.error ? json.error : `HTTP ${r.status}`; if (json && json.detail) console.warn(`${method} ${path}: ${json.detail}`); else if (raw !== humanError(raw, path)) console.warn(`${method} ${path}: ${raw}`); const err = new Error(humanError(raw, path)); err.raw = raw; err.status = r.status; throw err; }
    return json;
  };


  // ---- Library › Add music (STEWARD-BUNDLED-HOMESERVER.md §7): upload files, copy a folder, or use a folder in place ----
  // Uploads go one file at a time in 4 MiB chunks; a file's upload id is kept in this browser so a
  // reload or a dropped connection carries on where it stopped.
  const AUDIO_EXT = /\.(flac|mp3|m4a|mp4|aac|alac|ogg|oga|opus|wav|aiff?|wv|ape|dsf|dff|mka|wma)$/i;
  const up = { files: [], running: false };
  const upKey = (f) => `pm.up.${f.name}|${f.size}|${f.lastModified}`;
  const upGet = (k) => { try { return localStorage.getItem(k); } catch { return null; } };
  const upSet = (k, v) => { try { if (v == null) localStorage.removeItem(k); else localStorage.setItem(k, v); } catch { /* no storage: no resume */ } };
  const upTotals = () => { const t = { n: up.files.length, done: 0, failed: 0, bytes: 0, sent: 0 }; for (const u of up.files) { t.bytes += u.file.size; t.sent += u.done; if (u.state === 'stored' || u.state === 'already') t.done += 1; if (u.state === 'failed') t.failed += 1; } return t; };
  const drawUploads = () => {
    const box = $('#upbox'); if (!box || !up.files.length) { if (box) box.innerHTML = ''; return; }
    const t = upTotals(); const pct = t.bytes ? Math.round((100 * t.sent) / t.bytes) : 100;
    const word = { waiting: 'waiting', sending: 'sending', stored: 'added', already: 'already there', failed: 'failed' };
    const recent = up.files.slice(-30).reverse();
    box.innerHTML = `<div class="jobbar"><span style="width:${pct}%"></span></div><p class="small" role="status">${up.running ? 'Uploading' : 'Uploaded'} ${t.done} of ${countOf(t.n, 'files')} · ${fmtBytes(t.sent)} of ${fmtBytes(t.bytes)}${t.failed ? ` · ${t.failed} failed` : ''}</p>`
      + `<ul class="uplist">${recent.map((u) => `<li><span class="grow">${esc(u.rel || u.file.name)}</span><span class="muted small ${u.state === 'failed' ? 'joberr' : ''}" title="${esc(u.error || '')}">${u.state === 'sending' ? `${Math.round((100 * u.done) / (u.file.size || 1))}%` : word[u.state]}</span></li>`).join('')}</ul>`
      + (up.files.length > 30 ? `<p class="muted small">and ${countOf(up.files.length - 30, 'more files')}</p>` : '')
      + (!up.running ? '<button class="btn ghost sm" id="upclear">Clear the list</button>' : '');
    const c = $('#upclear', box); if (c) c.onclick = () => { up.files = []; drawUploads(); };
  };
  const sendOne = async (u) => {
    const f = u.file; const key = upKey(f);
    let id = upGet(key); let offset = 0; let chunk = 4 * 1024 * 1024;
    if (id) { try { const st = await api('GET', `/library/uploads/${id}`); if (st.state === 'stored') { u.done = f.size; u.state = 'stored'; upSet(key, null); return; } if (st.state === 'receiving') offset = st.offset || 0; else id = null; } catch { id = null; } }
    if (!id) {
      const r = await api('POST', '/library/uploads', { name: f.name, size: f.size, relative_path: u.rel || undefined });
      if (r.already) { u.done = f.size; u.state = 'already'; return; }
      id = r.id; offset = r.offset || 0; chunk = r.chunk_size || chunk; upSet(key, id);
    }
    for (;;) {
      const res = await fetch(`/v1/library/uploads/${encodeURIComponent(id)}?offset=${offset}`, { method: 'PUT', headers: { 'content-type': 'application/octet-stream', ...(state.token ? { authorization: 'Bearer ' + state.token } : {}) }, body: f.slice(offset, offset + chunk) });
      const j = await res.json().catch(() => ({}));
      if (res.status === 409 && j.offset != null) { offset = j.offset; continue; }
      if (!res.ok) throw new Error(humanError(j.error || `the node answered ${res.status}`, '/library/uploads'));
      offset = j.offset; u.done = Math.min(offset, f.size); drawUploads();
      if (j.state === 'failed') throw new Error(j.error || 'the homeserver did not take it');
      if (j.state === 'stored' || j.state === 'storing' || offset >= f.size) break;
    }
    u.state = 'stored'; upSet(key, null);
  };
  const runUploads = async () => {
    if (up.running) return; up.running = true; drawUploads();
    for (;;) {
      const u = up.files.find((x) => x.state === 'waiting'); if (!u) break;
      u.state = 'sending'; drawUploads();
      try { await sendOne(u); } catch (e) { u.state = 'failed'; u.error = e.message; }
      drawUploads();
    }
    up.running = false; drawUploads();
    const t = upTotals(); toast(t.failed ? `${countOf(t.done, 'files')} added, ${t.failed} failed` : `${countOf(t.done, 'files')} added to your library`, t.failed ? 'error' : undefined);
  };
  const addUploads = (files) => {
    const list = [...files].filter((f) => f.size > 0 && AUDIO_EXT.test(f.name));
    const skipped = files.length - list.length;
    for (const f of list) if (!up.files.some((x) => x.file.name === f.name && x.file.size === f.size && x.rel === (f.webkitRelativePath || f.relPath || '') && x.state !== 'failed')) up.files.push({ file: f, rel: f.webkitRelativePath || f.relPath || '', done: 0, state: 'waiting', error: null });
    if (skipped) toast(`${countOf(skipped, 'files')} left out: only music files are added`);
    if (list.length) runUploads(); else drawUploads();
  };
  // Dropped folders: walk them (webkitGetAsEntry), keeping each file's path inside the folder
  const filesFromDrop = async (dt) => {
    const entries = [...(dt.items || [])].map((it) => it.webkitGetAsEntry && it.webkitGetAsEntry()).filter(Boolean);
    if (!entries.length) return [...(dt.files || [])];
    const out = [];
    const walk = async (e, path) => {
      if (e.isFile) { const f = await new Promise((res, rej) => e.file(res, rej)).catch(() => null); if (f) { f.relPath = path + f.name; out.push(f); } return; }
      if (!e.isDirectory) return;
      const rd = e.createReader(); let batch;
      do { batch = await new Promise((res) => rd.readEntries(res, () => res([]))); for (const c of batch) await walk(c, `${path}${e.name}/`); } while (batch.length);
    };
    for (const e of entries) await walk(e, '');
    return out;
  };
  window.addEventListener('beforeunload', (e) => { if (up.running) { e.preventDefault(); e.returnValue = ''; } });

  // ---- Your library (owner, 2026-10-29; TASK-pinakes-social-ux G) ----
  // One place for the music: My music (what is on your homeserver, as songs, with saving and
  // tagging) and, for the node's owner, Node library (the folders, shares and drives the node plays
  // in place, scans, edits). One "Add music" button. Settings keeps settings only.
  // Settings-style rows, the same look as Settings
  const sHelp = (h) => (h ? `<p class="set-help">${h}</p>` : '');
  const sRow = (label, h, ctl, cls = '') => `<div class="set-row ${cls}"><div class="set-label"><div class="set-name">${label}</div>${sHelp(h)}</div>${ctl ? `<div class="set-ctl">${ctl}</div>` : ''}</div>`;
  const sBlock = (label, h, body) => `<div class="set-row set-block">${label || h ? `<div class="set-label">${label ? `<div class="set-name">${label}</div>` : ''}${sHelp(h)}</div>` : ''}<div class="set-body">${body}</div></div>`;
  const sGroup = (title, body) => `<div class="set-group">${title ? `<h3 class="set-h">${title}</h3>` : ''}<div class="set-rows">${body}</div></div>`;
  // The folders, shares and drives the node plays in place (was Settings › Library folders)
  const nodeLibraryHtml = ({ info, folders, ns, shares, drv }) => {
    const row = sRow; const block = sBlock; const group = sGroup;
      const scanErrs = info.last_scan && info.last_scan.error_count ? `<details class="scanerrs small"><summary>${countOf(info.last_scan.error_count, 'files')} could not be read in the last scan</summary><ul>${info.last_scan.errors.map(([fp, why]) => `<li><code>${esc(fp.split('/').slice(-2).join('/'))}</code>: ${esc(why.replace(fp, '').replace(/^[: ]+/, '') || 'unreadable')}</li>`).join('')}</ul></details>` : '';
      const library = group('Music folders',
        (folders.length ? folders.map(([fp, present]) => { const fromConfig = (ns.config_folders || []).includes(fp); return row(`<code class="path">${esc(fp)}</code>`, [present ? '' : 'Not found on this computer.', fromConfig ? 'From the node’s config (<code>node.toml</code>, or <code>PM_FOLDERS</code> in Docker), read at every start. To remove it, take it out there and restart the node; then Remove here forgets its songs.' : ''].filter(Boolean).join(' '), fromConfig ? '<span class="pill">From config</span>' : `<button class="btn sm ghost" data-folder-del="${esc(fp)}">Remove</button>`); }).join('') : row('No folders yet', 'Add the folder with your music below.', ''))
        + block('Add a folder', 'A folder on the node. It is read now and watched for new files; your files stay as they are.', `<div class="toolbar"><input class="input" id="folderadd" placeholder="/path/to/your/Music" aria-label="Folder to add"><button class="btn primary" id="folderbtn">${icon('plus')} Add folder</button></div>${scanErrs}`)
        + row('Check for changes', 'Reads every folder again; unchanged files are skipped.', '<button class="btn" id="scan">Scan now</button>'))
        + group('WebDAV shares', shares.map((sh) => row(esc(sh.name), `${esc(sh.url)} · ${countOf(sh.tracks, 'songs')}`, `<button class="btn sm" data-wd-scan="${esc(sh.id)}">Rescan</button><button class="btn sm ghost" data-wd-del="${esc(sh.id)}">Remove</button>`)).join('')
          + row('Add a WebDAV share', 'A music folder on Nextcloud, ownCloud or a NAS, played where it is (it is read again every 6 hours).', `<button class="btn" id="wdopen">${icon('plus')} Add share</button>`))
        + group('Cloud drives', drv.roots.map((r) => row(esc(r.name), `${r.provider === 'google' ? 'Google Drive' : 'OneDrive'} · ${countOf(r.tracks, 'songs')}`, `<button class="btn sm" data-dr-scan="${esc(r.id)}">Rescan</button><button class="btn sm ghost" data-dr-del="${esc(r.id)}">Remove</button>`)).join('')
          + drv.providers.map((p) => row(esc(p.name), !p.configured ? 'Not set up on this node: add its app in Settings › This node › Service apps' : p.connected ? `Signed in${p.user ? ` as ${esc(p.user)}` : ''}` : 'Sign in, then pick a music folder; it is read again every 6 hours.', !p.configured ? '' : p.connected ? `<button class="btn" data-dr-add="${esc(p.id)}" data-dr-name="${esc(p.name)}">${icon('plus')} Add a folder</button><button class="btn sm ghost" data-dr-out="${esc(p.id)}">Sign out</button>` : `<button class="btn" data-dr-login="${esc(p.id)}">Sign in</button>`)).join(''))
        + group('Fix my files\u2019 tags', block('', 'Write what the node knows into your files, so other players see it too: each artist on their own (not one artist called \u201cSefa, Dr. Peacock\u201d), and the MusicBrainz match of each song. You see every change first; each file\u2019s tags are saved before it changes, and the music itself is never touched.', '<div class="actions"><button class="btn" id="fixcheck">Check my files</button></div><div id="fixout" aria-live="polite"></div>'))
        + group('Identify', block('Match songs to MusicBrainz', 'Gives every file an identity that playlists, tags and other apps understand: IDs in the tags first, then the folder as an album (one MusicBrainz lookup for a whole album), then how it sounds (AcoustID) for the rest. Unsure matches, and songs where two ways disagree, wait for you.', '<div id="idcard"><div id="idbox" class="muted small">Loading…</div></div>'));
    return library;
  };
  const bindNodeLibrary = () => {
      // Fix my files' tags: the list first, then the write
      const loadFixes = async () => {
        const out = $('#fixout'); if (!out) return;
        out.innerHTML = '<div class="skeleton" style="height:60px;margin-top:10px"></div>';
        let r; try { r = await api('GET', '/library/tag-fixes?limit=50'); } catch (e) { out.innerHTML = `<p class="small warn-text">${esc(e.message)}</p>`; return; }
        const label = (k) => ({ ARTISTS: 'Artists', 'MUSICBRAINZ_TRACKID': 'MusicBrainz song', 'MusicBrainz Track Id': 'MusicBrainz song', 'MUSICBRAINZ_ALBUMID': 'MusicBrainz album', 'MusicBrainz Album Id': 'MusicBrainz album', 'MUSICBRAINZ_ARTISTID': 'MusicBrainz artists', 'MusicBrainz Artist Id': 'MusicBrainz artists', 'ACOUSTID_ID': 'AcoustID', 'Acoustid Id': 'AcoustID' }[k] || k);
        if (!r.total) { out.innerHTML = `<p class="small muted" style="margin-top:10px">Your files already say everything the node knows.${r.cannot ? ` (${countOf(r.cannot, 'files')} in a format the writer leaves alone.)` : ''}</p>`; return; }
        out.innerHTML = `<p class="small" style="margin-top:10px">${countOf(r.total, 'files')} would change${r.total > r.files.length ? ` (the first ${r.files.length} below)` : ''}${r.cannot ? `; ${countOf(r.cannot, 'files')} in a format the writer leaves alone` : ''}.</p>
          <div class="set-list fixlist">${r.files.map((f) => `<div class="set-item"><div class="grow"><div class="name">${esc(f.title)}</div><div class="sub">${f.changes.map((c) => `${esc(label(c.key))}: ${c.now.length ? `<s>${esc(c.now.join(' · '))}</s> → ` : ''}${esc(c.then.join(' · '))}`).join('<br>')}</div></div></div>`).join('')}</div>
          <div class="actions" style="margin-top:10px"><button class="btn primary" id="fixgo">Write into ${countOf(r.total, 'files')}</button></div>`;
        $('#fixgo').onclick = async () => { const b = $('#fixgo'); b.disabled = true; b.textContent = 'Writing…'; try { const w = await api('POST', '/library/tag-fixes', {}); toast(`${countOf(w.written, 'files')} fixed${w.skipped.length ? `; ${w.skipped.length} left as they were` : ''}. Backups: ${w.backup_dir}`); loadFixes(); } catch (e) { toast(e.message, 'error'); b.disabled = false; } };
      };
      const fc = $('#fixcheck'); if (fc) fc.onclick = loadFixes;
      $('#scan').onclick = async () => { try { await api('POST', '/library/scan', {}); toast('Scan started'); watchScan({ scanning: true }); } catch (e) { toast(e.message, 'error'); } };
      const addFolder = async () => { const p = $('#folderadd').value.trim(); if (!p) return; try { const r = await api('POST', '/library/folders', { path: p }); toast(r.scan_started ? 'Folder added; scanning' : 'Folder added; scanned after the current scan'); watchScan({ scanning: true }); render(); } catch (e) { toast(e.message, 'error'); } };
      $$('[data-dr-login]').forEach((b) => b.onclick = async () => { try { const r = await api('POST', `/library/drives/${b.dataset.drLogin}/login`); location.href = r.url; } catch (e) { toast(e.message, 'error'); } });
      $$('[data-dr-add]').forEach((b) => b.onclick = async () => { const r = await driveFolderDialog(b.dataset.drAdd, b.dataset.drName); if (r) { toast(`${r.name} added; scanning`); render(); } });
      $$('[data-dr-out]').forEach((b) => b.onclick = async () => { try { await api('DELETE', `/library/drives/${b.dataset.drOut}/account`); render(); } catch (e) { toast(e.message, 'error'); } });
      $$('[data-dr-scan]').forEach((b) => b.onclick = async () => { try { await api('POST', `/library/drives/roots/${b.dataset.drScan}/scan`); toast('Reading the folder again'); } catch (e) { toast(e.message, 'error'); } });
      $$('[data-dr-del]').forEach((b) => b.onclick = async () => { if (!confirm('Remove this folder and its songs from the library? The files on the drive stay.')) return; try { const r = await api('DELETE', `/library/drives/roots/${b.dataset.drDel}`); toast(`Removed, with ${countOf(r.tracks, 'songs')}`); render(); } catch (e) { toast(e.message, 'error'); } });
      $('#wdopen').onclick = async () => { const sh = await webdavDialog(); if (sh) { toast(`${sh.name} added; scanning`); render(); } };
      $$('[data-wd-scan]').forEach((b) => b.onclick = async () => { try { await api('POST', `/library/webdav/${b.dataset.wdScan}/scan`); toast('Reading the share again'); } catch (e) { toast(e.message, 'error'); } });
      $$('[data-wd-del]').forEach((b) => b.onclick = async () => { if (!confirm('Remove this share and its songs from the library? The files on the share stay.')) return; try { const r = await api('DELETE', `/library/webdav/${b.dataset.wdDel}`); toast(`Removed, with ${countOf(r.tracks, 'songs')}`); render(); } catch (e) { toast(e.message, 'error'); } });
      $('#folderbtn').onclick = addFolder; $('#folderadd').onkeydown = (e) => { if (e.key === 'Enter') addFolder(); };
      $$('[data-folder-del]', main).forEach((b) => b.onclick = async () => { if (!b.dataset.armed) { b.dataset.armed = '1'; b.textContent = 'Really remove?'; setTimeout(() => { if (b.isConnected) { delete b.dataset.armed; b.textContent = 'Remove'; } }, 3000); return; } try { const r = await api('DELETE', '/library/folders?path=' + encodeURIComponent(b.dataset.folderDel)); toast(`Folder removed · ${countOf(r.removed_tracks, 'tracks')} gone`); render(); } catch (e) { toast(e.message, 'error'); } });
    renderIdentify();
  };
  // Playlists from files and moving from Roon (was Settings › Import)
  const importHtml = () => {
    const block = sBlock; const group = sGroup;
      const importTab = group('Move from Roon', block('', '', `<ol class="muted small steps"><li>In Roon, open Tracks (and Albums), select all, then ⋯ › Export › Excel. Export each playlist you want the same way.</li><li>Choose the files here and check them: nothing changes yet.</li><li>Import what matched.</li></ol><input type="file" id="roonfiles" accept=".xlsx,.csv" multiple aria-label="Roon export files"><label class="check small"><input type="checkbox" id="roonfav" checked> Roon favourites become Favourites</label><label class="check small"><input type="checkbox" id="roonpl" checked> Playlist exports become playlists</label><label class="check small"><input type="checkbox" id="roontags"> Roon tags become tags on Pubky <span class="muted">(public)</span></label><div class="toolbar"><button class="btn" id="rooncheck">${icon('search')} Check</button><button class="btn primary" id="roongo" disabled>${icon('download')} Import</button></div><div id="roonout" aria-live="polite"></div>`));
    return group('Playlists from files', sBlock('', 'An M3U or M3U8 list, a CSV, or a Roon playlist export: each line is matched to a song (MusicBrainz, ISRC, then title and artist); what doesn’t match stays as its line.', `<label class="btn file-btn">${icon('upload')} Choose a playlist file<input type="file" id="libplimport" accept=".m3u,.m3u8,.csv,.xlsx"></label><div id="libplout" class="small" aria-live="polite"></div>`)
      + sRow('Playlists on a service', 'TIDAL, Spotify, Deezer and the others: open the playlist on Sources and choose Import.', '<button class="btn ghost" data-view-go="sources">Open Sources</button>'))
      + importTab;
  };
  const bindImport = () => {
    const pi = $('#libplimport');
    if (pi) pi.onchange = async (e) => {
      const f = e.target.files[0]; if (!f) return; const out = $('#libplout');
      out.innerHTML = '<span class="spin"></span> Importing…';
      try {
        const r = await fetch('/v1/playlists/import/file?file=' + encodeURIComponent(f.name), { method: 'POST', headers: { 'content-type': 'application/octet-stream', ...(state.token ? { authorization: 'Bearer ' + state.token } : {}) }, body: await f.arrayBuffer() });
        const j = await r.json(); if (!r.ok) throw new Error(j.error || r.status);
        out.innerHTML = `Imported “${esc(j.name)}”: ${j.imported} of ${j.parsed} matched${j.unresolved.length ? ` · not matched: ${esc(j.unresolved.slice(0, 4).map((d) => d.title).join(', '))}` : ''} · <button class="lnk" data-view-go="playlists">Open Playlists</button>`;
        $$('[data-view-go]', out).forEach((b) => b.onclick = () => setView(b.dataset.viewGo));
        state.playlists = await api('GET', '/playlists').catch(() => state.playlists);
      } catch (er) { out.innerHTML = `<span class="warn-text">${esc(er.message)}</span>`; }
      e.target.value = '';
    };
      // Move from Roon: a dry run per file, then the import of what matched (roon_import.rs).
      const roonRun = async (dry) => {
        const files = [...($('#roonfiles').files || [])]; const out = $('#roonout');
        if (!files.length) { toast('Choose the Roon export files first', 'error'); return; }
        const opts = `favourites=${$('#roonfav').checked}&playlist=${$('#roonpl').checked}&tags=${$('#roontags').checked}&dry_run=${dry}`;
        out.innerHTML = '<div class="skeleton" style="height:60px"></div>';
        const parts = []; let matched = 0;
        for (const f of files) {
          try {
            const res = await fetch(`/v1/import/roon?file=${encodeURIComponent(f.name)}&${opts}`, { method: 'POST', headers: { 'content-type': 'application/octet-stream', ...(state.token ? { authorization: 'Bearer ' + state.token } : {}) }, body: await f.arrayBuffer() });
            const r = await res.json(); if (!res.ok) throw new Error(r.error || res.status);
            matched += r.matched;
            const kind = { tracks: 'Tracks', albums: 'Albums', playlist: `Playlist “${r.name || f.name}”` }[r.kind] || r.kind;
            const did = [r.favourites ? `${r.favourites} favourite${r.favourites === 1 ? '' : 's'}${dry ? ' to add' : ' added'}` : '', r.tags_added ? `${r.tags_added} tag${r.tags_added === 1 ? '' : 's'}${dry ? ' to put on Pubky' : ' put on Pubky'}` : '', r.playlist ? `playlist made with ${countOf(r.playlist.imported, 'songs')}` : '', r.not_identified ? `${r.not_identified} need identifying first` : ''].filter(Boolean).join(' · ');
            parts.push(`<div class="roonfile"><div class="name">${esc(f.name)} <span class="muted small">${esc(kind)}</span></div><p class="small">${r.matched} of ${r.rows} matched this library${did ? ' · ' + esc(did) : ''}</p>`
              + (r.tags_found.length ? `<p class="muted small">Roon tags: ${r.tags_found.slice(0, 12).map(([l, c]) => `${esc(l)} (${c})`).join(', ')}${r.tags_found.length > 12 ? '…' : ''}</p>` : '')
              + (r.misses.length ? `<details class="small"><summary>${r.unmatched + r.not_identified} not moved, and why</summary><ul class="misses">${r.misses.map((m) => `<li><strong>${esc(m.what)}</strong>: ${esc(m.reason)}</li>`).join('')}</ul></details>` : '') + '</div>');
          } catch (e) { parts.push(`<div class="roonfile"><div class="name">${esc(f.name)}</div><p class="small joberr">${esc(e.message)}</p></div>`); }
        }
        out.innerHTML = (dry ? '<p class="muted small">Checked: nothing changed yet.</p>' : '<p class="small">Imported.</p>') + parts.join('');
        $('#roongo').disabled = !dry || !matched;
        if (dry && !matched) out.insertAdjacentHTML('afterbegin', '<p class="small">Nothing in these files matched this library yet: scan your music folders first (Your library › Node library), then check again.</p>');
        if (!dry) { state.playlists = await api('GET', '/playlists').catch(() => state.playlists); }
      };
      $('#roonfiles').onchange = () => { $('#roongo').disabled = true; $('#roonout').innerHTML = ''; };
      $('#rooncheck').onclick = () => roonRun(true);
      $('#roongo').onclick = () => roonRun(false);
  };
  // The chosen tab's content, as the tab panel the tabs control
  const libPanel = (tab, html) => `<div id="libpanel" role="tabpanel" aria-labelledby="libtab-${tab}">${html}</div>`;
  // "Saved · 2 min ago": where saving to the homeserver stands, in a few words
  const savedWords = (sv, pk) => (!sv ? ''
    : pk && pk.sync && pk.sync.running ? `<span class="spin"></span> ${esc(pk.sync.step || 'Saving')}…`
      : sv.state === 'saving' ? `<span class="spin"></span> Saving ${countOf(sv.pending, 'changes')}…`
        : sv.state === 'error' ? `<span class="warn-text">Not saved${sv.error ? `: ${esc(sv.error)}` : ''}</span>`
          : sv.pending ? `${countOf(sv.pending, 'changes')} to save`
            : `Saved${sv.last_saved_at ? ` · ${ago(sv.last_saved_at)}` : ''}`);
  // Edit a song's tags on the node (the file is not touched)
  // `e`: GET /library/tracks/{id}/edit: the song's edit (in the fields) and what the file says
  // (shown as each field's placeholder); an empty field is the file's value
  const editTagsDialog = (e) => new Promise((resolve) => {
    const d = document.createElement('dialog'); d.className = 'dialog';
    const f = e.file || {}; const ed = e.edit || {};
    const field = (k, label) => `<label class="field"><span>${label}</span><input class="input" data-k="${k}" value="${esc(ed[k] || '')}" placeholder="${esc(f[k] || '')}"></label>`;
    d.innerHTML = `<h2>Edit tags</h2><p class="muted small">Kept on this node; the file stays as it is. An empty field shows what the file says.</p>
      ${field('title', 'Title')}${field('artist', 'Artist')}${field('album', 'Album')}${field('album_artist', 'Album artist')}
      <div class="dialog-actions"><button class="btn ghost" data-undo>Back to the file’s tags</button><button class="btn ghost" data-close>Cancel</button><button class="btn primary" data-save>Save</button></div>`;
    document.body.appendChild(d);
    const tr = { open: () => d.open, close: (fromBack) => done(null, fromBack) };
    const done = (v, fromBack) => { const k = transients.indexOf(tr); if (k >= 0) transients.splice(k, 1); if (d.open) { if (d.close) d.close(); else d.removeAttribute('open'); } d.remove(); if (fromBack !== true) popTransient(); resolve(v); };
    transients.push(tr); pushTransient();
    $('[data-close]', d).onclick = () => done(null);
    $('[data-undo]', d).onclick = () => done({ undo: true });
    $('[data-save]', d).onclick = () => done(Object.fromEntries($$('[data-k]', d).map((i) => [i.dataset.k, i.value.trim()])));
    d.addEventListener('cancel', (e) => { e.preventDefault(); done(null); });
    if (d.showModal) d.showModal(); else d.setAttribute('open', '');
    $('[data-k]', d).focus();
  });
  const editTrackTags = async (id) => {
    let cur; try { cur = await api('GET', `/library/tracks/${id}/edit`); } catch (e) { return toast(e.message, 'error'); }
    const v = await editTagsDialog(cur); if (!v) return;
    try {
      if (v.undo) await api('DELETE', `/library/tracks/${id}/edit`); else await api('PUT', `/library/tracks/${id}/edit`, v);
      toast(v.undo ? 'Back to the file’s tags' : 'Saved on this node'); onLibraryChanged(); if (state.view === 'library') render();
    } catch (e) { toast(e.message, 'error'); }
  };
  const removeFromLibrary = async (id, title) => {
    const ok = await choiceDialog({ title: 'Remove from the library?', text: `“${title || 'This song'}” leaves the library; the file stays where it is and scans skip it. Your library › Node library can bring it back.`, choices: [{ label: 'Remove', value: true, primary: true }] });
    if (!ok) return;
    try { await api('DELETE', `/library/tracks/${id}`); toast('Removed from the library'); onLibraryChanged(); render(); } catch (e) { toast(e.message, 'error'); }
  };
  const editAlbumTags = async (albumId, title, artist) => {
    const v = await choiceDialog({ title: 'Rename the album', text: 'Kept on this node; the files stay as they are.', field: { label: 'Album title', value: title || '' }, choices: [{ label: 'Save', value: 'save', primary: true }] });
    if (!v || !v.text) return;
    try { await api('PUT', `/library/albums/${albumId}/edit`, { title: v.text, ...(artist != null ? { album_artist: artist } : {}) }); toast('Album renamed on this node'); onLibraryChanged(); render(); } catch (e) { toast(e.message, 'error'); }
  };
  // "Add music": one button, three ways in
  const addMusicDialog = async () => {
    const owner = isOwner();
    const pick = await choiceDialog({ title: 'Add music', text: 'Wherever the files are, they’re one library: albums, search and playback don’t change.', choices: [
      { label: 'Upload files', note: 'From this device to your homeserver, kept privately; a cut connection carries on where it stopped.', value: 'upload', primary: true },
      ...(owner ? [{ label: 'Add a folder or network share', note: 'Played where it is: a folder on the node’s machine, a NAS (WebDAV) or a cloud drive. Checked before it is saved.', value: 'folder' }] : []),
      ...(owner ? [{ label: 'Import playlists', note: 'M3U, CSV or a Roon export; service playlists from their page in Sources.', value: 'import' }] : []),
    ] });
    if (pick === 'upload') { state.libUpload = true; if (state.view === 'library' && (state.viewArg || 'mine') === 'mine') render(); else setView('library', null); }
    if (pick === 'folder') addPlaceDialog();
    if (pick === 'import') setView('library', 'import');
  };
  const addPlaceDialog = async () => {
    const how = await choiceDialog({ title: 'Add a folder or network share', text: 'The node reads it now and plays from it; nothing is copied.', choices: [
      { label: 'A folder on the node’s machine', note: 'A disk or a mounted share (USB, NAS, SMB mounted on the machine).', value: 'folder', primary: true },
      { label: 'A WebDAV share', note: 'Nextcloud, ownCloud or a NAS over the network, with its login.', value: 'webdav' },
      { label: 'A cloud drive', note: 'Google Drive or OneDrive: sign in on Node library.', value: 'drive' },
    ] });
    if (how === 'folder') {
      const v = await choiceDialog({ title: 'Add a folder', text: 'Its path on the node’s machine. The node checks it is there before saving it.', field: { label: 'Folder', value: '' }, choices: [{ label: 'Add folder', value: 'add', primary: true }] });
      if (!v || !v.text) return;
      try { const r = await api('POST', '/library/folders', { path: v.text }); toast(r.scan_started ? 'Folder added: reading your files' : 'Folder added'); watchScan({ scanning: true }); setView('library', 'node'); } catch (e) { toast(e.message, 'error'); }
    }
    if (how === 'webdav') { const sh = await webdavDialog(); if (sh) { toast(`${sh.name} added; scanning`); setView('library', 'node'); } }
    if (how === 'drive') setView('library', 'node');
  };

  // A small dialog with a few choices; resolves to the picked value, or null (Esc, Cancel).
  // With `field` ({label, value}) it shows a text field and resolves to {value, text}.
  const choiceDialog = ({ title, text, choices, field }) => new Promise((resolve) => {
    const d = document.createElement('dialog'); d.className = 'dialog';
    d.innerHTML = `<h2>${esc(title)}</h2>${text ? `<p class="muted">${esc(text)}</p>` : ''}${field ? `<label class="dialog-field"><span class="muted small">${esc(field.label)}</span><input class="input" data-field value="${esc(field.value || '')}" aria-label="${esc(field.label)}"></label>` : ''}<div class="choices">${choices.map((c, i) => `<button class="btn choice ${c.primary ? 'primary' : ''}" data-ci="${i}"><span class="choice-label">${esc(c.label)}</span>${c.note ? `<span class="choice-note">${esc(c.note)}</span>` : ''}</button>`).join('')}</div><div class="dialog-actions"><button class="btn ghost" data-ci="-1">Cancel</button></div>`;
    document.body.appendChild(d);
    const tr = { open: () => d.open, close: (fromBack) => done(null, fromBack) };
    const done = (v, fromBack) => { const k = transients.indexOf(tr); if (k >= 0) transients.splice(k, 1); if (d.open) { if (d.close) d.close(); else d.removeAttribute('open'); } d.remove(); if (fromBack !== true) popTransient(); resolve(v); };
    transients.push(tr); pushTransient();
    $$('[data-ci]', d).forEach((b) => b.onclick = () => { const i = Number(b.dataset.ci); if (i < 0) return done(null); const v = choices[i].value; done(field ? { value: v, text: $('[data-field]', d).value.trim() } : v); });
    d.addEventListener('cancel', (e) => { e.preventDefault(); done(null); });
    if (d.showModal) d.showModal(); else d.setAttribute('open', ''); const first = $('.choice', d); if (first) first.focus();
  });
  // Comments (TASK-pinakes-social-ux.md B1): social v1 notes whose parent is the item's link, read
  // from Nexus by parent; reply to one, delete your own. One handler per section, by delegation.
  const commentRow = (c) => `<li class="comment" data-cpost="${esc(c.post)}"><div class="comment-head"><b>${esc(c.name || `${c.author.slice(0, 8)}…`)}</b> <span class="muted small">${c.at ? esc(ago(Math.floor(c.at / 1000))) : ''}</span></div><p class="comment-text">${esc(c.content)}</p><div class="toolbar">${c.replies ? `<button class="btn ghost sm" data-creplies="${esc(c.post)}">${countOf(c.replies, 'replies')}</button>` : ''}<button class="btn ghost sm" data-creply="${esc(c.post)}">Reply</button>${c.mine ? `<button class="btn ghost sm" data-cdel="${esc(c.post)}">Delete</button>` : ''}</div><div class="comment-sub"></div></li>`;
  const commentForm = (label) => `<form class="comment-form"><textarea class="input" name="content" rows="2" maxlength="2000" placeholder="${esc(label)}" aria-label="${esc(label)}" required></textarea><button class="btn primary sm" disabled>Post</button></form>`;
  // "Post about this" (B2): a public social v1 note with the item as its embed, so any Pubky app
  // (pubky.app among them) shows it with the item's card.
  const postAbout = async (uri, title) => {
    const r = await choiceDialog({ title: 'Post about this', text: `A public post on your homeserver with ${title ? `“${title}”` : 'this'} attached; Pubky apps show it with its card.`, field: { label: 'What you want to say', value: '' }, choices: [{ label: 'Post', value: true, primary: true }] });
    if (!r || !r.value) return;
    if (!r.text) { toast('Write something to post', 'error'); return; }
    try { await api('POST', '/posts', { uri, content: r.text }); toast('Posted'); } catch (e) { toast(e.message, 'error'); }
  };
  // What someone is typing survives the page's re-renders (an item still being read renders
  // again every few seconds): kept per item, and per comment for a reply
  const commentDrafts = new Map();
  const commentTyping = () => $$('.comment-form textarea').some((t) => t.value.trim() || t === document.activeElement);
  const mountComments = (sec) => {
    const uri = sec.dataset.curi; const body = $('.comments-body', sec);
    const draftKey = (f) => `${uri}|${(f && f.dataset.replyTo) || ''}`;
    const fill = (f, text) => { if (!f || !text) return; f.elements.content.value = text; f.querySelector('button').disabled = false; };
    const openReply = (li, post) => {
      const sub = $('.comment-sub', li); if (!$(':scope > form', sub)) { sub.insertAdjacentHTML('afterbegin', commentForm('Reply')); $(':scope > form', sub).dataset.replyTo = post; }
      return $(':scope > form', sub);
    };
    const show = async () => {
      let t; try { t = await api('GET', `/comments?uri=${encodeURIComponent(uri)}`); } catch (e) { body.innerHTML = `<p class="muted small">${esc(e.message)}</p>`; return; }
      if (!sec.isConnected) return;
      body.innerHTML = `${commentForm('Add a comment')}${t.comments.length ? `<ul class="comments">${t.comments.map(commentRow).join('')}</ul>` : `<p class="muted small">No comments yet.</p>`}<p class="muted small">A comment is a public post on your homeserver, so every Pubky app shows it.</p>`;
      fill($('form.comment-form', body), commentDrafts.get(`${uri}|`));
      for (const [k, text] of commentDrafts) {
        const [u, post] = [k.slice(0, k.lastIndexOf('|')), k.slice(k.lastIndexOf('|') + 1)];
        const li = u === uri && post ? $$('li.comment', body).find((x) => x.dataset.cpost === post) : null;
        if (li) fill(openReply(li, post), text);
      }
    };
    const replies = async (li, post) => {
      let t; try { t = await api('GET', `/comments?post=${encodeURIComponent(post)}`); } catch (e) { toast(e.message, 'error'); return; }
      $('.comment-sub', li).innerHTML = t.comments.length ? `<ul class="comments">${t.comments.map(commentRow).join('')}</ul>` : '<p class="muted small">No replies yet.</p>';
    };
    // Post wakes up once there is something to post
    sec.addEventListener('input', (e) => {
      const f = e.target.closest('form.comment-form'); if (!f) return;
      const text = f.elements.content.value; f.querySelector('button').disabled = !text.trim();
      if (text.trim()) commentDrafts.set(draftKey(f), text); else commentDrafts.delete(draftKey(f));
    });
    sec.addEventListener('submit', async (e) => {
      const f = e.target.closest('form.comment-form'); if (!f) return; e.preventDefault();
      const content = f.elements.content.value.trim(); if (!content) return;
      const b = f.querySelector('button'); b.disabled = true;
      try {
        await api('POST', '/comments', f.dataset.replyTo ? { reply_to: f.dataset.replyTo, content } : { uri, content });
        commentDrafts.delete(draftKey(f));
        toast(f.dataset.replyTo ? 'Reply posted' : 'Comment posted');
        if (f.dataset.replyTo) await replies(f.closest('li.comment'), f.dataset.replyTo); else await show();
      } catch (err) { b.disabled = false; toast(err.message, 'error'); }
    });
    sec.addEventListener('click', async (e) => {
      const r = e.target.closest('[data-creply]');
      if (r) { const sub = $('.comment-sub', r.closest('li.comment')); if (!$(':scope > form', sub)) openReply(r.closest('li.comment'), r.dataset.creply).elements.content.focus(); return; }
      const rp = e.target.closest('[data-creplies]'); if (rp) { await replies(rp.closest('li.comment'), rp.dataset.creplies); return; }
      const d = e.target.closest('[data-cdel]');
      if (d) {
        const go = await choiceDialog({ title: 'Delete this comment?', text: 'It goes from your homeserver; apps that already showed it drop it once Nexus sees it gone.', choices: [{ label: 'Delete', value: true, primary: true }] });
        if (!go) return;
        d.disabled = true;
        try { await api('DELETE', `/comments?post=${encodeURIComponent(d.dataset.cdel)}`); toast('Comment deleted'); d.closest('li.comment').remove(); } catch (err) { d.disabled = false; toast(err.message, 'error'); }
      }
    });
    show();
  };
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
    try { const r = await api('POST', `/sources/${service}/playlists/${encodeURIComponent(id)}/import${how === 'keep' ? '?keep_source=true' : ''}`); state.playlists = await api('GET', '/playlists'); toast(`Imported “${r.playlist.summary.name}”: ${countOf(r.imported, 'songs')}${r.skipped ? `, ${r.skipped} skipped` : ''}${how === 'keep' ? `, pinned to ${name}` : ''}`); }
    catch (e) { toast(e.message, 'error'); }
    b.disabled = false;
  };
  // Theme: System (no attribute), or Light/Dark kept in this browser (index.html applies it early).
  const themePref = () => { try { return localStorage.getItem('pm.theme') || 'system'; } catch { return 'system'; } };
  // One design at three sizes (docs/ux/responsive.md): phone under 600 px (bottom tabs and a mini
  // player), tablet to 1024 px (a rail), desktop above (the sidebar). Settings › Appearance › Layout
  // can force Compact (the phone arrangement at any width) or Comfortable (the tablet one on a desktop).
  const layoutPref = () => { try { return localStorage.getItem('pm.layout') || 'auto'; } catch { return 'auto'; } };
  const shellFor = (w, pref) => (pref === 'compact' || w < 600 ? 'phone' : pref === 'comfortable' || w <= 1024 ? 'tablet' : 'desktop');
  const shellMode = () => document.documentElement.dataset.shell || 'desktop';
  const densePref = () => { try { return localStorage.getItem('pm.dense') === '1'; } catch { return false; } };
  const setTheme = (v) => { try { if (v === 'system') localStorage.removeItem('pm.theme'); else localStorage.setItem('pm.theme', v); } catch { /* no storage */ } if (v === 'system') delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = v; };
  // A button that needs text in a field: disabled (with the reason) until there is some.
  const needsText = (input, btn) => { if (!input || !btn) return; const sync = () => { const empty = !input.value.trim(); btn.disabled = empty; btn.title = empty ? `Type in “${input.placeholder || 'the field'}” first` : ''; }; input.addEventListener('input', sync); sync(); };
  // Copy files to the homeserver's private storage as a node job: how much first, then progress.
  const fmtBytes = (b) => (b >= 1e9 ? `${(b / 1e9).toFixed(1)} GB` : `${Math.max(1, Math.round(b / 1e6))} MB`);
  const startUpload = async (body) => {
    let plan; try { plan = await api('POST', '/pubky/files/upload', { ...body, dry_run: true }); } catch (e) { return toast(e.message, 'error'); }
    const go = await choiceDialog({ title: 'Copy to your homeserver', text: `${plan.total} file${plan.total === 1 ? '' : 's'}, ${fmtBytes(plan.bytes_total)}, into your homeserver's private storage (never public). Files already there are skipped.`, choices: [{ label: 'Copy', note: 'Runs on the node; you can leave this page.', value: true, primary: true }] });
    if (!go) return;
    let job; try { job = await api('POST', '/pubky/files/upload', body); } catch (e) { return toast(e.message, 'error'); }
    watchJob(job.id);
  };
  // The homeserver's private files in Settings: sizes that read right for small files, and the
  // broken ones (empty, or a second copy under a doubled name) marked with Remove.
  const fileSize = (b) => (b == null ? '' : b >= 1e9 ? `${(b / 1e9).toFixed(1)} GB` : b >= 1e6 ? `${(b / 1e6).toFixed(1)} MB` : b >= 1e3 ? `${Math.round(b / 1e3)} KB` : `${b} bytes`);
  const brokenWhy = (f, names) => {
    if (f.size === 0) return 'Empty: copy the library again to send it again';
    const n = f.path.split('/').pop(); const m = /^([0-9a-f]{16})-(.+)$/.exec(n);
    if (m && m[2].startsWith(`${m[1]}-`) && names.has(m[2])) return 'A second copy (doubled name)';
    return '';
  };
  const drawHomeFiles = (box, files) => {
    if (!files.length) { box.innerHTML = '<span>No private files on the homeserver yet. Copy the library, or an album from its ⋯ menu; audio only ever goes under /priv/.</span>'; return; }
    const names = new Set(files.map((f) => f.path.split('/').pop()));
    const rows = files.map((f) => ({ f, why: brokenWhy(f, names) })).sort((a, b) => (b.why ? 1 : 0) - (a.why ? 1 : 0));
    const broken = rows.filter((r) => r.why);
    const total = files.reduce((n, f) => n + (f.size || 0), 0);
    box.innerHTML = `<div class="eyebrow">Private files on the homeserver · ${files.length} · ${fileSize(total)}</div>`
      + (broken.length ? `<div class="set-warn small">${countOf(broken.length, 'files')} ${broken.length === 1 ? 'is' : 'are'} broken or a second copy. <button class="btn ghost sm" data-rmbroken>Remove ${broken.length === 1 ? 'it' : 'them'}</button></div>` : '')
      + `<div class="list compact">${rows.slice(0, 50).map(({ f, why }) => `<div class="list-row${why ? ' broken' : ''}"><div class="grow"><div class="name">${esc(f.path.split('/').pop())}</div>${why ? `<div class="muted small">${esc(why)}</div>` : ''}</div><span class="muted small">${fileSize(f.size)}</span>${why ? `<button class="btn ghost sm" data-rmfile="${esc(f.path)}">Remove</button>` : ''}</div>`).join('')}${files.length > 50 ? `<div class="muted small">… and ${files.length - 50} more</div>` : ''}</div>`;
    const removeFiles = async (paths, btn) => { btn.disabled = true; try { drawHomeFiles(box, await api('DELETE', '/pubky/files', { paths })); toast(`Removed ${countOf(paths.length, 'files')} from your homeserver`); } catch (e) { toast(e.message, 'error'); btn.disabled = false; } };
    const all = $('[data-rmbroken]', box); if (all) all.onclick = () => removeFiles(broken.map((r) => r.f.path), all);
    $$('[data-rmfile]', box).forEach((b) => b.onclick = () => removeFiles([b.dataset.rmfile], b));
  };
  // "Bring files here": how much first (and what is here already), where to, then a job.
  const bringFiles = async () => {
    let plan; try { plan = await api('POST', '/pubky/files/mirror', { dry_run: true }); } catch (e) { return toast(e.message, 'error'); }
    const broken = plan.broken ? ` ${countOf(plan.broken, 'files')} on the homeserver are empty or a second copy; they are left out (remove them below).` : '';
    if (!plan.total) return toast(`Nothing to bring: ${countOf(plan.skipped, 'files')} already on this node.${broken}`);
    const go = await choiceDialog({
      title: 'Bring files here',
      text: `${countOf(plan.total, 'files')} (${fmtBytes(plan.bytes_total)}) to bring${plan.skipped ? `, ${plan.skipped} already here` : ''}. They go into this folder, then into your library.${broken}`,
      field: { label: 'Folder', value: plan.dest },
      choices: [{ label: 'Bring them', note: 'Runs on the node at your copy speed; you can leave this page.', value: true, primary: true }],
    });
    if (!go) return;
    let job; try { job = await api('POST', '/pubky/files/mirror', go.text && go.text !== plan.dest ? { dest: go.text } : {}); } catch (e) { return toast(e.message, 'error'); }
    if (job.finished) return toast(`Nothing to bring: ${countOf(job.skipped, 'files')} already here.`);
    watchJob(job.id);
  };
  // A running files job (copy or bring) is a compact chip in the player bar; tapping it shows the
  // details with Cancel. Opened menus stay on top of it (overlap brief: no floating card).
  const fjob = { id: null, p: null, timer: null };
  const jobTitle = (p) => (p.kind === 'import' ? (p.finished ? (p.cancelled ? 'Copy stopped' : 'Copied into your library') : 'Copying into your library') : p.kind === 'bring'
    ? (p.finished ? (p.cancelled ? 'Stopped bringing files' : 'Brought files here') : 'Bringing files here')
    : (p.finished ? (p.cancelled ? 'Copy stopped' : 'Copied to your homeserver') : 'Copying to your homeserver'));
  const jobPct = (p) => (p.bytes_total ? Math.min(100, Math.round((100 * p.bytes_done) / p.bytes_total)) : (p.finished ? 100 : 0));
  const jobPop = () => $('#jobpop');
  const closeJobPop = (fromBack) => { const jp = jobPop(); if (!jp || jp.hidden) return; jp.hidden = true; $('#scrim').hidden = true; if (fromBack !== true) popTransient(); if (fjob.p && fjob.p.finished) hideJobChip(); };
  const hideJobChip = () => { const c = $('#jobchip'); if (c) c.hidden = true; fjob.p = null; };
  const drawJobPop = () => {
    const jp = jobPop(); const p = fjob.p; if (!jp || !p) return;
    const files = p.done + p.failed;
    jp.innerHTML = `<div class="pop-title">${esc(jobTitle(p))}</div><div class="jobpop-body">`
      + `<div class="jobbar"><span style="width:${jobPct(p)}%"></span></div>`
      + `<p class="small">${files} of ${countOf(p.total, 'files')} · ${fmtBytes(p.bytes_done)} of ${fmtBytes(p.bytes_total)}${p.skipped ? ` · ${p.skipped} already ${p.kind === 'bring' ? 'here' : 'there'}` : ''}${p.failed ? ` · ${p.failed} failed` : ''}</p>`
      + (p.kind === 'bring' && p.dest ? `<p class="muted small">Into ${esc(p.dest)}, then added to your library.</p>` : '')
      + (p.current && !p.finished ? `<p class="muted small jobcur">${esc(p.current)}</p>` : '')
      + (p.errors.length ? `<p class="small joberr">${p.errors.map(esc).join('<br>')}</p>` : '')
      + `<div class="row-btns">${p.finished ? '<button class="btn ghost" data-jobclose>Close</button>' : '<button class="btn ghost" data-jobcancel>Cancel</button><button class="btn ghost" data-jobclose>Hide</button>'}</div></div>`;
    const cl = $('[data-jobclose]', jp); if (cl) cl.onclick = () => closeJobPop();
    const ca = $('[data-jobcancel]', jp); if (ca) ca.onclick = async () => { ca.disabled = true; try { fjob.p = await api('POST', `/pubky/files/jobs/${fjob.id}/cancel`); drawJobPop(); } catch (e) { toast(e.message, 'error'); ca.disabled = false; } };
  };
  const drawJobChip = () => {
    const c = $('#jobchip'); const p = fjob.p; if (!c || !p) return;
    c.hidden = false;
    c.classList.toggle('done', !!p.finished);
    c.innerHTML = `${icon(p.kind === 'bring' ? 'download' : 'upload')}<span class="jc-text">${p.finished ? (p.failed ? `${p.failed} failed` : 'Done') : `${p.done + p.failed}/${p.total}`}</span><span class="jc-bar"><span style="width:${jobPct(p)}%"></span></span>`;
    const t = `${jobTitle(p)}: ${p.done + p.failed} of ${p.total} files, ${fmtBytes(p.bytes_done)} of ${fmtBytes(p.bytes_total)}`;
    c.title = t; c.setAttribute('aria-label', t);
    if (!jobPop().hidden) drawJobPop();
  };
  const watchJob = (id) => { fjob.id = id; clearTimeout(fjob.timer); tickJob(); };
  const tickJob = async () => {
    let p; try { p = await api('GET', `/pubky/files/jobs/${fjob.id}`); } catch { fjob.timer = setTimeout(tickJob, 5000); return; }
    fjob.p = p; drawJobChip();
    if (!p.finished) { fjob.timer = setTimeout(tickJob, 1000); return; }
    if (p.kind === 'bring' && p.done) toast(`Brought ${countOf(p.done, 'files')} here; they are being added to your library.`);
    // The chip stays a little so the end is seen, unless its details are open
    fjob.timer = setTimeout(() => { if (jobPop().hidden) hideJobChip(); }, 8000);
  };
  // A job already running (another tab started it, or the node carried on after a restart)
  const findJob = async () => { try { const p = await api('GET', '/pubky/files/jobs'); if (p && p.id && p.id !== fjob.id) watchJob(p.id); } catch { /* not signed in */ } };
  // Outside parts that do not answer (Pinakes, the homeserver, a service): one quiet line under
  // the node status, only when something is wrong; the library keeps playing without them.
  let healthTimer = null;
  const checkHealth = async () => {
    clearTimeout(healthTimer); healthTimer = setTimeout(checkHealth, 60000);
    let parts; try { parts = await api('GET', '/health'); } catch { return; }
    const el = $('#upstreams'); if (!el) return;
    const bad = parts.filter((p) => p.state !== 'ok');
    el.hidden = !bad.length; if (!bad.length) return;
    const first = bad[0];
    $('.up-text', el).textContent = (first.state === 'needs_sign_in' ? `${first.name}: sign in again` : `${first.name} not answering`) + (bad.length > 1 ? ` · ${bad.length - 1} more` : '');
    el.title = bad.map((p) => `${p.name}: ${p.detail || p.state}`).join('\n') + '\nYour library keeps playing.';
    el.onclick = () => showMenu(el, [{ title: 'Outside parts' }, ...parts.map((p) => ({ label: p.name, note: p.state === 'ok' ? `answering${p.ms != null ? ` · ${p.ms} ms` : ''}` : p.detail || p.state, icon: p.state === 'ok' ? 'check' : 'close', disabled: p.state !== 'needs_sign_in', run: () => { state.autoResign = true; setView('settings', 'accounts'); } })), { label: 'Check again', icon: 'shuffle', run: () => checkHealth() }]);
  };
  // Clickable rows that are not buttons or links are made reachable for the keyboard, also when
  // they arrive after a page first draws (Home's sections, search results).
  {
    let queued = false;
    const tag = () => { queued = false; document.querySelectorAll('.clickable:not(button):not(a):not([tabindex])').forEach((el) => { el.tabIndex = 0; if (!el.getAttribute('role')) el.setAttribute('role', 'button'); }); };
    new MutationObserver(() => { if (!queued) { queued = true; requestAnimationFrame(tag); } }).observe(document.body, { childList: true, subtree: true });
  }
  const login = () => { const d = $('#login'); if (!d.open) { $('#tok').value = state.token; d.showModal(); } };
  // Pubky Ring sign-in: the node starts a flow, we show the link (and a QR when the device has a camera elsewhere), poll until the owner approves, and keep the token it hands out.
  let ringPoll = null;
  $('#ringstart').onclick = async () => {
    const out = $('#ringout'); $('#ringstart').disabled = true; out.innerHTML = 'Starting…';
    try {
      const r = await fetch('/v1/auth/pubky/start', { method: 'POST' }); const j = await r.json(); if (!r.ok) throw new Error(j.error || `HTTP ${r.status}`);
      out.innerHTML = `<div class="qrbox">${j.qr_svg ? `<div class="qr">${j.qr_svg}</div>` : ''}<div><p>Scan with Pubky Ring, or <a href="${esc(ringUrl(j.url))}">open it in Ring on this device</a>.</p><p>Waiting for approval from <code>${esc(j.owner.slice(0, 12))}…</code></p></div></div>`;
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
  // The browser zone plays through one of two engines: the <audio> element, or YouTube's
  // official player (IFrame Player API) for YouTube links. `audio` stands for whichever is in use,
  // so the player bar, transport, media keys and repeat drive both the same way.
  const audioEl = $('#browser');
  const ytOf = (u) => { const m = /^youtube:video:([A-Za-z0-9_-]{11})$/.exec(u || '') || /^(?:https?:\/\/)?(?:www\.|m\.|music\.)?(?:youtube\.com\/(?:watch\?(?:[^#]*&)?v=|shorts\/|embed\/|live\/)|youtu\.be\/)([A-Za-z0-9_-]{11})/.exec(u || ''); return m ? m[1] : null; };
  const yt = (() => {
    const ev = new EventTarget(); const fire = (t, detail) => ev.dispatchEvent(new CustomEvent(t, { detail }));
    let player = null; let ready = null; let vid = null; let st = -1; let vol = 1; let tick = null;
    const api = () => (ready || (ready = new Promise((res, rej) => {
      if (window.YT && window.YT.Player) return res();
      const prev = window.onYouTubeIframeAPIReady; window.onYouTubeIframeAPIReady = () => { if (prev) prev(); res(); };
      const sc = document.createElement('script'); sc.src = 'https://www.youtube.com/iframe_api'; sc.async = true;
      sc.onerror = () => { ready = null; sc.remove(); rej(new Error('YouTube\u2019s player did not load (offline, or blocked on this network)')); };
      document.head.appendChild(sc);
    })));
    const has = (f) => player && typeof player[f] === 'function';
    return {
      addEventListener: (t, f) => ev.addEventListener(t, f),
      get paused() { return st !== 1 && st !== 3; },
      get ended() { return st === 0; },
      get currentTime() { return has('getCurrentTime') ? player.getCurrentTime() || 0 : 0; },
      set currentTime(v) { if (has('seekTo')) player.seekTo(v, true); },
      get duration() { return has('getDuration') ? player.getDuration() || NaN : NaN; },
      get volume() { return vol; },
      set volume(v) { vol = v; if (has('setVolume')) player.setVolume(Math.round(v * 100)); },
      get src() { return vid ? `https://www.youtube.com/watch?v=${vid}` : ''; },
      set src(_) { /* chosen through open() */ },
      async open(id) {
        await api();
        vid = id; st = -1;
        if (!player) {
          await new Promise((res) => {
            player = new window.YT.Player('ytframe', { videoId: id, width: '100%', height: '100%', playerVars: { autoplay: 1, playsinline: 1, rel: 0, origin: location.origin }, events: {
              onReady: () => { player.setVolume(Math.round(vol * 100)); player.playVideo(); res(); },
              onStateChange: (e) => { st = e.data; if (st === 1) fire('play'); else if (st === 2) fire('pause'); else if (st === 0) fire('ended'); },
              onError: (e) => fire('yterror', e.data),
            } });
          });
        } else player.loadVideoById(id);
        clearInterval(tick); tick = setInterval(() => { if (st === 1) fire('timeupdate'); }, 500);
      },
      play() { if (has('playVideo')) player.playVideo(); return Promise.resolve(); },
      pause() { if (has('pauseVideo')) player.pauseVideo(); },
      stop() { clearInterval(tick); if (has('stopVideo')) player.stopVideo(); vid = null; st = -1; },
    };
  })();
  const audio = new Proxy({}, {
    get: (_, k) => {
      if (k === 'addEventListener') return (t, f, o) => { audioEl.addEventListener(t, f, o); yt.addEventListener(t, f); };
      const e = state.ytOn ? yt : audioEl; const v = e[k];
      return typeof v === 'function' ? v.bind(e) : v;
    },
    set: (_, k, v) => { if (k === 'volume') { audioEl.volume = v; yt.volume = v; } else (state.ytOn ? yt : audioEl)[k] = v; return true; },
  });
  // The YouTube panel: YouTube's terms want its player visible, so it shows while a YouTube item plays
  const ytPanel = (it) => { const p = $('#ytpanel'); if (!p) return; if (it) { $('#yttitle').textContent = it.title || 'YouTube'; p.hidden = false; } else p.hidden = true; };
  $('#ytmin').onclick = () => { const p = $('#ytpanel'); const small = !p.classList.contains('small'); p.classList.toggle('small', small); $('#ytmin').setAttribute('aria-expanded', String(!small)); $('#ytmin').title = small ? 'Show the video' : 'Make the video smaller'; };
  // A YouTube item that can't be embedded (the uploader turned it off, removed, private): say so and go on
  yt.addEventListener('yterror', (e) => {
    const it = state.browserQueue[state.browserIndex];
    const why = { 2: 'the link is not a valid video', 5: 'this browser cannot play it', 100: 'the video is gone or private', 101: 'YouTube won\u2019t play it outside youtube.com (its owner turned that off, or YouTube wants you signed in first: see the player)', 150: 'YouTube won\u2019t play it outside youtube.com (its owner turned that off, or YouTube wants you signed in first: see the player)' }[e.detail] || `YouTube error ${e.detail}`;
    toast(`\u201c${(it && it.title) || 'This video'}\u201d can't play here: ${why}.`, 'error');
    if (state.browserIndex + 1 < state.browserQueue.length) browserPlay(state.browserIndex + 1);
  });
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
  // The Null output is for developers: hidden unless switched on in About this node (it stays
  // listed while it is the zone in use)
  const devOutput = (id) => /^null/.test(String(id || ''));
  const TOKEN_GUIDE = 'https://github.com/gillohner/pubky-music-node/blob/main/docs/guides/troubleshooting.md#it-asks-for-a-token-or-the-token-is-refused';
  // Group outputs (POST /zones): speakers of one kind, kept in sync. Sonos, HEOS and BluOS speakers
  // become that system's own group; outputs the node keeps in time itself (caps.groupable:
  // squeezelite, the app's players on hw: DACs) a node group, of outputs that share a sample
  // rate. Kinds don't mix, and other outputs (which the API would only group loosely) aren't offered.
  const GROUP_KINDS = [['sonos', 'Sonos'], ['heos', 'HEOS (Denon, Marantz)'], ['bluos', 'BluOS (Bluesound, NAD)'], ['node', 'Squeezelite players'], ['net', 'The app\u2019s players on a DAC']];
  const groupKind = (o) => o.group_by || (o.caps.groupable ? (/^net:/.test(o.id) ? 'net' : 'node') : null);
  const groupSync = (picked) => {
    if (picked.length < 2) return '';
    const k = groupKind(picked[0]);
    if (k === 'heos') return 'HEOS groups them itself, in sync (if HEOS doesn’t know one of them, they play loosely synced).';
    return k === 'node' || k === 'net' ? 'In sync: the node keeps them in time sample by sample.' : `${k === 'sonos' ? 'Sonos' : 'BluOS'} groups them itself, in sync.`;
  };
  // Who is using the page (#46): the owner, or a listener with their own Ring sign-in, who plays
  // the shared library on the zones the owner opened and has nothing of the owner's
  const isListener = () => !!(state.me && state.me.role === 'listener');
  const isOwner = () => !isListener();
  // Offline (the app's local server, pm-offline): only what this device keeps, played here
  const isOffline = () => !!(state.info && state.info.offline);
  const OFFLINE_VIEWS = new Set(['downloads', 'queue', 'settings']);
  const LISTENER_VIEWS_OFF = new Set(['home', 'favourites', 'playlists', 'playlist', 'radio', 'sources', 'source-item', 'identify', 'smart', 'smart-edit', 'dashboard', 'add-music']);
  const zoneOpen = (z) => !isListener() || !state.me.zones || z.id === BROWSER || state.me.zones.includes(z.id);
  const zoneList = () => [browserState(), ...Object.values(state.zones).filter((z) => (state.devOutputs || !devOutput(z.output) || z.id === state.zone) && zoneOpen(z))];
  // The browser's queue survives a reload, every song with where it was (UX review #6): stream
  // links are rebuilt on load (they carry the token), live radio is not kept.
  let lastSavedQueue = '';
  const isLiveRadio = (it) => it.album === 'Internet radio' && /^https?:/.test(it.uri || '');
  setInterval(() => {
    try {
      const keep = state.browserQueue.filter((it) => !isLiveRadio(it)).map((it) => { const { stream, ...rest } = it; return rest; });
      const j = JSON.stringify(keep);
      if (j !== lastSavedQueue) { lastSavedQueue = j; localStorage.setItem('pm.browserQueue', j); }
      const cur = state.browserQueue[state.browserIndex];
      if (cur && !isLiveRadio(cur)) localStorage.setItem('pm.browserAt', JSON.stringify({ index: keep.indexOf(keep.find((x) => x.id === cur.id)), pos: Math.floor(audio.currentTime || 0) }));
    } catch { /* storage unavailable */ }
  }, 2000);
  // Back where it was, paused: the song in the bar and its position
  const restoreBrowserQueue = () => {
    for (const it of state.browserQueue) if (!it.stream && it.track_id == null) { const u = streamUrlFor(it.uri); if (u) it.stream = u; }
    let at = null; try { at = JSON.parse(localStorage.getItem('pm.browserAt') || 'null'); } catch { /* none */ }
    const it = at && state.browserQueue[at.index];
    if (!it) return;
    state.browserIndex = at.index;
    audio.preload = 'metadata';
    audio.src = it.stream ? it.stream : streamUrlFor(it.uri) || `/v1/library/tracks/${it.track_id}/file${tokenQ()}`;
    if (at.pos > 0) audio.addEventListener('loadedmetadata', () => { try { audio.currentTime = at.pos; } catch { /* not seekable */ } renderBar(); }, { once: true });
    renderBar();
  };
  const ensureZone = () => { if (!isBrowser() && !state.zones[state.zone]) state.zone = BROWSER; };
  const renderZoneName = () => { const z = currentZone(); const name = z ? z.name : 'Zone'; $('#zonename').textContent = name; $('#zonebtn').setAttribute('aria-label', `Playing on ${name}: choose where to play`); $('#zonebtn').title = `Playing on ${name}`; };
  // A queue URI the browser can play directly: TIDAL tracks go through the node's proxy.
  const streamUrlFor = (uri) => { if (ytOf(uri)) return null; const m = /^(tidal|qobuz|soundcloud|bandcamp|deezer|jamendo|subsonic|jellyfin):track:([A-Za-z0-9._-]+)$/.exec(uri || ''); if (m) return `/v1/sources/${m[1]}/stream/${m[2]}${tokenQ()}`; return /^https?:/.test(uri || '') ? uri : null; };
  // Source tracks (TIDAL, archive…) as queue items for the current zone.
  const playSourceItems = async (qitems, replace = true, at = 'end') => {
    if (!qitems.length) return;
    // YouTube's player lives in this page: it can't send audio to the node's zones (unless yt-dlp is on)
    const ytZones = (state.sources || []).some((x) => x.id === 'youtube' && x.enabled !== false && /zones/.test(x.status));
    if (!isBrowser() && !ytZones && qitems.some((q) => ytOf(q.uri))) {
      const rest = qitems.filter((q) => !ytOf(q.uri));
      if (!rest.length) {
        showMenu(document.activeElement && document.activeElement !== document.body ? document.activeElement : $('#zonebtn'), [{ title: 'YouTube plays only in This browser' }, { label: 'Play in This browser', icon: 'zone', run: () => { state.zone = BROWSER; try { localStorage.setItem('pm.zone', BROWSER); } catch { /* */ } renderZoneName(); playSourceItems(qitems, replace, at); } }]);
        return;
      }
      toast(`${countOf(qitems.length - rest.length, 'YouTube items')} left out: YouTube plays only in This browser`);
      qitems = rest;
    }
    if (isBrowser()) {
      const bitems = qitems.map((q, k) => ({ id: Date.now() + k, uri: q.uri, stream: streamUrlFor(q.uri) || q.uri, title: q.title, artist: q.artist, album: q.album, duration_ms: q.duration_ms, image: q.image || null }));
      if (at === 'now' && state.browserQueue.length) { state.browserQueue.splice(state.browserIndex + 1, 0, ...bitems); browserPlay(state.browserIndex + 1); return; }
      if (replace || at === 'now') { state.browserQueue = bitems; browserPlay(0); } else if (at === 'next') state.browserQueue.splice(state.browserIndex + 1, 0, ...bitems); else state.browserQueue.push(...bitems);
      if (!replace) toast(bitems.length === 1 ? 'Added to queue' : `${countOf(bitems.length, 'tracks')} added`);
      return;
    }
    if (at === 'now') { try { await api('POST', `/zones/${encodeURIComponent(state.zone)}/queue`, { items: qitems, at: 'next', replace: false, play: false }); await transport({ action: 'next' }); refreshQueue(); } catch (e) { toast(e.message, 'error'); } return; }
    if (replace) startWait('play', state.zone, qitems[0].title || '', true);
    try { await api('POST', `/zones/${encodeURIComponent(state.zone)}/queue`, { items: qitems, at, replace, play: replace }); if (replace) startSent(); if (!replace) toast(qitems.length === 1 ? (at === 'next' ? 'Playing next' : 'Added to queue') : `${countOf(qitems.length, 'tracks')} added`); refreshQueue(); } catch (e) { if (replace) startFail(e.message); else toast(e.message, 'error'); }
  };
  // Play on a Spotify Connect device: the active one, else a menu of the devices online. Spotify streams it; the node only drives it.
  // Spotify can't share one app between nodes, so the owner makes their own: these steps, then only its client ID (PKCE through the return page, no secret)
  const spotSetup = (sp, hidden) => `<form class="set-group" id="spotapp" ${hidden ? 'hidden' : ''}><p class="muted small">Spotify lets each person use their own free developer app. It takes about two minutes, once:</p><ol class="muted small steps"><li>Open the <a href="https://developer.spotify.com/dashboard" target="_blank" rel="noopener">Spotify developer dashboard</a>, sign in with your Spotify account and choose <b>Create app</b> (any name and description).</li><li>Under <b>Redirect URIs</b> add this address, tick <b>Web API</b>, and save:<div class="toolbar"><code class="path">${esc(sp.redirect_uri)}</code><button type="button" class="btn ghost sm" id="spotcopy">Copy</button></div></li><li>Under <b>User Management</b>, add every Spotify account that will sign in here (up to 25).</li><li>Copy the <b>Client ID</b> from the app's Settings and paste it below. No secret is needed.</li></ol><div class="toolbar"><label class="sr-only" for="spotid">Spotify client ID</label><input id="spotid" name="id" class="input" autocomplete="off" spellcheck="false" placeholder="Client ID (32 letters and digits)" pattern="[0-9a-fA-F]{32}" required value="${esc(sp.client_id || '')}"><button class="btn primary">Save</button>${hidden ? '<button type="button" class="btn ghost" id="spotappcancel">Cancel</button>' : ''}</div></form>`;
  // The fallback for a sign-in whose last step could not reach this node (another network, an app allowing only
  // loopback redirects): paste the address the sign-in page landed on; the node finishes from its parameters.
  const pasteBack = (svc) => `<details class="small"><summary>Landed on a page that didn't load?</summary><form class="toolbar" id="${svc}paste"><input class="input" name="url" placeholder="Paste that page's address (…?code=…)" autocomplete="off" spellcheck="false" required aria-label="The address you landed on"><button class="btn sm" type="submit">Finish sign-in</button></form></details>`;
  const wirePasteBack = (svc, done) => { const f = $(`#${svc}paste`); if (f) f.onsubmit = async (e) => { e.preventDefault(); const b = f.querySelector('button'); b.disabled = true; try { await api('POST', `/sources/${svc}/callback`, { url: String(new FormData(f).get('url') || '').trim() }); toast(done); state.sources = null; render(); } catch (er) { toast(er.message, 'error'); b.disabled = false; } }; };
  const spotAppRow = (sp) => sp.app_from === 'settings' ? `<p class="muted small">Your Spotify app: <code class="path">${esc(sp.client_id || '')}</code> <button class="btn ghost sm" id="spotappchange">Change</button> <button class="btn ghost sm" id="spotappforget">Forget</button></p>` : sp.app_from === 'config' ? '<p class="muted small">This node’s Spotify app is set in its config.</p>' : '';
  const wireSpotSetup = () => {
    const f = $('#spotapp'); if (!f) return;
    const save = async (client_id, msg) => { try { await api('PUT', '/sources/spotify/app', { client_id }); toast(msg); render(); } catch (e) { toast(e.message, 'error'); } };
    f.onsubmit = (e) => { e.preventDefault(); save(String(new FormData(f).get('id') || '').trim(), 'Spotify app saved. Now connect Spotify'); };
    const cp = $('#spotcopy'); if (cp) cp.onclick = () => { const t = f.querySelector('code').textContent; (navigator.clipboard ? navigator.clipboard.writeText(t) : Promise.reject()).then(() => toast('Redirect URI copied')).catch(() => toast(t)); };
    const ch = $('#spotappchange'); if (ch) ch.onclick = () => { f.hidden = false; $('#spotid').focus(); };
    const ca = $('#spotappcancel'); if (ca) ca.onclick = () => { f.hidden = true; };
    const fg = $('#spotappforget'); if (fg) fg.onclick = () => save('', 'Spotify app forgotten');
  };
  // One Sign in per source, the ways side by side (STEWARD-CREDENTIALS-REUSE.md): the radio list of
  // ways, one status line with Change way and Sign out, and the import (found sign-ins, a picked
  // file) and paste areas every source shares. A source's config gives `actions(pick, si, h)` for
  // its own ways' areas (null = the shared import/paste area), `wire(pick, si, h, act)` for their
  // handlers, `extra(si, pick)` for rows under the status line, `after(si, h, el)` once rendered.
  const sgHelp = (h) => (h ? `<p class="set-help">${h}</p>` : '');
  const sgRow = (label, h, ctl) => `<div class="set-row"><div class="set-label"><div class="set-name">${label}</div>${sgHelp(h)}</div>${ctl ? `<div class="set-ctl">${ctl}</div>` : ''}</div>`;
  const sgBlock = (label, h, body) => `<div class="set-row set-block">${label || h ? `<div class="set-label">${label ? `<div class="set-name">${label}</div>` : ''}${sgHelp(h)}</div>` : ''}<div class="set-body">${body}</div></div>`;
  const sgGroup = (title, body) => `<div class="set-group">${title ? `<h3 class="set-h">${title}</h3>` : ''}<div class="set-rows">${body}</div></div>`;
  const signinChooser = async (cfg, si) => {
    const el = typeof cfg.el === 'string' ? $(cfg.el) : cfg.el; if (!el || !el.isConnected) return;
    if (!si) { try { si = await api('GET', `/sources/${cfg.source}/signin`); } catch (e) { if (el.isConnected) { el.innerHTML = `<div class="notice warn" role="alert">${esc(e.message)} <button class="btn sm" data-sg="retry">Try again</button></div>`; el.querySelector('[data-sg="retry"]').onclick = () => signinChooser(cfg); } return; } }
    if (!el.isConnected) return;
    const mem = state.signin = state.signin || {}; const m = mem[cfg.source] = mem[cfg.source] || {};
    const wayOf = (id) => si.ways.find((w) => w.id === id) || {};
    const usable = si.ways.filter((w) => w.available);
    // The way picked here, else the remembered one, else the recommended one
    let pick = [m.way, si.chosen].find((id) => usable.some((w) => w.id === id)) || (usable.find((w) => w.recommended) || usable[0] || si.ways[0] || {}).id;
    const choosing = !si.in_use || !!m.change;
    const rows = [];
    if (si.in_use) rows.push(sgRow('Signed in', `${esc(si.in_use.label)}${si.in_use.gives ? ' · ' + esc(si.in_use.gives) : ''}${si.in_use.user ? ' · ' + (/^\d+$/.test(si.in_use.user) ? 'user ' : '') + esc(si.in_use.user) : ''}.`, `<button class="btn ghost sm" data-sg="change">${choosing ? 'Keep this way' : 'Change way'}</button><button class="btn ghost sm" data-sg="out">Sign out</button>`));
    if (choosing) {
      const ways = si.ways.map((w) => `<label class="way ${w.available ? '' : 'off'} ${w.id === pick ? 'on' : ''}"><input type="radio" name="way-${esc(cfg.source)}" value="${esc(w.id)}" ${w.id === pick ? 'checked' : ''} ${w.available ? '' : 'disabled'} aria-describedby="way-${esc(cfg.source)}-${esc(w.id)}"><span class="way-text"><span class="way-name">${esc(w.label)}${w.recommended ? '<span class="pill ok">Recommended</span>' : ''}${si.in_use && si.in_use.way === w.id ? '<span class="pill">In use</span>' : ''}</span><span class="way-note" id="way-${esc(cfg.source)}-${esc(w.id)}">${esc(w.available ? w.note : w.reason || w.note)}</span></span></label>`).join('');
      rows.push(sgBlock(si.in_use ? 'Another way' : 'Sign in', si.in_use ? '' : 'Pick how; the node remembers it.', `<div class="ways" role="radiogroup" aria-label="How to sign in to ${esc(cfg.name)}">${ways}</div><div class="way-action" data-sg="action"></div>`));
    }
    if (choosing) rows.push('<div class="notice warn" role="alert" data-sg="error" hidden><span data-sg="error-text"></span><div class="actions"><button class="btn sm" data-sg="another">Try another way</button></div></div>');
    if (cfg.extra) rows.push(...[].concat(cfg.extra(si, pick) || []));
    el.innerHTML = sgGroup(cfg.title === undefined ? 'Sign in' : cfg.title, rows.join(''));
    const h = {
      si, el, m, wayOf,
      pick: () => pick,
      choose: async (way) => { m.way = way; try { await api('PUT', `/sources/${cfg.source}/signin`, { way }); return true; } catch (e) { toast(e.message, 'error'); return false; } },
      refill: (now) => signinChooser(cfg, now),
      done: () => { m.change = false; state.sources = null; render(); },
      useImport: async (body, what) => { try { const r = await api('POST', `/sources/${cfg.source}/signin/import`, body); toast(`Saved ${r.in_use ? r.in_use.label : what}`); m.change = false; state.sources = null; render(); } catch (e) { const notice = el.querySelector('[data-sg="error"]'); if (notice) { notice.querySelector('[data-sg="error-text"]').textContent = e.message; notice.hidden = false; } else toast(e.message, 'error'); } },
      useButton: (w, who) => `<div class="toolbar"><button class="btn primary" data-sg="use">Use ${esc(w.label)}</button><span class="muted small">Already signed in with it${who || ''}.</span></div>`,
      // What other apps on this machine hold (read-only), the file itself, where the node looked
      importArea: (o = {}) => {
        const found = si.imports || []; const places = si.places || [];
        const when = (f) => f.usable ? (f.renews ? 'the node renews it' : f.expires_at ? 'until ' + new Date(f.expires_at * 1000).toLocaleDateString() : '') : f.reason;
        const list = found.length ? `<div class="list">${found.map((f) => `<div class="list-row"><div class="grow"><div class="name">${esc(f.app)}</div><div class="sub">${esc([f.user ? 'user ' + f.user : '', when(f), f.place].filter(Boolean).join(' · '))}</div></div>${f.usable ? `<button class="btn primary sm" data-import="${esc(f.id)}">Use it</button>` : '<span class="pill">not usable</span>'}</div>`).join('')}</div>` : '';
        const intro = o.intro || `${found.length ? 'Or the file itself' : 'The file itself'} (${esc(places.filter((p) => !/not read|not looked for/.test(p)).map((p) => p.split(':')[0]).join(', ') || o.what || 'the file')}); it is only read.`;
        const file = `<form class="stack" data-sg="file"><p class="muted small">${intro}</p><div class="toolbar"><input class="input" type="file" name="file" aria-label="${esc(o.fileLabel || 'The file')}" accept="${esc(o.accept || '')}"><button class="btn primary sm" type="submit">Use this file</button></div></form>`;
        const paste = o.withPaste ? h.pasteArea(o) : '';
        return `${list}${file}${paste}${places.length ? `<details class="small"><summary>Where the node looked</summary><ul class="muted small steps">${places.map((p) => `<li>${esc(p)}</li>`).join('')}</ul></details>` : ''}`;
      },
      pasteArea: (o = {}) => `<form class="stack" data-sg="paste"><textarea class="input" name="text" rows="${o.rows || 3}" aria-label="${esc(o.label || 'What to paste')}" placeholder="${esc(o.placeholder || '')}" autocomplete="off" spellcheck="false" required></textarea><div class="toolbar"><button class="btn primary sm" type="submit">Use it</button><span class="muted small">${esc(o.note || 'Kept like the node’s own sign-in; never shown again.')}</span></div></form>`,
    };
    const act = el.querySelector('[data-sg="action"]');
    const renderAction = () => {
      if (!act) return;
      el.querySelectorAll('.way').forEach((l) => l.classList.toggle('on', l.querySelector('input').value === pick));
      const w = wayOf(pick);
      let html = null;
      if (si.in_use && si.in_use.way === pick) html = '<p class="muted small">This is the way in use.</p>';
      else if (cfg.actions) html = cfg.actions(pick, si, h);
      if (html == null) html = w.action === 'import' ? h.importArea() : w.action === 'paste' ? h.pasteArea() : '';
      act.innerHTML = html;
      const useIt = act.querySelector('[data-sg="use"]'); if (useIt) useIt.onclick = async () => { if (await h.choose(pick)) { toast(`Using ${w.label || 'this way'}`); h.done(); } };
      act.querySelectorAll('[data-import]').forEach((b) => b.onclick = async () => { b.disabled = true; await h.useImport({ id: b.dataset.import }, b.dataset.import); b.disabled = false; });
      const ff = act.querySelector('[data-sg="file"]'); if (ff) ff.onsubmit = async (e) => { e.preventDefault(); const file = ff.querySelector('input[type=file]').files[0]; if (!file) { toast('Pick the file first', 'error'); return; } h.useImport({ text: await file.text() }, 'the file'); };
      const pf = act.querySelector('[data-sg="paste"]'); if (pf) pf.onsubmit = (e) => { e.preventDefault(); h.useImport({ text: pf.querySelector('textarea').value }, 'the pasted sign-in'); };
      if (cfg.wire) cfg.wire(pick, si, h, act);
    };
    el.querySelectorAll(`input[name="way-${cfg.source}"]`).forEach((r) => r.onchange = () => { pick = r.value; m.way = r.value; renderAction(); });
    renderAction();
    const another = el.querySelector('[data-sg="another"]'); if (another) another.onclick = () => { el.querySelector('input[type=radio]:not(:disabled)')?.focus(); };
    const ch = el.querySelector('[data-sg="change"]'); if (ch) ch.onclick = () => { m.change = !m.change; signinChooser(cfg, si); };
    const out = el.querySelector('[data-sg="out"]'); if (out) out.onclick = async () => { try { await api('DELETE', `/sources/${cfg.source}/signin`); toast(`Signed out of ${cfg.name}`); m.change = false; state.sources = null; render(); } catch (e) { toast(e.message, 'error'); } };
    if (cfg.after) cfg.after(si, h, el);
  };
  // TIDAL: the official app in the browser, tidal-dl-ng's device code, your own client; quality and
  // health under the device way, which is what the Full tracks switch used to turn on (STEWARD-TIDAL-FULL.md).
  const tidalBrowserSignIn = async (btn) => { if (btn) btn.disabled = true; try { const r = await api('GET', '/sources/tidal/login'); location.href = r.url; let tries = 0; const t = setInterval(async () => { tries++; try { const st = await api('GET', '/sources/tidal/status'); if (st.signed_in) { clearInterval(t); toast('TIDAL signed in'); render(); } } catch { /* keep polling */ } if (tries > 60) clearInterval(t); }, 3000); } catch (e) { toast(e.message, 'error'); if (btn) btn.disabled = false; } };
  const tidalSigninCfg = (el) => ({
    source: 'tidal', name: 'TIDAL', el,
    actions: (pick, si, h) => {
      const u = si.details.unofficial || {}; const st = si.details.status || {}; const w = h.wayOf(pick);
      if (pick === 'app') return st.signed_in ? h.useButton(w, st.user_id ? ' as user ' + esc(st.user_id) : '') : `<div class="toolbar"><button class="btn primary" id="tidalin">${icon('zone')} Sign in at TIDAL</button><span class="muted small">Opens TIDAL’s sign-in in this browser and comes back here.</span></div>${st.redirect_uri && !/^Pubky Music/.test(w.label || '') ? `<details class="small"><summary>Your app’s settings</summary><p class="muted small">It must list <code>${esc(st.redirect_uri)}</code> as a redirect URI and have these scopes enabled (<code>[tidal] scopes</code> in the node config):</p><div class="toolbar"><code class="path">${esc(st.requested_scopes || '')}</code></div></details>` : ''}${pasteBack('tidal')}`;
      if (pick === 'device') {
        const client = u.client_from && u.client_from !== 'builtin' ? `<p class="muted small">Device client: ${u.client_from === 'config' ? 'set in the node config.' : 'entered here.'} ${u.client_from === 'settings' ? `<button class="btn ghost sm" id="tfclientforget">${u.builtin ? 'Use tidal-dl-ng’s instead' : 'Forget it'}</button>` : ''}</p>` : '';
        if (u.pending) return `<p class="muted small">On a phone or computer, open link.tidal.com and enter this code. This page notices when you are done.</p><div class="toolbar"><code class="tf-code">${esc(u.pending.user_code)}</code><a class="btn primary" href="${esc(webUrl(u.pending.uri))}" target="_blank" rel="noopener">${icon('external')} Open link.tidal.com</a></div>`;
        if (u.signed_in) return h.useButton(w, u.user ? ' as user ' + esc(u.user) : '') + client;
        return `<div class="toolbar"><button class="btn primary" id="tfin">Sign in with a code</button><span class="muted small">A code to enter at link.tidal.com on any device; no password goes through this node.</span></div>${client}`;
      }
      if (pick === 'own') return `<form class="stack" id="tfclient"><p class="muted small">A TIDAL <b>device client</b> id and secret you are allowed to use; the node keeps them like a sign-in and signs you in with a code. An app from TIDAL’s developer site is not a device client: that one goes in Settings › This node › Service apps, and it appears above as your own TIDAL app.</p><div class="toolbar"><input class="input" name="id" placeholder="Device client id" autocomplete="off" aria-label="Device client id" required><input class="input" name="secret" type="password" placeholder="Device client secret" autocomplete="off" aria-label="Device client secret"><button class="btn primary" type="submit">Save and sign in with a code</button></div></form>`;
      if (pick === 'import') return h.importArea({ fileLabel: 'Token file', accept: '.json,.toml,.txt,application/json' });
      if (pick === 'paste') return h.pasteArea({ label: 'Token file contents or access token', placeholder: 'Token file contents, or an access token (a refresh token on the next line)' });
      return null;
    },
    wire: (pick, si, h, act) => {
      const u = si.details.unofficial || {};
      const a = $('#tidalin'); if (a) a.onclick = async () => { if (await h.choose('app')) tidalBrowserSignIn(a); };
      wirePasteBack('tidal', 'TIDAL signed in');
      const d = $('#tfin'); if (d) d.onclick = async () => { d.disabled = true; if (await h.choose('device')) { try { await api('POST', '/sources/tidal/unofficial/login'); } catch (e) { toast(e.message, 'error'); } } h.refill(); };
      const cf = $('#tfclient'); if (cf) cf.onsubmit = async (e) => { e.preventDefault(); const f = new FormData(cf); cf.querySelector('button').disabled = true; try { await api('PUT', '/sources/tidal/unofficial', { client_id: f.get('id'), client_secret: f.get('secret') }); toast('Device client saved'); if (await h.choose('device')) await api('POST', '/sources/tidal/unofficial/login'); } catch (er) { toast(er.message, 'error'); } h.refill(); };
      const cfg = $('#tfclientforget'); if (cfg) cfg.onclick = async () => { try { await api('PUT', '/sources/tidal/unofficial', { client_id: '' }); toast(u.builtin ? 'Using tidal-dl-ng’s device client' : 'Device client forgotten'); } catch (e) { toast(e.message, 'error'); } h.refill(); };
    },
    extra: (si) => {
      const u = si.details.unofficial || {}; const st = si.details.status || {}; const rows = [];
      const deviceOn = !!(si.in_use && si.in_use.way !== 'app');
      if (u.health && !u.health.ok) rows.push(`<div class="notice warn" role="status">Full tracks switched themselves off: ${esc(u.health.detail)}. TIDAL plays 30-second previews until it works again.${u.from ? ' That sign-in came from ' + esc(u.from === 'pasted' ? 'a pasted token' : u.from) + '; pick another way below, or a newer file.' : ''}<div class="actions"><button class="btn sm" id="tfretry">Try again</button></div></div>`);
      if (deviceOn) {
        rows.push(sgRow('Quality', 'Max plays up to 24-bit/192 kHz where TIDAL has it.', `<select class="input" id="tfq" aria-label="TIDAL full track quality"><option value="max" ${u.quality === 'max' ? 'selected' : ''}>Max (up to 24-bit/192 kHz)</option><option value="high" ${u.quality === 'high' ? 'selected' : ''}>High (16-bit/44.1 kHz)</option></select>`));
        if (u.health && u.health.ok) rows.push(sgRow('Health', `Working: ${esc(u.health.detail)}.`, '<span class="pill ok">OK</span>'));
        if (!st.signed_in) rows.push(sgRow('Your collection', `Collection and playlist imports go through ${esc((si.ways.find((w) => w.id === 'app') || {}).label || 'the official app')}; sign in with it too for those.`, st.configured ? '<button class="btn sm" id="tsiapp2">Sign in with it too</button>' : ''));
      } else if (si.in_use && st.playback_limit) rows.push(sgRow('Playback', `TIDAL plays 30-second previews to apps on its ${esc(st.access_tier || 'THIRD_PARTY')} developer tier; search, albums and the imports are unaffected. For full tracks, change way to the device sign-in.`, '<span class="pill warn">previews</span>'));
      return rows;
    },
    after: (si, h) => {
      const u = si.details.unofficial || {};
      const putU = async (body, ok) => { try { await api('PUT', '/sources/tidal/unofficial', body); if (ok) toast(ok); } catch (e) { toast(e.message, 'error'); } h.refill(); };
      const tr = $('#tfretry'); if (tr) tr.onclick = () => putU({ enabled: true }, 'Trying full tracks again');
      const tq = $('#tfq'); if (tq) tq.onchange = () => putU({ quality: tq.value }, 'Quality saved');
      const a2 = $('#tsiapp2'); if (a2) a2.onclick = () => tidalBrowserSignIn(a2);
      clearTimeout(state.tidalFullPoll);
      if (u.pending) state.tidalFullPoll = setTimeout(async () => { let now; try { now = await api('GET', '/sources/tidal/signin'); } catch { return; } if (now.in_use && now.in_use.way === 'device' && !(si.in_use && si.in_use.way === 'device')) { toast('TIDAL signed in: full tracks'); h.done(); } else h.refill(now); }, 3000);
    },
  });
  // Qobuz: your own app and account, or streamrip's / qobuz-dl's config (found, picked or pasted).
  const qobuzSigninCfg = (el) => ({
    source: 'qobuz', name: 'Qobuz', el,
    actions: (pick, si, h) => {
      const u = si.details.unofficial || {};
      if (pick === 'own') {
        const appRow = u.app_from ? `<p class="muted small">App: ${u.app_from === 'config' ? 'set in the node config' : u.app_from === 'import' ? 'from the imported config' : 'entered here'}${u.app_id ? ' · <code class="path">' + esc(u.app_id) + '</code>' : ''}. ${u.app_from !== 'config' ? '<button class="btn ghost sm" data-qb="appchange">Change</button> <button class="btn ghost sm" data-qb="appforget">Forget</button>' : ''}</p>` : '';
        const appForm = `<form class="stack" data-qb="app" ${u.app_from && !h.m.changeApp ? 'hidden' : ''}><p class="muted small">The node has no built-in Qobuz app: a Qobuz app id and secret you are allowed to use. The node keeps them like a sign-in.</p><div class="toolbar"><input class="input" name="id" placeholder="App ID" inputmode="numeric" autocomplete="off" aria-label="Qobuz app ID" required><input class="input" name="secret" type="password" placeholder="App secret" autocomplete="off" aria-label="Qobuz app secret" required><button class="btn" type="submit">Save</button></div></form>`;
        const signin = u.app_from ? `<form class="stack" data-qb="in"><p class="muted small">Your password goes to Qobuz once, to sign in; the node keeps only the session Qobuz answers with.</p><div class="toolbar"><input class="input" name="email" type="email" placeholder="Qobuz email" autocomplete="username" aria-label="Qobuz email" required><input class="input" name="password" type="password" placeholder="Password" autocomplete="current-password" aria-label="Qobuz password" required><button class="btn primary" type="submit">Sign in</button></div></form>` : '';
        return appRow + appForm + signin;
      }
      if (pick === 'import') return h.importArea({ fileLabel: 'Config file', accept: '.toml,.ini,.txt' });
      if (pick === 'paste') return h.pasteArea({ label: 'streamrip or qobuz-dl config', placeholder: '[qobuz] … or [DEFAULT] … from streamrip’s config.toml or qobuz-dl’s config.ini', rows: 5 });
      return null;
    },
    wire: (pick, si, h, act) => {
      const put = async (body, ok) => { try { await api('PUT', '/sources/qobuz/unofficial', body); if (ok) toast(ok); } catch (e) { toast(e.message, 'error'); } h.refill(); };
      const af = act.querySelector('[data-qb="app"]'); if (af) af.onsubmit = (e) => { e.preventDefault(); const f = new FormData(af); h.m.changeApp = false; put({ app_id: String(f.get('id') || '').trim(), app_secret: String(f.get('secret') || '').trim() }, 'Qobuz app saved'); };
      const ac = act.querySelector('[data-qb="appchange"]'); if (ac) ac.onclick = () => { h.m.changeApp = true; af.hidden = false; af.querySelector('input').focus(); };
      const ag = act.querySelector('[data-qb="appforget"]'); if (ag) ag.onclick = () => put({ app_id: '' }, 'Qobuz app forgotten');
      const qi = act.querySelector('[data-qb="in"]'); if (qi) qi.onsubmit = async (e) => { e.preventDefault(); const f = new FormData(qi); qi.querySelector('button').disabled = true; try { await api('POST', '/sources/qobuz/unofficial/login', { email: f.get('email'), password: f.get('password') }); toast('Signed in to Qobuz'); if (await h.choose('own')) h.done(); } catch (er) { toast(er.message, 'error'); h.refill(); } };
    },
  });
  // Spotify: your own app (Connect control, library), librespot picked in a Spotify app, or
  // librespot with a credentials.json you pick; Soloist and librespot's own settings keep their rows.
  const spotifySigninCfg = (el) => ({
    source: 'spotify', name: 'Spotify', el,
    actions: (pick, si, h) => {
      const st = si.details.status || {}; const ls = si.details.librespot || {}; const w = h.wayOf(pick);
      if (pick === 'app') {
        if (!st.configured) return spotSetup(st, false);
        if (st.signed_in) return h.useButton(w, st.display_name ? ' as ' + esc(st.display_name) : '');
        return `<div class="toolbar"><button class="btn primary" id="spotin">${icon('zone')} Connect Spotify</button><span class="muted small">Opens Spotify’s sign-in; the app must list <code>${esc(st.redirect_uri)}</code> as a redirect URI and, while in development mode, your Spotify account under its user management.</span></div>${pasteBack('spotify')}${spotAppRow(st)}${st.app_from === 'settings' ? spotSetup(st, true) : ''}`;
      }
      if (pick === 'librespot') return ls.enabled ? `<p class="muted small">librespot is on: pick it in a Spotify app on this network; its device name and output are below.</p>` : `<div class="toolbar"><button class="btn primary" data-sg="use">Switch librespot on</button><span class="muted small">Then pick it in a Spotify app on this network. Its device name and output are set below.</span></div>`;
      if (pick === 'librespot-file') return h.importArea({ fileLabel: 'credentials.json', accept: '.json,application/json', what: 'credentials.json', intro: 'The credentials.json another librespot saved; it is kept readable by the node only.', withPaste: true, label: 'credentials.json contents', placeholder: '{"username": …, "auth_type": …, "auth_data": …}' });
      return null;
    },
    wire: (pick, si, h) => {
      const sp = $('#spotin'); if (sp) sp.onclick = async () => { if (await h.choose('app')) { try { const r = await api('GET', '/sources/spotify/login'); location.href = r.url; } catch (e) { toast(e.message, 'error'); } } };
      wireSpotSetup();
      wirePasteBack('spotify', 'Spotify signed in');
    },
  });
  // YouTube: your Data API key for search, or a cookies file you pick for yt-dlp.
  const youtubeSigninCfg = (el) => ({
    source: 'youtube', name: 'YouTube', el,
    actions: (pick, si, h) => {
      const key = (si.details.key || {}).set;
      if (pick === 'key') return key ? `<div class="toolbar"><span class="pill ok account">Search on · your API key</span><button class="btn ghost sm" data-yt="forget">Forget the key</button></div><p class="muted small">Each search spends 100 of the key’s 10,000 free daily units, so YouTube is searched only when you pick it, never in the everyday search.</p>` : `<form class="stack" data-yt="key"><p class="muted small">In the <a href="https://console.cloud.google.com/apis/library/youtube.googleapis.com" target="_blank" rel="noopener">Google Cloud console</a>, enable the YouTube Data API v3, then create an API key under Credentials and paste it here. It is checked once, then never shown again.</p><div class="toolbar"><label class="sr-only" for="ytkeyin">YouTube Data API key</label><input id="ytkeyin" class="input" name="key" type="password" autocomplete="off" spellcheck="false" placeholder="YouTube Data API key" required><button class="btn primary" type="submit">Save</button></div></form>`;
      if (pick === 'cookies') return h.importArea({ fileLabel: 'Cookies file', accept: '.txt,text/plain', what: 'the cookies file', intro: 'The Netscape cookies file you export for yt-dlp; the copy is kept readable by the node only, and yt-dlp on your speakers plays as your account.', withPaste: true, label: 'Cookies file contents', placeholder: '# Netscape HTTP Cookie File …', rows: 4 });
      return null;
    },
    wire: (pick, si, h, act) => {
      const kf = act.querySelector('[data-yt="key"]'); if (kf) kf.onsubmit = async (e) => { e.preventDefault(); const b = kf.querySelector('button'); b.disabled = true; try { await api('PUT', '/sources/youtube/key', { key: String(new FormData(kf).get('key') || '').trim() }); toast('YouTube key saved'); if (await h.choose('key')) h.done(); } catch (er) { toast(er.message, 'error'); b.disabled = false; } };
      const kg = act.querySelector('[data-yt="forget"]'); if (kg) kg.onclick = async () => { try { await api('PUT', '/sources/youtube/key', { key: '' }); toast('YouTube key forgotten'); h.done(); } catch (e) { toast(e.message, 'error'); } };
    },
  });
  // Last.fm (Settings › Accounts): your own API account, rescrobbled's sign-in, or a pasted one;
  // scrobbling on/off and the waiting plays stay as rows.
  const lastfmSigninCfg = (el) => ({
    source: 'lastfm', name: 'Last.fm', el, title: '',
    actions: (pick, si, h) => {
      const lf = si.details.lastfm || {};
      if (pick === 'own') {
        if (!lf.configured) return `<form class="stack" data-lf="app"><p class="muted small">Last.fm gives each app its own key: <a href="https://www.last.fm/api/account/create" target="_blank" rel="noopener">create one</a> (any name; no callback needed), then paste its API key and shared secret. Both stay on this node; the secret only signs requests here.</p><div class="toolbar"><input class="input" name="key" placeholder="API key" autocomplete="off" spellcheck="false" pattern="[0-9a-fA-F]{32}" aria-label="Last.fm API key" required><input class="input" name="secret" type="password" placeholder="Shared secret" autocomplete="off" aria-label="Last.fm shared secret" required><button class="btn" type="submit">Save</button></div></form>`;
        const appRow = lf.app_from === 'config' ? '<p class="muted small">This node’s Last.fm app is set in its config.</p>' : `<p class="muted small">Your Last.fm app: <code class="path">${esc(lf.api_key || '')}</code> <button class="btn ghost sm" data-lf="appforget">Forget</button></p>`;
        return `<div class="toolbar"><button class="btn primary" data-lf="in">Connect Last.fm</button><span class="muted small">Opens Last.fm’s permission page; it comes back here.</span></div>${appRow}`;
      }
      if (pick === 'import') return h.importArea({ fileLabel: 'Last.fm key, secret and session key', accept: '.toml,.txt', intro: 'From another machine, use a text file with three lines: the API key, shared secret and session key. rescrobbled’s config alone does not contain the session.', withPaste: true, label: 'API key, shared secret and session key', placeholder: 'API key\nshared secret\nsession key' });
      if (pick === 'paste') return h.pasteArea({ label: 'API key, shared secret and session key', placeholder: 'API key\nshared secret\nsession key', rows: 3 });
      return null;
    },
    wire: (pick, si, h, act) => {
      const af = act.querySelector('[data-lf="app"]'); if (af) af.onsubmit = async (e) => { e.preventDefault(); const f = new FormData(af); try { await api('PUT', '/lastfm', { api_key: String(f.get('key') || '').trim(), api_secret: String(f.get('secret') || '').trim() }); toast('Last.fm app saved'); h.refill(); } catch (er) { toast(er.message, 'error'); } };
      const ag = act.querySelector('[data-lf="appforget"]'); if (ag) ag.onclick = async () => { try { await api('PUT', '/lastfm', { api_key: '' }); toast('Last.fm app forgotten'); h.refill(); } catch (er) { toast(er.message, 'error'); } };
      const li = act.querySelector('[data-lf="in"]'); if (li) li.onclick = async () => { if (await h.choose('own')) { try { const r = await api('GET', '/lastfm/login'); location.href = r.url; } catch (er) { toast(er.message, 'error'); } } };
    },
    extra: (si) => {
      const lf = si.details.lastfm || {}; if (!lf.connected) return [];
      return [
        lf.problem ? `<div class="notice warn" role="status">${esc(lf.problem)}<div class="actions"><button class="btn sm" data-lf="again">Sign in again</button></div></div>` : '',
        sgRow('Send what I play', lf.pending ? `${lf.pending} plays wait for Last.fm; they go with the next play.` : 'Plays count after half the track or four minutes; now-playing at the start.', `<input type="checkbox" class="switch" role="switch" data-lf="submit" ${lf.submit ? 'checked' : ''} aria-label="Send what I play to Last.fm">`),
      ];
    },
    after: (si, h, el) => {
      const sw = el.querySelector('[data-lf="submit"]'); if (sw) sw.onchange = async (e) => { try { await api('PUT', '/lastfm', { submit: e.target.checked }); toast(e.target.checked ? 'Plays go to Last.fm' : 'Plays stay on this node'); } catch (er) { toast(er.message, 'error'); } };
      const ag = el.querySelector('[data-lf="again"]'); if (ag) ag.onclick = async () => { try { const r = await api('GET', '/lastfm/login'); location.href = r.url; } catch (er) { toast(er.message, 'error'); } };
    },
  });
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
  { const sp = new URLSearchParams(location.search); if (sp.get('drive') === 'ok') { toast('Drive signed in: add a folder'); history.replaceState(null, '', location.pathname); } else if (sp.get('drive') === 'error') { toast('Drive: ' + (sp.get('message') || 'sign-in failed'), 'error'); history.replaceState(null, '', location.pathname); } }
  { const sp = new URLSearchParams(location.search); if (sp.get('spotify') === 'ok') { toast('Spotify signed in'); history.replaceState(null, '', location.pathname); } else if (sp.get('spotify') === 'error') { toast('Spotify: ' + (sp.get('message') || 'sign-in failed'), 'error'); history.replaceState(null, '', location.pathname); } }
  const hitItem = (h, album) => ({ uri: h.uri || `${h.source || 'tidal'}:track:${h.id.replace(/^track:/, '')}`, title: h.title, artist: h.creator || '', album: album || '', duration_ms: h.duration_ms || null, image: h.image || null });
  const browserPlay = (i) => {
    const it = state.browserQueue[i];
    // The end of the queue: the last song stays on the bar, stopped, ready to play again
    if (!it && i >= state.browserQueue.length && state.browserQueue.length) { audio.pause(); try { audio.currentTime = 0; } catch { /* not loaded */ } renderBar(); return; }
    if (!it) { audio.pause(); if (state.ytOn) { yt.stop(); state.ytOn = false; ytPanel(null); } state.browserIndex = -1; renderBar(); return; }
    state.browserIndex = i;
    const yid = ytOf(it.uri);
    if (yid) {
      if (audioEl.getAttribute('src')) { audioEl.pause(); audioEl.removeAttribute('src'); audioEl.load(); }
      state.ytOn = true; ytPanel(it);
      yt.open(yid).catch((e) => toast(e.message, 'error'));
      mediaSessionUpdate(it); renderBar(); if (state.view === 'queue') render();
      return;
    }
    if (state.ytOn) { yt.stop(); state.ytOn = false; ytPanel(null); }
    if (/^deezer:/.test(it.uri || '') && !state.previewNoted) { state.previewNoted = true; toast('Deezer plays a 30-second preview of each song without a Deezer partner account'); }
    audio.src = it.stream ? it.stream : streamUrlFor(it.uri) || `/v1/library/tracks/${it.track_id}/file${tokenQ()}`;
    state.browserRetry = state.browserRetry && state.browserRetry.i === i ? state.browserRetry : { i, n: 0 };
    if (!(state.starting && state.starting.zone === BROWSER)) startWait('play', BROWSER, it.title || '');
    audio.play().catch(async (e) => {
      if (e.name === 'AbortError') { if (state.starting && state.starting.zone === BROWSER && (audio.paused || state.ytOn)) startDone(); return; }
      startFail(e.message, true);
      if (e.name !== 'NotSupportedError') { toast(`Playback: ${e.message}`, 'error'); return; }
      // Ask what came back: a busy node or a proxy in front of it can answer with an error page
      // instead of the file (owner, 2026-10-22: a library FLAC during a big copy)
      let why = 'its source sent no audio this browser can play';
      try { const r = await fetch(audio.src, { headers: { range: 'bytes=0-1' } }); const ct = r.headers.get('content-type') || ''; if (!r.ok) why = `the node answered ${r.status}${r.status >= 500 ? ' (busy or not reachable just now)' : ''}`; else if (!/^audio\/|octet-stream|mpegurl|video\/mp2t/.test(ct)) why = `the node sent ${ct.split(';')[0] || 'something'} instead of audio`; } catch { why = 'the node did not answer'; }
      if (state.browserRetry.n < 1 && state.browserIndex === i) { state.browserRetry.n++; toast(`“${it.title || 'This song'}” did not start: ${why}. Trying again…`); setTimeout(() => { if (state.browserIndex === i) browserPlay(i); }, 2000); return; }
      toast(`“${it.title || 'This song'}” can't play here: ${why}.${it.track_id != null ? '' : ' Try another version on the song\u2019s page.'}`, 'error');
    });
    // A song moved in from a zone was counted there already
    if (it.track_id != null && !it.moved) api('POST', '/library/history', { track_id: it.track_id, zone: 'browser' }).catch(() => {});
    it.moved = false;
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
  // The end of a song in this browser: repeat one plays it again, repeat all wraps to the start
  audio.addEventListener('ended', () => {
    const r = browserRepeat();
    if (r === 'one') return browserPlay(state.browserIndex);
    if (r === 'all' && state.browserIndex + 1 >= state.browserQueue.length) return browserPlay(0);
    browserPlay(state.browserIndex + 1);
  });
  // A stream that breaks off mid-song (a dropped connection) picks up where it was, once
  audio.addEventListener('error', () => {
    if (state.ytOn) return;
    const i = state.browserIndex; const at = audio.currentTime;
    if (i < 0 || !audio.src || !(at > 0)) return;
    state.midRetry = state.midRetry && state.midRetry.i === i ? state.midRetry : { i, n: 0 };
    if (state.midRetry.n >= 1) return;
    state.midRetry.n++;
    const src = audio.src; setTimeout(() => { if (state.browserIndex !== i) return; audio.src = src; audio.currentTime = at; audio.play().catch(() => {}); }, 1500);
  });
  // For tests (tools/ui-youtube.js): queue items in the current zone, read the browser engine
  window.pmPlayer = { play: (items) => playSourceItems(items, true), now: () => ({ youtube: !!state.ytOn, index: state.browserIndex, paused: audio.paused, time: audio.currentTime, src: audio.src }) };
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
      if (!replace) toast(items.length === 1 ? (at === 'next' ? 'Playing next' : 'Added to queue') : `${countOf(items.length, 'tracks')} added`);
      if (state.view === 'queue') render();
      return;
    }
    if (replace) startWait('play', state.zone, tracks[0].title, true);
    try {
      await api('POST', `/zones/${state.zone}/queue`, { track_ids: tracks.map((t) => t.id), replace, play: replace, at });
      if (replace) startSent();
      if (!replace) toast(tracks.length === 1 ? (at === 'next' ? 'Playing next' : 'Added to queue') : `${countOf(tracks.length, 'tracks')} added`);
      refreshQueue();
    } catch (e) { if (replace) startFail(e.message); else toast(e.message, 'error'); }
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
    const zt = state.zones[state.zone];
    if ((action.action === 'play' || (action.action === 'toggle' && zt.transport !== 'playing')) && zt.queue && zt.queue.length) startWait('play', state.zone, zt.now_playing ? zt.now_playing.item.title || '' : '');
    try {
      const st = await api('POST', `/zones/${state.zone}/transport`, action);
      state.zones[st.id] = st;
      renderBar();
    } catch (e) { if (state.starting && state.starting.zone === zt.id) startFail(e.message, true); toast(e.message, 'error'); }
  };
  const playStation = async (st) => {
    if (isBrowser()) {
      if (location.protocol === 'https:' && /^http:\/\//i.test(st.url)) { toast('This page is served over HTTPS, so the browser will not play an http:// stream. Pick a node zone, or a station with an https stream.', 'error'); return; }
      state.browserQueue = [{ id: Date.now(), uri: st.url, stream: st.url, title: st.name, artist: [st.country, (st.tags || []).slice(0, 3).join(', ')].filter(Boolean).join(' · '), album: 'Internet radio', duration_ms: 0 }];
      browserPlay(0); toast(`Tuning to ${st.name}`); return;
    }
    startWait('play', state.zone, st.name, true);
    try { await api('POST', '/radio/play', { zone: state.zone, station: st }); startSent(); toast(`Tuning to ${st.name}`); refreshQueue(); } catch (e) { startFail(e.message, true); toast(e.message, 'error'); }
  };
  const playPlaylist = async (id) => {
    try {
      if (isBrowser()) { const d = await api('GET', `/playlists/${id}`); playTracks(d.tracks.filter(Boolean), true); return; }
      startWait('play', state.zone, (state.playlists.find((x) => x.id === id) || {}).name || '', true);
      const r = await api('POST', `/playlists/${id}/play`, { zone: state.zone });
      startSent();
      toast(`Queued ${r.queued} song${r.queued === 1 ? '' : 's'}${r.missing.length ? `; ${r.missing.length} not playable here` : ''}`);
    } catch (e) { startFail(e.message, true); toast(e.message, 'error'); }
  };
  const refreshQueue = async (rerender = true) => {
    if (!isBrowser()) {
      try { state.queue = await api('GET', `/zones/${state.zone}/queue`); } catch { state.queue = []; }
    }
    if (rerender && state.view === 'queue') render();
    if (!$('#nowplaying').hidden) renderNowPlaying();
  };

  // Repeat (off → all → one) for the zone in use; this browser keeps its own choice
  const REPEATS = ['off', 'all', 'one'];
  const browserRepeat = () => { try { return localStorage.getItem('pm.repeat') || 'off'; } catch { return 'off'; } };
  const zoneRepeat = () => (isBrowser() ? browserRepeat() : (state.zones[state.zone] || {}).repeat || 'off');
  const renderModes = () => {
    const r = zoneRepeat(); const b = $('#repeatbtn'); if (!b) return;
    const words = { off: 'Repeat: off', all: 'Repeat: the whole queue', one: 'Repeat: this song' }[r];
    b.innerHTML = icon(r === 'one' ? 'repeat-one' : 'repeat'); b.classList.toggle('on', r !== 'off');
    b.setAttribute('aria-pressed', String(r !== 'off')); b.setAttribute('aria-label', words); b.title = words;
  };
  $('#repeatbtn').onclick = async () => {
    const next = REPEATS[(REPEATS.indexOf(zoneRepeat()) + 1) % REPEATS.length];
    if (isBrowser()) { try { localStorage.setItem('pm.repeat', next); } catch { /* no storage */ } renderModes(); return; }
    try { const st = await api('PUT', `/zones/${state.zone}/repeat`, { repeat: next }); if (st && st.id) state.zones[st.id] = st; else if (state.zones[state.zone]) state.zones[state.zone].repeat = next; renderModes(); } catch (e) { toast(e.message, 'error'); }
  };
  $('#shufflebtn').onclick = async () => {
    if (isBrowser()) {
      const q = state.browserQueue; const i = state.browserIndex;
      const rest = q.slice(i + 1); if (rest.length < 2) { toast('Nothing to shuffle: add more songs to the queue first'); return; }
      for (let k = rest.length - 1; k > 0; k--) { const j = Math.floor(Math.random() * (k + 1)); [rest[k], rest[j]] = [rest[j], rest[k]]; }
      state.browserQueue = [...q.slice(0, i + 1), ...rest]; toast('Shuffled what comes next'); if (state.view === 'queue') render(); return;
    }
    try { await api('POST', `/zones/${state.zone}/shuffle`); toast('Shuffled what comes next'); refreshQueue(); } catch (e) { toast(e.message, 'error'); }
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
  const closePop = (fromBack) => { if (zonePop.hidden) return; zonePop.hidden = true; $('#scrim').hidden = true; if (fromBack !== true) popTransient(); };
  // What a device is, in words (owner, 2026-10-29: the line showed raw ids like airplay:9056…):
  // the protocol of its output id, and the device's maker and model when the node knows them
  // (`model` in GET /v1/outputs); the id itself stays as the tooltip.
  const PROTO_WORDS = { alsa: 'ALSA DAC', cpal: 'System audio', null: 'Null output, for developers', pull: 'Pull stream', upnp: 'UPnP', airplay: 'AirPlay', cast: 'Chromecast', slim: 'Squeezelite', snapcast: 'Snapcast', mpd: 'MPD', net: 'The app', group: 'Group', rec: 'Recording', wav: 'WAV file', clock: 'Clock', bluos: 'BluOS', heos: 'HEOS', browser: 'This browser' };
  const deviceWords = (z) => {
    if (z.id === BROWSER) return 'Streams the files to this browser';
    const id = String(z.output || ''); const o = (state.outputs || []).find((x) => x.id === id);
    return [PROTO_WORDS[id.split(':')[0]] || id.split(':')[0], o && o.model].filter(Boolean).join(' · ');
  };
  // Something to move: the zone's queue, or the browser's.
  const hasQueue = (z) => !!z && (z.id === BROWSER ? state.browserQueue.length > 0 : !!(z.queue && z.queue.length));
  const otherZones = () => zoneList().filter((z) => z.id !== state.zone);
  // The browser takes over a queue at a song and a position; paused stays paused.
  const browserResumeAt = (index, posMs, paused) => {
    const it = state.browserQueue[index]; if (!it) { state.browserIndex = -1; renderBar(); return; }
    if (ytOf(it.uri)) { browserPlay(index); return; } // YouTube's player starts from the top
    if (state.ytOn) { yt.stop(); state.ytOn = false; ytPanel(null); }
    // The seek is for this song only: a later song (this one never loaded) is not moved
    audio.addEventListener('loadedmetadata', () => { if (state.browserIndex !== index) return; try { if (posMs > 1000) audio.currentTime = posMs / 1000; } catch { /* not seekable */ } renderBar(); }, { once: true });
    if (!paused) { it.moved = true; browserPlay(index); return; } // browserPlay says why when it can't play
    state.browserIndex = index;
    audio.src = it.stream ? it.stream : streamUrlFor(it.uri) || `/v1/library/tracks/${it.track_id}/file${tokenQ()}`;
    audio.preload = 'metadata'; audio.load();
    mediaSessionUpdate(it); renderBar();
  };
  // Whether this browser can play a queue item: a library file, a stream it reaches, YouTube
  const browserCanPlay = (it) => !!it && (/^track:\d+$/.test(it.uri || '') || !!ytOf(it.uri) || !!streamUrlFor(it.uri));
  // Move what plays to another device (owner, 2026-10-29): the song at its position, the queue,
  // repeat and radio; paused stays paused. Zone to zone is the node's transfer. This browser is no
  // zone of the node's, so out of it the target loads the browser's queue (POST /zones/{id}/load)
  // and the browser stops; into it the browser takes the zone's queue and the zone empties (radio
  // is the node's: it does not come along). The node's errors (403: a zone not open to listeners;
  // 400: an empty queue) show in the menu when it is open, else as a toast.
  let moving = false; // one move at a time (review: a second Move here sent the same queue twice)
  const moveTo = async (target) => {
    const from = state.zone;
    if (target === from || moving) return;
    moving = true; $$('[data-move]', zonePop).forEach((b) => { b.disabled = true; });
    // Zone to zone can take a while (the node opens the track first): say so at once when the menu is closed
    if (zonePop.hidden) { const tz0 = target === BROWSER ? { name: 'This browser' } : state.zones[target]; toast(`Moving to ${(tz0 && tz0.name) || 'the zone'}…`); }
    let resume = null; // the browser, paused for the move: back on when the move fails
    const cur = currentZone(); const curTitle = cur && cur.now_playing ? cur.now_playing.item.title || '' : '';
    const wasPlaying = cur && cur.transport === 'playing';
    try {
      if (from === BROWSER) {
        let queue = state.browserQueue;
        const cur = state.browserIndex >= 0 ? queue[state.browserIndex] : null;
        // YouTube plays in this page: a zone takes it only with yt-dlp on (as playSourceItems)
        const ytZones = (state.sources || []).some((x) => x.id === 'youtube' && x.enabled !== false && /zones/.test(x.status));
        if (!ytZones && queue.some((q) => ytOf(q.uri))) {
          if (cur && ytOf(cur.uri)) throw new Error('YouTube plays only in This browser');
          const rest = queue.filter((q) => !ytOf(q.uri));
          toast(`${countOf(queue.length - rest.length, 'YouTube items')} left out: YouTube plays only in This browser`);
          queue = rest;
        }
        const items = queue.map((it) => ({ uri: it.uri, title: it.title || null, artist: it.artist || null, album: it.album || null, duration_ms: it.duration_ms || null, image: it.image || null, limit: it.limit || null }));
        if (!items.length) throw new Error('Nothing to move: the queue is empty');
        const index = cur ? queue.indexOf(cur) : -1;
        const transport = !cur ? 'stopped' : audio.paused ? (audio.currentTime > 0 && !audio.ended ? 'paused' : 'stopped') : 'playing';
        // Pause here first: the node may take seconds to open the track, and both would play meanwhile
        if (transport === 'playing') { audio.pause(); resume = true; }
        const zs = await api('POST', `/zones/${encodeURIComponent(target)}/load`, { items, index: index >= 0 ? index : null, position_ms: Math.round((audio.currentTime || 0) * 1000), transport, repeat: browserRepeat(), radio: false });
        state.zones[zs.id] = zs;
        if (transport !== 'stopped' && zs.transport === 'stopped' && zs.error) throw new Error(zs.error);
        resume = null;
        if (state.ytOn) { yt.stop(); state.ytOn = false; ytPanel(null); } audioEl.removeAttribute('src'); audioEl.load();
        state.browserQueue = []; state.browserIndex = -1; try { localStorage.removeItem('pm.browserAt'); } catch { /* storage unavailable */ }
      } else if (target === BROWSER) {
        const z = await api('GET', `/zones/${encodeURIComponent(from)}`);
        if (!z.queue || !z.queue.length) throw new Error('Nothing to move: the queue is empty');
        const np = z.now_playing; const index = np ? np.index : (z.queue_index != null ? z.queue_index : -1); const pos = np ? np.position_ms || 0 : 0;
        // Refused before the zone empties: the song in hand must be one this browser can play
        const cur = index >= 0 ? z.queue[index] : null;
        if (cur && z.transport !== 'stopped' && !browserCanPlay(cur)) throw new Error(`“${cur.title || 'This song'}” plays on the node’s zones only; it stays on ${z.name}`);
        await api('DELETE', `/zones/${encodeURIComponent(from)}/queue`);
        if (state.zones[from]) { state.zones[from] = { ...state.zones[from], queue: [], now_playing: null, transport: 'stopped' }; }
        state.browserQueue = z.queue.map((it, k) => { const m = /^track:(\d+)$/.exec(it.uri || ''); return { id: Date.now() + k, uri: it.uri, track_id: m ? Number(m[1]) : undefined, stream: m ? null : streamUrlFor(it.uri) || it.uri, title: it.title, artist: it.artist, album: it.album, duration_ms: it.duration_ms, image: it.image || null, limit: it.limit || null }; });
        try { localStorage.setItem('pm.repeat', z.repeat || 'off'); } catch { /* storage unavailable */ }
        if (index >= 0 && index < state.browserQueue.length && z.transport !== 'stopped') browserResumeAt(index, pos, z.transport === 'paused'); else state.browserIndex = -1;
      } else {
        const was = state.zones[from];
        const zs = await api('POST', `/zones/${encodeURIComponent(from)}/transfer`, { to: target });
        state.zones[zs.id] = zs;
        if (state.zones[from]) state.zones[from] = { ...state.zones[from], queue: [], now_playing: null, transport: 'stopped' };
        if (was && was.transport !== 'stopped' && zs.transport === 'stopped' && zs.error) throw new Error(zs.error);
      }
      state.zone = target; try { localStorage.setItem('pm.zone', target); } catch { /* storage unavailable */ }
      // The bar says "Moving to …" until the target plays (the node answered, but its player may still be opening the track)
      const tz = state.zones[target];
      if (target !== BROWSER && wasPlaying && tz && tz.transport !== 'playing') startWait('move', target, curTitle);
      renderZoneName(); renderBar(); refreshQueue();
      const name = (currentZone() || {}).name || target;
      moving = false; // before the menu draws again, so its Move here buttons are on
      if (!zonePop.hidden) renderZonePop(); else toast(`Moved to ${name}`);
    } catch (e) {
      moving = false;
      if (resume) audio.play().catch(() => {});
      startFail(e.message, true);
      if (!zonePop.hidden) renderZonePop(e.message); else toast(e.message, 'error');
    } finally { moving = false; }
  };
  // Listening together on this node (presence.rs): who you listen along with, with Leave, and who
  // sees what you play. The node keeps it; the page asks every 20 s and when zones come or go.
  const personName = (l) => l.name || (l.key === 'owner' ? 'the owner' : shortKey(l.key));
  const VISIBLE = [['hidden', 'Nobody', 'what you play stays yours'], ['followed', 'People you follow', 'on Pubky'], ['everyone', 'Everyone on this node', '']];
  const renderAlong = () => {
    const a = state.along; $('#along').hidden = !a;
    if (a) $('#alongtext').textContent = `Along with ${personName(a)}`;
  };
  // Follow your music: into the group it plays on (you host it or listen along) from your own
  // zone, and back to your own zone once that group is gone. A zone you picked yourself stays.
  const followAlong = () => {
    const g = state.along || state.hosting; const target = g && g.zone ? g.zone.id : null;
    if (g && g.your_zone) state.alongHome = g.your_zone;
    let to = null;
    if (target && state.zones[target] && state.zone !== target && (state.zone === g.your_zone || !state.zones[state.zone])) to = target;
    else if (!g && !state.zones[state.zone] && state.alongHome && state.zones[state.alongHome]) to = state.alongHome;
    if (!g) state.alongHome = null;
    if (!to) return;
    state.zone = to; try { localStorage.setItem('pm.zone', to); } catch { /* storage unavailable */ }
    renderZoneName(); renderBar(); refreshQueue();
  };
  const refreshAlong = async () => {
    if (!state.me || isOffline() || document.hidden) return;
    try {
      const ls = await api('GET', '/presence');
      state.along = ls.find((l) => l.mine) || null; state.hosting = ls.find((l) => l.you && l.hosting) || null; state.presence = ls;
      if (state.presenceVisible == null) state.presenceVisible = (await api('GET', '/me/presence')).visible;
    } catch { return; }
    renderAlong(); followAlong(); renderListening();
  };
  // Home's "Listening now" (TASK-pinakes-social-ux L3): who else on this node is listening, what and
  // where, with Listen along when their zone is open to you. A zone you can't see: just who.
  // The node's own words name the reason (along already, someone else's zone, a maker's group,
  // a zone that is gone); a 403 gets a plain sentence
  const ALONG_ERRORS = { 403: 'That zone isn’t open to you.' };
  const renderListening = () => {
    const wrap = $('#h-listenwrap'); if (!wrap || state.view !== 'home') return;
    const others = (state.presence || []).filter((l) => !l.you);
    if (!others.length) { wrap.innerHTML = ''; return; }
    wrap.innerHTML = `<section class="section" id="h-listening"><div class="section-head"><h3>Listening now</h3></div><div class="list">${others.map((l, k) => {
      const name = personName(l);
      const face = l.avatar ? `<img class="thumb round" src="${esc(imgSrc(l.avatar))}" alt="" loading="lazy" data-err="hide">` : `<div class="avatar" aria-hidden="true">${esc(initials(name))}</div>`;
      const what = l.title ? ` to <b>${esc(l.title)}</b>${l.artist ? ` <span class="muted">· ${esc(l.artist)}</span>` : ''}` : '';
      const joining = state.joiningAlong === l.key;
      const act = l.mine ? '<span class="pill ok">Listening along</span>' : l.zone && l.can_join ? `<button class="btn sm" data-alongjoin="${k}" ${joining ? 'disabled aria-label="Joining…"' : `aria-label="Listen along with ${esc(name)}"`}>${joining ? '<span class="spin"></span> Joining…' : `${icon('zone')} Listen along`}</button>` : '';
      return `<div class="list-row listening">${face}${l.image ? `<img class="thumb" src="${esc(imgSrc(l.image))}" alt="" loading="lazy" data-err="hide">` : ''}<div class="grow"><div class="name">${esc(name)} is listening${what}</div>${l.zone ? `<div class="sub">on ${esc(l.zone.name)}${l.playing === false ? ' · paused' : ''}</div>` : ''}</div>${act}</div>`;
    }).join('')}</div></section>`;
    $$('[data-alongjoin]', wrap).forEach((b) => b.onclick = () => joinAlong(others[Number(b.dataset.alongjoin)], b));
  };
  // Listen along from a zone that is free to join: a node zone (This browser can't join a group),
  // not one someone else is listening on (joining stops what plays there) and not a listen-along
  // group. The zone in use when it is one, else a menu of them. The page only sees listeners who
  // let it, so the node is the guard: it refuses (409) a zone where someone else's music plays,
  // and then the menu offers the others (`skip`: zones refused so far).
  const joinAlong = (l, b, skip = new Set(), quiet = false) => {
    const busy = new Set((state.presence || []).filter((p) => !p.you && p.zone).map((p) => p.zone.id));
    const free = (id) => id !== BROWSER && !!state.zones[id] && !String(id).startsWith('group:') && !busy.has(id) && !skip.has(id) && (!l.zone || id !== l.zone.id);
    if (!skip.size && free(state.zone)) return doJoinAlong(l, state.zone, b, skip);
    const mine = zoneList().filter((z) => free(z.id));
    if (!mine.length) { if (quiet) return; toast(skip.size ? 'Someone else is listening on your other speakers too: Listen along needs free ones.' : 'Listening along needs speakers on this node that nobody else is using: This browser can’t join.', 'error'); return; }
    showMenu(b, [{ title: skip.size ? 'Someone listens there: listen from' : 'Listen from' }, ...mine.map((z) => ({ label: z.name, icon: 'zone', run: () => doJoinAlong(l, z.id, b, skip) }))], { all: true });
  };
  const doJoinAlong = async (l, zone, b, skip = new Set()) => {
    // The row may be drawn again while this runs (a zone came, the 20 s poll): it stays "Joining…"
    state.joiningAlong = l.key; renderListening();
    try {
      const zs = await api('POST', '/presence/along', { key: l.key, zone });
      state.zones[zs.id] = zs; state.zone = zs.id; try { localStorage.setItem('pm.zone', state.zone); } catch { /* storage unavailable */ }
      renderZoneName(); renderBar(); refreshQueue();
      state.joiningAlong = null;
      toast(`Listening along with ${personName(l)}`);
      await refreshAlong();
    } catch (e) {
      state.joiningAlong = null; renderListening();
      // Someone else's music plays on that zone: pick another of yours
      if (e.status === 409 && /someone else/i.test(e.raw || e.message)) {
        skip.add(zone); toast(e.message, 'error');
        const others = (state.presence || []).filter((p) => !p.you);
        const again = $$('#h-listenwrap [data-alongjoin]').find((x) => (others[Number(x.dataset.alongjoin)] || {}).key === l.key) || b;
        if (again.isConnected) joinAlong(l, again, skip, true);
        return;
      }
      toast(ALONG_ERRORS[e.status] || e.message, 'error');
    }
  };
  const leaveAlong = async () => {
    const a = state.along; const b = $('#alongleave'); if (!a) return;
    b.disabled = true;
    try {
      const zs = await api('DELETE', '/presence/along'); state.zones[zs.id] = zs;
      if (a.your_zone && state.zones[a.your_zone]) { state.zone = a.your_zone; try { localStorage.setItem('pm.zone', state.zone); } catch { /* storage unavailable */ } }
      state.along = null; renderAlong(); renderZoneName(); renderBar(); refreshQueue();
      toast(`You left ${personName(a)}'s music`);
    } catch (e) { toast(e.message, 'error'); } finally { b.disabled = false; }
  };
  $('#alongleave').onclick = leaveAlong;
  setInterval(refreshAlong, 20000);
  const setVisible = async (v) => {
    try { state.presenceVisible = (await api('PUT', '/me/presence', { visible: v })).visible; toast(v === 'hidden' ? 'Nobody sees what you play' : `${VISIBLE.find((x) => x[0] === v)[1]} can see what you play`); } catch (e) { toast(e.message, 'error'); }
  };
  const renderZonePop = (err) => {
    const movable = hasQueue(currentZone());
    const movingTo = state.starting && state.starting.what === 'move' ? state.starting.zone : null;
    zonePop.innerHTML = `<div class="pop-title">Play to</div>` + zoneList().map((z) => `
      <div class="pop-row"><button class="pop-item ${z.id === state.zone ? 'on' : ''}" data-zone="${esc(z.id)}">
        ${icon('zone')}<span><span class="name">${esc(z.name)}</span><span class="sub" title="${esc(z.id === BROWSER ? '' : z.output)}">${esc(deviceWords(z))}${z.id === BROWSER ? '' : `<span class="sr-only"> (${esc(z.output)})</span>`}</span></span>
        <span class="state">${z.transport === 'playing' ? `${icon('play')} playing` : ''}</span>
      </button>${movingTo === z.id ? '<span class="pop-act muted"><span class="spin"></span> Moving…</span>' : movable && z.id !== state.zone ? `<button class="pop-item pop-act" ${moving ? 'disabled' : ''} data-move="${esc(z.id)}" title="Move what plays, with the queue, to ${esc(z.name)}" aria-label="Move here: ${esc(z.name)}">Move here</button>` : ''}</div>`).join('')
      + (err ? `<div class="pop-err" role="alert">${esc(err.charAt(0).toUpperCase() + err.slice(1))}</div>` : '')
      // The sleep timer for the zone in use, here too (UX review: it was only in Now playing)
      + `<div class="pop-title">This zone</div>`
      + (movable && otherZones().length ? `<button class="pop-item" id="zpmove">${icon('zone')}<span><span class="name">Move to…</span><span class="sub">the song where it is, the queue, repeat and radio</span></span></button>` : '')
      + `<button class="pop-item" id="zpsleep">${icon('moon')}<span><span class="name">${esc(sleepLabel() === 'Sleep' ? 'Sleep timer' : sleepLabel())}</span><span class="sub">pause after a while</span></span></button>`
      + (isOffline() ? '' : `<div class="pop-title">Listening together</div><button class="pop-item" id="zpvisible">${icon('artist')}<span><span class="name">Who sees what you play</span><span class="sub">${esc((VISIBLE.find((x) => x[0] === (state.presenceVisible || 'hidden')) || VISIBLE[0])[1])}</span></span></button>`);
    const zs = $('#zpsleep', zonePop); if (zs) zs.onclick = () => { closePop(); showMenu($('#zonebtn'), [{ title: 'Sleep timer' }, ...[15, 30, 45, 60, 90].map((m) => ({ label: `${m} minutes`, icon: 'moon', run: () => setSleep(m * 60) })), { label: 'Off', icon: 'close', run: () => setSleep(null) }]); };
    const zv = $('#zpvisible', zonePop); if (zv) zv.onclick = () => { closePop(); showMenu($('#zonebtn'), [{ title: 'Who sees what you play' }, ...VISIBLE.map(([v, label, note]) => ({ label, note, icon: 'artist', on: (state.presenceVisible || 'hidden') === v, run: () => setVisible(v) }))]); };
    const zm = $('#zpmove', zonePop); if (zm) zm.onclick = () => { closePop(); showMenu($('#zonebtn'), [{ title: 'Move to' }, ...otherZones().map((o) => ({ label: o.name, icon: 'zone', note: deviceWords(o), run: () => moveTo(o.id) }))]); };
    $$('[data-move]', zonePop).forEach((b) => b.onclick = () => { b.disabled = true; b.innerHTML = '<span class="spin"></span> Moving…'; moveTo(b.dataset.move); });
    $$('[data-zone]', zonePop).forEach((b) => b.onclick = () => {
      state.zone = b.dataset.zone; localStorage.setItem('pm.zone', state.zone);
      closePop(); renderZoneName(); renderBar(); refreshQueue();
    });
  };
  $('#zonebtn').onclick = () => {
    if (!zonePop.hidden) { closePop(); return; }
    closeJobPop();
    renderZonePop();
    zonePop.hidden = false; $('#scrim').hidden = false; pushTransient();
    // The devices' models: the owner's output list, fetched once
    if (isOwner() && !state.outputs) api('GET', '/outputs').then((o) => { state.outputs = o; if (!zonePop.hidden) renderZonePop(); }).catch(() => {});
  };
  $$('[data-bnav]').forEach((b) => b.onclick = () => { const k = b.dataset.bnav; if (k === 'search') { if (!overlay.hidden) toggleNowPlaying(false); search.focus(); const q = search.value.trim(); setView('search', q ? { q } : undefined); return; } setView(k === 'library' ? (state.lastLib || 'albums') : k); });
  $('#me').onclick = () => setView('settings');
  $('#main').addEventListener('click', (e) => { const b = e.target.closest('[data-libnav]'); if (b) setView(b.dataset.libnav); });
  $('#jobchip').onclick = () => {
    const jp = jobPop(); if (!jp.hidden) { closeJobPop(); return; }
    closePop(); drawJobPop(); jp.hidden = false; $('#scrim').hidden = false; pushTransient();
  };
  $('#scrim').onclick = () => { closePop(); closeJobPop(); closeQPanel(); };
  // The menu button (desktop): collapses the sidebar to icons and back, remembered per browser.
  const shell = $('.shell');
  // The shell's arrangement; off the desktop the services warning moves from the sidebar to the top bar
  const applyShell = () => {
    const now = shellFor(window.innerWidth, layoutPref());
    document.documentElement.dataset.shell = now;
    if (densePref()) document.documentElement.dataset.dense = '1'; else delete document.documentElement.dataset.dense;
    const up = $('#upstreams');
    if (up) { if (now === 'desktop') { if (up.parentElement !== $('#sidebar')) $('#sidebar').appendChild(up); } else if (up.parentElement !== $('.topbar')) $('.topbar').insertBefore(up, $('#me')); }
  };
  window.addEventListener('resize', () => { if (shellFor(window.innerWidth, layoutPref()) !== shellMode()) applyShell(); });
  applyShell();
  const mq = (q) => !!(window.matchMedia && window.matchMedia(q).matches);
  const tablet = () => mq('(max-width: 1100px)');
  const applySide = () => { let m = 'auto'; try { m = localStorage.getItem('pm.sidebar') || 'auto'; } catch { /* no storage */ } shell.classList.toggle('collapsed', m === 'icons'); shell.classList.toggle('expanded', m === 'names'); };
  applySide();
  // Icon-only nav items keep their names as tooltips and accessible labels.
  $$('.nav-item').forEach((b) => { const t = (b.querySelector('span') || {}).textContent; if (t) { b.title = t; b.setAttribute('aria-label', t); } });
  $('#menu').setAttribute('aria-expanded', 'false');
  $('#menu').onclick = () => {
    const collapsedNow = shell.classList.contains('collapsed') || (tablet() && !shell.classList.contains('expanded'));
    const next = collapsedNow ? (tablet() ? 'names' : 'auto') : 'icons';
    try { localStorage.setItem('pm.sidebar', next); } catch { /* no storage */ }
    applySide();
    $('#menu').setAttribute('aria-expanded', String(next !== 'icons' && !(tablet() && next === 'auto')));
  };
  // Menus, popovers and dialogs close on back and the page stays (STEWARD-UX-BRIEF-2 navigation).
  // Opening one adds a transient history entry; back pops it and closes them. Closing one any
  // other way pops it too, and opening a page from inside one replaces it (setView).
  const transients = []; // { open: () => bool, close: (fromBack) => void }
  let popHandled = false; let selfPop = false; let popPending = null; let popResolve = null;
  const pushTransient = () => { if (history.state && history.state.transient) return; try { history.pushState({ ...(history.state || {}), transient: true }, '', location.pathname + location.search); } catch { /* sandboxed */ } };
  // Popping is asynchronous: a page opened right after waits for it (setView), or the pop would undo it.
  const popTransient = () => { if (transients.some((t) => t.open())) return; if (history.state && history.state.transient && !selfPop) { selfPop = true; popPending = new Promise((r) => { popResolve = r; setTimeout(() => { popPending = null; popResolve = null; r(); }, 600); }); history.back(); } };
  window.addEventListener('popstate', () => {
    if (selfPop) { selfPop = false; popHandled = true; if (popResolve) { popResolve(); popResolve = null; } popPending = null; return; }
    const open = transients.filter((t) => t.open()); if (!open.length) return;
    open.forEach((t) => t.close(true)); popHandled = true;
  });

  // ---- player bar ----------------------------------------------------------
  const ORDER = { bit_perfect: 0, enhanced: 1, lossless: 2, lossy: 3 };
  const LABEL = { bit_perfect: 'Bit-perfect', enhanced: 'Enhanced', lossless: 'Lossless', lossy: 'Lossy' };
  const Q_COLOUR = { bit_perfect: 'var(--q-bit)', enhanced: 'var(--q-enh)', lossless: 'var(--q-lossless)', lossy: 'var(--q-lossy)' };
  // The signal path as a block: the headline, then one row per stage (Now playing's Details and
  // the sound panel share it)
  const pathHtml = (np, cls = '') => {
    const h = np.signal_path ? headline(np.signal_path) : null;
    const fmtLine = np.signal_path ? fmtFormat(np.source_format) + ' → ' + fmtFormat(np.output_format) : '';
    return `<div class="path${cls ? ' ' + cls : ''}">
      <div class="path-head"><span class="qdot" style="width:10px;height:10px;border-radius:50%;background:${h ? Q_COLOUR[h] : 'var(--muted)'}"></span>${h ? LABEL[h] : 'Played by this browser'}<span class="muted" style="font-weight:400;margin-left:auto">${esc(fmtLine)}</span></div>
      ${np.signal_path ? np.signal_path.stages.map((st) => `<div class="path-row"><span class="pd" style="background:${Q_COLOUR[st.quality]}"></span><span class="pn">${esc(st.name)}</span><span class="pf">${esc(fmtFormat(st.in_format))} → ${esc(fmtFormat(st.out_format))}</span>${st.note ? `<span class="pnote">${esc(st.note)}</span>` : ''}</div>`).join('') : ''}
    </div>`;
  };
  // Stages that are DSP, not conversion: the pill says "DSP" when the path has one
  const DSP_STAGES = ['eq', 'convolution', 'crossfeed', 'loudness', 'headroom', 'volume'];
  const dspOn = (np) => !!(np && np.signal_path && np.signal_path.stages.some((s) => DSP_STAGES.includes(s.name)));
  // "24/96", "DSD64": bits and rate of a format, the short way
  const rateBits = (f) => (!f ? '' : f.kind === 'dsd' ? `DSD${Math.round(f.rate_hz / 44100)}` : `${f.bits}/${kHz(f.rate_hz)}`);
  // The pill's words (owner, 2026-10-29): what arrives, what the zone plays, and the verdict:
  // "FLAC 24/96 → 24/96 · Bit-perfect", "AAC 320 kbps → 16/44.1 · Lossy"
  const pathSummary = (np) => {
    const f = np.format; const sf = np.source_format; const of = np.output_format;
    const h = np.signal_path ? headline(np.signal_path) : null;
    const arrive = f ? (f.lossless || !f.codec ? `${f.codec || ''} ${rateBits(sf)}`.trim() : `${f.codec}${f.bitrate_kbps ? ` ${f.bitrate_kbps} kbps` : ''}`) : rateBits(sf);
    const out = of ? rateBits(of) : '';
    return [arrive + (out ? ` → ${out}` : ''), h ? LABEL[h] : ''].filter(Boolean).join(' · ');
  };
  // What is arriving, as people say it (STEWARD-UX-PLAYER-SOURCES §2): "FLAC 44.1/16 · TIDAL",
  // "AAC 320 kbps · Deezer", "MP3 128 kbps · preview", "FLAC 96/24 · local file", and "→ 48 kHz"
  // when the node converts for the zone. Null when the node doesn't say (older nodes).
  const kHz = (hz) => String(Math.round(hz / 100) / 10);
  const streamLabel = (np, long = false) => {
    const f = np && np.format; if (!f) return null;
    const sf = np.source_format; const of = np.output_format;
    const what = sf && sf.kind === 'dsd' ? `DSD${Math.round(sf.rate_hz / 44100)}`
      : f.lossless && sf ? (long ? `${f.codec} ${kHz(sf.rate_hz)} kHz / ${sf.bits} bit` : `${f.codec} ${kHz(sf.rate_hz)}/${sf.bits}`)
      : `${f.codec}${f.bitrate_kbps ? ` ${f.bitrate_kbps} kbps` : ''}`;
    const tier = f.tier ? ` ${f.tier.charAt(0).toUpperCase()}${f.tier.slice(1)}` : '';
    const from = f.preview ? 'preview' : f.service === 'library' ? 'local file' : f.service === 'stream' ? 'stream' : f.service === 'download' ? 'downloaded' : `${SERVICE_NAMES[f.service] || srcName(f.service)}${tier}`;
    const conv = sf && of && sf.kind !== 'dsd' && of.kind !== 'dsd' ? (of.rate_hz !== sf.rate_hz ? ` → ${kHz(of.rate_hz)} kHz` : f.lossless && of.bits < sf.bits ? ` → ${of.bits} bit` : '') : '';
    return `${what}${conv} · ${from}`;
  };
  // Why the node converts, from the signal path's own notes (resample, requantize, DSD to PCM)
  const conversionWhy = (np) => (np.signal_path ? np.signal_path.stages.filter((x) => /^(resample|requantize|dither|dop|src)$/.test(x.name) || (x.name === 'decode' && x.note)).map((x) => `${x.name}${x.note ? ': ' + x.note : ''}`) : []);
  const headline = (p) => p.stages.reduce((w, s) => (ORDER[s.quality] > ORDER[w] ? s.quality : w), 'bit_perfect');
  const albumIdOf = (it) => { const t = state.trackCache.get(it.uri); return t && t.album ? t.album.id : null; };
  // The artists a credit names ("Sefa, Dr. Peacock" → two links), like the node splits them
  const creditNames = (credit) => String(credit || '').split(/\s*(?:,|;|\/| & | feat\.? | ft\.? | featuring | x | vs\.? )\s*/i).map((x) => x.trim()).filter(Boolean);
  // Title, artists and album of what plays, each a link to its page (player bar and Now Playing)
  const npLinksHtml = (it, live) => {
    const songPage = npPageId(it.uri);
    const title = !live && songPage ? `<button class="lnk" data-np-song="${esc(songPage)}" title="Open this song's page">${esc(it.title || it.uri)}</button>` : esc(live || it.title || it.uri);
    const artistsHtml = live ? esc([it.title, it.album].filter(Boolean).join(' · ')) : [creditNames(it.artist).map((a) => `<button class="lnk" data-np-artist="${esc(a)}" title="Open ${esc(a)}">${esc(a)}</button>`).join(', '), it.album ? (albumIdOf(it) ? `<button class="lnk" data-np-album="${albumIdOf(it)}" title="Open the album">${esc(it.album)}</button>` : esc(it.album)) : ''].filter(Boolean).join(' · ');
    return { title, artistsHtml };
  };
  const bindNpLinks = (root, it, after) => {
    const go = (fn) => (e) => { e.stopPropagation(); if (after) after(); fn(); };
    $$('[data-np-song]', root).forEach((b) => b.onclick = go(() => setView('item', { kind: 'song', id: b.dataset.npSong })));
    $$('[data-np-album]', root).forEach((b) => b.onclick = go(() => setView('item', { kind: 'album', id: 'lib-' + b.dataset.npAlbum })));
    $$('[data-np-artist]', root).forEach((b) => b.onclick = go(async () => {
      const name = b.dataset.npArtist;
      // A library song: that artist's page in the library; else search for the name
      const tid = it.track_id ?? (/^track:(\d+)$/.exec(it.uri || '') || [])[1];
      if (tid != null) {
        try { const t = await api('GET', `/library/tracks/${tid}`); const a = (t.artists || []).find((x) => x.name.toLowerCase() === name.toLowerCase()); if (a && a.id != null) return setView('item', { kind: 'artist', id: 'lib-' + a.id }); } catch { /* search instead */ }
      }
      search.value = name; startSearch(name, 'everywhere'); setView('search', { q: name, in: 'everywhere' });
    }));
  };
  // The song playing now stands out in every track list on screen
  const markPlaying = () => {
    const z = currentZone(); const np = z && z.now_playing; const m = np && /^track:(\d+)$/.exec(np.item.uri || '');
    const id = m ? m[1] : null;
    $$('#main .track[data-tid]').forEach((r) => r.classList.toggle('cur', r.dataset.tid === id));
  };
  // The dashboard follows the zone: the whole screen when the song changes, else only the time
  const drawDashboard = () => {
    const box = $('#dash'); if (!box || state.view !== 'dashboard') return;
    const z = currentZone(); const np = z && z.now_playing; const raw = np && np.item;
    // A library song queued by another app may come without its details: the library has them
    const tid = trackIdOf(raw); const known = tid != null ? state.trackCache.get('track:' + tid) : null;
    if (tid != null && !known && !state.dashAsked) { state.dashAsked = tid; api('GET', `/library/tracks/${tid}`).then((t) => { rememberTracks([t]); state.dashKey = null; drawDashboard(); }).catch(() => {}).finally(() => { state.dashAsked = null; }); }
    const it = raw && known ? { ...raw, title: raw.title || known.title, artist: raw.artist || artists(known), album: raw.album || (known.album && known.album.title) } : raw;
    const key = it ? `${z.id}|${it.uri}|${it.title}` : `${z ? z.id : ''}|none`;
    if (key !== state.dashKey) {
      state.dashKey = key;
      box.innerHTML = it ? `${it.image || albumIdOf(it) ? `<div class="dash-bg" aria-hidden="true">${itemArt(it)}</div>` : ''}<div class="dash-inner">${itemArt(it, 'dash-art')}<div class="dash-text"><div class="eyebrow">Now playing on ${esc(z.name)}</div><h1>${esc(np.stream_title || it.title || '')}</h1><p class="dash-artist">${esc(it.artist || '')}</p>${it.album ? `<p class="muted">${esc(it.album)}</p>` : ''}<div class="dash-progress"><span id="dashbar"></span></div><div class="dash-times muted"><span id="dashpos"></span><span id="dashdur"></span></div></div></div><div class="dash-brand muted small">Pubky Music</div>`
        : `<div class="dash-inner empty"><div class="dash-text"><div class="eyebrow">${esc(z ? z.name : '')}</div><h1>Nothing playing</h1><p class="muted">Start something on this zone and it shows here.</p></div></div><div class="dash-brand muted small">Pubky Music</div>`;
    }
    if (it) {
      const pos = np.position_ms || 0; const dur = np.duration_ms || 0;
      const bar = $('#dashbar'); if (bar) bar.style.width = `${dur ? Math.min(100, (100 * pos) / dur) : 0}%`;
      const ps = $('#dashpos'); if (ps) ps.textContent = fmt(pos); const ds = $('#dashdur'); if (ds) ds.textContent = dur ? fmt(dur) : '';
    }
  };
  // Colour from where it plays (docs/ux/responsive.md §6). The player surfaces take the source's
  // accent: [light, dark], each checked in its theme (as text on the page ≥ 4.5:1, and the play
  // button's ink on it ≥ 4.5:1). Our own files and the homeserver keep ours. A tint only: no logos,
  // layouts or fonts of theirs, and lists keep the app's theme.
  const SRC_ACCENT = {
    spotify: ['#13803B', '#1ED760'], tidal: ['#007C89', '#33E8E0'], qobuz: ['#1F5BD6', '#6E9BFF'],
    deezer: ['#9B35F4', '#B35CFF'], youtube: ['#E00000', '#FF3232'], soundcloud: ['#C94300', '#FF5500'],
    bandcamp: ['#167A94', '#1DA0C3'], plex: ['#936608', '#E5A00D'], jellyfin: ['#9852AF', '#B26BC8'],
    jamendo: ['#CE364D', '#F24560'], audius: ['#C00ED3', '#D741E7'], archive: ['#3774A9', '#428BCA'], subsonic: ['#086ED5', '#0A84FF'],
  };
  const colourOn = (k) => { try { return localStorage.getItem(k) !== 'off'; } catch { return true; } };
  const svcOfItem = (it) => {
    const u = String((it && it.uri) || ''); const m = /^([a-z]+)[:-]/.exec(u);
    if (m && SRC_ACCENT[m[1]]) return m[1];
    if (/^https?:/.test(u)) { try { const k = srcSvc(new URL(u).hostname); return SRC_ACCENT[k] ? k : null; } catch { return null; } }
    return null;
  };
  let lastSrc;
  const setSourceColour = (np) => {
    const root = document.documentElement;
    // the node's own word on the service once now-playing carries it (#94), else the item's uri
    const svc = np && colourOn('pm.srcColour') ? (np.format && np.format.service ? srcSvc(np.format.service) : svcOfItem(np.item)) : null;
    if (svc === lastSrc) return; // renderBar runs several times a second: touch <html> only on a change
    lastSrc = svc;
    const a = svc && SRC_ACCENT[svc];
    if (a) { root.dataset.src = svc; root.style.setProperty('--src-l', a[0]); root.style.setProperty('--src-d', a[1]); }
    else { delete root.dataset.src; root.style.removeProperty('--src-l'); root.style.removeProperty('--src-d'); }
  };
  // Now playing's background: a soft gradient from the cover (its average colour, read through a small
  // canvas; a cover from another site that does not allow that gets none)
  const artTints = new Map();
  const setArtTint = () => {
    const root = document.documentElement; const img = $('#nowplaying .np-big img');
    const clear = () => { delete root.dataset.art; root.style.removeProperty('--np-h'); root.style.removeProperty('--np-s'); };
    if (!img || !colourOn('pm.artColour')) { clear(); return; }
    const src = img.currentSrc || img.src;
    const apply = (c) => { if (c) { root.dataset.art = '1'; root.style.setProperty('--np-h', String(c[0])); root.style.setProperty('--np-s', `${c[1]}%`); } else clear(); };
    if (artTints.has(src)) { const c = artTints.get(src); if (c === 'pending') clear(); else apply(c); return; }
    clear(); artTints.set(src, 'pending');
    if (artTints.size > 200) artTints.delete(artTints.keys().next().value);
    const probe = new Image(); probe.crossOrigin = 'anonymous';
    probe.onload = () => {
      let c = null;
      try {
        const cv = document.createElement('canvas'); cv.width = 12; cv.height = 12; const g = cv.getContext('2d');
        g.drawImage(probe, 0, 0, 12, 12); const d = g.getImageData(0, 0, 12, 12).data;
        let r = 0; let gr = 0; let b = 0; let n = 0;
        for (let i = 0; i < d.length; i += 4) { const hi = Math.max(d[i], d[i + 1], d[i + 2]); const lo = Math.min(d[i], d[i + 1], d[i + 2]); if (hi < 24 || lo > 235) continue; r += d[i]; gr += d[i + 1]; b += d[i + 2]; n += 1; }
        if (n) {
          // hue and saturation only: the lightness is set per theme, so text on it keeps its contrast
          const R = r / n / 255; const G = gr / n / 255; const B = b / n / 255; const hi = Math.max(R, G, B); const lo = Math.min(R, G, B); const l = (hi + lo) / 2; const dl = hi - lo;
          const sat = dl ? dl / (1 - Math.abs(2 * l - 1)) : 0;
          const hue = !dl ? 0 : hi === R ? 60 * (((G - B) / dl) % 6) : hi === G ? 60 * ((B - R) / dl + 2) : 60 * ((R - G) / dl + 4);
          c = [Math.round((hue + 360) % 360), Math.round(Math.min(1, sat) * 100)];
        }
      } catch { c = null; }
      artTints.set(src, c);
      const now = $('#nowplaying .np-big img'); if (now && (now.currentSrc || now.src) === src) apply(c);
    };
    probe.onerror = () => { artTints.set(src, null); const now = $('#nowplaying .np-big img'); if (now && (now.currentSrc || now.src) === src) clear(); };
    probe.src = src;
  };
  const renderBar = () => {
    markPlaying(); renderModes(); if (state.view === 'dashboard') drawDashboard();
    const z = currentZone(); const np = z && z.now_playing;
    setSourceColour(np);
    const playing = z && z.transport === 'playing';
    $('#toggle').innerHTML = icon(playing ? 'pause' : 'play');
    $('#toggle').setAttribute('aria-label', playing ? 'Pause' : 'Play');
    const q = $('#quality');
    // Pressing play answers at once (owner, 2026-10-29): "Starting on <device>…" until the zone
    // plays, or what went wrong, for a few seconds
    const st = state.starting && state.starting.zone === state.zone ? state.starting : null;
    const se = !st && state.startError && state.startError.zone === state.zone && Date.now() - state.startError.at < 10000 ? state.startError : null;
    const waitHtml = st ? `<div class="np-art placeholder"></div><div class="np-text"><div class="np-title np-wait" role="status"><span class="spin"></span> ${st.what === 'move' ? 'Moving to' : 'Starting on'} ${esc(z ? z.name : 'the zone')}…</div><div class="np-sub">${esc(st.title || '')}</div></div>` : '';
    const errHtml = se ? `<div class="np-art placeholder"></div><div class="np-text"><div class="np-title">Couldn’t start</div><div class="np-sub"><span class="warn-text" role="alert">${esc(se.msg)}</span></div></div>` : '';
    if (!np) {
      // A zone that stopped on an error says so here, until the next track starts (the node clears it)
      const err = z && z.transport === 'stopped' && z.error;
      $('#np').innerHTML = waitHtml || errHtml || `<div class="np-art placeholder"></div><div class="np-text"><div class="np-title muted">${err ? 'Stopped' : 'Nothing playing'}</div><div class="np-sub">${err ? `<span class="warn-text" role="alert" title="${esc(z.error)}">Couldn’t play on ${esc(z.name)}: ${esc(z.error)}</span>` : esc(z ? z.name : '')}</div></div>`;
      $('#pos').textContent = '0:00'; $('#dur').textContent = '0:00'; seek.value = 0; setFill(seek);
      q.className = 'quality'; $('.qtext', q).textContent = '—'; $('.qsrc', q).innerHTML = ''; $('.qdsp', q).hidden = true; $('.qshort', q).innerHTML = `<span class="qdev">${esc(z ? z.name : '')}</span>`; q.title = 'Sound: where it plays from, how it gets to you, DSP and volume';
      q.setAttribute('aria-label', `Sound. Nothing playing on ${z ? z.name : 'this zone'}. Open the sound panel`);
      syncQPanel();
      return;
    }
    const it = np.item;
    const live = np.stream_title;
    const links = npLinksHtml(it, live);
    $('#np').innerHTML = waitHtml || errHtml || `<button class="np-art np-artbtn" id="npart" title="Now playing" aria-label="Open Now playing">${itemArt(it, 'sm', npSvc(np), true)}</button><div class="np-text"><div class="np-title">${links.title}</div><div class="np-sub">${links.artistsHtml}</div></div>`;
    if (!waitHtml && !errHtml) bindNpLinks($('#np'), it, () => { if (!overlay.hidden) toggleNowPlaying(); });
    $('#pos').textContent = fmt(np.position_ms); $('#dur').textContent = fmt(np.duration_ms);
    if (!seek.dataset.dragging) { seek.value = np.duration_ms ? Math.round((np.position_ms / np.duration_ms) * 1000) : 0; setFill(seek); }
    // The pill (owner, 2026-10-29): where it plays from (the provider's mark), the path in short
    // and whether DSP is on; on phones the device and a quality badge (CSS swaps .qtext for .qshort)
    const h = np.signal_path ? headline(np.signal_path) : null;
    const dsp = dspOn(np);
    q.className = 'quality ' + (h ? 'q-' + h : '') + (dsp ? ' dsp' : '');
    const svc = npSvc(np); const svcName = svc === 'library' ? 'your library' : svc ? srcName(svc) : null;
    $('.qsrc', q).innerHTML = svc ? svcIcon(svc) : '';
    const words = np.signal_path || np.format ? pathSummary(np) : 'Played by this browser';
    $('.qtext', q).textContent = words;
    $('.qdsp', q).hidden = !dsp;
    const badge = np.output_format ? rateBits(np.output_format) : np.format ? (np.format.lossless ? 'Lossless' : np.format.codec || '') : '';
    $('.qshort', q).innerHTML = `<span class="qdev">${esc(z.name)}</span>${badge ? `<span class="qbadge">${esc(badge)}</span>` : ''}`;
    q.setAttribute('aria-label', `Sound: ${svcName ? `from ${svcName}, ` : ''}${words}${dsp ? ', DSP on' : ''}. Playing on ${z.name}. Open the sound panel`);
    q.title = np.signal_path ? np.signal_path.stages.map((s) => `${s.name}: ${fmtFormat(s.in_format)} → ${fmtFormat(s.out_format)} (${s.quality})${s.note ? ' · ' + s.note : ''}`).join('\n') : 'This browser decodes the original file itself; the node makes no signal-path claim.';
    syncQPanel();
  };
  // ---- immediate feedback on play (owner, 2026-10-29) ----
  // The play control just pressed: it shows a spinner until the music starts (capture phase, so
  // every play button on every page counts without each call site knowing)
  document.addEventListener('click', (e) => { const b = e.target.closest && e.target.closest('[data-act="play"], [data-splay], [data-tileplay], [data-dplay], [data-aplay], [data-vplay], #iplay, #ishuffle, #toggle, [data-npt="toggle"]'); if (b) state.startAnchor = { el: b, at: Date.now() }; }, true);
  // `replace`: the queue is being replaced, so the song still playing does not end the wait
  const startWait = (what, zoneId = state.zone, title = '', replace = false) => {
    startDone(true);
    const a = state.startAnchor && Date.now() - state.startAnchor.at < 1500 ? state.startAnchor.el : null;
    if (a) a.classList.add('starting');
    const zw = state.zones[zoneId]; const oldItem = replace && zw && zw.now_playing ? zw.now_playing.item.id : null;
    // `sent`: a replace waits for the node's answer first, so the old queue moving on by itself
    // (its song ended) isn't taken for the new one starting
    const at = Date.now(); state.starting = { zone: zoneId, what, title, at, anchor: a, oldItem, sent: !replace }; state.startError = null;
    setTimeout(() => { if (state.starting && state.starting.at === at) { const zn = state.zones[zoneId]; startFail(`${zn ? zn.name : 'The zone'} did not start playing in 20 seconds`); } }, 20000);
    renderBar();
  };
  // The node took the replace: from now on the zone playing another song ends the wait (it may
  // already be playing it)
  const startSent = () => {
    const s = state.starting; if (!s || s.sent) return; s.sent = true;
    const z = state.zones[s.zone]; if (z && z.transport === 'playing' && z.now_playing && z.now_playing.item.id !== s.oldItem) startDone();
  };
  const startDone = (quiet) => { const s = state.starting; if (!s) return; if (s.anchor) s.anchor.classList.remove('starting'); state.starting = null; if (!quiet) renderBar(); if (s.what === 'move' && !zonePop.hidden) renderZonePop(); };
  const startFail = (msg, quiet) => {
    const s = state.starting; if (!s) return; startDone(true);
    // The song still playing (a refused replace leaves it on) doesn't clear the error: a new one does
    const zf = state.zones[s.zone]; const still = zf && zf.transport === 'playing' && zf.now_playing ? zf.now_playing.item.id : null;
    state.startError = { zone: s.zone, msg: msg || 'something went wrong', at: Date.now(), still };
    if (!quiet) toast(`${(state.zones[s.zone] || {}).name || (s.zone === BROWSER ? 'This browser' : 'The zone')}: ${state.startError.msg}`, 'error');
    renderBar(); setTimeout(renderBar, 10500);
  };
  // The browser's own player: it starts (or fails) by itself
  audio.addEventListener('playing', () => { if (state.starting && state.starting.zone === BROWSER) startDone(); if (state.startError && state.startError.zone === BROWSER) { state.startError = null; renderBar(); } });
  audio.addEventListener('pause', () => { if (state.starting && state.starting.zone === BROWSER) startDone(); });
  $('#np').onclick = () => toggleNowPlaying();
  $('#np').onkeydown = (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggleNowPlaying(); } };
  // The quality sheet needs to know what is streaming (now_playing.format); without it (This browser,
  // an older node) the label opens Now playing, where the signal path is
  // ---- the sound panel (owner, 2026-10-29): the pill in full ----
  // Zone and device, the source, the signal path, this zone's DSP with quick switches, volume.
  const qPanel = $('#qpanel');
  let qPanelKey = null;
  const closeQPanel = (fromBack) => { if (qPanel.hidden) return; qPanel.hidden = true; $('#scrim').hidden = true; $('#quality').setAttribute('aria-expanded', 'false'); if (qPanel.contains(document.activeElement) || document.activeElement === document.body) $('#quality').focus(); if (fromBack !== true) popTransient(); };
  const qpKey = () => { const z = currentZone(); const np = z && z.now_playing; const cfg = state.dspCfg && state.dspCfg[state.zone]; return `${state.zone}|${z ? z.transport : ''}|${np ? np.item.id : ''}|${np && np.signal_path ? np.signal_path.stages.length : ''}|${cfg ? JSON.stringify(cfg) : ''}|${state.outputs ? 1 : 0}`; };
  // Re-render the open panel only when what it says changed (renderBar runs often while playing)
  const syncQPanel = () => { if (qPanel.hidden) return; const k = qpKey(); if (k !== qPanelKey) renderQPanel(); };
  const loadDsp = async () => { if (isBrowser()) return; const zone = state.zone; try { const cfg = await api('GET', `/zones/${encodeURIComponent(zone)}/dsp`); (state.dspCfg = state.dspCfg || {})[zone] = cfg; } catch (e) { (state.dspCfg = state.dspCfg || {})[zone] = { error: e.message }; } syncQPanel(); };
  // A quick switch: the whole config goes back with one part changed (what was set stays set)
  // One switch at a time (a second one is built on the first's answer), and nothing set is lost:
  // EQ remembers which bands were on (kept in this browser per zone), crossfeed and room correction
  // switch their own `enabled` and keep their values.
  const eqKey = (zone) => `pm.eqOn.${zone}`;
  const toggleDsp = async (what, on) => {
    if (state.dspBusy) return;
    state.dspBusy = true; state.dspFocus = what;
    $$('[data-dsp]', qPanel).forEach((c) => { c.disabled = true; });
    try {
    const zone = state.zone; const cfg = { ...((state.dspCfg || {})[zone] || {}) }; delete cfg.error;
    if (what === 'eq') {
      const bands = cfg.eq || [];
      if (!on) { try { localStorage.setItem(eqKey(zone), JSON.stringify(bands.map((b) => b.enabled !== false))); } catch { /* no storage: all come back on */ } }
      let was = null; if (on) { try { was = JSON.parse(localStorage.getItem(eqKey(zone)) || 'null'); } catch { was = null; } }
      const back = Array.isArray(was) && was.length === bands.length && was.some(Boolean) ? was : null;
      cfg.eq = bands.map((b, i) => ({ ...b, enabled: on ? (back ? back[i] : true) : false }));
    }
    if (what === 'room' && cfg.convolution) cfg.convolution = { ...cfg.convolution, enabled: on };
    if (what === 'loudness') { if (!on && cfg.loudness && cfg.loudness !== 'off') state.loudnessLast = cfg.loudness; cfg.loudness = on ? (state.loudnessLast || 'album') : 'off'; }
    if (what === 'crossfeed') cfg.crossfeed = cfg.crossfeed ? { ...cfg.crossfeed, enabled: on } : on ? { level_db: -6, cutoff_hz: 700, delay_us: 300, enabled: true } : null;
    try { await api('PUT', `/zones/${encodeURIComponent(zone)}/dsp`, cfg); (state.dspCfg = state.dspCfg || {})[zone] = cfg; toast('Applies from the next track'); } catch (e) { toast(e.message, 'error'); }
    await loadDsp();
    } finally { state.dspBusy = false; qPanelKey = null; syncQPanel(); }
  };
  const renderQPanel = () => {
    const z = currentZone(); if (!z) return; const np = z.now_playing; const cfg = (state.dspCfg || {})[state.zone];
    // A re-render (the outputs arrived, a switch answered) keeps the keyboard where it was
    const ae = document.activeElement; let had = qPanel.contains(ae) ? (ae.id ? '#' + ae.id : ae.dataset && ae.dataset.dsp ? `[data-dsp="${ae.dataset.dsp}"]` : null) : null;
    // A switch that answered gets the keyboard back (disabling it while it saved dropped the focus)
    if (!had && state.dspFocus && !state.dspBusy) { had = `[data-dsp="${state.dspFocus}"]`; state.dspFocus = null; }
    const svc = np && npSvc(np);
    const row = (id, label, on, disabled, note) => `<label class="qp-row${disabled ? ' off' : ''}"><span class="grow"><span class="name">${label}</span>${note ? `<span class="sub">${esc(note)}</span>` : ''}</span><input type="checkbox" class="switch" data-dsp="${id}" ${on ? 'checked' : ''} ${disabled ? 'disabled' : ''} aria-label="${esc(label)}"></label>`;
    let dsp;
    if (isBrowser()) dsp = '<p class="qp-note">This browser plays the file as it is: the node’s DSP is for its zones.</p>';
    else if (!cfg) dsp = '<p class="qp-note"><span class="spin"></span> Reading this zone’s DSP…</p>';
    else if (cfg.error) dsp = `<p class="qp-note">${esc(cfg.error)}</p>`;
    else {
      const eq = cfg.eq || []; const eqOn = eq.some((b) => b.enabled !== false); const conv = cfg.convolution; const cf = cfg.crossfeed; const ro = !isOwner();
      dsp = row('eq', 'Parametric EQ', eq.length > 0 && eqOn, ro || !eq.length, eq.length ? countOf(eq.length, 'bands') : 'no bands set')
        + row('room', 'Room correction', !!(conv && conv.enabled !== false), ro || !conv, conv ? String(conv.path).split('/').pop() : 'no impulse response set')
        + row('loudness', 'Volume leveling', !!cfg.loudness && cfg.loudness !== 'off', ro, cfg.loudness && cfg.loudness !== 'off' ? `ReplayGain by ${cfg.loudness}` : 'ReplayGain off')
        + row('crossfeed', 'Headphone crossfeed', !!(cf && cf.enabled !== false), ro, cf && cf.enabled !== false ? `${cf.level_db} dB below ${cf.cutoff_hz} Hz` : 'off')
        + (isOwner() ? `<button class="btn sm ghost" id="qpdsp">${icon('settings')} All DSP settings</button>` : '');
    }
    const out = (state.outputs || []).find((o) => o.id === z.output); const hv = out && out.caps && out.caps.hw_volume && out.caps.hw_volume.kind === 'db' ? out.caps.hw_volume : null;
    const bar = $('#vol');
    const vmin = hv ? hv.min : -60; const vmax = hv ? hv.max : 0; const vstep = hv ? hv.step || 1 : 1; const vval = Math.min(vmax, Math.max(vmin, Number(bar.value)));
    const srcLabel = np ? (svc === 'library' ? 'Your library' : svc ? srcName(svc) : (np.stream_title ? 'Live stream' : 'Unknown source')) : 'Nothing playing';
    qPanel.innerHTML = `<div class="pop-title">Playing on</div>
      <div class="qp-zone">${icon('zone')}<span class="grow"><span class="name">${esc(z.name)}</span><span class="sub" title="${esc(z.id === BROWSER ? '' : z.output)}">${esc(deviceWords(z))}</span></span><span class="state">${z.transport === 'playing' ? 'playing' : z.transport === 'paused' ? 'paused' : ''}</span></div>
      <div class="pop-title">Source</div>
      <div class="qp-src">${svc ? `<span class="src-badge ico" aria-hidden="true">${svcIcon(svc)}</span>` : ''}<span class="grow"><span class="name">${esc(srcLabel)}</span>${np ? `<span class="sub">${esc(streamLabel(np, true) || (isBrowser() ? 'Decoded by this browser' : ''))}</span>` : ''}</span></div>
      <div class="pop-title">Signal path</div>
      ${np && np.signal_path ? pathHtml(np, 'compact') : `<p class="qp-note">${isBrowser() ? 'This browser decodes the file itself; the node makes no signal-path claim.' : 'Nothing playing.'}</p>`}
      <div class="pop-title">DSP · ${esc(z.name)}</div>
      <div class="qp-dsp">${dsp}</div>
      <div class="pop-title">Volume</div>
      <div class="qp-vol"><svg class="ic" aria-hidden="true"><use href="#i-volume"/></svg><input type="range" id="qpvol" min="${vmin}" max="${vmax}" step="${vstep}" value="${vval}" aria-label="Volume of ${esc(z.name)}${hv ? '' : ', in dB'}"></div>
      <div class="qp-foot">${np && np.format ? '<button class="btn sm ghost" id="qpquality">Quality choices…</button>' : ''}<button class="btn sm ghost" id="qpnow">${icon('play')} Now playing</button></div>`;
    qPanelKey = qpKey();
    $$('[data-dsp]', qPanel).forEach((c) => { if (state.dspBusy) c.disabled = true; c.onchange = () => toggleDsp(c.dataset.dsp, c.checked); });
    const qd = $('#qpdsp', qPanel); if (qd) qd.onclick = () => { closeQPanel(); state.dspOpen = state.zone; setView('settings', 'playback'); };
    const qq = $('#qpquality', qPanel); if (qq) qq.onclick = () => { closeQPanel(); qualitySheet(); };
    const qn = $('#qpnow', qPanel); if (qn) qn.onclick = () => { closeQPanel(); toggleNowPlaying(true); };
    const v = $('#qpvol', qPanel); setFill(v);
    if (had) { const back = $(had, qPanel); if (back) back.focus(); }
    v.oninput = () => setFill(v);
    v.onchange = () => { bar.value = v.value; setFill(bar); if (isBrowser()) { audio.volume = Math.pow(10, v.value / 20); return; } api('POST', `/zones/${state.zone}/volume`, { volume: { db: Number(v.value) } }).catch((e) => toast(e.message, 'error')); };
  };
  $('#quality').onclick = () => {
    if (!qPanel.hidden) { closeQPanel(); return; }
    closePop(); closeJobPop();
    renderQPanel();
    qPanel.hidden = false; $('#scrim').hidden = false; $('#quality').setAttribute('aria-expanded', 'true'); pushTransient();
    const first = $('[data-dsp]:not(:disabled), #qpvol, button', qPanel); if (first) first.focus();
    loadDsp();
    if (isOwner() && !state.outputs) api('GET', '/outputs').then((o) => { state.outputs = o; syncQPanel(); }).catch(() => {});
  };

  // Now Playing's Pubky row: the heart, your tags on this song (added here), and its full page
  // The heart in the title row and the Tags sheet, for the song on screen. The heart draws at once
  // from the known favourites; an answer that comes back after a newer render is dropped.
  let npGen = 0;
  const fillNpPubky = async (it, gen = npGen) => {
    const live = () => gen === npGen;
    const tid = it.track_id ?? (/^track:(\d+)$/.exec(it.uri || '') || [])[1];
    const songPage = npPageId(it.uri);
    const pageUri = songPage && !/^lib-/.test(songPage) ? uriOf('song', songPage) : null;
    let t = null;
    let fav = tid != null ? state.favUris.has(`track:${tid}`) : pageUri ? state.favUris.has(pageUri) : false;
    const ref = () => (tid != null ? { track_id: Number(tid) } : pageUri ? { uri: pageUri } : null);
    const drawHeart = () => {
      const h = $('#npheart', overlay); if (!h || !live()) return;
      if (!ref()) { h.innerHTML = ''; return; }
      const word = fav ? 'In favourites' : 'Add to favourites';
      const had = document.activeElement && document.activeElement.id === 'npfav';
      h.innerHTML = `<button class="icon-btn np-heart ${fav ? 'on' : ''}" id="npfav" aria-pressed="${fav}" aria-label="${word}" title="${word}">${icon(fav ? 'heart-fill' : 'heart')}</button>`;
      $('#npfav', overlay).onclick = async () => { try { await setFavourite(ref(), !fav); fav = !fav; drawHeart(); } catch (e) { toast(e.message, 'error'); } };
      if (had) $('#npfav', overlay).focus({ preventScroll: true });
      npRestore();
    };
    drawHeart();
    if (tid != null) { try { t = await api('GET', `/library/tracks/${tid}`); } catch { t = null; } if (!live()) return; if (t) { fav = isFav(t); drawHeart(); } }
    if (!$('#nppubky', overlay)) return;
    const target = t ? { track_id: Number(tid) } : pageUri ? { uri: pageUri } : null;
    const pageRow = songPage ? `<div class="np-pubky-row"><button class="btn ghost sm" id="nppage">${icon('compass')} Song page: sources, versions, tags</button></div>` : '';
    let tags = [];
    if (target) { try { tags = (await api('GET', `/tags?${new URLSearchParams(Object.entries(target).map(([k, v]) => [k, String(v)]))}`)).tags || []; } catch { tags = []; } }
    const box = $('#nppubky', overlay); if (!box || !live()) return;
    box.innerHTML = pageRow + (target
      ? `<div class="np-tags"><span class="eyebrow">Tags</span>${tags.map((x) => `<span class="chip tag"><button class="chiplink" data-np-tag="${esc(x.label)}">#${esc(x.label)}</button>${x.pending ? '<span class="muted small tag-wait" title="Published once this file is matched to a song">waits for a match</span>' : ''}</span>`).join('')}<input class="input taginput" id="nptagadd" placeholder="Add a tag…" maxlength="20" aria-label="Add a tag to this song"></div>`
      : t ? '<p class="muted small">Pick which song this is (the ? on its row) to tag it and keep it in playlists.</p>'
        : `<p class="muted">Nothing to tag here: ${it.stream || np2Stream(it) ? 'this is a live stream' : 'this song has no page yet'}.</p>`);
    const pb = $('#nppage', overlay); if (pb) pb.onclick = () => setView('item', { kind: 'song', id: songPage });
    $$('[data-np-tag]', box).forEach((x) => x.onclick = () => setView('tag', x.dataset.npTag));
    const ta = $('#nptagadd', overlay);
    if (ta) {
      // A half-typed tag survives re-renders while the song stays the same
      if (state.npTagDraft && state.npTagDraft.id === it.id) ta.value = state.npTagDraft.value;
      ta.oninput = () => { state.npTagDraft = { id: it.id, value: ta.value }; };
      npRestore();
      ta.onkeydown = async (e) => { if (e.key !== 'Enter' || !ta.value.trim()) return; try { await api('PUT', '/tags', { ...target, label: ta.value.trim() }); toast(`Tagged #${ta.value.trim().toLowerCase()}`); state.npTagDraft = null; fillNpPubky(it); } catch (er) { toast(er.message, 'error'); } };
    }
  };
  const np2Stream = (it) => !!(it && it.album === 'Internet radio');
  // ---- Now Playing overlay -------------------------------------------------
  const overlay = $('#nowplaying');
  // Back closes it like a menu; opening a page from it takes its history entry (setView)
  const npTr = { open: () => !overlay.hidden, close: (fromBack) => toggleNowPlaying(false, fromBack) };
  const toggleNowPlaying = (force, fromBack) => {
    const open = force === true ? true : force === false ? false : overlay.hidden;
    const was = !overlay.hidden;
    overlay.hidden = !open;
    document.body.classList.toggle('np-open', open);
    if (open) { renderNowPlaying(); if (!was) { transients.push(npTr); pushTransient(); } return; }
    const k = transients.indexOf(npTr); if (k >= 0) transients.splice(k, 1);
    if (npCompact()) state.npSheet = null;
    if (was && fromBack !== true) popTransient();
  };
  // Phones: swipe up on the mini player opens Now playing; swipe down from its top closes it
  {
    let y0 = null; let d0 = null;
    const player = $('.player');
    player.addEventListener('touchstart', (e) => { y0 = shellMode() === 'phone' && !e.target.closest('input') ? e.touches[0].clientY : null; }, { passive: true });
    player.addEventListener('touchend', (e) => { if (y0 != null && y0 - e.changedTouches[0].clientY > 40 && overlay.hidden) toggleNowPlaying(true); y0 = null; });
    overlay.addEventListener('touchstart', (e) => { d0 = shellMode() === 'phone' && overlay.scrollTop <= 0 && !e.target.closest('input, .np-sheet') ? e.touches[0].clientY : null; }, { passive: true });
    overlay.addEventListener('touchend', (e) => { if (d0 != null && e.changedTouches[0].clientY - d0 > 80 && !overlay.hidden) toggleNowPlaying(); d0 = null; });
  }
  // Now playing on one screen (docs/ux/responsive.md, build step 2): the player, and five sheets
  // (Queue, Lyrics, Details, Sources, Tags). Phones open a sheet over the player from the bottom row;
  // tablets and desktops keep one open beside it. The open sheet survives re-renders (state.npSheet).
  const NP_SHEETS_ALL = [['queue', 'Queue', 'queue'], ['lyrics', 'Lyrics', 'notes'], ['details', 'Details', 'info'], ['sources', 'Sources', 'archive'], ['tags', 'Tags', 'compass']];
  const npSheets = () => NP_SHEETS_ALL.filter(([k]) => k !== 'tags' || isOwner()); // listeners cannot tag
  // Compact: the phone arrangement (sheets over the player), also on tablets narrower than two columns need
  const npCompact = () => shellMode() === 'phone' || window.innerWidth <= 720;
  const npSheetNow = () => { const k = npSheets().some(([x]) => x === state.npSheet) ? state.npSheet : null; return npCompact() ? k : k || 'queue'; };
  // What a re-render keeps: the sheet's scroll and the focused control
  const npKeep = () => {
    const a = document.activeElement; const body = $('.np-sheet-body', overlay);
    let focus = null;
    if (a && overlay.contains(a)) {
      const where = a.closest('.np-sheet-tabs') ? '.np-sheet-tabs ' : a.closest('.np-tabs') ? '.np-tabs ' : a.closest('.np-strip') ? '.np-strip ' : a.closest('.np-transport') ? '.np-transport ' : '';
      if (a.id) focus = '#' + a.id;
      else for (const k of ['npsheet', 'npqi', 'npt']) if (a.dataset && a.dataset[k] != null) { focus = `${where}[data-${k}="${a.dataset[k]}"]`; break; }
    }
    const lines = $('.lyric-lines', overlay);
    return { sheet: overlay.dataset.sheet || '', scroll: body ? body.scrollTop : 0, lyric: lines ? lines.scrollTop : 0, focus };
  };
  const renderNowPlaying = () => {
    const keep = npKeep(); npGen += 1; const gen = npGen;
    const z = currentZone(); const np = z && z.now_playing;
    if (!np) {
      setTimeout(setArtTint, 0); // no cover: no tint
      overlay.innerHTML = `<div class="np-page np2"><div class="np-top"><button class="icon-btn" id="npclose" aria-label="Close Now playing" title="Close">${icon('chevron')}</button><span class="spacer"></span><button class="btn ghost" id="npq">${icon('queue')} Queue</button></div><div class="empty">${icon('notes')}<h3>Nothing playing on ${esc(z ? z.name : 'this zone')}</h3><p>Pick an album or a track and press play.</p></div></div>`;
      $('#npclose').onclick = () => toggleNowPlaying();
      $('#npq').onclick = () => setView('queue');
      return;
    }
    const it = np.item;
    const q = (isBrowser() ? state.browserQueue : state.queue) || [];
    const idx = z.queue_index != null && q[z.queue_index] && q[z.queue_index].id === it.id ? z.queue_index : q.findIndex((x) => x.id === it.id);
    const rest = idx >= 0 ? q.slice(idx + 1) : [];
    const h = np.signal_path ? headline(np.signal_path) : null;
    const colour = Q_COLOUR;
    const fmtLine = np.signal_path ? fmtFormat(np.source_format) + ' → ' + fmtFormat(np.output_format) : '';
    const sheet = npSheetNow();
    const songPage = npPageId(it.uri);
    const links = np.stream_title ? null : npLinksHtml(it, null);
    const sheetBody = (k) => {
      if (k === 'queue') {
        return `<p class="muted small np-sheet-meta">${rest.length ? `Playing next · ${countOf(rest.length, 'songs')}` : 'Nothing after this song'}</p>`
          + (rest.length ? `<div class="list np-queue">${rest.slice(0, 100).map((n, k2) => `<button class="list-row np-qitem" data-npqi="${idx + 1 + k2}" title="Play this now"><span class="grow"><span class="name">${esc(n.title || n.uri)}</span><span class="sub">${esc(n.artist || '')}</span></span><span class="muted small">${n.duration_ms ? fmt(n.duration_ms) : ''}</span></button>`).join('')}</div>` : '')
          + `<div class="np-sheet-foot"><button class="btn sm ghost" id="npqfull">${icon('queue')} Reorder or clear the queue</button></div>`;
      }
      if (k === 'lyrics') return '<p class="muted" id="nplyricsload">Looking for lyrics…</p><div class="lyrics" id="nplyrics" hidden></div><p class="muted" id="nplyricsnone" hidden>No lyrics found for this song.</p>';
      if (k === 'details') {
        return pathHtml(np)
            + `<dl class="kv np-facts">${it.album ? `<dt>Album</dt><dd>${esc(it.album)}</dd>` : ''}${it.artist ? `<dt>Artist</dt><dd>${esc(it.artist)}</dd>` : ''}${np.duration_ms ? `<dt>Length</dt><dd>${fmt(np.duration_ms)}</dd>` : ''}<dt>Playing on</dt><dd>${esc(z.name)}</dd></dl>`;
      }
      if (k === 'sources') return `<div id="npsources">${songPage ? '<div class="skeleton" style="height:80px"></div>' : '<p class="muted">This song has no page of its own yet, so its other sources are not known.</p>'}</div>`;
      return '<div class="np-pubky" id="nppubky" aria-live="polite"></div>';
    };
    const tab = ([k, l, ic], cls) => { const on = sheet === k ? ' on' : ''; return `<button class="${cls === 'chip' ? 'chip' : 'np-tab'}${on}" data-npsheet="${k}" ${cls === 'chip' ? `role="tab" id="nptab-${k}" aria-controls="npsheetpanel" aria-selected="${sheet === k}" tabindex="${sheet === k ? 0 : -1}"` : `aria-pressed="${sheet === k}"`}>${cls === 'chip' ? '' : icon(ic)}<span>${l}</span></button>`; };
    const title = np.stream_title ? esc(np.stream_title) : links.title;
    const artist = np.stream_title ? esc(it.artist || '') : links.artistsHtml;
    overlay.innerHTML = `
      <div class="np-page np2${sheet ? ' has-sheet' : ''}${npCompact() ? ' compact' : ''}">
        <div class="np-top"><button class="icon-btn" id="npclose" aria-label="Close Now playing" title="Close (swipe down)">${icon('chevron')}</button><span class="spacer"></span><button class="chip np-zone" id="npzone" aria-label="Playing on ${esc(z.name)}: choose another zone">${icon('zone')}<span class="np-zone-name">${esc(z.name)}</span></button><span class="spacer"></span><button class="icon-btn" id="npmore" aria-label="More: dashboard, sleep timer${isBrowser() ? '' : ', DSP'}" title="More">${icon('more')}</button></div>
        <div class="np-body">
          <section class="np-player" aria-label="Now playing">
            <div class="np-big">${itemArt(it, '', npSvc(np))}</div>
            <div class="np-titlerow"><div class="np-titles">${np.stream_title ? `<div class="eyebrow">Live · ${esc(it.title || '')}</div>` : ''}<h1>${title}</h1><div class="artist">${artist}</div></div><span id="npheart"></span></div>
            <button class="quality np-quality" id="npquality" title="How the sound gets to you"><span class="qdot" style="background:${h ? colour[h] : 'var(--muted)'}"></span><span>${streamLabel(np) ? esc(streamLabel(np)) : `${h ? LABEL[h] : 'This browser'}${fmtLine ? ` · ${esc(fmtFormat(np.source_format))}` : ''}`}</span></button>
            <div class="np-progress"><input type="range" id="npseek" min="0" max="1000" value="${np.duration_ms ? Math.round((np.position_ms / np.duration_ms) * 1000) : 0}" aria-label="Seek"><div class="muted small np-times"><span id="nppos">${fmt(np.position_ms)}</span><span>${fmt(np.duration_ms)}</span></div></div>
            <div class="np-transport" role="group" aria-label="Playback"><button class="icon-btn" data-npt="shuffle" aria-label="Shuffle what comes next">${icon('shuffle')}</button><button class="icon-btn" data-npt="prev" aria-label="Previous">${icon('prev')}</button><button class="icon-btn play" data-npt="toggle" aria-label="${z.transport === 'playing' ? 'Pause' : 'Play'}">${icon(z.transport === 'playing' ? 'pause' : 'play')}</button><button class="icon-btn" data-npt="next" aria-label="Next">${icon('next')}</button><button class="icon-btn" data-npt="repeat" aria-label="Repeat">${icon(zoneRepeat() === 'one' ? 'repeat-one' : 'repeat')}</button></div>
            <nav class="np-tabs" aria-label="More about this song">${npSheets().map((t) => tab(t, 'np-tab')).join('')}</nav>
          </section>
          <section class="np-sheet" ${sheet ? '' : 'hidden'} aria-label="${sheet ? esc(NP_SHEETS_ALL.find(([k]) => k === sheet)[1]) : ''}">
            <div class="np-strip"><div class="np-strip-art">${itemArt(it, '', npSvc(np))}</div><div class="np-strip-text"><div class="name">${np.stream_title ? esc(np.stream_title) : esc(it.title || '')}</div><div class="sub">${esc(it.artist || '')}</div></div><button class="icon-btn play" data-npt="toggle" aria-label="${z.transport === 'playing' ? 'Pause' : 'Play'}">${icon(z.transport === 'playing' ? 'pause' : 'play')}</button></div>
            <div class="np-sheet-head"><span class="grab" aria-hidden="true"></span><div class="chips np-sheet-tabs" role="tablist" aria-label="More about this song">${npSheets().map((t) => tab(t, 'chip')).join('')}</div><button class="icon-btn np-sheet-close" id="npsheetclose" aria-label="Close the sheet" title="Close">${icon('close')}</button></div>
            <div class="np-sheet-body" role="tabpanel" id="npsheetpanel" ${sheet ? `aria-labelledby="nptab-${sheet}"` : ''} tabindex="-1">${sheet ? sheetBody(sheet) : ''}</div>
          </section>
        </div>
      </div>`;
    const sk = $('#npseek'); setFill(sk);
    // Now playing's own transport (phones: the mini player hides behind it)
    $$('[data-npt]', overlay).forEach((b) => b.onclick = () => { const id = { shuffle: '#shufflebtn', prev: '#prev', toggle: '#toggle', next: '#next', repeat: '#repeatbtn' }[b.dataset.npt]; $(id).click(); setTimeout(renderNowPlaying, 400); });
    sk.addEventListener('input', () => { sk.dataset.dragging = '1'; setFill(sk); $('#nppos').textContent = fmt((sk.value / 1000) * (np.duration_ms || 0)); });
    sk.addEventListener('change', () => { delete sk.dataset.dragging; transport({ action: 'seek', position_ms: Math.round((sk.value / 1000) * (np.duration_ms || 0)) }); });
    $('#npclose').onclick = () => toggleNowPlaying();
    $('#npzone').onclick = () => $('#zonebtn').click();
    const openSheet = (k) => { state.npSheet = k; renderNowPlaying(); const f = $(`.np-sheet-tabs [data-npsheet="${k}"]`, overlay); if (f) f.focus(); };
    $$('[data-npsheet]', overlay).forEach((b) => b.onclick = () => openSheet(b.dataset.npsheet));
    // Tabs: arrow keys move between them (WAI-ARIA tabs)
    $('.np-sheet-tabs', overlay).onkeydown = (e) => {
      const ks = npSheets().map(([k]) => k); const i = ks.indexOf(sheet); if (i < 0) return;
      const j = e.key === 'ArrowRight' ? (i + 1) % ks.length : e.key === 'ArrowLeft' ? (i - 1 + ks.length) % ks.length : e.key === 'Home' ? 0 : e.key === 'End' ? ks.length - 1 : -1;
      if (j >= 0) { e.preventDefault(); e.stopPropagation(); openSheet(ks[j]); }
    };
    $('#npsheetclose').onclick = () => { const was = sheet; state.npSheet = null; renderNowPlaying(); const t = $(`.np-tabs [data-npsheet="${was}"]`, overlay) || $('.np-tabs [data-npsheet]', overlay); if (t) t.focus(); };
    // What is streaming opens the quality sheet (#94); without it, Details (the signal path)
    $('#npquality').onclick = () => (np.format ? qualitySheet() : openSheet('details'));
    {
      // Phones: drag the sheet's head down to close it
      let y0 = null; const head = $('.np-sheet-head', overlay);
      head.addEventListener('touchstart', (e) => { y0 = e.touches[0].clientY; }, { passive: true });
      head.addEventListener('touchend', (e) => { e.stopPropagation(); if (y0 != null && e.changedTouches[0].clientY - y0 > 60 && npCompact()) { state.npSheet = null; renderNowPlaying(); } y0 = null; });
    }
    $('#npmore').onclick = (e) => { const b = e.currentTarget; showMenu(b, [
      { label: 'Dashboard', note: 'Full screen, for a TV or tablet', icon: 'external', run: () => window.open(`/now-playing?zone=${encodeURIComponent(state.zone)}`, '_blank', 'noopener') },
      { label: sleepLabel() === 'Sleep' ? 'Sleep timer' : sleepLabel(), icon: 'moon', run: () => setTimeout(() => showMenu(b, [{ title: 'Sleep timer' }, ...[15, 30, 45, 60, 90].map((m) => ({ label: `${m} minutes`, icon: 'moon', run: () => setSleep(m * 60) })), { label: 'Off', icon: 'close', run: () => setSleep(null) }]), 0) },
      ...(isBrowser() ? [] : [{ label: 'DSP', note: 'Equaliser, crossfeed, room correction', icon: 'settings', run: () => { state.dspOpen = state.zone; setView('settings', 'playback'); } }]),
      ...(songPage ? [{ label: 'Song page', note: 'Sources, versions, tags', icon: 'compass', run: () => setView('item', { kind: 'song', id: songPage }) }] : []),
    ]); };
    bindNpLinks(overlay, it, () => toggleNowPlaying());
    $$('[data-npqi]', overlay).forEach((b) => b.onclick = () => { transport({ action: 'play', index: Number(b.dataset.npqi) }); setTimeout(renderNowPlaying, 500); });
    const qf = $('#npqfull'); if (qf) qf.onclick = () => setView('queue');
    overlay.dataset.sheet = sheet || ''; overlay.dataset.layout = `${shellMode()}-${npCompact()}`;
    state.npWant = { gen, at: Date.now(), scroll: sheet && keep.sheet === sheet ? keep.scroll : null, lyric: sheet === 'lyrics' && keep.sheet === 'lyrics' ? keep.lyric : null, focus: keep.focus };
    npRestore();
    if (sheet === 'sources' && songPage) fillNpSources(songPage, gen);
    fillNpPubky(it, gen);
    loadLyrics(it);
    setArtTint();
    npRestore();
    refreshSleep();
  };
  // The quality sheet (STEWARD-UX-PLAYER-SOURCES §2), from the player's quality label: what is
  // arriving, what the zone makes of it and why, the service's quality choice, and the way to the
  // Play from order and the zone's output settings.
  // On a source's tab, bring its quality setting into view (it may load a moment after the page)
  const focusQualityControl = (svc, tries = 20) => {
    if (state.view !== 'sources') return; // they went elsewhere
    // that service's own control ("Qobuz quality", "TIDAL full track quality"), not another's
    const name = String(SERVICE_NAMES[svc] || srcName(svc) || '').toLowerCase();
    const el = name && $$('[aria-label]', main).find((e) => { const l = e.getAttribute('aria-label').toLowerCase(); return l.startsWith(name) && l.includes('quality') && e.offsetParent !== null; });
    if (!el) { if (tries > 0) setTimeout(() => focusQualityControl(svc, tries - 1), 200); return; }
    el.scrollIntoView({ block: 'center' }); el.focus({ preventScroll: true });
    const row = el.closest('.set-row') || el; row.classList.add('flash'); setTimeout(() => row.classList.remove('flash'), 1600);
  };
  const qualitySheet = async () => {
    const z = currentZone(); const np = z && z.now_playing; if (!np) return;
    const f = np.format || {};
    const svc = f.service || '';
    const d = document.createElement('dialog'); d.className = 'dialog qsheet';
    const svcRow = svc ? '' : ' hidden';
    const conv = conversionWhy(np);
    // "As it arrives" only when the signal path says bit-perfect; else what changes the sound
    const enhancing = np.signal_path ? np.signal_path.stages.filter((x) => x.quality !== 'bit_perfect').map((x) => `${x.name}${x.note ? ': ' + x.note : ''}`) : [];
    const onTheWay = conv.length ? `. Changed on the way: ${esc(conv.join('; '))}`
      : np.signal_path && headline(np.signal_path) === 'bit_perfect' ? ', as it arrives'
        : enhancing.length ? `. On the way: ${esc(enhancing.join('; '))}` : '';
    const arriving = streamLabel(np, true) || (np.signal_path ? fmtFormat(np.source_format) : 'Played by this browser');
    const zonePlays = np.output_format ? `Plays it as ${np.output_format.kind === 'dsd' ? fmtFormat(np.output_format) : `${kHz(np.output_format.rate_hz)} kHz / ${np.output_format.bits} bit`}` : '';
    d.innerHTML = `<h2>Sound quality</h2>
      <div class="set-rows">
        <div class="set-row"><div class="set-label"><div class="set-name">Arriving</div><p class="set-help">${esc(arriving)}</p></div></div>
        ${zonePlays ? `<div class="set-row"><div class="set-label"><div class="set-name">On ${esc(z.name)}</div><p class="set-help">${esc(zonePlays)}${onTheWay}.</p></div></div>` : ''}
        <div class="set-row" id="qsvc"${svcRow}><div class="set-label"><div class="set-name">${esc(svc === 'library' ? 'Your files' : SERVICE_NAMES[svc] || srcName(svc) || 'Source')}</div><p class="set-help muted">${svc === 'library' ? 'Your files play as they are.' : '<span class="spin"></span> Asking for its quality choices…'}</p></div></div>
      </div>
      <div class="dialog-actions">${isOwner() ? '<button class="btn ghost" data-qplay>Play from order</button><button class="btn ghost" data-qout>Output and DSP</button>' : ''}<button class="btn" data-qclose>Close</button></div>`;
    document.body.appendChild(d);
    const tr = { open: () => d.open, close: (fromBack) => done(fromBack) };
    const done = (fromBack) => { const k = transients.indexOf(tr); if (k >= 0) transients.splice(k, 1); if (d.open) { if (d.close) d.close(); else d.removeAttribute('open'); } d.remove(); if (fromBack !== true) popTransient(); };
    transients.push(tr); pushTransient();
    $('[data-qclose]', d).onclick = () => done();
    d.addEventListener('cancel', (e) => { e.preventDefault(); done(); });
    const qp = $('[data-qplay]', d); if (qp) qp.onclick = () => { done(); if (!overlay.hidden) toggleNowPlaying(); setView('settings', 'playback'); };
    const qo = $('[data-qout]', d); if (qo) qo.onclick = () => { done(); if (!overlay.hidden) toggleNowPlaying(); state.dspOpen = state.zone; setView('settings', 'playback'); };
    if (d.showModal) d.showModal(); else d.setAttribute('open', '');
    if (svc === 'library') return;
    const box = $('#qsvc .set-help', d);
    const goSource = `<button class="lnk" data-qsrc>${esc(SERVICE_NAMES[svc] || srcName(svc))} settings</button>`;
    // Straight to the service's own quality setting (its Sources tab), for owners
    const goQuality = isOwner() && svc && svc !== 'stream' && svc !== 'library' ? `<button class="lnk qjump" data-qsrc>${esc(SERVICE_NAMES[svc] || srcName(svc))} quality settings ›</button>` : '';
    let qs = null;
    if (isOwner() && svc && svc !== 'stream') { try { qs = await api('GET', `/sources/${encodeURIComponent(svc)}/quality`); } catch { qs = null; } }
    if (!d.isConnected) return;
    if (qs && (qs.options || []).length) {
      const when = qs.applies === 'now' ? 'Applies now.' : 'Applies from the next song.';
      box.classList.remove('muted');
      box.innerHTML = goQuality + `<span class="chips" role="radiogroup" aria-label="Quality">${qs.options.map((o) => `<button class="chip scope ${o.id === qs.current ? 'on' : ''}" role="radio" aria-checked="${o.id === qs.current}" data-qopt="${esc(o.id)}" title="${esc(o.note || '')}">${esc(o.label)}</button>`).join('')}</span><span class="muted small"> ${when}</span>${qs.note ? `<span class="muted small qnote">${esc(qs.note)}</span>` : ''}`;
      $$('[data-qopt]', box).forEach((b) => b.onclick = async () => { try { await api('PUT', `/sources/${encodeURIComponent(svc)}/quality`, { id: b.dataset.qopt }); $$('[data-qopt]', box).forEach((x) => { const on = x === b; x.classList.toggle('on', on); x.setAttribute('aria-checked', String(on)); }); toast(`${SERVICE_NAMES[svc] || srcName(svc)}: ${b.textContent}. ${when}`); } catch (e) { toast(e.message, 'error'); } });
    } else {
      box.innerHTML = f.preview ? `Previews only (30 s). Full songs need another service.${isOwner() ? ' ' + goSource : ''}` : qs && qs.note ? esc(qs.note) : svc === 'stream' || !isOwner() ? 'As the stream sends it.' : `Its quality is set in ${goSource}.`;
    }
    const gs = $('[data-qsrc]', d); if (gs) gs.onclick = async () => { done(); if (!overlay.hidden) toggleNowPlaying(); await setView('sources', { source: svc }); focusQualityControl(svc); };
  };
  // Put back what the last render had (scroll, focus) once the content it needs is there; for that
  // render only, and only in its first seconds so a late answer never yanks the view
  const npRestore = () => {
    const w = state.npWant; if (!w || w.gen !== npGen || Date.now() - w.at > 3000) return;
    const body = $('.np-sheet-body', overlay); if (body && w.scroll != null && body.scrollHeight - body.clientHeight >= w.scroll) { body.scrollTop = w.scroll; w.scroll = null; }
    const lines = $('.lyric-lines', overlay); if (lines && w.lyric != null) { lines.scrollTop = w.lyric; w.lyric = null; }
    const free = !document.activeElement || document.activeElement === document.body || !overlay.contains(document.activeElement); // never take focus someone has moved since
    if (w.focus && free) { const f = $(w.focus, overlay); if (f) { f.focus({ preventScroll: true }); if (f.id === 'nptagadd') f.setSelectionRange(f.value.length, f.value.length); w.focus = null; } }
  };
  // A change of arrangement (rotating a phone, resizing a window) redraws an open Now playing
  window.addEventListener('resize', () => { if (!overlay.hidden && overlay.dataset.layout && overlay.dataset.layout !== `${shellMode()}-${npCompact()}`) renderNowPlaying(); });
  // The Sources sheet: where else this song plays (the song page's "also on")
  const npSrcCache = new Map(); const npSrcPending = new Map();
  const fillNpSources = async (songPage, gen = npGen) => {
    let pg = npSrcCache.get(songPage);
    if (!pg) {
      let req = npSrcPending.get(songPage);
      if (!req) { req = api('GET', `/item/song/${encodeURIComponent(songPage)}`).catch(() => null).finally(() => npSrcPending.delete(songPage)); npSrcPending.set(songPage, req); }
      pg = await req;
      if (pg && !pg.reading && !pg.pending) { npSrcCache.set(songPage, pg); if (npSrcCache.size > 50) npSrcCache.delete(npSrcCache.keys().next().value); }
    }
    const box = $('#npsources', overlay); if (!box || gen !== npGen) return;
    // Play from another source (TASK-pinakes-social-ux A4): the same song, where it is, on a node
    // zone; only from the library or a service that is on and working here (a source the node
    // can't play would leave the zone stopped)
    const rk = await loadSvcRank().catch(() => null);
    if (!$('#npsources', overlay) || gen !== npGen) return;
    const also = rankSvc((pg && pg.also_on) || [], (v) => srcSvc(v.source));
    const z0 = currentZone(); const cur = z0 && z0.now_playing ? z0.now_playing.item : null;
    // Full songs only: a service that gives 30 s previews here, or YouTube (only This browser plays it),
    // would swap the song for something less
    const previewOnly = (svc) => /^previews\b/i.test(String(((state.sources || []).find((x) => x.id === svc) || {}).status || ''));
    const canPlay = (v) => { const svc = srcSvc(v.source); if (svc === 'youtube' || /^youtube:/.test(switchUri(v) || '') || previewOnly(svc)) return false; return rk ? rk.healthy.has(svc) : svc === 'library'; };
    const canSwitch = (v) => !isBrowser() && cur && switchUri(v) && switchUri(v) !== cur.uri && canPlay(v);
    box.innerHTML = (also.length ? `<div class="list">${also.map((v, k) => `<div class="list-row">${svcBadge(srcSvc(v.source))}<span class="grow name">${esc(v.label || srcName(srcSvc(v.source)))}</span>${canSwitch(v) ? `<button class="btn sm" data-npswitch="${k}" aria-label="Play from here: ${esc(v.label || srcName(srcSvc(v.source)))}">${icon('play')} Play from here</button>` : ''}${webUrl(v.url) ? `<a class="icon-btn" href="${esc(webUrl(v.url))}" target="_blank" rel="noopener" aria-label="Open on ${esc(v.label || v.source)}" title="Open on ${esc(v.label || v.source)}">${icon('external')}</a>` : ''}</div>`).join('')}</div>` : '<p class="muted">No other sources found for this song.</p>')
      + `<div class="np-sheet-foot"><button class="btn sm ghost" id="npsongpage">${icon('compass')} Song page: versions and every source</button></div>`;
    $('#npsongpage', overlay).onclick = () => setView('item', { kind: 'song', id: songPage });
    $$('[data-npswitch]', box).forEach((b) => b.onclick = () => switchSource(also[Number(b.dataset.npswitch)], b, cur.id));
    npRestore();
  };
  // What a source's entry plays as in a queue: a library track, a service's own uri, a YouTube video
  const switchUri = (v) => (v.uri && (v.source !== 'library' || v.uri.startsWith('track:')) ? v.uri : ytOf(v.url) ? `youtube:video:${ytOf(v.url)}` : null);
  // The playing song moves to another source at the same position: the zone's queue is loaded
  // again with that one item swapped (repeat, radio and paused-or-playing stay)
  // `itemId`: the song the sheet was drawn for; if another one plays by now, nothing changes
  const switchSource = async (v, b, itemId) => {
    const zone = state.zone; const uri = switchUri(v);
    const playingIt = () => { const z = state.zones[zone]; return z && z.now_playing && z.now_playing.item.id === itemId ? z : null; };
    if (!uri || !playingIt()) { toast('Another song is playing now: open Sources again', 'error'); return; }
    const name = v.label || srcName(srcSvc(v.source));
    b.disabled = true; b.innerHTML = '<span class="spin"></span> Switching…';
    const z0 = playingIt(); const version = z0.queue_version;
    if (z0.transport === 'playing') startWait('play', zone, `${z0.now_playing.item.title || ''} · from ${name}`, true);
    try {
      let queue = await api('GET', `/zones/${encodeURIComponent(zone)}/queue`);
      // The queue changed meanwhile (a song added elsewhere): read it again, so nothing is lost
      if (state.zones[zone] && state.zones[zone].queue_version !== version) queue = await api('GET', `/zones/${encodeURIComponent(zone)}/queue`);
      const at = queue.findIndex((q) => q.id === itemId);
      const z = playingIt();
      if (at < 0 || !z) throw new Error('another song is playing now: open Sources again');
      // The new source's own cover and length: the old item's cover stays only between two items
      // of the same service (a library file shows its album's); the length comes with the source
      const scheme = (u) => String(u || '').split(':')[0];
      const items = queue.map((q, k) => (k === at ? { ...q, uri, image: scheme(uri) !== 'track' && scheme(uri) === scheme(q.uri) ? q.image || null : null, duration_ms: null } : q));
      const zs = await api('POST', `/zones/${encodeURIComponent(zone)}/load`, { items, index: at, position_ms: z.now_playing.position_ms || 0, transport: z.transport === 'paused' ? 'paused' : 'playing', repeat: z.repeat || 'off', radio: !!z.radio });
      // The zone's answer says whether the new source plays (a paused zone sends no event to wait for)
      if (zs && zs.id) state.zones[zs.id] = zs;
      if (zs && zs.error) throw new Error(zs.error);
      toast(`Playing from ${name}`);
      refreshQueue();
    } catch (e) { startFail(e.message, true); toast(e.message, 'error'); if (b.isConnected) { b.disabled = false; b.innerHTML = `${icon('play')} Play from here`; } }
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
    try { const v = await api('GET', `/zones/${encodeURIComponent(state.zone)}/sleep`); if (v.ends_at_ms) state.sleepAt[state.zone] = v.ends_at_ms; else delete state.sleepAt[state.zone]; } catch { /* keep */ }
  };
  // Lyrics for the playing library track (tags or a sidecar .lrc); synced lines follow the position.
  const trackIdOf = (it) => { const m = /^track:(\d+)$/.exec(it && it.uri || ''); return m ? Number(m[1]) : null; };
  const renderLyrics = (l) => {
    const box = $('#nplyrics'); if (!box) return;
    const none = $('#nplyricsnone'); if (none) none.hidden = !!l;
    const ld = $('#nplyricsload'); if (ld) ld.hidden = true;
    setTimeout(npRestore, 0);
    if (!l) { box.hidden = true; box.innerHTML = ''; return; }
    box.hidden = false;
    const off = state.lyricsOffset || 0;
    box.innerHTML = `<h3>Lyrics${l.synced ? ' <span class="muted small">synced</span>' : ''}${l.source === 'lrclib' ? ' <a class="muted small" href="https://lrclib.net" target="_blank" rel="noopener">from LRCLIB</a>' : ''}${l.synced ? ` <span class="lyric-nudge"><button class="icon-btn sm" data-lnudge="-500" title="Lines earlier (0.5 s)" aria-label="Show lines half a second earlier">−</button><span class="muted small">${off ? `${off > 0 ? '+' : ''}${(off / 1000).toFixed(1)} s` : ''}</span><button class="icon-btn sm" data-lnudge="500" title="Lines later (0.5 s)" aria-label="Show lines half a second later">+</button></span>` : ''}</h3><div class="lyric-lines">${l.lines.map((ln) => `<p class="lyric-line" ${ln.ms != null ? `data-ms="${ln.ms}"` : ''}>${esc(ln.text) || icon('notes')}</p>`).join('')}</div>`;
  };
  // Library songs: their own lyrics, else LRCLIB (the node decides); streamed songs: LRCLIB by title
  const lyricsKey = (it) => { const id = trackIdOf(it); return id != null ? `t${id}` : it && it.title && it.artist ? `s${it.uri || ''}|${it.title}|${it.artist}` : null; };
  const loadLyrics = async (it) => {
    const key = lyricsKey(it); const id = trackIdOf(it);
    if (!key || (it && it.stream && /^https?:/.test(it.uri || '') && it.album === 'Internet radio')) { renderLyrics(null); return; }
    if (state.lyrics.has(key)) { renderLyrics(state.lyrics.get(key)); return; }
    let l = null;
    try { l = id != null ? await api('GET', `/library/tracks/${id}/lyrics`) : await api('GET', `/lyrics?${new URLSearchParams({ title: it.title, artist: String(it.artist).split(/,| & /)[0], ...(it.album ? { album: it.album } : {}), ...(it.duration_ms ? { duration_ms: String(it.duration_ms) } : {}) })}`); } catch { l = null; }
    state.lyrics.set(key, l);
    if (lyricsKey(currentNowPlayingItem()) === key) { state.lyricsOffset = 0; renderLyrics(l); }
  };
  document.addEventListener('click', (e) => { const b = e.target.closest && e.target.closest('[data-lnudge]'); if (!b) return; state.lyricsOffset = (state.lyricsOffset || 0) + Number(b.dataset.lnudge); const it = currentNowPlayingItem(); const l = state.lyrics.get(lyricsKey(it)); if (l) renderLyrics(l); });
  const currentNowPlayingItem = () => { const z = currentZone(); return z && z.now_playing ? z.now_playing.item : null; };
  const followLyrics = (positionMs) => {
    const lines = $$('#nplyrics .lyric-line[data-ms]'); if (!lines.length) return;
    const at = positionMs - (state.lyricsOffset || 0);
    let cur = null; for (const ln of lines) { if (Number(ln.dataset.ms) <= at) cur = ln; else break; }
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
  // Settings, one topic per tab (STEWARD-UX-SETTINGS.md)
  // Side tabs: a vertical tab list beside one panel per tab (Settings; Sources reuses it). On a
  // phone the list is a page of its own and each tab a page with Back. `tabs`: [[key, label, icon,
  // extra html after the label (a status dot, a switch)?]]; `ids`: panel ids that differ from
  // `<prefix>-<key>`. The view's address is `/<view>/<key>` (add the view to the router).
  //   const t = sideTabs({ view: 'sources', title: 'Sources', tabs, prefix: 'src' });
  //   main.innerHTML = t.page(t.panel('tidal', html) + …); t.wire(main);
  const sideTabs = ({ view, title, tabs: entries, prefix = view, ids = {}, remembered }) => {
    const phone = !!(window.matchMedia && matchMedia('(max-width: 720px)').matches); // the side tabs' CSS is by width
    // Entries are tabs ([id, label, icon, extra]) and group headers ({group, title, saved}) between them
    const tabs = entries.filter(Array.isArray);
    const known = (k) => tabs.some(([x]) => x === k);
    const asked = known(state.viewArg) ? state.viewArg : null;
    const active = asked || (phone ? null : (known(remembered) ? remembered : tabs[0][0]));
    const name = (k) => (tabs.find(([x]) => x === k) || [k, k])[1];
    const pid = (k) => ids[k] || `${prefix}-${k}`;
    const nav = () => `<nav class="set-nav" role="tablist" aria-orientation="vertical" aria-label="${esc(title)}">${(() => { let grp = null; return entries.map((e) => (!Array.isArray(e) ? (grp = `${prefix}grp-${e.group}`, `<div class="set-group-h" role="presentation" id="${grp}"><span class="set-group-t">${esc(e.title)}</span>${e.saved ? `<span class="set-group-s">${esc(e.saved)}</span>` : ''}</div>`) : (([k, l, ic, extra]) => `<button class="set-tab ${active === k ? 'on' : ''}" role="tab" id="${prefix}tab-${k}" aria-selected="${active === k}" aria-controls="${pid(k)}" data-stab="${k}"${grp ? ` aria-describedby="${grp}"` : ''}>${icon(ic)}<span>${esc(l)}</span>${extra || ''}${icon('chevron', 'ic chev')}</button>`)(e))).join(''); })()}</nav>`;
    const panel = (k, html) => `<section class="set-panel" id="${pid(k)}" data-panel="${k}" role="tabpanel" aria-labelledby="${prefix}tab-${k}" ${active === k ? '' : 'hidden'}>${phone ? `<button class="btn ghost set-back" data-tabs-back>${icon('back')} ${esc(title)}</button>` : ''}<h2 class="set-title">${esc(name(k))}</h2>${html}</section>`;
    const page = (panels) => (phone && active ? '' : pageHead(title)) + `<div class="settings ${active ? 'has-panel' : 'no-panel'}">${nav()}<div class="set-body-wrap">${panels}</div></div>`;
    // On a computer a tab switches in place (the address follows); on a phone it is its own page
    const wire = (main, onShow) => {
      const show = (k) => {
        if (phone) { setView(view, k); return; }
        state.viewArg = k;
        $$('.set-panel', main).forEach((pnl) => { pnl.hidden = pnl.dataset.panel !== k; });
        $$('.set-tab', main).forEach((t) => { const on = t.dataset.stab === k; t.classList.toggle('on', on); t.setAttribute('aria-selected', String(on)); });
        try { history.replaceState({ ...(history.state || {}), arg: k }, '', `/${view}/${k}`); } catch { /* sandboxed */ }
        if (onShow) onShow(k);
      };
      $$('[data-stab]', main).forEach((t) => t.onclick = () => show(t.dataset.stab));
      $('.set-nav', main).onkeydown = (e) => { if (!['ArrowDown', 'ArrowUp'].includes(e.key)) return; e.preventDefault(); const all = $$('.set-tab', main); const i = all.indexOf(document.activeElement); const n = all[(i + (e.key === 'ArrowDown' ? 1 : all.length - 1)) % all.length]; n.focus(); if (!phone) show(n.dataset.stab); };
      $$('[data-tabs-back]', main).forEach((b) => b.onclick = () => goBack(view));
      return show;
    };
    return { phone, active, name, nav, panel, page, wire };
  };
  // Settings in four groups, by where each setting is saved (STEWARD-SETTINGS-GROUPS.md, docs/ux/responsive.md):
  // You (your homeserver), This device, This node (admin), This homeserver (admin, with a bundled one)
  const SETTINGS_GROUPS = { you: 'Saved on your homeserver: follows you to every node and device', device: 'Only on this device', node: 'For everyone on this node', homeserver: 'The homeserver this node runs' };
  const settingsTabs = ({ saved = {}, bundled = false, tunnel = false }) => [
    { group: 'you', title: 'You', saved: saved.you || SETTINGS_GROUPS.you },
    ['accounts', 'My accounts', 'artist'], ['home', 'Home', 'home'], ['quality', 'Playback quality', 'zone'],
    { group: 'device', title: 'This device', saved: saved.device || SETTINGS_GROUPS.device },
    ['appearance', 'Display', 'sun'],
    { group: 'node', title: 'This node · admin', saved: saved.node || SETTINGS_GROUPS.node },
    ['apps', 'Service apps', 'settings'], ['people', 'People', 'artist'], ['playback', 'Zones and outputs', 'zone'],
    ['nodehome', 'Home for everyone', 'home'],
    ...(tunnel && !bundled ? [['remote', 'Remote access', 'external']] : []),
    ['discover', 'Discover', 'compass'], ['backup', 'Backup', 'download'], ['about', 'About and updates', 'info'],
    ...(bundled ? [{ group: 'homeserver', title: 'This homeserver · admin', saved: saved.homeserver || SETTINGS_GROUPS.homeserver }, ['homeserver', 'Homeserver', 'archive'], ...(tunnel ? [['remote', 'Domain and tunnel', 'external']] : [])] : []),
  ];
  const SETTINGS_TABS = settingsTabs({ bundled: true, tunnel: true }).filter(Array.isArray); // every tab, for routes
  // ---- paths and back (navigation brief): real URLs, back/forward restore view and scroll ----
  const NAV_VIEWS = ['library', 'home', 'favourites', 'albums', 'artists', 'genres', 'tracks', 'playlists', 'radio', 'sources', 'queue', 'zones', 'settings', 'identify', 'add-music', 'downloads'];
  const VIEW_LABELS = { library: 'Your library', downloads: 'Downloads', 'add-music': 'Add music', home: 'Home', favourites: 'Favourites', albums: 'Albums', artists: 'Artists', genres: 'Genres', tracks: 'Tracks', playlists: 'Playlists', radio: 'Radio', sources: 'Sources', discover: 'Discover', queue: 'Queue', zones: 'Zones', settings: 'Settings', identify: 'Confirm matches', album: 'Album', artist: 'Artist', genre: 'Genre', playlist: 'Playlist', shared: 'Shared playlist' };
  const b64u = (x) => btoa(unescape(encodeURIComponent(x))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  const unb64u = (x) => decodeURIComponent(escape(atob(x.replace(/-/g, '+').replace(/_/g, '/'))));
  const itemSeg = (kind) => (kind === 'artist' ? 'artist' : kind === 'album' ? 'album' : 'song');
  // A page id is the canonical identity in short form: mb-<mbid>, isrc-<ISRC>, else u-<base64url of the link>.
  const idOf = (uri) => { let m = uri.match(/^https:\/\/musicbrainz\.org\/(recording|artist|release-group)\/([0-9a-f-]{36})$/); if (m) return 'mb-' + m[2]; m = uri.match(/^https:\/\/musicbrainz\.org\/isrc\/([A-Za-z0-9]{12})$/); if (m) return 'isrc-' + m[1].toUpperCase(); m = svcPage(uri); if (m) return m; return 'u-' + b64u(uri); };
  // A service's own song/album/artist page has a readable id (/song/tidal-1), as on the node
  const SVC_HOSTS = { 'tidal.com': 'tidal', 'listen.tidal.com': 'tidal', 'www.tidal.com': 'tidal', 'www.deezer.com': 'deezer', 'deezer.com': 'deezer', 'open.spotify.com': 'spotify' };
  const SVC_BASES = { tidal: 'https://tidal.com', deezer: 'https://www.deezer.com', spotify: 'https://open.spotify.com' };
  const svcPage = (uri) => { let u; try { u = new URL(uri); } catch { return null; } const yt = /^https:\/\/www\.youtube\.com\/watch\?v=([A-Za-z0-9_-]{11})$/.exec(uri); if (yt) return `youtube-${yt[1]}`; if (u.search || u.hash) return null; const svc = SVC_HOSTS[u.host]; if (!svc) return null; const p = u.pathname.split('/').filter(Boolean); const [k, id] = p.length === 2 ? p : p.length === 3 ? p.slice(1) : []; if (!['track', 'album', 'artist'].includes(k)) return null; if (svc === 'spotify' ? !/^[A-Za-z0-9]{22}$/.test(id) : !/^[0-9]{1,20}$/.test(id)) return null; return `${svc}-${id}`; };
  // The song page for a queue item: a library track, or a service track by its page.
  const npPageId = (uri) => { if (!uri) return null; let m = uri.match(/^track:(\d+)$/); if (m) return 'lib-' + m[1]; m = uri.match(/^(tidal|deezer|jamendo):track:([A-Za-z0-9]+)$/); if (m) return idOf({ tidal: `https://tidal.com/track/${m[2]}`, deezer: `https://www.deezer.com/track/${m[2]}`, jamendo: `https://www.jamendo.com/track/${m[2]}` }[m[1]]); if (/^https?:\/\//.test(uri)) return idOf(uri); return null; };
  const uriOf = (seg, id) => { if (id.startsWith('mb-')) return `https://musicbrainz.org/${seg === 'artist' ? 'artist' : seg === 'album' ? 'release-group' : 'recording'}/${id.slice(3)}`; if (id.startsWith('isrc-')) return `https://musicbrainz.org/isrc/${id.slice(5)}`; if (id.startsWith('u-')) { try { return unb64u(id.slice(2)); } catch { return null; } } const yv = /^youtube-([A-Za-z0-9_-]{11})$/.exec(id); if (yv) return `https://www.youtube.com/watch?v=${yv[1]}`; const sm = /^(tidal|deezer|spotify)-([A-Za-z0-9]+)$/.exec(id); if (sm) return `${SVC_BASES[sm[1]]}/${seg === 'artist' ? 'artist' : seg === 'album' ? 'album' : 'track'}/${sm[2]}`; return null; };
  const pathFor = (v, arg) => {
    if (v === 'item' && arg && arg.kind && arg.id) return `/${arg.kind}/${arg.id}`;
    if (v === 'library' && (arg === 'node' || arg === 'import')) return `/library/${arg}`;
    if (v === 'home-section' && arg && arg.section) return `/home/${arg.section}${arg.window && arg.window !== '7d' ? `?window=${arg.window}` : ''}`;
    if (v === 'tag' && arg) return `/tag/${String(arg).split(',').map(encodeURIComponent).join(',')}`;
    if (v === 'playlist' && arg != null) return `/playlist/${encodeURIComponent(arg)}`;
    if (v === 'person' && arg) return `/person/${arg}`;
    if (v === 'smart' && arg) return `/smart/${encodeURIComponent(arg)}`;
    if (v === 'smart-edit') return null;
    if (v === 'dashboard') return `/now-playing${arg ? `?zone=${encodeURIComponent(arg)}` : ''}`;
    if (v === 'shared' && arg && arg.user) return `/shared/${arg.user}${arg.id ? '/' + arg.id : ''}`;
    if (v === 'genre' && arg != null) return `/genre/${encodeURIComponent(arg)}`;
    if (v === 'sources' && arg && arg.source) return `/sources/${encodeURIComponent(arg.source)}`;
    if (v === 'source-item' || v === 'source-artist') return null;
    if (v === 'discover-item' && arg && arg.uri) return `/${itemSeg(arg.kind)}/${idOf(arg.uri)}`;
    if (v === 'search') return arg && arg.q ? `/search?q=${encodeURIComponent(arg.q)}${arg.in && arg.in !== 'everywhere' ? `&in=${encodeURIComponent(arg.in)}` : ''}` : '/search';
    if (v === 'home') return '/';
    if (v === 'settings' && typeof arg === 'string' && SETTINGS_TABS.some(([k]) => k === arg)) return `/settings/${arg}`;
    return NAV_VIEWS.includes(v) ? '/' + v : null;
  };
  const routeFromPath = () => {
    // #settings/<tab> (a link from anywhere) and /settings/<tab> open that tab
    const h = /^#settings(?:\/([a-z]+))?$/.exec(location.hash || '');
    if (h) return { v: 'settings', arg: h[1] || null };
    // #sources/<service> likewise opens that source's tab
    const hs = /^#sources(?:\/([a-z0-9-]+))?$/.exec(location.hash || '');
    if (hs) return hs[1] ? { v: 'sources', arg: { source: hs[1] } } : { v: 'sources' };
    const p = location.pathname.replace(/\/+$/, '') || '/';
    if (p === '/library/node' || p === '/settings/library') return { v: 'library', arg: 'node' };
    if (p === '/library/import' || p === '/settings/import') return { v: 'library', arg: 'import' };
    const st = p.match(/^\/settings\/([a-z]+)$/);
    if (st && SETTINGS_TABS.some(([k]) => k === st[1])) return { v: 'settings', arg: st[1] };
    let m = p.match(/^\/(song|album|artist)\/([^/]+)$/);
    if (m && (m[2].startsWith('lib-') || uriOf(m[1], m[2]))) return { v: 'item', arg: { kind: m[1], id: m[2] } };
    m = p.match(/^\/home\/([a-z-]+)$/);
    if (m) { const w = new URLSearchParams(location.search).get('window'); return { v: 'home-section', arg: w ? { section: m[1], window: w } : { section: m[1] } }; }
    m = p.match(/^\/(?:tag|discover\/tag)\/([^/]+)$/);
    if (m) return { v: 'tag', arg: m[1].split(',').map(decodeURIComponent).join(',') };
    if (p === '/discover' || p.startsWith('/discover/')) return { v: 'home' };
    m = p.match(/^\/(playlist|genre|sources?)\/([^/]+)$/);
    if (m) { const a = decodeURIComponent(m[2]); return m[1] === 'playlist' ? { v: 'playlist', arg: a } : m[1] === 'genre' ? { v: 'genre', arg: /^\d+$/.test(a) ? Number(a) : a } : { v: 'sources', arg: { source: a } }; }
    if (p === '/now-playing') return { v: 'dashboard', arg: new URLSearchParams(location.search).get('zone') || null };
    m = p.match(/^\/smart\/([A-Za-z0-9_-]+)$/);
    if (m) return { v: 'smart', arg: m[1] };
    m = p.match(/^\/person\/([a-z0-9]{52})$/);
    if (m) return { v: 'person', arg: m[1] };
    m = p.match(/^\/shared\/([a-z0-9]{52})(?:\/([0-9A-Z]{13}))?$/);
    if (m) return { v: 'shared', arg: m[2] ? { user: m[1], id: m[2] } : { user: m[1] } };
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
  const hereLabel = () => state.view === 'item' ? (state.itemTitle || 'Back') : state.view === 'tag' ? `#${String(state.viewArg).split(',').join(' #')}` : state.view === 'home-section' ? (HOME_TITLES[state.viewArg && state.viewArg.section] || 'Home') : state.view === 'playlist' || state.view === 'shared' ? (state.itemTitle || 'Playlist') : state.view === 'search' ? `Results for “${(state.viewArg && state.viewArg.q) || ''}”` : (VIEW_LABELS[state.view] || 'Back');
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
        ['playlist', 'abc123'], ['shared', { user: 'zswn37ir9t8ge4r4posn4r7ow3eh384c8ud4kkxm585igt38x31o' }], ['shared', { user: 'zswn37ir9t8ge4r4posn4r7ow3eh384c8ud4kkxm585igt38x31o', id: '0035WKVKQDHSM' }], ['genre', 4], ['sources', { source: 'tidal' }], ['search', { q: 'queen' }], ['search', { q: 'queen', in: 'tidal' }]];
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
    if (popPending && how !== 'pop') return popPending.then(() => setView(v, arg, how)); // a menu's entry is still being popped
    { const t = toItem(v, arg); if (t) { v = 'item'; arg = t; } }
    if (v === 'discover') { if (arg && arg.tag) { v = 'tag'; arg = arg.tag; } else { v = 'home'; arg = undefined; } }
    // A section of its own starts with an empty search box (second review: "queen" stayed after leaving Search)
    if (isListener() && LISTENER_VIEWS_OFF.has(v)) { v = 'albums'; arg = undefined; }
    if (isOffline() && !OFFLINE_VIEWS.has(v)) { v = 'downloads'; arg = undefined; }
    { const sb = document.getElementById('search'); if (sb && NAV_VIEWS.includes(v) && document.activeElement !== sb) sb.value = ''; }
    if (how !== 'pop') {
      const path = pathFor(v, arg) || location.pathname + location.search;
      const cur = history.state || {};
      try { history.replaceState({ ...cur, scroll: main.scrollTop }, '', location.pathname + location.search); } catch { /* sandboxed */ }
      // Changing a query or tab inside the same page replaces; opening another page pushes.
      // A page opened from a menu takes the menu's transient entry (still a new page for back).
      const viaTransient = !!cur.transient && how !== 'replace';
      const replace = how === 'replace' || viaTransient || (v === state.view && (v === 'search' || v === 'discover' || path === location.pathname + location.search)) || !state.view;
      const entry = { transient: false, v, arg: histArg(v, arg), backLabel: replace && !viaTransient ? cur.backLabel : hereLabel(), depth: replace && !viaTransient ? (cur.depth || 0) : (cur.depth || 0) + 1, scroll: 0 };
      try { if (replace) history.replaceState(entry, '', path); else history.pushState(entry, '', path); } catch { /* sandboxed */ }
      navDepth = entry.depth;
    }
    state.view = v; state.viewArg = arg;
    // The tabs (phone) and the rail (tablet): Home, Search, Library (every library view and item page), Zones, Settings
    const LIB_VIEWS = ['library', 'albums', 'artists', 'tracks', 'genres', 'genre', 'playlists', 'playlist', 'favourites', 'item', 'album', 'artist', 'shared', 'sources', 'source-item', 'radio', 'person', 'identify', 'add-music'];
    if (LIB_TOP.some(([k]) => k === v)) state.lastLib = v;
    $$('.bnav').forEach((b) => { const k = b.dataset.bnav; const on = k === 'library' ? LIB_VIEWS.includes(v) : k === v; b.classList.toggle('on', on); if (on) b.setAttribute('aria-current', 'page'); else b.removeAttribute('aria-current'); });
    $$('.nav-item').forEach((b) => { const on = b.dataset.view === v || (v === 'item' && b.dataset.view === (state.itemSection || (arg && arg.kind === 'artist' ? 'artists' : 'albums'))) || (v === 'album' && b.dataset.view === 'albums') || (v === 'artist' && b.dataset.view === 'artists') || (v === 'genre' && b.dataset.view === 'genres') || ((v === 'playlist' || v === 'shared') && b.dataset.view === 'playlists') || (v === 'source-item' && b.dataset.view === 'sources'); b.classList.toggle('on', on); if (on) b.setAttribute('aria-current', 'page'); else b.removeAttribute('aria-current'); });
    $('#scrim').hidden = true; closePop();
    if (!overlay.hidden) toggleNowPlaying(false, true); // the page takes Now playing's history entry
    main.scrollTop = 0;
    return render();
  };
  window.addEventListener('popstate', async (e) => {
    if (popHandled) { popHandled = false; return; } // back only closed a menu or dialog
    if (e.state && e.state.transient) return; // forward onto a closed menu's entry: nothing to show
    const st = e.state || routeFromPath() || { v: 'home' };
    navDepth = st.depth || 0;
    if (st.v === 'search' && st.arg && st.arg.q) { search.value = st.arg.q; startSearch(st.arg.q, st.arg.in || 'everywhere'); await setView('search', st.arg, 'pop'); }
    else await setView(st.v, st.arg, 'pop');
    if (st.scroll) main.scrollTop = st.scroll;
  });
  // The back arrow: one step back when there is in-app history, else the parent page.
  const goBack = (fallback) => { if (navDepth > 0) history.back(); else if (fallback && fallback.startsWith('item:')) { const [, kind, ...id] = fallback.split(':'); setView('item', { kind, id: id.join(':') }); } else setView(fallback || 'home'); };
  $$('.nav-item').forEach((b) => b.onclick = () => setView(b.dataset.view));
  // Back from TIDAL's sign-in page.
  { const p = new URLSearchParams(location.search); const j = p.get('jamendo'); if (j) { history.replaceState(null, '', location.pathname); setTimeout(() => { if (j === 'signed_in') toast('Jamendo signed in'); else toast('Jamendo sign-in failed: ' + (p.get('message') || j), 'error'); }, 300); } }
  { const p = new URLSearchParams(location.search); const t = p.get('tidal'); if (t) { history.replaceState(null, '', location.pathname); setTimeout(() => { if (t === 'signed_in') { toast('TIDAL signed in'); setView('sources', { source: 'tidal' }); } else toast('TIDAL sign-in failed: ' + (p.get('detail') || t), 'error'); }, 300); } }
  { const p = new URLSearchParams(location.search); const t = p.get('lastfm'); if (t) { history.replaceState(null, '', location.pathname); setTimeout(() => { if (t === 'signed_in') { toast(`Last.fm connected${p.get('user') ? ' · ' + p.get('user') : ''}`); setView('settings', 'accounts'); } else toast('Last.fm sign-in failed: ' + (p.get('detail') || t), 'error'); }, 300); } }

  const search = $('#search');
  if (window.matchMedia && matchMedia('(max-width: 600px)').matches) search.placeholder = 'Search music';
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
    // Tag labels as you type (yours first), beside the results: STEWARD-FIX-search-tags.md
    const word = (/(?:#|\btag:)(\S+)$/.exec(q.trim()) || [])[1] || (/#|\btag:/.test(q) ? '' : q.trim().toLowerCase().replace(/\s+/g, '-'));
    if (word && word.length <= 20) api('GET', `/search/tags?prefix=${encodeURIComponent(word)}`).then((t) => { run.tags = t; searchChanged(run); }).catch(() => {});
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
  // Keyboard on the search box (TASK-pinakes-social-ux A1): ↑/↓ walk the results across the
  // groups, Enter opens the one in focus, Shift+Enter plays it; the box keeps the focus and says
  // which row is active (aria-activedescendant on the combobox, and a polite line naming it).
  // Typing drops the position; a source answering later keeps it on the same search.
  const searchKeys = {
    at: -1,
    q: null,
    items: () => $$('#sresults .srow, #sresults .tile, #sresults [data-sother], #sresults .toprow'),
    reset() {
      this.at = -1; this.key = null; search.removeAttribute('aria-activedescendant');
      $$('#sresults .kfocus').forEach((el) => el.classList.remove('kfocus'));
    },
    // A row by what it is (its group and its first lines), not its place: answers arriving later
    // move rows around
    keyOf: (el) => `${(el.closest('[data-skind]') || { dataset: {} }).dataset.skind || ''}|${(el.innerText || '').split('\n').filter(Boolean).slice(0, 2).join('|')}`,
    // After a re-render: the same search keeps the same row wherever it went; when it is gone the
    // highlight goes too (and says so), so Enter never opens a row nobody announced
    keep(q) {
      if (q !== this.q || this.at < 0) { this.q = q; this.at = -1; this.key = null; search.removeAttribute('aria-activedescendant'); return; }
      const items = this.items(); const k = items.findIndex((el) => this.keyOf(el) === this.key);
      if (k < 0) { this.at = -1; this.key = null; search.removeAttribute('aria-activedescendant'); const said = $('#sresults-said'); if (said) said.textContent = 'The results changed'; return; }
      this.at = k; this.show(false);
    },
    show(scroll) {
      const items = this.items(); const el = items[this.at]; if (!el) return;
      items.forEach((x) => x.classList.remove('kfocus'));
      el.classList.add('kfocus');
      if (!el.id) el.id = `sres-${this.at}`;
      search.setAttribute('aria-activedescendant', el.id);
      if (scroll) el.scrollIntoView({ block: 'nearest' });
    },
    move(d) {
      const items = this.items(); if (!items.length) return;
      this.at = Math.max(0, Math.min(items.length - 1, this.at + d));
      this.key = this.keyOf(items[this.at]);
      this.show(true);
      const said = $('#sresults-said'); if (said) said.textContent = `${(items[this.at].innerText || '').split('\n').filter(Boolean).slice(0, 2).join(', ')} (${this.at + 1} of ${items.length})`;
    },
    // Shift+Enter plays; a row with nothing to play (a person, a post) opens instead
    act(play) {
      const el = this.items()[this.at]; if (!el) return;
      const playBtn = play && $('[data-splay], [data-tileplay], [data-splaytop]', el);
      (playBtn || $('.name .lnk, [data-sopen]', el) || el).click();
    },
  };
  search.addEventListener('keydown', (e) => {
    if (state.view !== 'search') return;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); searchKeys.move(e.key === 'ArrowDown' ? 1 : -1); }
    else if (e.key === 'Enter' && searchKeys.at >= 0) { e.preventDefault(); e.stopImmediatePropagation(); clearTimeout(searchTimer); searchKeys.act(e.shiftKey); }
  }, true);
  const isLink = (q) => /^https?:\/\/\S+$/i.test(q);
  const linkKind = (u) => (/\/(artist|artists)\//.test(u) ? 'artist' : /\/(album|albums|release|release-group)\//.test(u) ? 'album' : 'song');
  const runSearch = async (how) => {
    const q = search.value.trim();
    if (!q) { if (state.view === 'search') setView('home'); return; }
    // A pasted link skips searching: its item page (by-uri, then resolve; "Reading this link…").
    if (isLink(q)) { setView('item', { kind: linkKind(q), id: idOf(q) }); return; }
    // A pasted Pubky key (with or without pubky:// or the pubky prefix) opens that person
    const pk = /^(?:pubky:\/\/|pubky)?([a-z0-9]{52})\/?$/.exec(q);
    if (pk) { setView('person', pk[1]); return; }
    const scope = (state.viewArg && state.view === 'search' && state.viewArg.in) || state.searchScope || 'everywhere';
    startSearch(q, scope);
    setView('search', { q, in: scope }, how === 'pop' ? 'pop' : 'push');
  };
  search.addEventListener('input', () => { searchKeys.reset(); clearTimeout(searchTimer); searchTimer = setTimeout(runSearch, 250); });
  search.addEventListener('keydown', (e) => { if (e.key === 'Enter') { clearTimeout(searchTimer); runSearch(); } if (e.key === 'Escape') { search.value = ''; search.blur(); if (state.view === 'search') setView('home'); } });

  document.addEventListener('keydown', (e) => {
    const typing = ['INPUT', 'SELECT', 'TEXTAREA'].includes(e.target.tagName) || e.target.isContentEditable;
    if (e.key === 'Escape') { if (!overlay.hidden) toggleNowPlaying(); closePop(); closeMenu(); closeQPanel(); return; }
    if (typing) return;
    // A clickable row (a div) is a button for the keyboard: Enter or Space opens it
    const row = e.target.closest && e.target.closest('.clickable[tabindex]');
    if (row && e.target === row && (e.key === 'Enter' || e.code === 'Space')) { e.preventDefault(); row.click(); return; }
    if (e.key === '/') { e.preventDefault(); search.focus(); search.select(); return; }
    if (e.code === 'Space') { e.preventDefault(); transport({ action: 'toggle' }); }
    if (e.key === 'ArrowRight' && !e.metaKey && !e.ctrlKey) transport({ action: 'next' });
    if (e.key === 'ArrowLeft' && !e.metaKey && !e.ctrlKey) transport({ action: 'previous' });
    if (e.key.toLowerCase() === 'n') toggleNowPlaying();
    if (e.key === '?') { e.preventDefault(); choiceDialog({ title: 'Keyboard shortcuts', text: '/  search  ·  Space  play or pause  ·  ←  →  previous or next  ·  N  now playing  ·  Esc  close  ·  ?  this list', choices: [] }); }
  });

  // ---- templates -----------------------------------------------------------
  const isFav = (t) => { if (t.id != null && state.favUris.has(`track:${t.id}`)) return true; const c = t.canonical; const uri = Array.isArray(c) ? c[0] : c; const url = uri && typeof uri === 'object' ? canonicalUrl(uri) : null; return url ? state.favUris.has(url) : false; };
  const canonicalUrl = (u) => {
    if (!u) return null;
    // The node's shape: { kind: 'recording', mbid } (fresh-eyes review: every library track
    // looked unidentified because only the older shape below was read)
    if (typeof u.kind === 'string' && u.mbid) return `https://musicbrainz.org/${u.kind.replace('_', '-')}/${u.mbid}`;
    if (u.isrc && typeof u.isrc === 'string') return 'https://musicbrainz.org/isrc/' + u.isrc;
    if (typeof u.url === 'string' && /^https?:/.test(u.url)) return u.url;
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
  // Playlists and hearts name a song the way other apps understand (its MusicBrainz entry), so a
  // file nobody has matched yet asks "Which song is this?" first (fresh-eyes review: adding your
  // own songs to a playlist failed with "no canonical identity"). Resolves to the tracks, now
  // identified, or null when the owner stops.
  const whichSong = async (t) => {
    const who = artists(t);
    toast(`Looking up “${t.title}” on MusicBrainz…`);
    let c; try { c = await api('GET', `/library/tracks/${t.id}/candidates`); } catch (e) { toast(e.message, 'error'); return null; }
    const about = `“${t.title}”${who ? ` by ${who}` : ''}`;
    if (c.needs_contact) {
      const go = await choiceDialog({ title: 'Which song is this?', text: `Playlists and favourites name each song the way other apps understand, by its entry in MusicBrainz (the open music encyclopedia). To look ${about} up there, the node first needs your email or website (Get started on Home).`, choices: [{ label: 'Open Get started', value: 'home', primary: true }] });
      if (go) setView('home');
      return null;
    }
    if (!c.suggestions.length) {
      await choiceDialog({ title: 'Which song is this?', text: `MusicBrainz has nothing that matches ${about}${who ? '' : ' (the file has no artist)'}, so it can't go into playlists or favourites yet. You can still play it and queue it. Giving the file its proper title and artist tags usually fixes this.`, choices: [{ label: 'OK', value: 'ok', primary: true }] });
      return null;
    }
    const mins = (ms) => (ms ? fmt(ms) : '');
    const pick = await choiceDialog({ title: 'Which song is this?', text: `Playlists and favourites name each song by its entry in MusicBrainz (the open music encyclopedia). Pick the one that is ${about}${t.duration_ms ? ` (${mins(t.duration_ms)})` : ''}; it is remembered for this file.`, choices: c.suggestions.map((x, k) => ({ label: `${x.title || t.title}${x.artist ? ` · ${x.artist}` : ''}`, note: [x.release_title, `${Math.round(x.score * 100)}% match`].filter(Boolean).join(' · '), value: x.id, primary: k === 0 })) });
    if (pick == null) return null;
    try { const done = await api('POST', `/library/suggestions/${pick}/accept`); rememberTracks([done]); return done; } catch (e) { toast(e.message, 'error'); return null; }
  };
  const trackRows = (tracks, { showAlbum = true, numbers = 'index', key = 'tr', start = 0, bare = false } = {}) => {
    rememberTracks(tracks);
    const z = currentZone(); const np = z && z.now_playing;
    const rows = tracks.map((t, k) => {
      const i = start + k;
      const cur = np && np.item.uri === 'track:' + t.id;
      const fav = isFav(t); const can = trackHasId(t);
      return `<div class="track ${showAlbum ? '' : 'compact'} ${cur ? 'cur' : ''}" data-i="${i}" data-tid="${t.id}">
        <div class="no"><span class="num">${numbers === 'track' ? (t.track_no ?? '') : i + 1}</span><button class="rowplay" data-act="play" title="Play from here">${icon('play')}</button></div>
        <div class="main-col"><div class="title"><a class="lnk" data-act="song">${esc(t.title)}</a>${can ? '' : `<button class="idmark" data-act="identify" title="Not matched to a known song yet. Matching lets other apps and people on Pubky recognise it; it plays fine either way">Match</button>`}</div><div class="by">${t.artists && t.artists[0] && t.artists[0].id != null ? `<a class="lnk" data-act="tartist">${esc(artists(t))}</a>` : esc(artists(t))}</div></div>
        ${showAlbum ? `<div class="album-col">${t.album ? `<button class="lnk" data-act="album" data-album="${t.album.id}">${esc(t.album.title)}</button>` : ''}</div>` : ''}
        <div class="fmt-col"><span class="fmt ${LOSSY[t.file && t.file.container] ? 'lossy' : ''}">${esc(fileFormat(t.file))}</span></div>
        <div class="dur">${fmt(t.duration_ms)}</div>
        <div class="row-actions">
          <button class="icon-btn heart ${fav ? 'on' : ''}" data-act="fav" title="${fav ? 'Remove from favourites' : 'Add to favourites'}">${icon(fav ? 'heart-fill' : 'heart')}</button>
          <button class="icon-btn" data-act="next" title="Play next">${icon('playnext')}</button>
          <button class="icon-btn" data-act="add" title="Add to queue">${icon('plus')}</button>
          <button class="icon-btn" data-act="more" title="More">${icon('more')}</button>
        </div>
      </div>`;
    }).join('');
    return bare ? rows : `<div class="tracks" data-list="${key}">${rows}</div>`;
  };
  // Track rows on #main: one listener, bound once, reading the current page's lists (every render
  // used to add another, so ⋯ ran once per visit with the lists of pages left behind)
  let rowLists = {}; let rowsBound = false;
  const bindTrackRows = (container, pageLists) => {
    rowLists = pageLists; if (rowsBound) return; rowsBound = true;
    container.addEventListener('click', async (e) => {
      const lists = rowLists;
      const btn = e.target.closest('[data-act]');
      // On a touch screen a tap on the row plays from there (no hover to reveal the play button)
      if (!btn) {
        const row = e.target.closest('.track'); const list = row && row.closest('[data-list]');
        if (row && list && !e.target.closest('a, button, input') && window.matchMedia && matchMedia('(hover: none)').matches) { const ts = lists[list.dataset.list] || []; playTracks(ts.slice(Number(row.dataset.i)), true); }
        return;
      }
      const row = btn.closest('.track'); const list = btn.closest('[data-list]');
      const tracks = lists[list ? list.dataset.list : 'tr'] || [];
      const i = Number(row ? row.dataset.i : -1);
      const act = btn.dataset.act;
      if (act === 'album') { setView('album', Number(btn.dataset.album)); return; }
      if (act === 'identify') { const t = tracks[i]; if (!t) return; const done = await whichSong(t); if (done) { tracks[i] = done; btn.remove(); toast(`Identified: ${done.title}`); } return; }
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
  const tile = (a) => `<button class="tile" data-album="${a.id}" title="${esc(a.title)}">${art(a.id, a.title, '', 'library', true)}<span class="tile-play" data-tileplay="${a.id}" role="button" title="Play album">${icon('play')}</span><div class="t">${esc(a.title)}</div><div class="a">${esc(a.album_artist || '')}${a.year ? ` · ${a.year}` : ''}${fmtBadge(a.id)}</div>${coverFrom('library')}</button>`;
  const tagRow = (tags, ref) => `<div class="tagrow" data-tagref='${esc(JSON.stringify(ref))}'><span class="eyebrow" title="Public on Pubky">Tags</span>${tags.map((t) => `<span class="chip tag"><button class="chiplink" data-tag-open="${esc(t.label)}" title="Open in Discover">#${esc(t.label)}</button><button class="chipx" data-tag-del="${esc(t.label)}" title="${t.synced ? 'Public on your homeserver · remove' : 'Not pushed yet · remove'}">×</button></span>`).join('')}<input class="taginput" id="tagadd" placeholder="Add a tag…" maxlength="20"></div>`;
  const bindTagRow = (ref) => {
    const add = $('#tagadd'); if (!add) return;
    add.onkeydown = async (e) => { if (e.key !== 'Enter' || !add.value.trim()) return; try { await api('PUT', '/tags', { ...ref, label: add.value.trim() }); toast(`Tagged #${add.value.trim().toLowerCase()}`); render(); } catch (er) { toast(er.message, 'error'); } };
    $$('[data-tag-del]', main).forEach((b) => b.onclick = async () => { try { await api('DELETE', '/tags?' + new URLSearchParams({ ...Object.fromEntries(Object.entries(ref).map(([k, v]) => [k, String(v)])), label: b.dataset.tagDel }).toString()); render(); } catch (er) { toast(er.message, 'error'); } });
    $$('[data-tag-open]', main).forEach((b) => b.onclick = () => setView('discover', { tag: b.dataset.tagOpen }));
  };
  // People on Pubky (STEWARD-UX-PEOPLE.md): profiles from Nexus via the node, cached here too.
  const profileCache = new Map();
  const loadProfiles = async (ids) => {
    const want = [...new Set(ids)].filter((x) => !profileCache.has(x)).slice(0, 50);
    if (want.length) { try { (await api('GET', `/profiles?ids=${want.join(',')}`)).forEach((c) => profileCache.set(c.id, c)); } catch { /* names wait */ } }
    return [...new Set(ids)].map((x) => profileCache.get(x)).filter(Boolean);
  };
  const face = (c, size = 'sm') => (c.avatar ? `<img class="face ${size}" src="${esc(imgSrc(c.avatar))}" alt="" loading="lazy" data-err="hide">` : `<span class="face ${size} mono" style="background:hsl(${hue(c.id)} 35% 34%)">${esc(initials(c.name))}</span>`);
  const faceStack = (cards, total) => `<span class="facestack">${cards.slice(0, 3).map((c) => face(c)).join('')}</span>${total > 3 ? `<span class="muted small">+${total - 3}</span>` : ''}`;
  const peopleMenu = (anchor, title, cards) => showMenu(anchor, [{ title }, ...cards.map((c) => ({ label: c.you ? `${c.name} (you)` : c.name, note: c.following ? 'you follow' : (c.known ? null : 'not on Pubky search yet'), icon: 'artist', run: () => setView('person', c.id) }))]);
  // The faces beside each tag chip in a box, and on the "N people tagged this" line
  const fillTagFaces = async (box, faces, counts) => {
    if (!box || !faces) return;
    const all = Object.values(faces).flat();
    if (!all.length) return;
    await loadProfiles(all);
    $$('[data-tag-open]', box).forEach((el) => {
      const label = el.dataset.tagOpen; const ids = faces[label]; if (!ids || !ids.length) return;
      const chip = el.closest('.chip') || el; if (chip.nextElementSibling && chip.nextElementSibling.dataset.faces === label) return;
      const cards = ids.map((x) => profileCache.get(x)).filter(Boolean); if (!cards.length) return;
      chip.insertAdjacentHTML('afterend', `<button class="faces" data-faces="${esc(label)}" title="Who tagged #${esc(label)}" aria-label="Who tagged #${esc(label)}">${faceStack(cards, counts.get(label) || cards.length)}</button>`);
      chip.nextElementSibling.onclick = (e) => peopleMenu(e.currentTarget, `#${label}`, cards);
    });
    const line = $('[data-taggers]', box.closest('section') || box);
    if (line) { const ids = [...new Set(all)]; const cards = ids.map((x) => profileCache.get(x)).filter(Boolean); if (cards.length) { line.innerHTML = `${faceStack(cards, Number(line.dataset.taggers) || cards.length)} <span>${line.textContent}</span>`; line.classList.add('faces-line'); line.onclick = () => peopleMenu(line, 'Who tagged this', cards); } }
  };
  // Your library › Node library › Cloud drives › Add a folder: browse the drive's folders, pick one.
  const driveFolderDialog = (provider, name) => new Promise((resolve) => {
    const d = document.createElement('dialog'); d.className = 'dialog';
    d.innerHTML = `<h2>A folder on ${esc(name)}</h2><p class="muted">Its songs join the library and play from the drive; nothing is copied.</p><div id="dfpath" class="muted small"></div><div id="dfbox" class="list"></div><p class="muted small" id="dfmsg"></p><div class="toolbar"><button class="btn primary" id="dfadd" disabled>Add this folder</button><button class="btn ghost" data-dfclose>Cancel</button></div>`;
    document.body.appendChild(d);
    const tr = { open: () => d.open, close: (fromBack) => done(fromBack) };
    const done = (fromBack, added) => { const k = transients.indexOf(tr); if (k >= 0) transients.splice(k, 1); if (d.open) { if (d.close) d.close(); else d.removeAttribute('open'); } d.remove(); if (fromBack !== true) popTransient(); resolve(added || null); };
    transients.push(tr); pushTransient();
    $('[data-dfclose]', d).onclick = () => done();
    d.addEventListener('cancel', (e) => { e.preventDefault(); done(); });
    const trail = [{ id: 'root', name }];
    const show = async () => {
      const here = trail[trail.length - 1];
      $('#dfpath', d).textContent = trail.map((t) => t.name).join(' › ');
      $('#dfadd', d).disabled = trail.length < 2;
      $('#dfbox', d).innerHTML = '<p class="muted small"><span class="spin"></span> Reading the drive…</p>';
      try {
        const fs = await api('GET', `/library/drives/${provider}/folders?parent=${encodeURIComponent(here.id)}`);
        $('#dfbox', d).innerHTML = (trail.length > 1 ? '<button class="list-row" data-dfup>‹ Back</button>' : '') + (fs.length ? fs.map((f, k) => `<button class="list-row" data-dfopen="${k}"><span class="grow">${esc(f.name)}</span>${icon('chevron')}</button>`).join('') : '<p class="muted small">No folders inside.</p>');
        $$('[data-dfopen]', d).forEach((b) => b.onclick = () => { trail.push(fs[Number(b.dataset.dfopen)]); show(); });
        const up = $('[data-dfup]', d); if (up) up.onclick = () => { trail.pop(); show(); };
      } catch (e) { $('#dfbox', d).innerHTML = `<p class="muted small">${esc(e.message)}</p>`; }
    };
    $('#dfadd', d).onclick = async () => {
      const here = trail[trail.length - 1];
      try { done(false, await api('POST', '/library/drives', { provider, folder_id: here.id, name: here.name })); } catch (e) { $('#dfmsg', d).textContent = e.message; }
    };
    if (d.showModal) d.showModal(); else d.setAttribute('open', '');
    show();
  });
  // Your library › Node library › Add a WebDAV share: the folder's address, user and password; the node
  // checks it, keeps it and scans it (POST /library/webdav).
  const webdavDialog = () => new Promise((resolve) => {
    const d = document.createElement('dialog'); d.className = 'dialog';
    d.innerHTML = `<h2>Add a WebDAV share</h2><p class="muted">A music folder on Nextcloud, ownCloud or a NAS. The node reads it over the network and plays from it; nothing is copied.</p>
      <label class="field"><span>Folder address</span><input class="input" id="wdurl" placeholder="https://cloud.example.org/remote.php/dav/files/me/Music/" autocomplete="off"></label>
      <label class="field"><span>User name</span><input class="input" id="wduser" autocomplete="username"></label>
      <label class="field"><span>Password or app password</span><input class="input" id="wdpass" type="password" autocomplete="current-password"></label>
      <label class="field"><span>Name (optional)</span><input class="input" id="wdname" placeholder="NAS"></label>
      <p class="muted small" id="wdmsg"></p><div class="toolbar"><button class="btn primary" id="wdadd">Add share</button><button class="btn ghost" data-wdclose>Cancel</button></div>`;
    document.body.appendChild(d);
    const tr = { open: () => d.open, close: (fromBack) => done(fromBack) };
    const done = (fromBack, added) => { const k = transients.indexOf(tr); if (k >= 0) transients.splice(k, 1); if (d.open) { if (d.close) d.close(); else d.removeAttribute('open'); } d.remove(); if (fromBack !== true) popTransient(); resolve(added || null); };
    transients.push(tr); pushTransient();
    $('[data-wdclose]', d).onclick = () => done();
    d.addEventListener('cancel', (e) => { e.preventDefault(); done(); });
    $('#wdadd', d).onclick = async () => {
      const b = $('#wdadd', d); b.disabled = true; $('#wdmsg', d).innerHTML = '<span class="spin"></span> Checking the share…';
      try {
        const sh = await api('POST', '/library/webdav', { url: $('#wdurl', d).value.trim(), user: $('#wduser', d).value.trim(), password: $('#wdpass', d).value, name: $('#wdname', d).value.trim() || undefined });
        done(false, sh);
      } catch (e) { b.disabled = false; $('#wdmsg', d).textContent = e.message; }
    };
    if (d.showModal) d.showModal(); else d.setAttribute('open', '');
    $('#wdurl', d).focus();
  });
  // An item page's own tags: on the song (every source) or only on this source's link; each
  // shows its scope and can move to the other one.
  const scopedTagRow = (songTags, linkTags, linkName, kind, others = []) => {
    const count = new Map(others.map(([l, c]) => [l, c]));
    const mine = new Set([...songTags, ...linkTags].map((t) => t.label));
    const chip = (t, scope) => `<span class="chip tag ${scope === 'link' ? 'tag-link' : ''}" title="${scope === 'link' ? `Only on this ${esc(linkName)} link` : `On the ${kind}, whatever source plays it`}">${scope === 'link' ? svcBadge(String(linkName).toLowerCase()) : ''}<button class="chiplink" data-tag-open="${esc(t.label)}" title="Open in Discover">#${esc(t.label)}${count.get(t.label) ? ` <span class="muted">${count.get(t.label)}</span>` : ''}</button>${t.pending ? '<span class="muted small tag-wait" title="Published once this file is matched to a song">waits for a match</span>' : ''}${linkName ? `<button class="chipx" data-tag-move="${scope}:${esc(t.label)}" title="${scope === 'link' ? `Put it on the ${kind} instead` : `Keep it on this ${esc(linkName)} link only`}" aria-label="Move #${esc(t.label)}">${icon(scope === 'link' ? 'compass' : 'disc')}</button>` : ''}<button class="chipx" data-tag-del="${scope}:${esc(t.label)}" title="${t.synced ? 'Public on your homeserver · remove' : 'Not pushed yet · remove'}">×</button></span>`;
    const theirs = others.filter(([l]) => !mine.has(l)).map(([l, c]) => `<button class="chip" data-tag-open="${esc(l)}" title="Others' tag · open in Discover">#${esc(l)} <span class="muted">${c}</span></button>`).join('');
    return `<div class="tagrow">${songTags.map((t) => chip(t, 'song')).join('')}${linkTags.map((t) => chip(t, 'link')).join('')}${theirs}<input class="taginput" id="tagadd" placeholder="Add a tag…" maxlength="20">${linkName ? `<select class="input sm" id="tagscope" aria-label="Where the tag goes"><option value="song">on the ${kind}</option><option value="link">on this ${esc(linkName)} link only</option></select>` : ''}</div>${linkName ? `<p class="muted small">A tag on the ${kind} shows wherever it plays from; a tag on this ${esc(linkName)} link stays on that link.</p>` : ''}`;
  };
  const bindScopedTags = (songRef, linkRef) => {
    const add = $('#tagadd'); if (!add) return;
    const refOf = (scope) => (scope === 'link' && linkRef ? linkRef : songRef);
    const del = (ref, label) => api('DELETE', '/tags?' + new URLSearchParams({ ...Object.fromEntries(Object.entries(ref).map(([k, v]) => [k, String(v)])), label }).toString());
    add.onkeydown = async (e) => { if (e.key !== 'Enter' || !add.value.trim()) return; const sc = $('#tagscope') ? $('#tagscope').value : 'song'; try { await api('PUT', '/tags', { ...refOf(sc), label: add.value.trim() }); toast(`Tagged #${add.value.trim().toLowerCase()}`); render(); } catch (er) { toast(er.message, 'error'); } };
    $$('[data-tag-del]', main).forEach((b) => b.onclick = async () => { const [sc, ...l] = b.dataset.tagDel.split(':'); try { await del(refOf(sc), l.join(':')); render(); } catch (er) { toast(er.message, 'error'); } });
    $$('[data-tag-move]', main).forEach((b) => b.onclick = async () => { const [sc, ...l] = b.dataset.tagMove.split(':'); const label = l.join(':'); try { await api('PUT', '/tags', { ...refOf(sc === 'link' ? 'song' : 'link'), label }); await del(refOf(sc), label); toast(sc === 'link' ? `#${label} is on the song now` : `#${label} is on this link only now`); render(); } catch (er) { toast(er.message, 'error'); } });
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
  // Long-press on a touch screen (and right-click) on a song row opens its ⋯ menu
  document.addEventListener('contextmenu', (e) => { const row = e.target.closest && e.target.closest('.srow, .track, [data-srow]'); const more = row && row.querySelector('[data-smore], [data-act="more"]'); if (!more || e.target.closest('input, textarea, a[href]')) return; e.preventDefault(); more.click(); });
  // Song rows on touch screens (docs/ux/responsive.md, build step 3): swipe right to play next, left to
  // add to a playlist (the owner's choice); a long press opens the row's menu, also where the browser
  // sends no contextmenu (iOS). Each runs the row's own ⋯ menu, so every kind of song row behaves alike.
  {
    let sw = null; let lastCtx = 0; let lastLong = 0; let longEnd = 0;
    document.addEventListener('contextmenu', (e) => { lastCtx = Date.now(); if (Date.now() - lastLong < 2000 && e.target.closest && e.target.closest('.srow, .track, [data-srow]')) { e.preventDefault(); e.stopImmediatePropagation(); } }, true);
    // the click a long press leaves on release (iOS) must not play the row
    document.addEventListener('click', (e) => {
      // the click iOS sends on releasing a long press: whatever is under the finger then (a row, the menu sheet)
      if (Date.now() - longEnd < 400 || (Date.now() - lastLong < 1500 && e.target.closest && e.target.closest('.srow, .track, [data-srow]') && !e.target.closest('[data-smore], [data-act="more"]'))) { e.preventDefault(); e.stopImmediatePropagation(); lastLong = 0; longEnd = 0; }
    }, true);
    const ROWS = '.srow, .track, [data-srow]';
    const moreOf = (row) => row.querySelector('[data-smore], [data-act="more"]');
    const reset = (row) => { row.style.transform = ''; row.classList.remove('swiping', 'swipe-ready'); delete row.dataset.swipeLabel; };
    document.addEventListener('touchstart', (e) => {
      if (sw) { clearTimeout(sw.timer); if (sw.on) reset(sw.row); }
      const row = e.target.closest && e.target.closest(ROWS);
      if (!row || e.touches.length !== 1 || e.target.closest('input, textarea, [data-qgrip], [draggable="true"]') || row.closest('.selecting') || !moreOf(row)) { sw = null; return; }
      const t = e.touches[0];
      sw = { row, x: t.clientX, y: t.clientY, dx: 0, on: false, moved: false };
      sw.timer = setTimeout(() => { const s2 = sw; if (s2 && s2.row === row && !s2.moved && Date.now() - lastCtx > 800) { s2.long = true; lastLong = Date.now(); moreOf(row).click(); } }, 600);
    }, { passive: true });
    document.addEventListener('touchmove', (e) => {
      if (!sw) return; const t = e.touches[0]; const dx = t.clientX - sw.x; const dy = t.clientY - sw.y;
      if (!sw.dir && (Math.abs(dx) > 10 || Math.abs(dy) > 10)) { sw.moved = true; sw.dir = Math.abs(dx) > Math.abs(dy) * 1.5 ? 'x' : 'y'; } // locked on the first move
      if (sw.dir !== 'x') return;
      if (!sw.on) { sw.on = true; sw.row.classList.add('swiping'); }
      sw.dx = Math.max(isListener() ? 0 : -140, Math.min(140, dx)); // listeners have no playlists to add to
      sw.row.style.transform = `translateX(${sw.dx}px)`;
      sw.row.dataset.swipeLabel = sw.dx > 0 ? 'Play next' : 'Add to playlist';
      sw.row.classList.toggle('swipe-ready', Math.abs(sw.dx) >= 80);
    }, { passive: true });
    const end = () => {
      if (!sw) return; clearTimeout(sw.timer); const s2 = sw; sw = null;
      if (s2.long) longEnd = Date.now();
      if (!s2.on) return;
      reset(s2.row);
      if (Math.abs(s2.dx) >= 80) { const more = moreOf(s2.row); menuPick = { label: s2.dx > 0 ? 'Play next' : 'Add to playlist', at: Date.now(), anchor: more }; more.click(); }
    };
    document.addEventListener('touchend', end, { passive: true });
    document.addEventListener('touchcancel', () => { if (sw) { clearTimeout(sw.timer); if (sw.on) reset(sw.row); sw = null; } }, { passive: true });
  }
  const closeMenu = (fromBack) => { if (menuEl.hidden) return; menuEl.hidden = true; if (fromBack !== true) popTransient(); };
  transients.push({ open: () => !menuEl.hidden, close: closeMenu }, { open: () => !zonePop.hidden, close: closePop }, { open: () => !qPanel.hidden, close: closeQPanel }, { open: () => !jobPop().hidden, close: closeJobPop });
  const OWNER_MENU = /favourite|playlist|^tag|post about|link another|wrong match|homeserver|upload|copy to|rename|description|identify|which song|fix tags|delete|remove from library/i;
  let menuPick = null; // { label, at }: run that item of the next menu instead of showing it (row swipes)
  // `opts.all`: every item stays for a listener too (the items are names, not owner actions)
  const showMenu = (anchor, items, opts = {}) => {
    if (isListener() && !opts.all) { items = items.filter((it) => it.title || !OWNER_MENU.test(it.label || '')); if (!items.some((it) => !it.title)) { if (menuPick && menuPick.anchor === anchor) { toast(`${menuPick.label} isn't available here`); menuPick = null; } return; } }
    if (menuPick && (menuPick.anchor !== anchor || Date.now() - menuPick.at > 4000)) { if (Date.now() - menuPick.at > 4000) menuPick = null; } // another menu: open it as usual
    else if (menuPick) {
      const want = menuPick; menuPick = null; closeMenu();
      {
        const it = items.find((x) => !x.title && !x.disabled && String(x.label || '').replace(/…$/, '') === want.label);
        if (it) Promise.resolve(it.run()); else toast(`${want.label} isn't available for this row`);
        return;
      }
    }
    menuEl.innerHTML = items.map((it, i) => it.title ? `<div class="pop-title">${esc(it.title)}</div>` : `<button class="pop-item ${it.on ? 'on' : ''}" data-mi="${i}" ${it.disabled ? 'disabled' : ''} ${it.on != null ? `aria-checked="${!!it.on}" role="menuitemradio"` : ''}>${it.svc ? svcBadge(it.svc) : it.icon ? icon(it.icon) : ''}<span>${esc(it.label)}${it.note ? `<span class="pop-note">${esc(it.note)}</span>` : ''}</span>${it.on ? icon('check', 'ic pop-check') : ''}</button>`).join('');
    // The item runs first: a page it opens replaces the transient entry; otherwise it is popped.
    $$('[data-mi]', menuEl).forEach((b) => b.onclick = () => { menuEl.hidden = true; Promise.resolve(items[Number(b.dataset.mi)].run()).finally(popTransient); });
    menuEl.hidden = false; pushTransient();
    const r = anchor.getBoundingClientRect(); const mw = 280;
    menuEl.style.left = Math.max(8, Math.min(window.innerWidth - mw - 8, r.right - mw)) + 'px';
    menuEl.style.top = Math.min(window.innerHeight - menuEl.offsetHeight - 8, r.bottom + 6) + 'px';
    menuEl.style.bottom = 'auto'; menuEl.style.right = 'auto'; menuEl.style.width = mw + 'px';
    // Phones: a sheet from the bottom, not a dropdown that runs off the screen (UX review #3)
    menuEl.classList.toggle('sheet', shellMode() === 'phone');
    if (menuEl.classList.contains('sheet')) { menuEl.style.left = '0px'; menuEl.style.right = '0px'; menuEl.style.top = 'auto'; menuEl.style.bottom = '0px'; menuEl.style.width = 'auto'; }
    // A click outside only closes the menu (it does not also press what is under it).
    setTimeout(() => { const outside = (e) => { if (menuEl.hidden) { document.removeEventListener('click', outside, true); return; } if (!menuEl.contains(e.target)) { e.stopPropagation(); e.preventDefault(); document.removeEventListener('click', outside, true); closeMenu(); } }; document.addEventListener('click', outside, true); }, 0);
  };
  const addToPlaylist = async (playlistId, tracks, uris) => {
    // Songs not identified yet go in too, kept on this node until they are
    const localN = uris ? 0 : tracks.filter((t) => t && !trackHasId(t)).length;
    try { await api('POST', `/playlists/${encodeURIComponent(playlistId)}/items`, uris ? { uris } : { track_ids: tracks.map((t) => t.id) }); const n = uris ? uris.length : tracks.length; toast((n === 1 ? 'Added to playlist' : `${countOf(n, 'tracks')} added to playlist`) + (localN ? `. ${localN === n ? (n === 1 ? 'It stays' : 'They stay') : `${localN} stay`} on this node until you pick which song ${localN === 1 ? 'it is' : 'each is'} (the ?).` : '')); }
    catch (e) { toast(e.message, 'error'); }
  };
  // "Add to playlist" for canonical URLs (Internet Archive files): the same menu, by uris.
  const uriMenu = async (anchor, uris, name) => {
    if (!state.playlists.length) { try { state.playlists = await api('GET', '/playlists'); } catch { /* keep */ } }
    return playlistPicker((pl) => addToPlaylist(pl.id, [], uris), name || 'New playlist', async (n) => { try { await api('POST', '/playlists', { name: n, uris }); state.playlists = await api('GET', '/playlists'); toast(`Created “${n}”`); } catch (e) { toast(e.message, 'error'); } });
  };
  const trackMenu = async (anchor, tracks) => {
    if (!state.playlists.length) { try { state.playlists = await api('GET', '/playlists'); } catch { /* keep */ } }
    await pubkyUser();
    const single = tracks.length === 1 ? tracks[0] : null;
    const favItem = single ? [{ label: isFav(single) ? 'Remove from favourites' : 'Add to favourites', icon: isFav(single) ? 'heart-fill' : 'heart', run: async () => { try { await setFavourite({ track_id: single.id }, !isFav(single)); render(); } catch (e) { toast(e.message, 'error'); } } }] : [];
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
      // Phones hide the "Match" chip in song rows (no room), so matching is offered here too
      ...(single && single.id != null && !trackHasId(single) && isOwner() ? [{ label: 'Which song is this?…', icon: 'search', run: async () => { const done = await whichSong(single); if (done) { toast(`Identified: ${done.title}`); render(); } } }] : []),
      { label: 'Add to playlist…', icon: 'playlist', run: () => playlistPicker((pl) => addToPlaylist(pl.id, tracks), (tracks.length === 1 ? tracks[0].title : (tracks[0].album ? tracks[0].album.title : 'New playlist')) || 'New playlist', async (name) => { try { const d = await api('POST', '/playlists', { name, track_ids: tracks.map((t) => t.id) }); state.playlists = await api('GET', '/playlists'); toast(`Created “${d.summary ? d.summary.name : name}”`); } catch (e) { toast(e.message, 'error'); } }) },
      ...radioItem,
      ...versionItems,
      ...nav,
      // The owner's library: edit tags on the node, take the song out (Your library › Node library)
      ...(single && single.id != null && isOwner() ? [{ label: 'Edit tags…', icon: 'notes', run: () => editTrackTags(single.id) }, { label: 'Remove from library', icon: 'trash', run: () => removeFromLibrary(single.id, single.title) }] : []),
      ...(tracks.length && tracks.every((t) => t && trackHasId(t)) && state.pubkyUser ? [{ label: tracks.length === 1 ? 'Copy to my homeserver' : `Copy ${countOf(tracks.length, 'songs')} to my homeserver`, icon: 'upload', run: () => startUpload({ track_ids: tracks.map((t) => t.id) }) }] : []),
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
    const sub = nr.refreshing ? `checking ${nr.artists_checked} of ${countOf(nr.artists_total, 'artists')}…` : `${nr.artists_checked} of ${nr.artists_total} tagged artists checked`;
    const rows = nr.items.slice(0, 12).map((r) => `<div class="list-row"><div class="grow"><div class="name">${esc(r.title)} <span class="muted small">${esc(r.kind)}</span></div><div class="sub">${esc(r.artist)} · ${esc(r.first_release_date)}</div></div>${r.in_library ? `<button class="btn ghost small" data-album="${r.in_library.id}">In library</button>` : `<a class="btn ghost small" href="${esc(webUrl(r.url))}" target="_blank" rel="noopener">MusicBrainz</a>`}</div>`).join('');
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
    jellyfin: '<path d="M12 3c-2.5 0-9.5 13-8.5 15S20.5 20 20.5 18 14.5 3 12 3zm0 6c1.2 0 4.5 6.3 4 7.3S8.4 17.3 8 16.3 10.8 9 12 9z"/>',
    jamendo: '<path d="M13 3h4v11.5a5.5 5.5 0 1 1-11 0h4a1.5 1.5 0 0 0 3 0z"/>',
    archive: '<path d="M3 20h18v2H3zM4 9h2v9H4zm4 0h2v9H8zm4 0h2v9h-2zm4 0h2v9h-2zm4 0h1v9h-1zM2 7l10-5 10 5v1H2z"/>',
    qobuz: '<path d="M12 3a8 8 0 1 0 4.9 14.3l2.4 2.4 1.4-1.4-2.4-2.4A8 8 0 0 0 12 3zm0 2.5a5.5 5.5 0 1 1 0 11 5.5 5.5 0 0 1 0-11z"/>',
    musicbrainz: '<path d="M12 2l9 5v10l-9 5-9-5V7zm-1 4.5L6 9v6l5 2.5zm2 0v11l5-2.5V9z"/>',
    library: '<path d="M4 4h16v3H4zm0 5h16v3H4zm0 5h10v3H4zm12 0h4v3h-4z"/>',
  };
  const serviceKey = (site) => String(site || '').toLowerCase().replace(/[^a-z]/g, '').replace(/^youtubemusic$/, 'youtube').replace(/^internetarchive$/, 'archive');
  const svcIcon = (site) => { const g = SERVICE_GLYPHS[serviceKey(site)]; return `<svg viewBox="0 0 24 24" aria-hidden="true">${g || '<path d="M9 3v12.3A3.5 3.5 0 1 0 11 18.5V7h6V3z"/>'}</svg>`; };
  const svcBadge = (site, extra = '') => site ? `<span class="src-badge ico${extra ? ' ' + extra : ''}" title="${esc(site)}">${svcIcon(site)}</span>` : '';
  // A service we can name on a cover: the library, or one with an accent or a glyph. Not a
  // MusicBrainz or Pubky page (nothing plays from those) and not a plain link.
  const knownSvc = (k) => (k && k !== 'musicbrainz' && (k === 'library' || Object.prototype.hasOwnProperty.call(SRC_ACCENT, k) || Object.prototype.hasOwnProperty.call(SERVICE_GLYPHS, k)) ? k : null);
  // The provider badge on a cover (owner, 2026-10-29): where this plays from, in the cover's bottom
  // left corner. The service's glyph in its own colour on a dark disc with a light ring, so it reads
  // on light and dark covers; our own files get a white glyph. Absolute over the cover (no layout
  // shift) and named for screen readers: "From TIDAL".
  // Inside a button (a tile, the player bar's art) the badge is decoration (`quiet`): the button's
  // name would otherwise start with "From …"; coverFrom() adds the words at the end of the button.
  const coverBadge = (svc, quiet = false) => {
    const k = knownSvc(svc === 'library' ? 'library' : srcSvc(svc)); if (!k) return '';
    const name = k === 'library' ? 'your library' : srcName(k); const a = SRC_ACCENT[k];
    return `<span class="src-badge ico cover"${quiet ? ' aria-hidden="true"' : ` role="img" aria-label="From ${esc(name)}"`} title="From ${esc(name)}"${a ? ` style="--svc:${a[1]}"` : ''}>${svcIcon(k)}</span>`;
  };
  const coverFrom = (svc) => { const k = knownSvc(svc === 'library' ? 'library' : srcSvc(svc)); return k ? `<span class="sr-only">, from ${esc(k === 'library' ? 'your library' : srcName(k))}</span>` : ''; };
  // Where a row, tile or queue item plays from: what it says itself (a service, a source, a site, a
  // "plays from" answer), else its library id, else the service in its id or uri.
  const coverSvc = (x) => {
    if (!x) return null;
    const said = x.service || x.site || (typeof x.source === 'string' ? x.source : null) || (x.plays && x.plays.service);
    const k = said && knownSvc(srcSvc(said)); if (k) return k;
    if (x.track_id != null) return 'library';
    const id = String(x.id || ''); if (/^lib-/.test(id)) return 'library';
    let m = /^([a-z]+)-/.exec(id); if (m && knownSvc(m[1])) return m[1];
    const u = String(x.uri || x.page || ''); if (/^track:\d+$/.test(u)) return 'library';
    m = /^([a-z]+):/.exec(u); if (m && !/^https?$/.test(m[1])) return knownSvc(m[1]);
    if (/^https?:/.test(u)) { try { return knownSvc(srcSvc(new URL(u).hostname)); } catch { return null; } }
    return null;
  };
  // Now playing: the node's own word on the service once it carries it (#94), else the item.
  const npSvc = (np) => (np && np.format && np.format.service ? srcSvc(np.format.service) : coverSvc(np && np.item));
  const discoverTiles = (items) => `<div class="grid">${items.map((it, i) => { const h = hue(it.title || it.uri); const playable = true; return `<button class="tile" data-ditem="${i}" title="${esc(itemName(it))}"><div class="art" style="background:linear-gradient(135deg, hsl(${h} 38% 30%), hsl(${(h + 40) % 360} 42% 18%))"><span>${esc(initials(it.title || 'M'))}</span>${it.image ? `<img loading="lazy" alt="" src="${esc(it.image)}" data-err="remove">` : ''}${coverBadge(it.site, true) || svcBadge(it.site)}${playable ? `<span class="tile-play" data-dplay="${i}" role="button" title="Play">${icon('play')}</span>` : ''}</div><div class="t">${esc(itemName(it))}${it.in_library ? ' <span class="pill ok">owned</span>' : ''}</div><div class="a">${esc([it.artist, it.group_size ? `${it.group_size} services` : null].filter(Boolean).join(' · '))}</div></button>`; }).join('')}</div>`;
  // A row's name: the title, never a URL ("Untitled on TIDAL" when unknown).
  const itemName = (it) => it.title || `Untitled on ${it.site || 'another service'}`;
  // Describe music URLs that are not in the library (favourites, playlists, history).
  const describe = async (uris) => { if (!uris.length) return []; try { return await api('POST', '/discover/describe', { uris }); } catch { return uris.map((u) => ({ uri: u, page: u, title: null, site: null, also_on: [], kind: 'other' })); } };
  // A source's state for its tab's dot: connected, not set up, an error, or switched off.
  const srcState = (x) => {
    if (!x.enabled) return { cls: 'off', label: 'Turned off', long: 'Turned off.' };
    const st = String(x.status || '');
    if (st === 'ready' || /^signed in\b/.test(st)) return { cls: 'ok', label: 'Connected', long: st === 'signed in' ? 'Signed in.' : st === 'ready' ? 'Ready.' : st.charAt(0).toUpperCase() + st.slice(1) + '.' };
    if (st === 'previews') return { cls: 'ok', label: 'Connected', long: '30-second previews, no account needed.' };
    if (/^browser\b/.test(st)) return { cls: 'ok', label: 'Ready', long: `Plays in This browser, in YouTube\u2019s own player${/zones/.test(st) ? ', and on the speakers through yt-dlp' : ''}.${/search/.test(st) ? ' Search uses your YouTube Data API key.' : ''}` };
    if (st === 'needs_config' || /^not configured/.test(st) || st === 'needs sign-in') return { cls: 'none', label: 'Not set up', long: st === 'needs sign-in' ? 'Not signed in yet.' : 'Not set up on this node yet.' };
    if (/^ready\b/.test(st)) return { cls: 'ok', label: 'Ready', long: st.charAt(0).toUpperCase() + st.slice(1) + '.' };
    // Only a real failure is red; any other note is shown as it is
    if (/error|fail|expired|refused|revoked|unreachable|broken/i.test(st)) return { cls: 'bad', label: 'Problem', long: st };
    return { cls: 'none', label: 'Not set up', long: st };
  };
  // Sources switched off disappear from the sidebar and search scopes too.
  const applySourcesOff = () => {
    const off = new Set((state.sources || []).filter((x) => x.enabled === false).map((x) => x.id));
    $$('.nav-item[data-view]').forEach((b) => { if (b.dataset.view !== 'sources') b.hidden = off.has(b.dataset.view); });
    if (state.searchScope && off.has(state.searchScope)) state.searchScope = 'everywhere';
  };
  const FEATURE_WORDS = { search: 'Search', play: 'Plays on your zones', connect: 'Plays on its own devices', artists: 'Artist pages', library: 'Your library', playlists: 'Your playlists', downloads: 'Downloads what you bought', lyrics: 'Lyrics', scrobble: 'Reports your plays' };
  const SERVICE_NAMES = { tidal: 'TIDAL', qobuz: 'Qobuz', soundcloud: 'SoundCloud', bandcamp: 'Bandcamp', deezer: 'Deezer', spotify: 'Spotify', jamendo: 'Jamendo', subsonic: 'your server', jellyfin: 'Jellyfin', library: 'your library', audius: 'Audius', archive: 'Internet Archive', musicbrainz: 'MusicBrainz' };
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
  // "Add to playlist…" (UX review #3): one dialog instead of every playlist in the menu: a search
  // field, "New playlist" first, then your playlists, most recently changed first.
  const playlistPicker = (add, newName, makeNew) => new Promise((resolve) => {
    const d = document.createElement('dialog'); d.className = 'dialog';
    d.innerHTML = `<h2>Add to playlist</h2><input class="input" id="ppq" placeholder="Find a playlist" aria-label="Find a playlist" autocomplete="off"><div class="list compact picker" id="pplist"></div><div class="dialog-actions"><button class="btn ghost" data-ppclose>Cancel</button></div>`;
    document.body.appendChild(d);
    const tr = { open: () => d.open, close: (fromBack) => done(fromBack) };
    const done = (fromBack) => { const k = transients.indexOf(tr); if (k >= 0) transients.splice(k, 1); if (d.open) { if (d.close) d.close(); else d.removeAttribute('open'); } d.remove(); if (fromBack !== true) popTransient(); resolve(); };
    transients.push(tr); pushTransient();
    const draw = () => {
      const q = $('#ppq', d).value.trim().toLowerCase();
      const list = [...state.playlists].sort((a, b) => (b.updated_at || 0) - (a.updated_at || 0)).filter((p) => !q || p.name.toLowerCase().includes(q));
      $('#pplist', d).innerHTML = `<button class="list-row" data-ppnew>${icon('plus')}<div class="grow"><div class="name">New playlist${q ? `: “${esc($('#ppq', d).value.trim())}”` : ''}</div></div></button>` + list.map((p, k) => `<button class="list-row" data-ppi="${k}">${icon('playlist')}<div class="grow"><div class="name">${esc(p.name)}</div><div class="sub">${countOf(p.items || 0, 'songs')}</div></div></button>`).join('');
      $('[data-ppnew]', d).onclick = async () => { done(); await makeNew($('#ppq', d) && $('#ppq', d).value.trim() || newName); };
      $$('[data-ppi]', d).forEach((b) => b.onclick = async () => { const pl = list[Number(b.dataset.ppi)]; done(); await add(pl); });
    };
    $('#ppq', d).oninput = draw; draw();
    $('[data-ppclose]', d).onclick = () => done();
    d.addEventListener('cancel', (e) => { e.preventDefault(); done(); });
    if (d.showModal) d.showModal(); else d.setAttribute('open', '');
    $('#ppq', d).focus();
  });
  const itemMenu = async (anchorEl, o) => {
    await pubkyUser();
    if (!state.playlists.length) { try { state.playlists = await api('GET', '/playlists'); } catch { /* keep */ } }
    const uris = (o.songs || []).map((t) => t.page || (t.id && (uriOf('song', t.id) || null))).filter(Boolean);
    const trackIds = (o.songs || []).filter((t) => t.track_id != null).map((t) => t.track_id);
    const single = (o.songs || []).length === 1 ? o.songs[0] : null;
    const singleFav = !!(single && ((single.track_id != null && state.favUris.has(`track:${single.track_id}`)) || (single.page && state.favUris.has(single.page)) || (single.id && state.favUris.has(uriOf('song', single.id) || ''))));
    // The page you are on: no "Go to" it
    const onThis = (kind, pid) => state.view === 'item' && state.viewArg && state.viewArg.kind === kind && (state.viewArg.id === pid);
    const addTo = (pl) => (trackIds.length === (o.songs || []).length && trackIds.length ? addToPlaylist(pl.id, trackIds.map((id) => ({ id })), null) : addToPlaylist(pl.id, [], uris.length ? uris : o.uri ? [o.uri] : []));
    showMenu(anchorEl, [{ title: o.title || 'Item' },
      { label: 'Play next', icon: 'playnext', run: () => playSongs(o.songs || [], 0, 'next', o.plays_from, anchorEl, o.fallback) },
      { label: 'Add to queue', icon: 'plus', run: () => playSongs(o.songs || [], 0, 'end', o.plays_from, anchorEl, o.fallback) },
      { label: 'Add to playlist…', icon: 'playlist', run: () => playlistPicker(addTo, o.title || 'New playlist', async (name) => { try { await api('POST', '/playlists', trackIds.length === (o.songs || []).length && trackIds.length ? { name, track_ids: trackIds } : { name, uris: uris.length ? uris : o.uri ? [o.uri] : [] }); state.playlists = await api('GET', '/playlists'); toast(`Created “${name}”`); } catch (e) { toast(e.message, 'error'); } }) },
      ...(single ? [{ label: singleFav ? 'Remove from favourites' : 'Add to favourites', icon: singleFav ? 'heart-fill' : 'heart', run: async () => { try { await setFavourite(single.track_id != null ? { track_id: single.track_id } : { uri: single.page || uriOf('song', single.id) }, !singleFav); render(); } catch (e) { toast(e.message, 'error'); } } }] : []),
      ...(single && trackIds.length === 1 && !isBrowser() ? [{ label: 'Start radio', icon: 'radio', run: async () => { try { await playSongs([single], 0, 'replace', o.plays_from, anchorEl, o.fallback); await api('PUT', `/zones/${encodeURIComponent(state.zone)}/radio`, { on: true }); toast('Radio: similar music follows when the queue ends'); } catch (e) { toast(e.message, 'error'); } } }] : []),
      ...(single && single.id && !onThis('song', single.id) ? [{ label: 'Go to song page', icon: 'compass', run: () => setView('item', { kind: 'song', id: single.id }) }] : []),
      ...(o.artistId ? [{ label: 'Go to artist', icon: 'artist', run: () => setView('item', { kind: 'artist', id: o.artistId }) }] : o.artistName ? [{ label: `Search “${o.artistName}”`, icon: 'artist', run: () => { search.value = o.artistName; runSearch(); } }] : []),
      ...(o.albumId && !onThis('album', o.albumId) ? [{ label: 'Go to album', icon: 'disc', run: () => setView('item', { kind: 'album', id: o.albumId }) }] : []),
      ...(o.openOn || []).slice(0, 4).map((v) => ({ label: `Open on ${SERVICE_NAMES[v.source] || v.label.split(' · ')[0] || (() => { try { return new URL(v.url).hostname.replace(/^www\./, ''); } catch { return 'the web'; } })()}`, icon: 'external', run: () => openWeb(v.url) })),
      ...(o.extra || []),
      // The owner's library: edit tags on the node, take a song out (Your library › Node library)
      ...(isOwner() && single && single.track_id != null ? [{ label: 'Edit tags…', icon: 'notes', run: () => editTrackTags(single.track_id) }, { label: 'Remove from library', icon: 'trash', run: () => removeFromLibrary(single.track_id, single.title) }] : []),
      ...(isOwner() && /^\/album\/lib-\d+$/.test(o.path || '') ? [{ label: 'Rename album…', icon: 'disc', run: () => editAlbumTags(Number(o.path.split('-').pop()), o.title) }] : []),
      ...(o.path ? [{ label: 'Copy link', icon: 'compass', run: () => copyLink(o.path) }] : []),
      // Library files only: services' songs are not the user's files
      ...(state.pubkyUser && trackIds.length ? [{ label: trackIds.length === 1 ? 'Copy to my homeserver' : `Copy ${countOf(trackIds.length, 'songs')} to my homeserver`, icon: 'upload', run: () => startUpload({ track_ids: trackIds }) }] : []),
    ]);
  };
  // One song row: title → song page, artist → artist page, album → album page, ⋯ = the item menu.
  // A collection item that doesn't play (a web page, a post): its Pinakes card in the list, with
  // Open instead of Play; filled in by fillCards (GET /v1/cards).
  const cardRow = (t, k, opts) => `<div class="list-row srow card-row${opts.rowClass ? ' ' + opts.rowClass(t, k) : ''}" data-srow="${k}" data-card="${esc(t.page)}"><span class="num muted small">${opts.numbers === 'index' ? k + 1 : (t.no ?? k + 1)}</span><span class="thumb card-thumb" aria-hidden="true">${icon('compass')}</span><div class="grow"><div class="name">${esc(t.title)}</div><div class="sub card-sub">Reading the page…</div></div>${webUrl(t.page) ? `<a class="btn ghost sm" href="${esc(webUrl(t.page))}" target="_blank" rel="noopener" aria-label="Open ${esc(t.title)} (new tab)">${icon('external')} Open</a>` : ''}${opts.extra ? opts.extra(t, k) : ''}<button class="icon-btn" data-cmore="${k}" title="More" aria-label="More: ${esc(t.title)}">${icon('more')}</button></div>`;
  // At most two at a time: the node's other calls (play, the zone) must not wait behind them
  const fillCards = (box) => {
    const rows = $$('[data-card]', box); let next = 0;
    const one = async (row) => {
      const sub = $('.card-sub', row);
      for (let tries = 0; tries < 3; tries++) {
        let c; try { c = await api('GET', `/cards?uri=${encodeURIComponent(row.dataset.card)}`); } catch { break; }
        if (!row.isConnected) return;
        if (c.status === 'found' || (c.status === 'reading' && c.name)) {
          if (c.name) $('.name', row).textContent = c.name;
          sub.textContent = [c.site, c.description].filter(Boolean).join(' · ');
          const img = webUrl(c.image); if (img) $('.card-thumb', row).outerHTML = `<img class="thumb" src="${esc(img)}" alt="" loading="lazy" data-err="hide">`;
        }
        if (c.status !== 'reading') { if (c.status === 'unknown') sub.textContent = 'A link'; return; }
        await new Promise((r) => setTimeout(r, 3000));
      }
      // Never read (or the node didn't answer): say what it is, not "Reading" for good
      if (row.isConnected && sub.textContent === 'Reading the page…') sub.textContent = 'A link';
    };
    const worker = async () => { while (next < rows.length) { const row = rows[next++]; if (row.isConnected) await one(row); } };
    worker(); worker();
  };
  const songRows = (songs, opts = {}) => `<div class="list">${songs.map((t, k) => t.card ? cardRow(t, k, opts) : `<div class="list-row clickable srow${opts.rowClass ? ' ' + opts.rowClass(t, k) : ''}" data-srow="${k}"><span class="num muted small">${opts.numbers === 'index' ? k + 1 : (t.no ?? k + 1)}</span><button class="icon-btn" data-splay="${k}" title="Play from here">${icon('play')}</button>${t.image && opts.covers !== false ? `<span class="thumbwrap"><img class="thumb" src="${esc(imgSrc(t.image))}" alt="" loading="lazy" data-err="hide">${t.sources ? '' : coverBadge(coverSvc(t))}</span>` : ''}<div class="grow"><div class="name">${t.id ? `<a class="lnk" data-ssong="${k}">${esc(t.title)}</a>` : esc(t.title)}</div><div class="sub">${[t.artist ? (t.artist_id ? `<a class="lnk" data-sartist="${k}">${esc(t.artist)}</a>` : `<a class="lnk" data-sartistq="${k}">${esc(t.artist)}</a>`) : '', t.album && opts.showAlbum !== false ? (t.album_id ? `<a class="lnk" data-salbum="${k}">${esc(t.album)}</a>` : esc(t.album)) : ''].filter(Boolean).join(' · ')}</div></div><span class="muted small">${t.duration_ms ? fmt(t.duration_ms) : ''}</span>${opts.extra ? opts.extra(t, k) : ''}<button class="icon-btn" data-smore="${k}" title="More">${icon('more')}</button></div>`).join('')}</div>`;
  // Multi-select (UX review, next tier): "Select" on a song list turns on checkboxes and a bar
  // with what to do to all of them; Escape or Cancel ends it. `opts.remove(indices)` on playlists.
  let selBar = null;
  const endSelect = () => { if (selBar) { selBar.remove(); selBar = null; } $$('.selecting').forEach((b) => b.classList.remove('selecting')); $$('.selbox').forEach((c) => c.remove()); $$('[data-selstart]').forEach((b) => { b.setAttribute('aria-pressed', 'false'); }); };
  const enableSelect = (section, songs, opts = {}) => {
    if (!section || !songs.length) return;
    const head = $('.section-head', section); if (!head || $('[data-selstart]', head)) return;
    head.insertAdjacentHTML('beforeend', `<button class="btn ghost sm" data-selstart aria-pressed="false">${icon('check')} Select</button>`);
    const sel = new Set();
    const draw = () => {
      if (!selBar) { selBar = document.createElement('div'); selBar.className = 'selbar'; selBar.setAttribute('role', 'toolbar'); selBar.setAttribute('aria-label', 'Selected songs'); document.body.appendChild(selBar); }
      const picked = [...sel].sort((a, b) => a - b).map((k) => songs[k]);
      selBar.innerHTML = `<span class="selcount">${countOf(sel.size, 'songs')} selected</span><button class="btn sm primary" data-sel="play" ${sel.size ? '' : 'disabled'}>${icon('play')} Play</button><button class="btn sm" data-sel="queue" ${sel.size ? '' : 'disabled'}>${icon('plus')} Add to queue</button><button class="btn sm" data-sel="playlist" ${sel.size ? '' : 'disabled'}>${icon('playlist')} Add to playlist…</button><button class="btn sm" data-sel="tag" ${sel.size ? '' : 'disabled'}>Tag…</button>${opts.remove ? `<button class="btn sm danger" data-sel="remove" ${sel.size ? '' : 'disabled'}>${icon('trash')} Remove</button>` : ''}<button class="btn sm ghost" data-sel="all">${sel.size === songs.length ? 'None' : 'All'}</button><button class="icon-btn" data-sel="cancel" aria-label="Stop selecting">${icon('close')}</button>`;
      $$('[data-sel]', selBar).forEach((b) => b.onclick = async () => {
        const what = b.dataset.sel;
        if (what === 'cancel') return endSelect();
        if (what === 'all') { if (sel.size === songs.length) sel.clear(); else songs.forEach((_, k) => sel.add(k)); $$('.selbox input', section).forEach((c) => { c.checked = sel.has(Number(c.dataset.k)); }); return draw(); }
        // Picked cards (web pages, posts) don't play: the songs among the picked ones do
        const songsPicked = picked.filter((t) => !t.card);
        if ((what === 'play' || what === 'queue') && !songsPicked.length) { toast('Nothing to play: links and posts open, they don’t play'); return; }
        if (what === 'play') { await playSongs(songsPicked, 0, 'replace', opts.plays_from, b, opts.fallback); return endSelect(); }
        if (what === 'queue') { await playSongs(songsPicked, 0, 'end', opts.plays_from, b, opts.fallback); return endSelect(); }
        if (what === 'playlist') {
          if (!state.playlists.length) { try { state.playlists = await api('GET', '/playlists'); } catch { /* keep */ } }
          const ids = picked.filter((t) => t.track_id != null).map((t) => t.track_id); const uris = picked.filter((t) => t.track_id == null).map((t) => t.page || (t.id && uriOf('song', t.id))).filter(Boolean);
          const add = async (pl) => { try { if (ids.length) await api('POST', `/playlists/${encodeURIComponent(pl.id)}/items`, { track_ids: ids }); if (uris.length) await api('POST', `/playlists/${encodeURIComponent(pl.id)}/items`, { uris }); toast(`${countOf(picked.length, 'songs')} added to “${pl.name}”`); endSelect(); } catch (e) { toast(e.message, 'error'); } };
          return playlistPicker(add, 'New playlist', async (name) => { try { await api('POST', '/playlists', { name, track_ids: ids, uris }); state.playlists = await api('GET', '/playlists'); toast(`Created “${name}”`); endSelect(); } catch (e) { toast(e.message, 'error'); } });
        }
        if (what === 'tag') {
          const r = await choiceDialog({ title: `Tag ${countOf(picked.length, 'songs')}`, text: 'One Pubky tag on each of them (public). Songs not matched yet keep it until they are.', field: { label: 'Tag', value: '' }, choices: [{ label: 'Tag them', value: true, primary: true }] });
          if (!r || !r.text) return;
          let n = 0; for (const t of picked) { const ref = t.track_id != null ? { track_id: t.track_id } : (t.page || t.id) ? { uri: t.page || uriOf('song', t.id) } : null; if (!ref) continue; try { await api('PUT', '/tags', { ...ref, label: r.text }); n++; } catch { /* skip that one */ } }
          toast(`Tagged ${countOf(n, 'songs')} #${r.text.toLowerCase()}`); return endSelect();
        }
        if (what === 'remove') { try { await opts.remove([...sel].sort((a, b) => b - a)); endSelect(); } catch (e) { toast(e.message, 'error'); } }
      });
    };
    $('[data-selstart]', head).onclick = (e) => {
      if (section.classList.contains('selecting')) return endSelect();
      endSelect(); section.classList.add('selecting'); e.currentTarget.setAttribute('aria-pressed', 'true');
      $$('.srow[data-srow]', section).forEach((row) => { const k = Number(row.dataset.srow); row.insertAdjacentHTML('afterbegin', `<label class="selbox" title="Select"><input type="checkbox" data-k="${k}" aria-label="Select ${esc(songs[k] ? songs[k].title : '')}"></label>`); const c = $('.selbox input', row); c.onclick = (ev) => ev.stopPropagation(); c.onchange = () => { if (c.checked) sel.add(k); else sel.delete(k); draw(); }; });
      draw();
    };
  };
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && selBar) endSelect(); });
  // A compact header once the page's big one scrolls away: the title and Play stay at hand
  const stickyHead = (title, onPlay) => {
    const hero = $('.hero', main); if (!hero || !window.IntersectionObserver) return;
    main.insertAdjacentHTML('afterbegin', `<div class="stickyhead" hidden><strong>${esc(title)}</strong><button class="btn sm primary" data-sticky-play>${icon('play')} Play</button></div>`);
    const bar = $('.stickyhead', main);
    $('[data-sticky-play]', bar).onclick = (e) => onPlay(e.currentTarget);
    const io = new IntersectionObserver(([en]) => { if (!document.body.contains(bar)) { io.disconnect(); return; } bar.hidden = en.isIntersecting; }, { root: main, threshold: 0 });
    io.observe(hero);
  };
  const bindSongRows = (box, all, ctx = {}) => {
    // Cards (web pages, posts) don't play: play the songs only, from the one clicked
    const songs = all.filter((t) => !t.card); const at = (k) => Math.max(0, songs.indexOf(all[k]));
    $$('[data-splay]', box).forEach((b) => b.onclick = (e) => { e.stopPropagation(); playSongs(songs, at(Number(b.dataset.splay)), 'replace', ctx.plays_from, b, ctx.fallback); });
    $$('[data-srow]:not([data-card])', box).forEach((r) => r.onclick = (e) => { if (e.target.closest('a,button')) return; playSongs(songs, at(Number(r.dataset.srow)), 'replace', ctx.plays_from, r, ctx.fallback); });
    fillCards(box);
    // A card's ⋯: open it, copy its link, and the row's own moves (on phones the only way to them)
    $$('[data-cmore]', box).forEach((b) => b.onclick = (e) => {
      e.stopPropagation(); const k = Number(b.dataset.cmore); const t = all[k]; const web = webUrl(t.page);
      showMenu(b, [{ title: t.title }, ...(web ? [{ label: 'Open', icon: 'external', run: () => window.open(web, '_blank', 'noopener') }] : []),
        { label: 'Copy link', icon: 'copy', run: async () => { try { await navigator.clipboard.writeText(t.page); toast('Link copied'); } catch { toast(t.page); } } },
        ...(ctx.rowExtra ? ctx.rowExtra(k, b.closest('.srow')) : [])]);
    });
    $$('[data-ssong]', box).forEach((a) => a.onclick = (e) => { e.stopPropagation(); setView('item', { kind: 'song', id: all[Number(a.dataset.ssong)].id }); });
    $$('[data-sartist]', box).forEach((a) => a.onclick = (e) => { e.stopPropagation(); setView('item', { kind: 'artist', id: all[Number(a.dataset.sartist)].artist_id }); });
    $$('[data-sartistq]', box).forEach((a) => a.onclick = (e) => { e.stopPropagation(); search.value = all[Number(a.dataset.sartistq)].artist; runSearch(); });
    $$('[data-salbum]', box).forEach((a) => a.onclick = (e) => { e.stopPropagation(); setView('item', { kind: 'album', id: all[Number(a.dataset.salbum)].album_id }); });
    $$('[data-smore]', box).forEach((b) => b.onclick = (e) => { e.stopPropagation(); const t = all[Number(b.dataset.smore)]; itemMenu(b, { title: t.title, songs: [t], plays_from: ctx.plays_from, fallback: ctx.fallback, artistId: t.artist_id, artistName: t.artist, albumId: t.album_id, openOn: t.page ? [{ source: serviceOfUrl(t.page) || (t.uri || '').split(':')[0], url: t.page, label: '' }] : [], path: t.id ? `/song/${t.id}` : null, extra: ctx.rowExtra ? ctx.rowExtra(Number(b.dataset.smore), b.closest('.srow')) : [] }); });
  };
  // One tile for albums and artists: click opens the item page, the hover Play plays it.
  const itemTileGrid = (tiles, kind) => `<div class="grid">${tiles.map((a, k) => `<button class="tile" data-tile="${k}" title="${esc(a.title)}"><div class="art" style="background:linear-gradient(135deg, hsl(${hue(a.title)} 38% 30%), hsl(${(hue(a.title) + 40) % 360} 42% 18%))"><span>${esc(initials(a.title))}</span>${a.image ? `<img loading="lazy" alt="" src="${esc(imgSrc(a.image))}" data-err="remove">` : ''}${coverBadge(coverSvc(a), true)}<span class="tile-play" data-tileplay="${k}" role="button" title="Play">${icon('play')}</span></div><div class="t">${esc(a.title)}</div><div class="a">${a.year || ''}</div>${coverFrom(coverSvc(a))}</button>`).join('')}</div>`;
  const bindItemTiles = (box, tiles, kind) => {
    $$('[data-tile]', box).forEach((b) => b.onclick = (e) => { if (e.target.closest('[data-tileplay]')) return; setView('item', { kind, id: tiles[Number(b.dataset.tile)].id }); });
    $$('[data-tileplay]', box).forEach((b) => b.onclick = async (e) => { e.stopPropagation(); try { const pg = await api('GET', `/item/${kind}/${tiles[Number(b.dataset.tileplay)].id}`); playSongs(pg.songs, 0, 'replace', pg.plays_from, b, { artist: pg.artist && pg.artist.name, album: pg.title, image: pg.image }); } catch (er) { toast(er.message, 'error'); } });
  };

  const SOURCE_RANK = ['library', 'tidal', 'deezer', 'spotify', 'archive', 'audius', 'pubky'];
  // Groups in the order people scan them: who, then what (UX review #4)
  const SEARCH_KINDS = [['artist', 'Artists'], ['album', 'Albums'], ['song', 'Songs'], ['playlist', 'Playlists'], ['person', 'People'], ['post', 'Posts'], ['podcast', 'Podcasts'], ['concert', 'Concerts']];
  // Facets (TASK-pinakes-social-ux A2): chips under the scope chips, counted from the merged
  // results in hand; each facet keeps at most one value on, and the rows and tiles filter by them.
  const SEARCH_FACETS = [['service', 'Source'], ['quality', 'Quality'], ['decade', 'Years'], ['site', 'Site']];
  const QUALITY_WORDS = { hi_res: 'Hi-res', lossless: 'Lossless', lossy: 'Lossy' };
  // People and posts are Pubky's too: one Source chip for the three
  const facetValues = (h) => ({ service: [...new Set((h.sources || []).map((x) => (x === 'people' || x === 'posts' ? 'pubky' : x)))], quality: h.quality ? [h.quality] : [], decade: h.year ? [`${Math.floor(h.year / 10) * 10}s`] : [], site: h.site ? [h.site] : [] });
  const facetWord = (facet, v) => (facet === 'service' ? (v === 'library' ? 'Your library' : v === 'pubky' ? 'Pubky' : srcName(v)) : facet === 'quality' ? QUALITY_WORDS[v] || v : v);
  const facetsOf = (byKind) => { const counts = Object.fromEntries(SEARCH_FACETS.map(([f]) => [f, new Map()])); for (const list of Object.values(byKind)) for (const h of list) { const fv = facetValues(h); for (const [f] of SEARCH_FACETS) for (const v of fv[f]) counts[f].set(v, (counts[f].get(v) || 0) + 1); } return counts; };
  const passesFacets = (h, on) => SEARCH_FACETS.every(([f]) => !on[f] || facetValues(h)[f].includes(on[f]));
  // A hit "matches" when every word of the query is in its title or subtitle
  const searchWords = (x) => String(x || '').toLowerCase().normalize('NFKD').replace(/[\u0300-\u036f]/g, '').split(/[^a-z0-9]+/).filter(Boolean);
  const closeMatch = (h, q) => { const want = searchWords(q); const have = new Set(searchWords(`${h.title} ${h.subtitle || ''}`)); return want.length > 0 && want.every((w) => have.has(w) || [...have].some((x) => x.startsWith(w))); };
  const srcName = (src) => (src === 'library' ? 'Library' : src === 'pubky' ? 'Pubky' : SERVICE_NAMES[src] || (state.sources || []).find((x) => x.id === src)?.name || src);
  // One order for every list of services (STEWARD-UX-PLAYER-SOURCES §1): services switched on
  // and working first, in the owner's play order; then the rest. Your library always works.
  // Matched by prefix ("signed in · user", "ready · …"); "browser" plays in This browser with no set-up
  const HEALTHY_STATUS = /^(ready|previews|connected|signed in|ok|browser)\b/i;
  const srcHealthy = (x) => !!x && x.enabled !== false && (typeof x.healthy === 'boolean' ? x.healthy : HEALTHY_STATUS.test(String(x.status || '')));
  const loadSvcRank = async () => {
    if (!isOwner()) return null;
    if (state.svcRank && Date.now() - state.svcRank.at < 60000) return state.svcRank.failed ? null : state.svcRank;
    if (state.svcRankLoading) return state.svcRankLoading;
    const gen = state.svcRankGen || 0;
    const load = (async () => {
      try {
        const [srcs, po] = await Promise.all([api('GET', '/sources'), api('GET', '/play-order').catch(() => ({ order: [], services: [] }))]);
        state.sources = srcs;
        // The play order knows who can play right now; fall back to each service's status
        const connected = new Map((po.services || []).map((x) => [x.id, x.connected]));
        const ok = (x) => srcHealthy(x) && connected.get(x.id) !== false;
        const rank = { at: Date.now(), healthy: new Set(['library', ...srcs.filter(ok).map((x) => x.id)]), off: new Set(srcs.filter((x) => x.enabled === false).map((x) => x.id)), order: po.order || [] };
        // A change saved while this was loading wins: keep nothing from before it
        if (gen === (state.svcRankGen || 0)) state.svcRank = rank;
        return rank;
      } catch { if (gen === (state.svcRankGen || 0)) state.svcRank = { at: Date.now(), failed: true }; return null; } finally { if (state.svcRankLoading === load) state.svcRankLoading = null; }
    })();
    state.svcRankLoading = load;
    return load;
  };
  // A source row's service id: Pinakes names sites by slug or host ("youtube-music", "listen.tidal.com");
  // a host counts by its main domain
  const srcSvc = (src) => { const v = String(src || '').toLowerCase(); if (v === 'library') return 'library'; const parts = v.split('.'); return serviceKey(parts.length > 1 ? parts[parts.length - 2] : v); };
  // Sort anything that names a service (`svc(x)` gives its id): healthy first, then by play order
  const rankSvc = (list, svc = (x) => x, r = state.svcRank) => {
    if (!r || r.failed) return list;
    const pos = (id) => { const i = r.order.indexOf(id); return i < 0 ? 999 : i; };
    return list.map((x, k) => [x, k]).sort(([a, i], [b, j]) => (r.healthy.has(svc(a)) ? 0 : 1) - (r.healthy.has(svc(b)) ? 0 : 1) || pos(svc(a)) - pos(svc(b)) || i - j).map(([x]) => x);
  };
  // Merge hits across sources: one row per song (Pinakes group, ISRC, UPC, MBID), best source first.
  const mergeSearch = (run) => {
    const byKind = Object.fromEntries(SEARCH_KINDS.map(([k]) => [k, []]));
    const index = new Map();
    const answers = Object.values(run.answers).filter((x) => x.status === 'ok').sort((x, y) => SOURCE_RANK.indexOf(x.source) - SOURCE_RANK.indexOf(y.source));
    for (const ans of answers) for (const h of ans.hits) {
      if (!byKind[h.kind]) continue;
      // Artists without a shared id: the same name is one row across services, and a service's
      // later namesakes (tribute acts, typos) are dropped: five identical "Queen" rows helped no
      // one (second review #13)
      const byName = h.kind === 'artist' && searchWords(h.title).join(' ');
      let found = (h.keys || []).map((k) => index.get(h.kind + '|' + k)).find(Boolean);
      if (!found && byName) {
        const same = index.get('artist-name|' + byName);
        if (same && same.sources.includes(ans.source)) continue;
        found = same;
        if (found && !found.image && h.image) found.image = h.image;
      }
      if (found) { if (h.tagged && !found.tagged) found.tagged = h.tagged; if (!found.sources.includes(ans.source)) found.sources.push(ans.source); for (const k of h.keys || []) index.set(h.kind + '|' + k, found); continue; }
      const m = { ...h, sources: [ans.source] };
      byKind[h.kind].push(m);
      for (const k of h.keys || []) index.set(h.kind + '|' + k, m);
      if (byName) index.set('artist-name|' + byName, m);
    }
    return byKind;
  };
  const renderSearchBody = (run) => {
    const chips = $('#scopechips'); const box = $('#sresults'); if (!chips || !box) return;
    const scope = run.scope;
    if (!state.sources && !state.sourcesLoading) { state.sourcesLoading = true; api('GET', '/sources').then((l) => { state.sources = l; state.sourcesLoading = false; applySourcesOff(); renderSearchBody(run); }).catch(() => { state.sourcesLoading = false; }); }
    const services = (state.sources || []).filter((x) => x.enabled !== false && (!x.features || x.features.includes('search')) && !/^not configured/.test(x.status) && x.status !== 'needs_config' && x.status !== 'browser');
    const scopes = [['everywhere', 'Everywhere'], ['library', 'Library'], ['pubky', 'Pubky'], ...services.map((x) => [x.id, x.name])];
    const stat = (src) => { const ans = run.answers[src]; if (ans) return ans.status === 'ok' ? `<span class="cnt">${ans.hits.length}</span>` : `<span class="bad" title="${esc(ans.error || '')}">!</span>`; return run.sources.includes(src) && run.pending ? '<span class="spin"></span>' : ''; };
    chips.innerHTML = scopes.map(([id, name]) => `<button class="chip scope ${scope === id ? 'on' : ''}" data-scope-in="${id}">${id === 'everywhere' || id === 'library' || id === 'pubky' ? '' : svcBadge(id)}${esc(name)} ${scope === 'everywhere' || scope === id ? stat(id === 'everywhere' ? '' : id) : ''}</button>`).join('');
    $$('[data-scope-in]', chips).forEach((b) => b.onclick = () => { state.searchScope = b.dataset.scopeIn; startSearch(run.q, b.dataset.scopeIn); setView('search', { q: run.q, in: b.dataset.scopeIn }, 'replace'); });
    const allKinds = mergeSearch(run);
    run.facets = run.facets || {};
    const facetCounts = facetsOf(allKinds);
    const byKind = Object.fromEntries(Object.entries(allKinds).map(([k, list]) => [k, list.filter((h) => passesFacets(h, run.facets))]));
    const facetChips = SEARCH_FACETS.map(([f, label]) => { const vals = [...facetCounts[f].entries()].sort((a, b) => b[1] - a[1]); if (vals.length < 2 && !run.facets[f]) return ''; return `<span class="facet" data-facet-group="${f}"><span class="muted small">${label}</span>${vals.slice(0, 6).map(([v, n]) => `<button class="chip sm ${run.facets[f] === v ? 'on' : ''}" data-facet="${f}:${esc(v)}" aria-pressed="${run.facets[f] === v}">${esc(facetWord(f, v))} <span class="muted">${n}</span></button>`).join('')}</span>`; }).filter(Boolean).join('');
    const facetBar = facetChips ? `<div class="chips facets" role="group" aria-label="Narrow the results">${facetChips}${Object.values(run.facets).some(Boolean) ? '<button class="chip sm ghost" data-facet="">Clear</button>' : ''}</div>` : '';
    const problems = run.sources.filter((src) => (run.answers[src] && run.answers[src].status !== 'ok') || (!run.answers[src] && run.slow.has(src)));
    const pendingAny = run.pending && run.sources.some((src) => !run.answers[src]);
    const skel = (n, tiles) => (tiles ? `<div class="sk-grid">${'<div class="skeleton"></div>'.repeat(n)}</div>` : `<div class="skeleton" style="height:${n * 54}px"></div>`);
    const sections = [];
    const counts = {}; for (const ans of Object.values(run.answers)) for (const [k, c] of ans.counts || []) counts[k] = Math.max(counts[k] || 0, c);
    // A Top result only when it really matches; a fuzzy hit is not promoted
    const top = [byKind.artist.find((x) => x.sources.includes('library')), byKind.song.find((x) => x.sources.includes('library')), byKind.artist[0], byKind.album[0], byKind.song[0]].find((x) => x && closeMatch(x, run.q));
    const anyHits = SEARCH_KINDS.some(([k]) => byKind[k].length);
    const tg = run.tags; if (tg && ((tg.mine || []).length || (tg.others || []).length)) sections.push(`<section class="section fadein" id="stags"><div class="section-head"><h3>Tags</h3><span class="muted small">on Pubky</span></div><div class="chips">${(tg.mine || []).map((t) => `<button class="chip on" data-stag="${esc(t.label)}" title="Your tag">Your #${esc(t.label)} <span class="muted">${countOf(t.count, 'items')}</span></button>`).join('')}${(tg.others || []).map((l) => `<button class="chip" data-stag="${esc(l)}">#${esc(l)}</button>`).join('')}</div></section>`);
    if (!top && anyHits && !run.q.includes('#')) sections.push('<p class="muted small" id="snomatch">No exact match. The closest results are below.</p>');
    if (top) sections.push(`<section class="section fadein" id="stop"><div class="section-head"><h3>Top result</h3></div><div class="card toprow"><div class="art ${top.kind === 'artist' ? 'round' : ''}">${top.image ? `<img src="${esc(imgSrc(top.image))}" alt="" data-err="remove">` : ''}</div><div class="grow"><div class="name big"><a class="lnk" data-sopen="top">${esc(top.title)}</a></div><div class="sub">${esc([{ song: 'Song', album: 'Album', artist: 'Artist', playlist: 'Playlist', podcast: 'Podcast', concert: 'Concert' }[top.kind], top.subtitle].filter(Boolean).join(' · '))} <span class="src-badges">${top.sources.map((x) => svcBadge(x === 'library' ? 'library' : x)).join('')}</span></div></div><button class="btn primary" data-splaytop>${icon('play')} Play</button></div></section>`);
    const songCap = run.allSongs ? 50 : 6;
    for (const [kind, label] of SEARCH_KINDS) {
      const items = byKind[kind];
      if (!items.length && !pendingAny) continue;
      const head = `<div class="section-head"><h3>${label}</h3><span class="muted small">${items.length ? items.length : ''}${counts[kind] && counts[kind] > items.length ? ` of ${counts[kind]} on Pubky` : ''}</span></div>`;
      let body;
      if (!items.length) body = skel(kind === 'song' ? 4 : 1, kind === 'album' || kind === 'artist');
      else if (kind === 'song') body = songRows(items.slice(0, songCap).map((h) => ({ ...h, artist: h.subtitle, id: h.id })), { showAlbum: true, numbers: 'index', extra: (t) => `${t.tagged ? `<span class="muted small tagged-by">tagged ${esc(t.tagged)}</span>` : ''}<span class="src-badges">${t.sources.map((x) => svcBadge(x === 'library' ? 'library' : x)).join('')}</span>` });
      else if (kind === 'album' || kind === 'artist') body = itemTileGrid(items.slice(0, 12).map((h) => ({ ...h, year: h.year || h.subtitle })), kind);
      // People open their page; your playlists open here; posts and the rest open on their site
      else body = `<div class="list">${items.slice(0, 10).map((h, k) => { const own = kind === 'playlist' && h.sources.includes('library'); const lead = kind === 'person' ? (h.image ? `<img class="thumb round" src="${esc(imgSrc(h.image))}" alt="" loading="lazy" data-err="hide">` : `<div class="avatar">${esc(initials(h.title || 'P'))}</div>`) : own ? icon('playlist') : svcBadge(h.sources[0]); return `<div class="list-row clickable" data-sother="${kind}:${k}">${lead}<div class="grow"><div class="name">${esc(h.title)}</div><div class="sub">${esc(h.subtitle || '')}</div></div>${kind === 'person' || own ? icon('chevron', 'ic right') : icon('external')}</div>`; }).join('')}</div>`;
      const more = kind === 'song' && items.length > songCap ? `<button class="btn ghost sm" data-sallsongs>See all ${items.length} songs</button>` : '';
      sections.push(`<section class="section ${items.length ? 'fadein' : ''}" data-skind="${kind}">${head}${body}${more}</section>`);
    }
    const short = (e, src) => { const m = humanError(String(e || ''), `/sources/${src}`).replace(/^[a-z]+: /, '').split(/(?<=\.)\s/)[0]; return m.length > 110 ? m.slice(0, 107) + '…' : m; };
    const notes = problems.length ? `<div class="snotes">${problems.map((src) => `<span class="snote" title="${esc((run.answers[src] && run.answers[src].error) || 'still waiting')}">${svcBadge(src)} ${esc(srcName(src))} didn't answer${run.answers[src] && run.answers[src].error ? `: ${esc(short(run.answers[src].error, src))}` : ''} · <a data-sretry="${src}">Retry</a></span>`).join('')}</div>` : '';
    const nothing = !pendingAny && !SEARCH_KINDS.some(([k]) => byKind[k].length);
    const nothingAtAll = nothing && !(tg && ((tg.mine || []).length || (tg.others || []).length));
    box.innerHTML = notes + facetBar + (nothingAtAll ? emptyState('search', 'Nothing matched', scope === 'everywhere' ? 'Not in your library, your services or on Pubky.' : `Nothing on ${srcName(scope)}. Try Everywhere.`) : sections.join(''));
    // A facet chip keeps the focus across the re-render (or it moves to the first chip, after Clear)
    $$('[data-facet]', box).forEach((b) => b.onclick = () => {
      const key = b.dataset.facet; const [f, ...rest] = key.split(':'); const v = rest.join(':');
      if (!f) run.facets = {}; else run.facets[f] = run.facets[f] === v ? null : v;
      renderSearchBody(run);
      const chips = $$('#sresults [data-facet]'); const again = chips.find((x) => x.dataset.facet === key && key) || chips[0];
      if (again) again.focus(); else search.focus();
    });
    searchKeys.keep(`${run.q}|${run.scope}`);
    $$('[data-stag]', box).forEach((b) => b.onclick = () => setView('discover', { tag: b.dataset.stag }));
    const sa = $('[data-sallsongs]', box); if (sa) sa.onclick = () => { run.allSongs = true; renderSearchBody(run); };
    // Bind: rows and tiles use the shared components.
    const songsSec = $('[data-skind="song"]', box);
    if (songsSec && byKind.song.length) { const ss = byKind.song.slice(0, run.allSongs ? 50 : 6).map((h) => ({ ...h, artist: h.subtitle })); bindSongRows(songsSec, ss, {}); enableSelect(songsSec, ss); }
    for (const kind of ['album', 'artist']) { const sec = $(`[data-skind="${kind}"]`, box); if (sec && byKind[kind].length) bindItemTiles(sec, byKind[kind].slice(0, 12), kind); }
    $$('[data-sother]', box).forEach((r) => r.onclick = () => { const [k, n] = r.dataset.sother.split(':'); const h = byKind[k][Number(n)]; if (k === 'person') setView('person', h.id); else if (k === 'playlist' && h.sources.includes('library')) setView('playlist', h.id); else if (h.page) openWeb(h.page); });
    const to = $('[data-sopen="top"]', box); if (to) to.onclick = () => setView('item', { kind: top.kind === 'song' ? 'song' : top.kind, id: top.id });
    const tp = $('[data-splaytop]', box); if (tp) tp.onclick = async (e) => { if (top.kind === 'song') return playSongs([{ ...top, artist: top.subtitle }], 0, 'replace', null, e.currentTarget); try { const pg = await api('GET', `/item/${top.kind}/${top.id}`); playSongs(pg.songs || [], 0, 'replace', pg.plays_from, e.currentTarget, { artist: pg.artist && pg.artist.name, album: pg.title, image: pg.image }); } catch (er) { toast(er.message, 'error'); } };
    $$('[data-sretry]', box).forEach((b) => b.onclick = () => { searchRuns.delete(`${run.q.toLowerCase()}|${run.scope}`); startSearch(run.q, run.scope); render(); });
  };
  // ---- Home (STEWARD-UX-HOME.md): everything Pinakes knows about music, one section per call ----
  const HOME_TITLES = { 'trending-songs': 'Trending songs', 'trending-albums': 'Trending albums', 'trending-artists': 'Trending artists', following: 'From people you follow', tags: 'Tags people use for music', concerts: 'Concerts near you', podcasts: 'Podcasts', yours: 'Yours on Pubky', new: 'New on Pubky' };
  const WINDOWS = [['24h', 'Today'], ['7d', 'This week'], ['30d', 'This month']];
  const homeKind = (it) => (it.kind === 'track' ? 'song' : it.kind);
  // A Discover item as a shared song row or tile.
  const asRow = (it) => ({ title: itemName(it), artist: it.artist, album: it.versions ? `${it.versions + 1} versions` : null, id: idOf(it.page || it.uri), page: it.page || it.uri, uri: it.in_library ? null : it.playable, track_id: it.in_library ? it.in_library.id : null, image: it.image });
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
      + (tiles.length ? `<div data-hgrid>${itemTileGrid(tiles.slice(0, full ? 60 : 12).map(asTile), 'album')}</div>` : '')
      + (events.length ? `<div class="list">${events.slice(0, full ? 60 : 6).map((it, k) => `<div class="list-row clickable" data-hopen="${k}"><div class="grow"><div class="name">${esc(itemName(it))}</div><div class="sub">${esc([it.description, it.site].filter(Boolean).join(' · '))}</div></div>${icon('external')}</div>`).join('')}</div>` : '');
  };
  const bindHome = (el, sec) => {
    const songs = sec.items.filter((it) => it.kind === 'track'); const tiles = sec.items.filter((it) => ['album', 'artist', 'podcast', 'playlist'].includes(it.kind)); const events = sec.items.filter((it) => it.kind === 'concert' || it.kind === 'other');
    if (songs.length) bindSongRows(el, songs.map(asRow), {});
    // Only this section's own tiles (an item page has other tile grids with their own handlers)
    $$('[data-hgrid] [data-tile]', el).forEach((b) => b.onclick = (e) => { if (e.target.closest('[data-tileplay]')) return; const it = tiles[Number(b.dataset.tile)]; if (!it) return; if (it.kind === 'podcast') return openWeb(it.page || it.uri); setView('item', { kind: homeKind(it), id: idOf(it.page || it.uri) }); });
    $$('[data-hgrid] [data-tileplay]', el).forEach((b) => b.onclick = (e) => { e.stopPropagation(); const it = tiles[Number(b.dataset.tileplay)]; if (it) discoverPlay(it, null, b); });
    $$('[data-hopen]', el).forEach((r) => r.onclick = () => { const it = events[Number(r.dataset.hopen)]; openWeb(it.page || it.uri); });
    $$('[data-tag-open]', el).forEach((b) => b.onclick = () => setView('tag', b.dataset.tagOpen));
    $$('[data-home-signin]', el).forEach((b) => b.onclick = () => { state.autoResign = true; setView('settings', 'accounts'); });
    const cs = $('[data-city-save]', el); if (cs) cs.onclick = async () => { const city = $('[data-city]', el).value.trim(); if (!city) return; try { const l = await api('PUT', '/home/location', { city }); toast(`Concerts near ${l.label}`); render(); } catch (e) { toast(e.message, 'error'); } };
  };
  const homeCache = new Map(); // section|window → { at, sec }
  const loadHomeSection = async (name, window, limit) => {
    const key = `${name}|${window || ''}|${limit || ''}`; const c = homeCache.get(key);
    if (c && Date.now() - c.at < 60000) return c.sec;
    const sec = await api('GET', `/home/${name}?${new URLSearchParams({ ...(window ? { window } : {}), ...(limit ? { limit } : {}) })}`).catch((e) => ({ section: name, items: [], tags: [], error: e.message }));
    sec.items = sec.items || []; sec.tags = sec.tags || [];
    // "Needs a contact / a sign-in" answers are not kept: they change as soon as the owner acts
    if (!sec.error && !sec.needs) homeCache.set(key, { at: Date.now(), sec });
    return sec;
  };
  const mins = (ms) => { if (ms && ms < 60000) return `${Math.round(ms / 1000)} s`; const m = Math.round((ms || 0) / 60000); return m >= 60 ? `${Math.floor(m / 60)} h ${m % 60} min` : `${m} min`; };
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
    if (plan.preview) options.push({ label: `Play a 30 s preview (${SERVICE_NAMES[plan.preview.service] || plan.preview.service})`, icon: 'play', run: () => playSourceItems([{ uri: plan.preview.id.startsWith('track:') ? `${plan.preview.service}:${plan.preview.id}` : plan.preview.id, title: it.title || '', artist: it.artist || '', album: `30 s preview · ${SERVICE_NAMES[plan.preview.service] || plan.preview.service}`, duration_ms: 30000, image: it.image || null }], false, 'now') });
    if (anchor) showMenu(anchor, options); else toast('Not on your services: connect one in Sources', 'error');
  };
  const bindDiscoverRows = (items) => {
    $$('[data-ditem]', main).forEach((row) => row.onclick = (e) => { if (e.target.closest('[data-dplay],[data-dmore]')) return; { const it = items[Number(row.dataset.ditem)]; setView('discover-item', { uri: it.uri, kind: it.kind }); } });
    $$('[data-dplay]', main).forEach((b) => b.onclick = (e) => { e.stopPropagation(); discoverPlay(items[Number(b.dataset.dplay)], null, b); });
    $$('[data-view="sources"]', main).forEach((a) => a.onclick = (e) => { e.stopPropagation(); setView('sources'); });
    $$('[data-dmore]', main).forEach((b) => b.onclick = (e) => { e.stopPropagation(); const it = items[Number(b.dataset.dmore)]; showMenu(b, [{ title: it.title || it.uri }, { label: 'Details, tags and notes', icon: 'compass', run: () => setView('discover-item', { uri: it.uri, kind: it.kind }) }, ...(it.in_library ? [{ label: 'Play from library', icon: 'play', run: () => playTracks([it.in_library], true) }, { label: 'Open album', icon: 'disc', run: () => it.in_library.album && it.in_library.album.id ? setView('album', { id: it.in_library.album.id }) : toast('No album for this track') }] : []), ...(it.playable ? [{ label: `Play on ${it.site || 'service'}`, icon: 'zone', run: () => discoverPlay(it) }] : []), ...it.also_on.filter((v) => v.uri).map((v) => ({ label: `Play on ${v.source.toUpperCase()}`, icon: 'zone', run: () => discoverPlay(it, v) })), { label: `Open ${it.site ? 'on ' + it.site : 'link'}`, icon: 'external', run: () => openWeb(it.uri) }]); });
  };
  const emptyState = (iconName, title, text, action) => `<div class="empty">${icon(iconName)}<h3>${esc(title)}</h3><p>${esc(text)}</p>${action ? `<button class="btn primary" data-empty-action="${action.view}">${esc(action.label)}</button>` : ''}</div>`;
  const LAYOUTS = [['auto', 'Automatic', 'By the width of the window'], ['compact', 'Compact', 'Tabs and a mini player, as on a phone'], ['comfortable', 'Comfortable', 'A rail, as on a tablet']];
  const layoutGroup = (group, row) => group('Layout', row('Arrangement', 'Kept on this device.', `<div class="chips layoutswitch" role="radiogroup" aria-label="Layout">${LAYOUTS.map(([v, l, d]) => `<button class="chip scope ${layoutPref() === v ? 'on' : ''}" role="radio" aria-checked="${layoutPref() === v}" data-layout-pick="${v}" title="${d}">${l}</button>`).join('')}</div>`)
    + row('Dense lists', 'More songs and covers on a tablet or computer screen.', `<input type="checkbox" class="switch" role="switch" id="densepick" ${densePref() ? 'checked' : ''} aria-label="Dense lists">`));
  const colourGroup = (group, row) => group('Colour', row('Colour from where it plays', 'The player takes a tint from the service: TIDAL, Qobuz, Spotify…; your own files keep ours.', `<input type="checkbox" class="switch" role="switch" data-colour-pick="pm.srcColour" ${colourOn('pm.srcColour') ? 'checked' : ''} aria-label="Colour from where it plays">`)
    + row('Colour from the cover', 'Now playing takes a soft gradient from the album art.', `<input type="checkbox" class="switch" role="switch" data-colour-pick="pm.artColour" ${colourOn('pm.artColour') ? 'checked' : ''} aria-label="Colour from the cover">`));
  const bindLayoutPick = () => {
    $$('[data-colour-pick]', main).forEach((c) => c.onchange = () => {
      try { if (c.checked) localStorage.removeItem(c.dataset.colourPick); else localStorage.setItem(c.dataset.colourPick, 'off'); } catch { /* no storage */ }
      const z = currentZone(); lastSrc = undefined; setSourceColour(z && z.now_playing); setArtTint();
    });
    $$('[data-layout-pick]', main).forEach((b) => b.onclick = () => {
      try { if (b.dataset.layoutPick === 'auto') localStorage.removeItem('pm.layout'); else localStorage.setItem('pm.layout', b.dataset.layoutPick); } catch { /* no storage */ }
      $$('[data-layout-pick]', main).forEach((x) => { const on = x === b; x.classList.toggle('on', on); x.setAttribute('aria-checked', String(on)); });
      applyShell();
    });
    const dp = $('#densepick', main);
    if (dp) dp.onchange = () => { try { if (dp.checked) localStorage.setItem('pm.dense', '1'); else localStorage.removeItem('pm.dense'); } catch { /* no storage */ } applyShell(); };
  };
  // The library's sections as a row of chips above the page title; shown on phones and tablets (desktop has the sidebar)
  const LIB_TOP = [['library', 'Your library'], ['albums', 'Albums'], ['artists', 'Artists'], ['tracks', 'Songs'], ['playlists', 'Playlists'], ['favourites', 'Favourites'], ['genres', 'Genres'], ['radio', 'Radio'], ['sources', 'Sources']];
  const libNav = () => {
    if (!LIB_TOP.some(([k]) => k === state.view)) return '';
    const off = new Set((state.sources || []).filter((x) => x.enabled === false).map((x) => x.id));
    const items = LIB_TOP.filter(([k]) => !(isListener() && LISTENER_VIEWS_OFF.has(k)) && !off.has(k));
    return `<nav class="libnav chips" aria-label="Library">${items.map(([k, l]) => `<button class="chip ${k === state.view ? 'on' : ''}" data-libnav="${k}" ${k === state.view ? 'aria-current="page"' : ''}>${l}</button>`).join('')}</nav>`;
  };
  const pageHead = (title, count, actions = '') => libNav() + `<div class="page-head"><h2>${esc(title)}${count != null ? `<span class="count">${esc(count)}</span>` : ''}</h2><div class="actions">${actions}</div></div>`;
  // One way back (08-item-page.md): history, labelled with the previous page; on a deep link the page's section.
  const backBtn = (view, label) => `<button class="back" data-back="${view}">${icon('back')} ${esc((navDepth > 0 && history.state && history.state.backLabel) || label)}</button>`;
  const loading = () => { main.innerHTML = `<div class="skeleton" style="height:28px;width:220px"></div><div class="sk-grid">${'<div class="skeleton"></div>'.repeat(8)}</div>`; };

  const loadFavourites = async () => { const f = await api('GET', '/favourites'); state.favUris = new Set(f.uris || []); return f; };
  // A song not identified yet is a favourite on this node only until "Which song is this?"
  const setFavourite = async (ref, on) => {
    let note = '';
    if (on) {
      const r = await api('PUT', '/favourites', ref); const k = ((r && r.uri) || '').split(':')[0];
      if (isOffline()) note = ' (sent to your node when it is back)';
      else if (k === 'track') note = ' (kept on this node until you pick which song it is: the ? on its row)';
      else if (k === 'album' || k === 'artist') note = ` (kept on this node until MusicBrainz knows this ${k})`;
    } else await api('DELETE', '/favourites?' + new URLSearchParams(Object.entries(ref).map(([k, v]) => [k, String(v)])).toString());
    await loadFavourites();
    toast(on ? 'Added to favourites' + note : 'Removed from favourites');
  };
  // Whether the node holds a Pubky session (menus offer homeserver uploads then).
  // "Share what I'm listening to" (listening together): the player bar's Live switch, the owner's,
  // shown once signed in with Pubky; off by default.
  const wireLive = async () => {
    const b = $('#livebtn'); if (!b) return;
    if (!(await pubkyUser())) { b.hidden = true; return; }
    const show = (v) => { b.hidden = false; b.setAttribute('aria-pressed', String(!!v.share)); b.classList.toggle('on', !!v.share); };
    state.liveShow = show;
    try { show(await api('GET', '/me/listening')); } catch { b.hidden = true; return; }
    b.onclick = async () => {
      const on = b.getAttribute('aria-pressed') !== 'true';
      b.disabled = true;
      // On an error the switch shows what the node has now, not what was asked
      try { show(await api('PUT', '/me/listening', { share: on })); toast(on ? 'Live: what you play here is a public post on your homeserver' : 'Live is off: your session post ended'); } catch (e) { toast(e.message, 'error'); try { show(await api('GET', '/me/listening')); } catch { /* keep it */ } } finally { b.disabled = false; }
    };
  };
  const pubkyUser = async () => { if (isListener()) return null; if (state.pubkyUser === undefined) { try { const pk = await api('GET', '/pubky'); state.pubkyUser = pk.user || null; state.pubkyStatus = pk; } catch { state.pubkyUser = null; } } return state.pubkyUser; };
  // A session approved for fewer folders than the node uses: one plain line with the way out.
  const RESIGN_TEXT = 'This node needs permission for new folders on your homeserver. Sign in again with Pubky Ring to approve them.';
  const resignNotice = (pk = state.pubkyStatus) => pk && pk.needs_resign ? `<div class="notice warn resign" role="status"><span>${esc(pk.notice || RESIGN_TEXT)}</span><button class="btn sm primary" data-resign>Sign in again</button></div>` : '';
  const bindResign = (root) => $$('[data-resign]', root).forEach((b) => b.onclick = () => { state.autoResign = true; setView('settings', 'accounts'); });
  // A sync's answer as one plain line: the re-sign notice instead of a raw 403.
  const syncToast = (r) => { const resign = (r.errors || []).find((e) => e === RESIGN_TEXT); if (resign) { state.pubkyUser = undefined; return toast(RESIGN_TEXT, 'error'); } toast(`Saved: ${r.pushed} sent to your homeserver, ${r.pulled} received${r.errors.length ? `, ${r.errors.length} errors` : ''}${r.pending ? `; ${r.pending} more going out in the background` : ''}${(r.notes || []).length ? `. ${r.notes.join(' ')}` : ''}`); };
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
  // Long lists draw a screenful at a time and add more as their end scrolls into view, so a
  // library of 50,000 tracks opens as fast as a small one. `more()` appends the next part and
  // says whether anything is left (it may fetch). Returns a function that stops the growing.
  const growList = (box, more) => {
    const sentinel = document.createElement('div'); sentinel.className = 'grow-more'; sentinel.setAttribute('aria-hidden', 'true');
    box.after(sentinel);
    let busy = false; let done = false;
    const io = new IntersectionObserver(async (es) => {
      if (busy || done || !es.some((e) => e.isIntersecting)) return;
      busy = true;
      try { done = !(await more()); } catch (e) { done = true; toast(e.message, 'error'); }
      busy = false;
      if (done) { io.disconnect(); sentinel.remove(); } else { io.unobserve(sentinel); io.observe(sentinel); }
    }, { rootMargin: '1200px 0px' });
    io.observe(sentinel);
    return () => { done = true; io.disconnect(); sentinel.remove(); };
  };
  // The same over items already in memory: `html(slice, start)` draws a part; `bind()` wires it.
  const growItems = (box, items, html, bind, step = 120) => {
    let n = 0;
    const more = () => { const part = items.slice(n, n + step); if (part.length) { box.insertAdjacentHTML('beforeend', html(part, n)); n += part.length; if (bind) bind(); } return n < items.length; };
    more();
    return n < items.length ? growList(box, more) : () => {};
  };
  // A client-side filter box for long lists: hides rows/tiles whose text does not contain the query.
  // Library list controls (UX review #7): sort, a favourites toggle and an A–Z jump in one row,
  // remembered per list in this browser.
  const LIST_SORTS = {
    albums: [['name', 'Name'], ['artist', 'Artist'], ['year', 'Year'], ['added', 'Recently added']],
    artists: [['name', 'Name'], ['albums', 'Most albums'], ['added', 'Recently added']],
  };
  const listPref = (kind) => { try { return JSON.parse(localStorage.getItem(`pm.list.${kind}`) || '{}'); } catch { return {}; } };
  const setListPref = (kind, p) => { try { localStorage.setItem(`pm.list.${kind}`, JSON.stringify(p)); } catch { /* no storage */ } };
  const albumFav = (a) => !!(a.release_mbid && state.favUris && state.favUris.has(`https://musicbrainz.org/release/${a.release_mbid}`));
  const sortList = (kind, list, how) => {
    const by = (f) => [...list].sort((a, b) => String(f(a) || '').localeCompare(String(f(b) || ''), undefined, { sensitivity: 'base', numeric: true }));
    if (kind === 'albums') {
      if (how === 'artist') return by((a) => `${a.album_artist || ''}\u0000${a.title}`);
      if (how === 'year') return [...list].sort((a, b) => (b.year || 0) - (a.year || 0));
      if (how === 'added') return [...list].sort((a, b) => (b.id || 0) - (a.id || 0));
      return by((a) => a.title);
    }
    if (how === 'albums') return [...list].sort((a, b) => (b.albums || 0) - (a.albums || 0));
    if (how === 'added') return [...list].sort((a, b) => (b.id || 0) - (a.id || 0));
    return by((a) => a.name);
  };
  const firstLetter = (x) => { const c = String(x || '').trim().charAt(0).toUpperCase().normalize('NFKD').charAt(0); return /[A-Z]/.test(c) ? c : '#'; };
  const listControls = (kind, pref, letters) => `<div class="listbar"><label class="small muted">Sort <select class="input sm" data-lsort aria-label="Sort">${LIST_SORTS[kind].map(([k, l]) => `<option value="${k}" ${pref.sort === k ? 'selected' : ''}>${l}</option>`).join('')}</select></label>${kind === 'albums' ? `<button class="chip ${pref.fav ? 'on' : ''}" data-lfav aria-pressed="${!!pref.fav}">${icon(pref.fav ? 'heart-fill' : 'heart')} Favourites</button>` : ''}${letters.length > 1 ? `<span class="azbar" role="group" aria-label="Jump to a letter">${letters.map((l) => `<button class="az" data-az="${l}">${l}</button>`).join('')}</span>` : ''}</div>`;
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

  // ---- first run (steward next-3, item 2) ------------------------------------
  // Home's "Get started" card: only the steps that matter on this node and to this person
  // (GET /v1/me/first-steps): add music, play something, the contact Discover needs, speakers and
  // Pubky sign-in. It goes once every step that isn't optional is done, or when hidden: by the
  // owner for everyone (kept on the node; Settings › Home for everyone shows it again), by a
  // listener in their browser.
  const setupHiddenHere = () => { try { return localStorage.getItem('pm.setupHidden') === '1'; } catch { return false; } };
  const renderSetup = async () => {
    const box = $('#h-setup'); if (!box) return;
    const owner = isOwner();
    const [info, fs, user, bundled] = await Promise.all([
      api('GET', '/info'), api('GET', '/me/first-steps'), owner ? pubkyUser() : null,
      owner ? api('GET', '/homeserver/bundled').catch(() => null) : null,
    ]).catch(() => [null]);
    if (!info || !fs || state.view !== 'home' || !box.isConnected) return;
    state.info = info;
    // Hidden in this browser before the node kept it: the owner's choice moves to the node
    if (owner && setupHiddenHere() && !fs.hidden) { try { localStorage.removeItem('pm.setupHidden'); } catch { /* no storage */ } api('PUT', '/node/settings', { first_steps_hidden: true }).catch(() => {}); fs.show = false; }
    state.setupShown = fs.show && (owner || !setupHiddenHere());
    if (!state.setupShown) { box.innerHTML = ''; return; }
    // A folder added from the card makes its step needless; it stays, ticked, until the page reloads
    if (fs.steps.some((x) => x.id === 'music')) state.setupMusic = true;
    else if (state.setupMusic && owner) fs.steps.unshift({ id: 'music', done: info.library.tracks > 0, optional: false });
    const hsHere = !!(bundled && bundled.configured && bundled.reachable);
    const tracks = info.library.tracks > 0;
    const has = Object.fromEntries(fs.steps.map((x) => [x.id, x.done]));
    const musicTodo = fs.steps.some((x) => x.id === 'music' && !x.done);
    let suggestions = [];
    if (!state.sources) { try { state.sources = await api('GET', '/sources'); } catch { state.sources = []; } }
    if (musicTodo && !info.scanning) suggestions = await api('GET', '/library/folders/suggest').catch(() => []);
    if (state.view !== 'home' || !box.isConnected) return;
    const n = (x) => Number(x).toLocaleString();
    const musicBody = info.scanning
      ? `<p class="small" role="status">Reading your files… ${countOf(n(info.library.tracks), 'tracks')} so far.</p>`
      : `${suggestions.length ? `<div class="setup-suggest">${suggestions.map((f) => `<button class="btn sm" data-addfolder="${esc(f.path)}">${icon('plus')} ${esc(f.path)} <span class="muted">${f.more ? 'over ' : ''}${n(f.audio_files)} audio files</span></button>`).join('')}</div>` : ''}
        <div class="toolbar"><input class="input" id="setupfolder" placeholder="${suggestions.length ? 'Or another folder' : 'The folder with your music, e.g. /home/you/Music'}" aria-label="Music folder on this computer"><button class="btn primary" id="setupadd">Add folder</button></div>
        <p class="muted small">No music files on this computer? <a class="lnk" href="/sources" data-setup-sources>Play from ${(state.sources || []).some((x) => x.id === 'jamendo' && x.status === 'ready') ? 'Internet Archive, Audius or Jamendo' : 'Internet Archive or Audius'}</a>, no account needed.</p>`;
    const text = {
      music: () => ['Add your music', has.music ? `${countOf(n(info.library.tracks), 'tracks')}${info.library.albums ? ` · ${countOf(n(info.library.albums), 'albums')}` : ''}. More folders: Your library › Node library.` : 'Pick the folder with your music files. The node reads it and keeps watching it for new files; your files stay as they are.', musicBody],
      play: () => ['Play something', tracks ? 'It plays in this browser. To play on speakers or another device, use the zone button at the bottom right.' : musicTodo ? 'Once your music is in.' : 'Search for a song, or browse your services on Sources. It plays in this browser, or on a speaker from the zone button.', tracks ? `<button class="btn sm primary" id="setupplay">${icon('play')} Shuffle my music</button>` : musicTodo ? '' : '<button class="btn sm" data-setup-sources>Open Sources</button>'],
      contact: () => ['See what people on Pubky play', has.contact ? 'Trending songs, tags and friends\' picks show on Home.' : 'The music index (Pinakes), MusicBrainz and Wikipedia ask each app for a way to reach whoever runs it: an email or a website. It goes only to them.', '<div class="toolbar"><input class="input" id="setupcontact" placeholder="you@example.org or https://your.site" aria-label="Your email or website"><button class="btn" id="setupcontactsave">Save</button></div>'],
      speakers: () => ['Play on your speakers (optional)', 'No speaker found yet. Speakers and streamers on your network show up by themselves; Snapcast and MPD need a line in the node’s config.', `<button class="btn sm" id="setupspeakers">Look for speakers</button> <a class="lnk small" href="https://github.com/gillohner/pubky-music-node/blob/main/docs/guides/speakers.md" target="_blank" rel="noopener">Which speakers work</a>`],
      pubky: () => (hsHere && !has.pubky
        ? ['Create your Pubky account here (optional)', 'This node has its own Pubky homeserver. Create your account on it with the Pubky Ring app: your playlists, favourites, tags and files stay on this machine. Or sign in to a homeserver you already use.', '<button class="btn sm primary" id="setupcreate">Create account here</button> <button class="btn sm" id="setupsignin">Use an existing homeserver</button>']
        : ['Keep your playlists on Pubky (optional)', has.pubky || user ? 'Signed in: playlists, favourites and tags are kept on your homeserver.' : 'Sign in with the Pubky Ring app: your playlists, favourites and tags are kept in your own Pubky storage and follow you to another node.', '<button class="btn sm" id="setupsignin">Sign in with Pubky Ring</button>']),
    };
    const step = (x, no) => { const [title, sub, body] = text[x.id](); return `<li class="setup-step ${x.done ? 'done' : ''}" data-step="${x.id}"><span class="stepmark" aria-hidden="true">${x.done ? icon('check') : no}</span><div class="grow"><div class="name">${title}${x.done ? '<span class="sr-only"> (done)</span>' : ''}</div><div class="sub">${sub}</div>${x.done ? '' : body}</div></li>`; };
    const done = fs.steps.filter((x) => x.done).length;
    box.innerHTML = `<section class="section fadein" aria-labelledby="setuph"><div class="card setup-card"><div class="section-head"><h3 id="setuph">Get started</h3><span class="muted small">${done} of ${fs.steps.length} done</span><button class="link" id="setuphide">Hide these steps</button></div>${musicTodo ? '<p class="muted small">Moving from another node? <a class="lnk" href="/settings" id="setuprestore">Restore it from a backup</a> instead.</p>' : ''}<ol class="setup-steps">`
      + fs.steps.filter((x) => text[x.id]).map((x, i) => step(x, i + 1)).join('')
      + '</ol></div></section>';
    const again = () => setTimeout(() => { if (state.view === 'home') renderSetup(); }, 1500);
    const addFolder = async (path) => {
      if (!path) return;
      try { const r = await api('POST', '/library/folders', { path }); toast(r.scan_started ? 'Folder added: reading your files' : 'Folder added'); watchScan({ scanning: true }); renderSetup(); } catch (e) { toast(e.message, 'error'); }
    };
    $$('[data-addfolder]', box).forEach((b) => b.onclick = () => addFolder(b.dataset.addfolder));
    const fi = $('#setupfolder'); if (fi) { $('#setupadd').onclick = () => addFolder(fi.value.trim()); fi.onkeydown = (e) => { if (e.key === 'Enter') addFolder(fi.value.trim()); }; }
    $$('[data-setup-sources]', box).forEach((so) => so.onclick = (e) => { e.preventDefault(); setView('sources'); });
    // While the files are read the card counts them; when done, Home shows the albums once
    if (info.scanning) { state.setupScanning = true; again(); } else if (state.setupScanning) { state.setupScanning = false; render(); }
    const pb = $('#setupplay'); if (pb) pb.onclick = async () => {
      // Up to 200 tracks in a random order (albums or not: untagged files have none)
      try { const r = await api('GET', '/library/tracks?limit=200'); const t = r.tracks.slice(); for (let i = t.length - 1; i > 0; i--) { const j = Math.floor(Math.random() * (i + 1)); [t[i], t[j]] = [t[j], t[i]]; } await playTracks(t, true); again(); } catch (e) { toast(e.message, 'error'); }
    };
    const ci = $('#setupcontact'); if (ci) {
      const save = async () => { const c = ci.value.trim(); if (!c) return; try { await api('PUT', '/node/settings', { contact: c }); state.discoveryOff = false; homeCache.clear(); toast('Saved: Discover is on'); render(); } catch (e) { toast(e.message, 'error'); } };
      $('#setupcontactsave').onclick = save; ci.onkeydown = (e) => { if (e.key === 'Enter') save(); };
    }
    const sp = $('#setupspeakers'); if (sp) sp.onclick = () => setView('settings', 'playback');
    const sr = $('#setuprestore'); if (sr) sr.onclick = (e) => { e.preventDefault(); state.focusRestore = true; setView('settings', 'backup'); };
    const si = $('#setupsignin'); if (si) si.onclick = () => { state.autoResign = true; setView('settings', 'accounts'); };
    const sc = $('#setupcreate'); if (sc) sc.onclick = () => { state.autoCreate = true; setView('settings', 'accounts'); };
    $('#setuphide').onclick = async () => {
      if (owner) {
        try { await api('PUT', '/node/settings', { first_steps_hidden: true }); toast('Hidden for everyone on this node. Settings › Home for everyone shows them again.'); } catch (e) { toast(e.message, 'error'); return; }
      } else { try { localStorage.setItem('pm.setupHidden', '1'); } catch { /* still hidden for now */ } }
      state.setupShown = false; box.innerHTML = '';
    };
  };

  // ---- render --------------------------------------------------------------
  const render = async () => {
    // The search box is a combobox over the results page only
    if (state.view !== 'search') { search.setAttribute('aria-expanded', 'false'); search.removeAttribute('aria-controls'); search.removeAttribute('aria-activedescendant'); }
    const v = state.view;
    // The dashboard is a full screen of its own (a TV or tablet): no sidebar, bars or player
    document.body.classList.toggle('kiosk', v === 'dashboard');
    endSelect();
    const lists = {};
    try {
      switch (v) {
        case 'home': {
          if (!state.sources) { try { state.sources = await api('GET', '/sources'); } catch { state.sources = []; } }
          const win = state.homeWindow || '7d';
          const sec = (name, title, opts = {}) => `<section class="section" id="h-${name}"><div class="section-head"><h3>${title}</h3>${opts.windows ? `<span class="chips win">${WINDOWS.map(([w, l]) => `<button class="chip ${w === win ? 'on' : ''}" data-win="${w}">${l}</button>`).join('')}</span>` : ''}<button class="link" data-seeall="${name}">See all</button></div><div class="hbody"><div class="skeleton" style="height:${opts.h || 160}px"></div></div></section>`;
          await pubkyUser();
          // Rows you can hide and reorder (UX review #10): your own layout (Settings › You › Home), over the node's
          const HOME_ROWS = [['listening', 'Listening now'], ['played', 'Recently played'], ['added', 'Recently added'], ['forgotten', 'Forgotten favourites'], ['lb', 'Recommended for you'], ['trending-songs', 'Trending songs', { windows: true, h: 300 }], ['trending-albums', 'Trending albums', { windows: true }], ['trending-artists', 'Trending artists', { windows: true }], ['following', 'From people you follow', { h: 120 }], ['tags', 'Tags people use for music', { h: 60 }], ['concerts', 'Concerts near you', { h: 80 }], ['podcasts', 'Podcasts'], ['yours', 'Yours on Pubky', { h: 120 }], ['new', 'New on Pubky', { h: 120 }]];
          // Your Home (Settings › You): your own layout where you set one, else the node's default
          if (!state.homeLayout) {
            const lay = await api('GET', '/me/prefs').then((p) => (p.home_layout && p.home_layout.value) || { order: [], hidden: [] }).catch(() => api('GET', '/home/layout').catch(() => ({ order: [], hidden: [] })));
            if (state.view !== 'home') return; // they went elsewhere while the homeserver answered
            state.homeLayout = lay; state.homeSaved = lay;
          }
          const lay = state.homeLayout; const known = HOME_ROWS.map(([k]) => k);
          const ordered = [...(lay.order || []).filter((k) => known.includes(k)), ...known.filter((k) => !(lay.order || []).includes(k))];
          const showRow = (k) => !(lay.hidden || []).includes(k);
          const rowOf = (k) => HOME_ROWS.find(([x]) => x === k);
          const localSec = (k, title) => `<section class="section" id="h-${k}"><div class="section-head"><h3>${title}</h3></div><div class="hbody"><div class="skeleton" style="height:150px"></div></div></section>`;
          const rowHtml = (k) => { const [, title, opts] = rowOf(k); if (k === 'lb') return '<div id="h-lbwrap"></div>'; if (k === 'listening') return '<div id="h-listenwrap"></div>'; if (['played', 'added', 'forgotten'].includes(k)) return localSec(k, title); return sec(k, title, opts || {}); };
          const editPanel = state.homeEdit ? `<section class="section homeedit" id="h-edit"><div class="section-head"><h3>Rows on Home</h3><button class="btn sm primary" id="hdone">Done</button></div><div class="list compact">${ordered.map((k, i) => `<div class="list-row"><label class="grow"><input type="checkbox" data-hshow="${k}" ${showRow(k) ? 'checked' : ''}> ${esc(rowOf(k)[1])}</label><button class="icon-btn" data-hup="${i}" ${i === 0 ? 'disabled' : ''} aria-label="Move ${esc(rowOf(k)[1])} up">${icon('chevron', 'ic up')}</button><button class="icon-btn" data-hdown="${i}" ${i === ordered.length - 1 ? 'disabled' : ''} aria-label="Move ${esc(rowOf(k)[1])} down">${icon('chevron')}</button></div>`).join('')}</div></section>` : '';
          main.innerHTML = pageHead('Home', null, `<button class="btn ghost sm" id="hedit" aria-pressed="${!!state.homeEdit}">${icon('settings')} Edit Home</button>`) + editPanel + resignNotice() + '<div id="h-setup"></div>'
            + ordered.filter(showRow).map(rowHtml).join('');
          // Saves go one at a time and the newest layout is the one that ends up saved: while one is
          // on its way, a later change waits and is sent when it lands (each save takes seconds)
          const saveLayout = (next) => {
            if (!state.homeSaved) state.homeSaved = state.homeLayout;
            state.homeLayout = next; render();
            state.homePending = next;
            if (state.homeSaving) return;
            state.homeSaving = (async () => {
              while (state.homePending) {
                const send = state.homePending; state.homePending = null;
                try {
                  const p = await api('PUT', '/me/prefs', { home_layout: send });
                  state.homeSaved = (p.home_layout && p.home_layout.value) || send;
                } catch (e) {
                  toast(e.message, 'error');
                  if (!state.homePending) { state.homeLayout = state.homeSaved; if (state.view === 'home') render(); }
                }
              }
              state.homeSaving = null;
            })();
          };
          $('#hedit').onclick = () => { state.homeEdit = !state.homeEdit; render(); };
          // Listening now: what is known at once, then the node's latest
          renderListening(); refreshAlong();
          const hd = $('#hdone'); if (hd) hd.onclick = () => { state.homeEdit = false; render(); };
          $$('[data-hshow]', main).forEach((c) => c.onchange = () => { const hidden = new Set(lay.hidden || []); if (c.checked) hidden.delete(c.dataset.hshow); else hidden.add(c.dataset.hshow); saveLayout({ order: ordered, hidden: [...hidden] }); });
          const move = (i, d) => { const o = [...ordered]; const [x] = o.splice(i, 1); o.splice(i + d, 0, x); saveLayout({ order: o, hidden: lay.hidden || [] }); };
          $$('[data-hup]', main).forEach((b) => b.onclick = () => move(Number(b.dataset.hup), -1));
          $$('[data-hdown]', main).forEach((b) => b.onclick = () => move(Number(b.dataset.hdown), 1));
          $$('[data-win]', main).forEach((b) => b.onclick = () => { state.homeWindow = b.dataset.win; render(); });
          $$('[data-seeall]', main).forEach((b) => b.onclick = () => setView('home-section', { section: b.dataset.seeall, window: state.homeWindow || '7d' }));
          renderSetup();
          // Local rows first: recently played, recently added, forgotten favourites
          api('GET', '/library/home?limit=12').then((h) => {
            if (state.view !== 'home') return;
            const albumRow = (k, list, emptyText) => {
              const el = $(`#h-${k} .hbody`); if (!el) return;
              const albums = (list || []).filter((a) => a.id != null).slice(0, 12);
              if (!albums.length && state.setupShown) { el.parentElement.remove(); return; }
              el.innerHTML = albums.length ? itemTileGrid(albums.map((a) => ({ id: 'lib-' + a.id, title: a.title, image: `/v1/library/albums/${a.id}/cover`, year: a.album_artist || a.year || '' })), 'album') : `<p class="muted small">${emptyText}</p>`;
              el.parentElement.classList.add('fadein');
              if (albums.length) bindItemTiles(el, albums.map((a) => ({ id: 'lib-' + a.id, title: a.title })), 'album');
            };
            albumRow('played', h.recently_played, 'Albums you play show up here.');
            albumRow('added', h.recently_added, 'New music in your folders shows up here.');
            const fe = $('#h-forgotten .hbody');
            if (fe) {
              const ts = h.forgotten || [];
              if (!ts.length) { fe.parentElement.remove(); } else {
                const songs = ts.map((t, k) => ({ no: k + 1, title: t.title, artist: artists(t), album: t.album && t.album.title, album_id: t.album && t.album.id != null ? 'lib-' + t.album.id : null, duration_ms: t.duration_ms, track_id: t.id, id: trackPageId(t), image: t.album && t.album.id != null ? `/v1/library/albums/${t.album.id}/cover` : null }));
                fe.innerHTML = songRows(songs, { numbers: 'index' }); fe.parentElement.classList.add('fadein'); bindSongRows(fe, songs, {});
              }
            }
          }).catch(() => { for (const k of ['played', 'added', 'forgotten']) { const el = $(`#h-${k} .hbody`); if (el) el.innerHTML = '<p class="muted small">The library did not answer.</p>'; } });
          // ListenBrainz's picks for the user, when connected (Settings › ListenBrainz).
          api('GET', '/listenbrainz').then(async (lb) => {
            if (!lb.connected || state.view !== 'home') return;
            const recs = await api('GET', '/listenbrainz/recommendations?limit=10');
            const wrap = $('#h-lbwrap'); if (!wrap || state.view !== 'home' || !recs.length) return;
            const songs = recs.map((x, k) => ({ no: k + 1, title: itemName(x), artist: x.artist, image: x.image, page: x.page || x.uri, uri: x.playable || null, id: idOf(x.uri) }));
            wrap.innerHTML = `<section class="section fadein" id="h-lb"><div class="section-head"><h3>Recommended for you</h3><span class="muted small">from your ListenBrainz listens</span></div>${songRows(songs, { numbers: 'index' })}</section>`;
            bindSongRows($('#h-lb'), songs, {});
          }).catch(() => { /* not connected or not answering: no section */ });
          // Every Pubky section in parallel, each rendered as it arrives.
          for (const name of ['trending-songs', 'trending-albums', 'trending-artists', 'following', 'tags', 'concerts', 'podcasts', 'yours', 'new'].filter(showRow)) {
            loadHomeSection(name, name.startsWith('trending') ? win : null).then((sc) => {
              const el = $(`#h-${name} .hbody`); if (!el || state.view !== 'home') return;
              // Without a contact Pubky has nothing to show yet: Get started says how, once
              if (sc.needs === 'contact') { el.parentElement.remove(); return; }
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
          main.innerHTML = pageHead('Favourites', `${countOf(f.albums.length, 'albums')} · ${countOf(f.tracks.length, 'tracks')} · ${countOf(f.artists.length, 'artists')}`, f.tracks.length ? `<button class="btn primary" id="favplay">${icon('play')} Play tracks</button>` : '')
            + (none ? emptyState('heart', 'Nothing hearted yet', 'Press the heart on a song, album or artist to keep it here. Signed in with Pubky, your favourites are also kept in your own Pubky storage.') : '')
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
          const lp = listPref('albums');
          const sorted = sortList('albums', albums.filter((a) => inFocus(a, state.focus) && (!lp.fav || albumFav(a))), lp.sort || 'name');
          const shown = sorted;
          const letters = (lp.sort || 'name') === 'name' && shown.length > 40 ? [...new Set(shown.map((a) => firstLetter(a.title)))] : (lp.sort === 'artist' && shown.length > 40 ? [...new Set(shown.map((a) => firstLetter(a.album_artist)))] : []);
          main.innerHTML = pageHead('Albums', state.focus || lp.fav ? `${shown.length} of ${albums.length}` : albums.length, (albums.length > 12 ? filterBox('Filter albums') : '') + (isOwner() ? `<button class="btn" data-addmusic>${icon('plus')} Add music</button>` : '')) + (albums.length ? focusChips(albums) + listControls('albums', lp, letters) : '') + (shown.length ? '<div class="grid" id="albumgrid"></div>' : albums.length ? emptyState('disc', 'Nothing in this focus', 'No album has a file of that quality.') : emptyState('disc', 'No albums yet', 'Add the folder with your music in Settings (Library folders), or follow Get started on Home.', { view: 'settings', label: 'Open settings' }));
          $$('[data-focus]', main).forEach((b) => b.onclick = () => { state.focus = b.dataset.focus; render(); });
          const ls = $('[data-lsort]', main); if (ls) ls.onchange = () => { setListPref('albums', { ...lp, sort: ls.value }); render(); };
          const lf = $('[data-lfav]', main); if (lf) lf.onclick = () => { setListPref('albums', { ...lp, fav: !lp.fav }); render(); };
          const grid = $('#albumgrid');
          if (grid) {
            let stop = () => {};
            const draw = (list) => { stop(); grid.innerHTML = ''; stop = growItems(grid, list, (part) => part.map(tile).join(''), bindTiles); };
            draw(shown);
            // A–Z: the list from that letter on (the grid draws as you scroll)
            $$('[data-az]', main).forEach((b) => b.onclick = () => { const key = lp.sort === 'artist' ? (a) => firstLetter(a.album_artist) : (a) => firstLetter(a.title); const i = shown.findIndex((a) => key(a) === b.dataset.az); if (i >= 0) { draw(shown.slice(i)); $$('[data-az]', main).forEach((x) => x.classList.toggle('on', x === b)); } });
            const f = $('#filter');
            if (f) {
              f.oninput = () => { const q = f.value.trim().toLowerCase(); const list = q ? shown.filter((a) => `${a.title} ${a.album_artist || ''} ${a.year || ''}`.toLowerCase().includes(q)) : shown; draw(list); const c = $('.page-head .count'); if (c) c.dataset.filtered = q ? `${list.length} shown` : ''; };
              f.onkeydown = (e) => { if (e.key === 'Escape') { f.value = ''; f.oninput(); f.blur(); } };
            }
          }
          break;
        }
        case 'artists': {
          loading();
          state.artists = await api('GET', '/library/artists');
          if (state.view !== 'artists') return;
          const alp = listPref('artists');
          const artistsSorted = sortList('artists', state.artists, alp.sort || 'name');
          const aletters = (alp.sort || 'name') === 'name' && artistsSorted.length > 40 ? [...new Set(artistsSorted.map((a) => firstLetter(a.name)))] : [];
          main.innerHTML = pageHead('Artists', state.artists.length, state.artists.length > 12 ? filterBox('Filter artists') : '') + (state.artists.length ? listControls('artists', alp, aletters) + '<div class="list" id="artistlist"></div>' : emptyState('artist', 'No artists yet', 'Artists come from the tags of scanned files.'));
          const alist = $('#artistlist');
          if (alist) {
            const row = (a) => `<div class="list-row clickable" data-artist="${a.id}"><div class="avatar" style="background:hsl(${hue(a.name)} 35% 28%)">${esc(initials(a.name))}</div><div class="grow"><div class="name">${esc(a.name)}</div><div class="sub">${[a.albums ? countOf(a.albums, 'albums') : '', countOf(a.tracks, 'tracks')].filter(Boolean).join(' · ')}</div></div>${icon('chevron', 'ic chev')}</div>`;
            const bindRows = () => $$('[data-artist]', alist).forEach((el) => el.onclick = () => setView('artist', Number(el.dataset.artist)));
            let stop = () => {};
            const draw = (list) => { stop(); alist.innerHTML = ''; stop = growItems(alist, list, (part) => part.map(row).join(''), bindRows, 150); };
            draw(artistsSorted);
            $$('[data-az]', main).forEach((b) => b.onclick = () => { const i = artistsSorted.findIndex((a) => firstLetter(a.name) === b.dataset.az); if (i >= 0) { draw(artistsSorted.slice(i)); $$('[data-az]', main).forEach((x) => x.classList.toggle('on', x === b)); } });
            const als = $('[data-lsort]', main); if (als) als.onchange = () => { setListPref('artists', { ...alp, sort: als.value }); render(); };
            const f = $('#filter');
            if (f) {
              f.oninput = () => { const q = f.value.trim().toLowerCase(); const list = q ? artistsSorted.filter((a) => a.name.toLowerCase().includes(q)) : artistsSorted; draw(list); const c = $('.page-head .count'); if (c) c.dataset.filtered = q ? `${list.length} shown` : ''; };
              f.onkeydown = (e) => { if (e.key === 'Escape') { f.value = ''; f.oninput(); f.blur(); } };
            }
          }
          break;
        }
        case 'identify': {
          const sug = await api('GET', '/library/suggestions');
          if (state.view !== 'identify') return;
          const byTrack = new Map(); for (const x of sug) { if (!byTrack.has(x.track_id)) byTrack.set(x.track_id, { track: x.track, options: [] }); byTrack.get(x.track_id).options.push(x); }
          const byFolder = new Map(); for (const g of byTrack.values()) { const f = (g.track.file.path || '').replace(/[^/]*$/, ''); if (!byFolder.has(f)) byFolder.set(f, []); byFolder.get(f).push(g); }
          // The release most suggestions in a folder point at (the folder as an album).
          const releaseOf = (groups) => { const n = new Map(); groups.forEach((g) => g.options.forEach((o) => { if (o.release_mbid) n.set(o.release_mbid, (n.get(o.release_mbid) || 0) + 1); })); let best = null; n.forEach((c, r) => { if (!best || c > best[1]) best = [r, c]; }); return best && best[0]; };
          main.innerHTML = pageHead('Confirm matches', byTrack.size) + (byTrack.size ? `<p class="muted small" style="margin-top:-6px">Each file, with what MusicBrainz knows about it. A confirmed match becomes the file's identity (stored in the node, your files stay as they are).</p>` + [...byFolder.entries()].map(([folder, groups]) => `<section class="section" data-folder="${esc(folder)}"><div class="section-head"><h3>${esc(folder.split('/').filter(Boolean).slice(-2).join(' / ') || 'Library')}</h3><span class="muted small">${groups.length} file${groups.length === 1 ? '' : 's'}</span>${releaseOf(groups) ? `<button class="btn sm" data-align="${esc(folder)}">Compare with the album</button>` : ''}</div><div class="alignbox" hidden></div><div class="list">${groups.map((g) => `<div class="idrow"><div class="idfile"><div class="name">${g.track.track_no ? `<span class="muted">${g.track.track_no}.</span> ` : ''}${esc(g.track.title)}</div><div class="sub">${esc([artists(g.track), fmt(g.track.duration_ms), (g.track.file.path || '').split('/').pop()].filter(Boolean).join(' · '))}</div></div><div class="idopts">${g.options.map((o) => `<div class="idopt"><div class="grow"><div class="name">${esc(o.title || 'Recording')} <span class="muted small">${esc(o.artist || '')}</span></div><div class="sub">${o.release_title ? esc(o.release_title) + ' · ' : ''}${o.source === 'fingerprint' ? 'audio fingerprint' : o.source === 'album' ? 'the folder as an album' : 'search by tags'} · ${Math.round(o.score * 100)}% · <a href="https://musicbrainz.org/recording/${esc(o.recording_mbid)}" target="_blank" rel="noopener">MusicBrainz ${icon('external')}</a></div></div><button class="btn primary sm" data-accept="${o.id}">This one</button><button class="btn ghost sm" data-reject="${o.id}">Not this</button></div>`).join('')}</div></div>`).join('')}</div></section>`).join('') : emptyState('search', 'Nothing to confirm', 'Unsure matches from Identify library show up here.', { view: 'settings', label: 'Open Settings' }));
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
            box.innerHTML = `<div class="alignhead"><span></span><b>Your folder</b><span></span><b>${esc(r.title)} <span class="muted small">${esc((r['artist-credit'] || []).map((a) => a.name).join(', '))} · ${esc(r.date || '')} · ${countOf(tracks.length, 'tracks')}</span></b></div>${rows}<div class="actions" style="margin-top:10px">${matches.length ? `<button class="btn primary sm" data-acceptall="${matches.join(',')}">Accept all ${matches.length} matching</button>` : ''}<a class="btn ghost sm" href="https://musicbrainz.org/release/${esc(rel)}" target="_blank" rel="noopener">On MusicBrainz ${icon('external')}</a></div>`;
            const all = $('[data-acceptall]', box); if (all) all.onclick = async () => { all.disabled = true; let ok = 0; for (const id of all.dataset.acceptall.split(',')) { try { await api('POST', `/library/suggestions/${id}/accept`); ok++; } catch { /* next */ } } toast(`${ok} file${ok === 1 ? '' : 's'} identified`); render(); };
          });
          $$('[data-accept]', main).forEach((b) => b.onclick = async () => { try { await api('POST', `/library/suggestions/${b.dataset.accept}/accept`); toast('Identified'); render(); } catch (e) { toast(e.message, 'error'); } });
          $$('[data-reject]', main).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/library/suggestions/${b.dataset.reject}`); render(); } catch (e) { toast(e.message, 'error'); } });
          break;
        }
        case 'genres': {
          loading();
          // One tag idea: your Pubky tags and the genres in your files' metadata, side by side
          const [genres, mine, ns] = await Promise.all([api('GET', '/library/genres'), api('GET', '/tags').then((r) => r.tags || []).catch(() => []), api('GET', '/node/settings').catch(() => ({}))]);
          if (state.view !== 'genres') return;
          const byLabel = new Map(); for (const t of mine) byLabel.set(t.label, (byLabel.get(t.label) || 0) + 1);
          const tagsSec = `<section class="section"><div class="section-head"><h3>Your Pubky tags</h3><span class="muted small">public, on your homeserver</span></div>${byLabel.size ? `<div class="chips">${[...byLabel.entries()].sort((a, b) => b[1] - a[1]).map(([l, n]) => `<button class="chip" data-mytag="${esc(l)}">#${esc(l)} <span class="muted">${n}</span></button>`).join('')}</div>` : '<p class="muted small">No tags yet. Add them on any song, album or artist page.</p>'}<label class="small genre-pub"><input type="checkbox" id="genretags" ${ns.genre_tags ? 'checked' : ''}> Publish my files' genres as tags too (public on Pubky, on songs that are matched)</label></section>`;
          main.innerHTML = pageHead('Genres', genres.length) + tagsSec + `<section class="section"><div class="section-head"><h3>From your files</h3><span class="muted small">the genres in your files' metadata</span></div>` + (genres.length ? `<div class="cards">${genres.map((g) => `<button class="card genre" data-genre="${g.id}" style="text-align:left;cursor:pointer;background:linear-gradient(135deg, hsl(${hue(g.name)} 32% 22%), var(--surface))"><h3 style="margin-bottom:4px">${esc(g.name)}</h3><div class="muted small">${g.albums} album${g.albums === 1 ? '' : 's'} · ${g.tracks} track${g.tracks === 1 ? '' : 's'}</div></button>`).join('')}</div>` : emptyState('genre', 'No genres yet', 'Genres come from the files\' tags; tag your music and rescan.'));
          main.insertAdjacentHTML('beforeend', '</section>');
          $$('[data-genre]', main).forEach((b) => b.onclick = () => setView('genre', Number(b.dataset.genre)));
          $$('[data-mytag]', main).forEach((b) => b.onclick = () => setView('discover', { tag: b.dataset.mytag }));
          const gt = $('#genretags'); if (gt) gt.onchange = async () => { try { await api('PUT', '/node/settings', { genre_tags: gt.checked }); toast(gt.checked ? 'Your files\u2019 genres are now tags on matched songs; they go to Pubky with the next sync' : 'New genres won\u2019t be published; the tags already made stay (remove them on each song)'); render(); } catch (e) { toast(e.message, 'error'); gt.checked = !gt.checked; } };
          break;
        }
        case 'genre': {
          const d = await api('GET', `/library/genres/${state.viewArg}`);
          if (state.view !== 'genre') return;
          main.innerHTML = backBtn('genres', 'Genres') + `<div class="page-head"><div><div class="eyebrow">Genre</div><h1>${esc(d.genre.name)}</h1><div class="muted">${countOf(d.genre.albums, 'albums')} · ${countOf(d.genre.tracks, 'tracks')}</div></div><div class="actions"><button class="btn primary" id="gplay">${icon('play')} Play all</button></div></div>`
            + (d.albums.length ? `<section class="section"><div class="section-head"><h3>Albums</h3></div>${albumGrid(d.albums)}</section>` : '')
            + `<section class="section"><div class="section-head"><h3>Tracks</h3></div>${trackRows(d.tracks, { key: 'genre' })}</section>`;
          lists.genre = d.tracks;
          $('#gplay').onclick = () => playTracks(d.tracks, true);
          break;
        }
        case 'tracks': {
          loading();
          // Pages from the node as the list scrolls; the filter is a library search on the node.
          const PAGE = 300;
          const [first, stats] = await Promise.all([api('GET', `/library/tracks?limit=${PAGE}`), api('GET', '/library/stats').catch(() => null)]);
          if (state.view !== 'tracks') return;
          const total = Math.max(stats ? stats.tracks : 0, first.tracks.length);
          main.innerHTML = pageHead('Tracks', total, (total > 12 ? filterBox('Filter tracks') : '') + (total ? `<button class="btn" id="shuffle">Play all</button>` : '') + (isOwner() ? `<button class="btn" data-addmusic>${icon('plus')} Add music</button>` : '')) + (total ? '<div class="tracks" data-list="all" id="tracklist"></div><p class="muted small" id="tracknote" hidden></p>' : emptyState('notes', 'No tracks yet', 'Add the folder with your music in Settings (Library folders), or follow Get started on Home.', { view: 'settings', label: 'Open settings' }));
          const box = $('#tracklist');
          if (box) {
            const shown = []; lists.all = shown;
            let stop = () => {}; let run = 0;
            const reset = () => { stop(); stop = () => {}; box.innerHTML = ''; shown.length = 0; };
            const append = (rows) => { box.insertAdjacentHTML('beforeend', trackRows(rows, { key: 'all', start: shown.length, bare: true })); shown.push(...rows); };
            const browse = () => {
              reset(); append(first.tracks); $('#tracknote').hidden = true;
              if (first.tracks.length === PAGE) {
                const mine = run;
                stop = growList(box, async () => {
                  const r = await api('GET', `/library/tracks?limit=${PAGE}&after=${shown[shown.length - 1].id}`);
                  if (state.view !== 'tracks' || mine !== run) return false;
                  append(r.tracks); return r.tracks.length === PAGE;
                });
              }
            };
            browse();
            const f = $('#filter'); let timer = null;
            if (f) {
              f.oninput = () => {
                clearTimeout(timer);
                timer = setTimeout(async () => {
                  const q = f.value.trim(); const mine = ++run; const c = $('.page-head .count');
                  if (!q) { browse(); if (c) c.dataset.filtered = ''; return; }
                  const r = await api('GET', `/library/search?${new URLSearchParams({ q, sources: 0, limit: 500 })}`).catch((e) => { toast(e.message, 'error'); return null; });
                  if (!r || mine !== run || state.view !== 'tracks') return;
                  reset(); append(r.tracks);
                  const note = $('#tracknote'); note.hidden = r.tracks.length < 500; note.textContent = 'The first 500 matches: type more to narrow them down.';
                  if (c) c.dataset.filtered = `${r.tracks.length}${r.tracks.length === 500 ? '+' : ''} shown`;
                }, 200);
              };
              f.onkeydown = (e) => { if (e.key === 'Escape') { f.value = ''; f.oninput(); f.blur(); } };
            }
            // Play all: what is shown when filtered, else every track (fetched on the press).
            const sh = $('#shuffle');
            if (sh) sh.onclick = async () => {
              if ((f && f.value.trim()) || shown.length >= total) return playTracks(shown, true);
              sh.disabled = true; toast(`Loading ${countOf(total, 'tracks')}…`);
              try { playTracks(await pageAll('/library/tracks', (t) => t.id, (r) => r.tracks), true); } catch (e) { toast(e.message, 'error'); }
              sh.disabled = false;
            };
          }
          break;
        }
        case 'search': {
          const a = state.viewArg || {};
          if (!a.q) { main.innerHTML = emptyState('search', 'Search', 'Type in the box above: your library, your services and Pubky at once. Paste a link to open it.'); break; }
          const scope = a.in || 'everywhere'; state.searchScope = scope;
          if (search.value.trim() !== a.q) search.value = a.q;
          const run = startSearch(a.q, scope);
          main.innerHTML = `<div class="page-head"><h2>Results for “${esc(a.q)}”</h2></div><div class="scopechips" id="scopechips"></div><div id="sresults"></div><div id="sresults-said" class="sr-only" aria-live="polite"></div>`;
          search.setAttribute('aria-controls', 'sresults'); search.setAttribute('aria-expanded', 'true');
          renderSearchBody(run);
          break;
        }
        case 'dashboard': {
          // A castable now-playing screen (UX review, next tier): /now-playing?zone=<id>
          const zid = state.viewArg;
          if (zid && (zid === BROWSER || state.zones[zid])) state.zone = zid;
          main.innerHTML = '<div class="dash" id="dash" aria-live="polite"></div>';
          state.dashKey = null; drawDashboard();
          break;
        }
        case 'smart': {
          // A smart playlist: its songs right now
          loading();
          const sid = state.viewArg;
          let d; try { d = await api('GET', `/smart/${encodeURIComponent(sid)}/tracks`); } catch (e) { main.innerHTML = backBtn('playlists', 'Playlists') + `<div class="notice">${esc(e.message)}</div>`; break; }
          if (state.view !== 'smart' || state.viewArg !== sid) return;
          state.itemTitle = d.playlist.name;
          const ts = d.tracks || [];
          const songs = ts.map((t, k) => ({ no: k + 1, title: t.title, artist: artists(t), artist_id: t.artists && t.artists[0] && t.artists[0].id != null ? 'lib-' + t.artists[0].id : null, album: t.album && t.album.title, album_id: t.album && t.album.id != null ? 'lib-' + t.album.id : null, duration_ms: t.duration_ms, track_id: t.id, id: trackPageId(t) }));
          const covers = ts.map((t) => (t.album && t.album.id != null ? `/v1/library/albums/${t.album.id}/cover` : null));
          main.innerHTML = backBtn('playlists', 'Playlists') + `<div class="hero">${mosaic(covers, d.playlist.name)}<div><div class="eyebrow">Smart playlist · songs change as your library does</div><h1>${esc(d.playlist.name)}</h1><p class="sub">${countOf(ts.length, 'songs')}${d.matched > ts.length ? ` of ${d.matched} that match` : ''} · ${esc(smartSummary(d.playlist))}</p>
              <div class="actions" style="margin-top:12px"><button class="btn primary" id="smplay" ${ts.length ? '' : 'disabled'}>${icon('play')} Play</button><button class="btn" id="smshuffle" ${ts.length ? '' : 'disabled'}>${icon('shuffle')} Shuffle</button><button class="btn ghost" id="smedit">${icon('settings')} Edit rules</button></div></div></div>`
            + `<section class="section" id="smsongs"><div class="section-head"><h3>Songs</h3></div>${ts.length ? songRows(songs, { numbers: 'index' }) : '<p class="muted small">No song matches these rules right now.</p>'}</section>`;
          $('[data-back]', main).onclick = () => goBack('playlists');
          if (ts.length) { bindSongRows($('#smsongs'), songs, {}); enableSelect($('#smsongs'), songs); }
          $('#smplay').onclick = () => playTracks(ts, true);
          $('#smshuffle').onclick = () => playTracks([...ts].sort(() => Math.random() - 0.5), true);
          $('#smedit').onclick = () => setView('smart-edit', sid);
          break;
        }
        case 'smart-edit': {
          // The rules, with a live preview of what they pick
          const sid = state.viewArg;
          let cur = { name: '', rules: [{ field: 'genre', op: 'contains', value: '' }], matches: 'all', sort: 'random', limit: 100, seeds: [], minutes: null, exclude: {} };
          if (sid && sid !== 'new') { try { cur = (await api('GET', `/smart/${encodeURIComponent(sid)}/tracks`)).playlist; } catch (e) { toast(e.message, 'error'); } }
          if (state.view !== 'smart-edit') return;
          const draw = () => {
            main.innerHTML = backBtn('playlists', 'Playlists') + pageHead(sid === 'new' ? 'New smart playlist' : 'Edit smart playlist')
              + `<div class="smartedit"><label class="field"><span>Name</span><input class="input" id="smname" value="${esc(cur.name)}" placeholder="Late-night techno"></label>
                <div class="field"><span>Start from</span><div class="chips" id="smseeds">${(cur.seeds || []).map((sd, i) => `<span class="chip">${esc(sd.label || `${sd.kind} ${sd.id}`)} <button class="icon-btn" data-smseedrm="${i}" aria-label="Remove ${esc(sd.label || sd.kind)}" title="Remove">${icon('close')}</button></span>`).join('')}<input class="input sm" id="smseedq" placeholder="a song, artist, album or genre…" autocomplete="off" aria-label="Add a seed" style="min-width:220px"></div><div id="smseedhits" class="list"></div><p class="muted small">With seeds, the playlist grows from songs like these (same genres, then the same artists); the rules below then narrow it.</p></div>
                <p class="small">Songs that match <select class="input sm" id="smmatch" aria-label="Match"><option value="all" ${cur.matches !== 'any' ? 'selected' : ''}>all</option><option value="any" ${cur.matches === 'any' ? 'selected' : ''}>any</option></select> of these rules:</p>
                <div id="smrules">${cur.rules.map((r, i) => smartRuleRow(r, i)).join('')}</div>
                <button class="btn sm ghost" id="smadd">${icon('plus')} Add a rule</button>
                <div class="toolbar" style="margin-top:12px"><label class="small">Order <select class="input sm" id="smsort">${SMART_SORTS.map(([k, l]) => `<option value="${k}" ${cur.sort === k ? 'selected' : ''}>${l}</option>`).join('')}</select></label><label class="small">At most <input class="input sm" id="smlimit" type="number" min="1" max="1000" value="${cur.limit || 100}" style="width:90px"> songs</label><label class="small">or <input class="input sm" id="smminutes" type="number" min="0" max="6000" value="${cur.minutes || ''}" placeholder="any" style="width:90px"> minutes</label></div>
                <div class="toolbar"><label class="small">Leave out artists <input class="input sm" id="smexart" value="${esc(((cur.exclude || {}).artists || []).join(', '))}" placeholder="comma-separated" style="width:200px"></label><label class="small">genres <input class="input sm" id="smexgen" value="${esc(((cur.exclude || {}).genres || []).join(', '))}" placeholder="comma-separated" style="width:160px"></label><label class="small">songs played in the last <input class="input sm" id="smexdays" type="number" min="0" value="${(cur.exclude || {}).played_within_days || ''}" placeholder="0" style="width:70px"> days</label></div>
                <div class="toolbar"><button class="btn primary" id="smsave">${icon('check')} Save</button>${sid !== 'new' ? `<button class="btn danger" id="smdel">${icon('trash')} Delete</button>` : ''}</div></div>
              <section class="section"><div class="section-head"><h3>Preview</h3><span class="muted small" id="smcount"></span></div><div id="smprev"><p class="muted small">Add a rule to see which songs match.</p></div></section>`;
            $('[data-back]', main).onclick = () => goBack('playlists');
            const readRules = () => $$('[data-smrule]', main).map((row) => { const g = (k) => { const el = $(`[data-k="${k}"]`, row); return el ? el.value : undefined; }; const field = g('field'); const r = { field, op: g('op') || '' }; if (field === 'favourite') r.value = g('value') !== 'no'; else if (['plays', 'last_played'].includes(field)) r.value = Number(g('value') || 0); else if (field === 'year') { r.value = Number(g('value') || 0) || null; r.to = Number(g('to') || 0) || null; } else r.value = g('value') || ''; return r; });
            const list = (id) => $(id).value.split(',').map((x) => x.trim()).filter(Boolean);
            const sync = () => { const days = Number($('#smexdays').value) || 0; cur = { ...cur, name: $('#smname').value, matches: $('#smmatch').value, sort: $('#smsort').value, limit: Number($('#smlimit').value) || 100, minutes: Number($('#smminutes').value) || null, exclude: { artists: list('#smexart'), genres: list('#smexgen'), ...(days ? { played_within_days: days } : {}) }, rules: readRules() }; };
            // A rule left blank (the editor's starting row) counts for nothing
            const filled = () => cur.rules.filter((r) => !(typeof r.value === 'string' && !r.value.trim()));
            const body = () => ({ name: cur.name, rules: filled(), matches: cur.matches, sort: cur.sort, limit: cur.limit, seeds: cur.seeds || [], ...(cur.minutes ? { minutes: cur.minutes } : {}), exclude: cur.exclude || {} });
            let st = null;
            $('#smseedq').oninput = () => { clearTimeout(st); const q = $('#smseedq').value.trim(); const hits = $('#smseedhits'); if (q.length < 2) { hits.innerHTML = ''; return; } st = setTimeout(async () => { try { const r = await api('GET', `/library/search?q=${encodeURIComponent(q)}&limit=5`); if (!$('#smseedhits')) return; const cands = [...(r.artists || []).map((a) => ({ kind: 'artist', id: a.id, label: `${a.name} (artist)` })), ...(r.albums || []).map((a) => ({ kind: 'album', id: a.id, label: `${a.title} (album)` })), ...(r.genres || []).map((g) => ({ kind: 'genre', id: g.id, label: `${g.name} (genre)` })), ...(r.tracks || []).map((t) => ({ kind: 'track', id: t.id, label: `${t.title}${artists(t) ? ' · ' + artists(t) : ''}` }))].filter((c) => c.id != null).slice(0, 8); hits.innerHTML = cands.map((c, k) => `<button class="list-row clickable" data-smseed="${k}"><div class="grow"><div class="name">${esc(c.label)}</div></div></button>`).join(''); $$('[data-smseed]', hits).forEach((b) => b.onclick = () => { sync(); cur.seeds = [...(cur.seeds || []), cands[Number(b.dataset.smseed)]]; draw(); preview(); }); } catch { hits.innerHTML = ''; } }, 250); };
            $$('[data-smseedrm]', main).forEach((b) => b.onclick = () => { sync(); cur.seeds.splice(Number(b.dataset.smseedrm), 1); draw(); preview(); });
            let t = null;
            const preview = () => { clearTimeout(t); t = setTimeout(async () => { if (state.view !== 'smart-edit' || !$('#smname')) return; sync(); const box = $('#smprev'); if (!box) return; if (!filled().length && !(cur.seeds || []).length) { box.innerHTML = '<p class="muted small">Add a rule or a seed to see which songs match.</p>'; $('#smcount').textContent = ''; return; } try { const r = await api('POST', '/smart/preview', { ...body(), limit: Math.min(cur.limit, 20) }); $('#smcount').textContent = `${countOf(r.matched, 'songs')} match`; const songs = r.tracks.map((x, k) => ({ no: k + 1, title: x.title, artist: artists(x), duration_ms: x.duration_ms, track_id: x.id, id: trackPageId(x) })); box.innerHTML = songs.length ? songRows(songs, { numbers: 'index' }) : '<p class="muted small">No song matches yet.</p>'; if (songs.length) bindSongRows(box, songs, {}); } catch (e) { box.innerHTML = `<p class="muted small">${esc(e.message)}</p>`; } }, 350); };
            main.oninput = (e) => { if (state.view !== 'smart-edit') { main.oninput = null; return; } if (e.target.closest('.smartedit')) preview(); };
            main.onchange = (e) => { if (state.view !== 'smart-edit') { main.onchange = null; return; } if (e.target.dataset && e.target.dataset.k === 'field') { sync(); const i = Number(e.target.closest('[data-smrule]').dataset.smrule); cur.rules[i] = { field: e.target.value, op: SMART_FIELDS[e.target.value].ops[0][0], value: '' }; draw(); } preview(); };
            $('#smadd').onclick = () => { sync(); cur.rules.push({ field: 'genre', op: 'contains', value: '' }); draw(); };
            $$('[data-smrm]', main).forEach((b) => b.onclick = () => { sync(); cur.rules.splice(Number(b.dataset.smrm), 1); draw(); });
            $('#smsave').onclick = async () => { sync(); if (!cur.name.trim()) { toast('Give it a name', 'error'); $('#smname').focus(); return; } try { const saved = sid === 'new' ? await api('POST', '/smart', body()) : await api('PUT', `/smart/${encodeURIComponent(sid)}`, body()); toast(`Saved “${saved.name}”`); setView('smart', saved.id); } catch (e) { toast(e.message, 'error'); } };
            const del = $('#smdel'); if (del) del.onclick = async () => { if (!del.dataset.armed) { del.dataset.armed = '1'; del.innerHTML = `${icon('trash')} Really delete?`; return; } try { await api('DELETE', `/smart/${encodeURIComponent(sid)}`); toast('Smart playlist deleted'); setView('playlists'); } catch (e) { toast(e.message, 'error'); } };
            preview();
          };
          draw();
          break;
        }
        case 'playlists': {
          state.playlists = await api('GET', '/playlists');
          const smarts = await api('GET', '/smart').catch(() => []);
          if (state.view !== 'playlists') return;
          main.innerHTML = pageHead('Playlists', state.playlists.length) + `
            <div class="toolbar"><input class="input" id="plname" placeholder="New playlist name"><button class="btn" id="plcreate">${icon('plus')} Create</button><label class="btn file-btn">${icon('upload')} Import M3U, CSV or Roon export<input type="file" id="plimport" accept=".m3u,.m3u8,.csv,.xlsx"></label></div>
            <div class="toolbar"><input class="input" id="plshared" placeholder="Paste a pubky:// link to a shared playlist, or a Pubky key" style="flex:1;min-width:240px"><button class="btn" id="plopenshared">${icon('external')} Open</button></div>`
            + (state.playlists.length ? `<div class="list">${state.playlists.map((p) => `<div class="list-row clickable" data-pl="${esc(p.id)}">${mosaic(p.covers, p.name, 'pl-thumb')}<div class="grow"><div class="name">${esc(p.name)}</div><div class="sub">${p.items} item${p.items === 1 ? '' : 's'}${p.description ? ' · ' + esc(p.description) : ''}</div></div><span class="pill ${p.synced ? 'ok' : ''}">${p.synced ? 'synced' : 'local'}</span><button class="icon-btn" data-plplay="${esc(p.id)}" title="Play">${icon('play')}</button></div>`).join('')}</div>` : emptyState('playlist', 'No playlists yet', 'Create one, import a file from another player, or add tracks from any list.'))
            + `<section class="section"><div class="section-head"><h3>Smart playlists</h3><button class="btn sm" id="smartnew">${icon('plus')} New smart playlist</button></div><p class="muted small">Songs picked by rules over your library and your tags; they change as your library does.</p>${smarts.length ? `<div class="list">${smarts.map((x) => `<div class="list-row clickable" data-smart="${esc(x.id)}">${mosaic([], x.name, 'pl-thumb')}<div class="grow"><div class="name">${esc(x.name)}</div><div class="sub">${esc(smartSummary(x))}</div></div><span class="pill">smart</span></div>`).join('')}</div>` : ''}</section>`;
          $('#smartnew').onclick = () => setView('smart-edit', 'new');
          $$('[data-smart]', main).forEach((r) => r.onclick = () => setView('smart', r.dataset.smart));
          $('#plcreate').onclick = async () => { const n = $('#plname').value.trim(); if (!n) { $('#plname').focus(); return; } try { await api('POST', '/playlists', { name: n }); toast(`Created “${n}”`); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#plname').onkeydown = (e) => { if (e.key === 'Enter') $('#plcreate').click(); };
          needsText($('#plname'), $('#plcreate')); needsText($('#plshared'), $('#plopenshared'));
          const openShared = async () => { try { const t = await api('GET', `/shared/resolve?link=${encodeURIComponent($('#plshared').value)}`); setView('shared', t.id ? { user: t.user, id: t.id } : { user: t.user }); } catch (e) { toast(e.message, 'error'); } };
          $('#plopenshared').onclick = openShared; $('#plshared').onkeydown = (e) => { if (e.key === 'Enter' && $('#plshared').value.trim()) openShared(); };
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
        case 'person': {
          // A person on Pubky: their own profile.json (via Nexus), the music they tag, what they share
          loading();
          const pid = state.viewArg;
          let pg; try { pg = await api('GET', `/profiles/${pid}`); } catch (e) { main.innerHTML = backBtn('home', 'Home') + `<div class="notice">${esc(e.message)}</div>`; break; }
          const shared = await api('GET', `/shared/${pid}/playlists`).catch(() => []);
          if (state.view !== 'person' || state.viewArg !== pid) return;
          state.itemTitle = pg.name;
          const links = (pg.links || []).map((l) => `<a class="chip" href="${esc(webUrl(l.url))}" target="_blank" rel="noopener">${icon('external')} ${esc(l.title || l.url.replace(/^https?:\/\//, ''))}</a>`).join('');
          const tagged = pg.tagged || [];
          main.innerHTML = backBtn('home', 'Home')
            + `<div class="hero person">${pg.avatar ? `<div class="art round"><img src="${esc(imgSrc(pg.avatar))}" alt="" style="width:100%;border-radius:50%" data-err="remove"></div>` : `<div class="art placeholder monogram" style="background:linear-gradient(135deg, hsl(${hue(pid)} 35% 34%), hsl(${hue(pid)} 35% 20%));border-radius:50%" aria-hidden="true">${esc(initials(pg.name))}</div>`}<div><div class="eyebrow">${['Person on Pubky', pg.you ? 'you' : '', pg.followed_by ? 'follows you' : ''].filter(Boolean).join(' · ')}</div><h1>${esc(pg.name)}</h1><p class="sub">${keyHtml(pid)}</p>${pg.status ? `<p class="muted">${esc(pg.status)}</p>` : ''}${pg.bio ? `<p class="blurb">${esc(pg.bio)}</p>` : ''}
              <div class="actions" style="margin-top:12px">${pg.you ? '' : `<button class="btn ${pg.following ? '' : 'primary'}" id="pfollow" aria-pressed="${!!pg.following}">${pg.following ? `${icon('check')} Following` : `${icon('plus')} Follow`}</button>`}<a class="btn ghost" href="${esc(webUrl(pg.profile_url))}" target="_blank" rel="noopener">${icon('external')} Profile on pubky.app</a></div>
              ${links ? `<div class="chips" style="margin-top:10px">${links}</div>` : ''}<p class="muted small">${countOf(pg.followers || 0, 'followers')} · ${pg.following_count || 0} following</p></div></div>`
            + `<section class="section"><div class="section-head"><h3>Tags they use</h3><span class="muted small">public on Pubky</span></div>${(pg.tags || []).length ? `<div class="chips">${pg.tags.map(([l, c]) => `<button class="chip" data-ptag="${esc(l)}">#${esc(l)} <span class="muted">${c}</span></button>`).join('')}</div>` : '<p class="muted small">No music tags from them on Pubky yet.</p>'}</section>`
            + (tagged.length ? `<section class="section"><div class="section-head"><h3>Recently tagged</h3></div><div class="list">${tagged.map((h, k) => `<div class="list-row clickable" data-ptagged="${k}">${h.image ? `<img class="thumb" src="${esc(imgSrc(h.image))}" alt="" loading="lazy" data-err="hide">` : svcBadge('pubky')}<div class="grow"><div class="name">${esc(h.title)}</div><div class="sub">${esc([{ song: 'Song', album: 'Album', artist: 'Artist' }[h.kind] || h.kind, h.subtitle].filter(Boolean).join(' · '))}</div></div></div>`).join('')}</div></section>` : '')
            + `<section class="section"><div class="section-head"><h3>Playlists they share</h3><span class="muted small">${shared.length || ''}</span></div>${shared.length ? `<div class="list">${shared.map((p) => `<div class="list-row clickable" data-pshared="${esc(p.id)}"><div class="avatar" style="background:hsl(${hue(p.name)} 35% 28%)">${icon('playlist')}</div><div class="grow"><div class="name">${esc(p.name)}</div><div class="sub">${countOf(p.items || 0, 'songs')}</div></div></div>`).join('')}</div>` : '<p class="muted small">No shared playlists.</p>'}</section>`;
          $('[data-back]', main).onclick = () => goBack('home');
          $$('[data-ptag]', main).forEach((b) => b.onclick = () => setView('discover', { tag: b.dataset.ptag }));
          $$('[data-ptagged]', main).forEach((r) => r.onclick = () => { const h = tagged[Number(r.dataset.ptagged)]; setView('item', { kind: h.kind === 'artist' ? 'artist' : h.kind === 'album' ? 'album' : 'song', id: h.id }); });
          $$('[data-pshared]', main).forEach((r) => r.onclick = () => setView('shared', { user: pid, id: r.dataset.pshared }));
          const fb = $('#pfollow'); if (fb) fb.onclick = async () => { const on = fb.getAttribute('aria-pressed') !== 'true'; fb.disabled = true; try { await api(on ? 'PUT' : 'DELETE', `/profiles/${pid}/follow`); profileCache.delete(pid); toast(on ? `Following ${pg.name}` : `Not following ${pg.name} any more`); render(); } catch (e) { toast(e.message, 'error'); fb.disabled = false; } };
          break;
        }
        case 'shared': {
          // Someone's shared playlists, or one of them: read-only here; "Save a copy" makes it yours.
          loading();
          const a = state.viewArg || {}; const who = shortKey(a.user);
          if (!a.id) {
            let list; try { list = await api('GET', `/shared/${a.user}/playlists`); } catch (e) { main.innerHTML = backBtn('playlists', 'Playlists') + `<div class="notice">${esc(e.message)}</div>`; break; }
            if (state.view !== 'shared') return;
            state.itemTitle = `Shared by ${who}`;
            main.innerHTML = backBtn('playlists', 'Playlists') + pageHead(`Shared by ${who}`, list.length) + (list.length ? `<div class="list">${list.map((p) => `<div class="list-row clickable" data-shpl="${esc(p.id)}"><div class="avatar" style="background:hsl(${hue(p.name)} 35% 28%)">${icon('playlist')}</div><div class="grow"><div class="name">${esc(p.name)}</div><div class="sub">${p.items} song${p.items === 1 ? '' : 's'}${p.description ? ' · ' + esc(p.description) : ''}</div></div></div>`).join('')}</div>` : emptyState('playlist', 'Nothing shared yet', 'This person has no shared playlists on Pubky.'));
            $$('[data-shpl]', main).forEach((r) => r.onclick = () => setView('shared', { user: a.user, id: r.dataset.shpl }));
            break;
          }
          let sp; try { sp = await api('GET', `/shared/${a.user}/playlists/${a.id}`); } catch (e) { main.innerHTML = backBtn('playlists', 'Playlists') + `<div class="notice">${esc(e.message)}</div>`; break; }
          if (state.view !== 'shared') return;
          state.itemTitle = sp.summary.name;
          // A web page or a post plays nowhere: its card, in its place (review of #155, M4)
          const cardOf = (k) => { const e = sp.entries && sp.entries[k]; if (!e || (e.kind !== 'link' && e.kind !== 'post')) return null; return { no: k + 1, card: true, title: (() => { try { return new URL(e.uri).hostname.replace(/^www\./, ''); } catch { return e.uri; } })(), page: e.uri }; };
          const songs = sp.play_from.items.map((r, k) => { const card = cardOf(k); if (card) return card; const lib = r.plays_from && r.plays_from.service === 'library'; return { no: k + 1, title: itemName({ title: r.title, site: r.site }), artist: r.artist, image: r.image, duration_ms: r.duration_ms, track_id: lib ? r.track_id : null, uri: r.stream_uri || null, page: r.page || r.uri, id: idOf(r.uri), plays: r.plays_from, pinned: !!(r.pin && !r.pin_unavailable), unavailable: r.plays_from ? null : (r.reason || 'Not on your connected services') }; });
          const playable = songs.filter((t) => !t.unavailable && !t.card);
          const songCount = songs.filter((t) => !t.card).length; const linkCount = songs.length - songCount;
          const chip = (t) => t.unavailable ? `<span class="chip srcchip none" title="${esc(t.unavailable)}">${icon('close')}<span>Unavailable</span></span>` : t.plays ? `<span class="chip srcchip${t.pinned ? ' pinned' : ''}" title="${esc(t.plays.label)}${t.pinned ? ' · the author’s pick' : ''}">${svcBadge(t.plays.service)}<span>${esc(srcName(t.plays.service))}</span>${t.pinned ? icon('pin') : ''}</span>` : '';
          main.innerHTML = backBtn('playlists', 'Playlists') + `<div class="hero">${mosaic((sp.play_from.items || []).map((r) => r.image), sp.summary.name)}<div><div class="eyebrow">Shared playlist · by <code>${esc(who)}</code></div><h1>${esc(sp.summary.name)}</h1><p class="sub">${countOf(songCount, 'songs')}${linkCount ? ` · ${countOf(linkCount, 'links')}` : ''} · ${playable.length} play here</p>
              <div class="actions" style="margin-top:12px"><button class="btn primary" id="shplay" ${playable.length ? '' : 'disabled title="None of these songs is on your services"'}>${icon('play')} Play</button><button class="btn" id="shcopy">${icon('plus')} Save a copy</button><button class="btn ghost" id="shlink" title="Copy the pubky:// link">${icon('external')} Copy link</button></div></div></div>`
            + `<section class="section" id="shsongs"><div class="section-head"><h3>Songs</h3><span class="muted small">where each plays from on your node</span></div>${songs.length ? songRows(songs, { numbers: 'index', rowClass: (t) => (t.unavailable ? 'unavail' : ''), extra: (t) => chip(t) }) : emptyState('playlist', 'This playlist is empty', '')}</section>`
            + (sp.summary.description ? `<section class="section"><div class="section-head"><h3>About</h3></div><p>${esc(sp.summary.description)}</p></section>` : '');
          bindSongRows($('#shsongs'), songs, {});
          $('#shplay').onclick = (e) => playSongs(playable, 0, 'replace', null, e.currentTarget);
          $('#shcopy').onclick = async () => { try { const d = await api('POST', `/shared/${a.user}/playlists/${a.id}/copy`); state.plPf = null; toast(`Saved “${d.summary.name}” to your playlists`); setView('playlist', d.summary.id); } catch (e) { toast(e.message, 'error'); } };
          $('#shlink').onclick = async () => { try { await navigator.clipboard.writeText(sp.summary.link); toast('Link copied'); } catch { toast(sp.summary.link); } };
          break;
        }
        case 'playlist': {
          loadSvcRank(); // for the "Play this song from" menu's order
          loading();
          const plId = state.viewArg;
          await pubkyUser();
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
            // A web page or a post plays nowhere: it shows as its card, in its place (B3)
            const kind = d.entries && d.entries[k] && d.entries[k].kind;
            if (kind === 'link' || kind === 'post') return { no: k + 1, card: true, title: (() => { try { return new URL(it.uri).hostname.replace(/^www\./, ''); } catch { return it.uri; } })(), page: it.uri };
            const t = d.tracks[k]; const r = pf && pf.items[k];
            const base = r ? { plays: r.plays_from, pinned: !!(r.pin && !r.pin_unavailable), pinLost: !!r.pin_unavailable, unavailable: r.plays_from ? null : (r.reason || 'Not on your connected services'), sources: r.sources, connect: r.connect } : {};
            const lib = r && r.plays_from && r.plays_from.service === 'library';
            if (t && pfPending) return { pending: true, no: k + 1, title: t.title, artist: artists(t), duration_ms: t.duration_ms, track_id: t.id, id: trackPageId(t) };
            if (t && (!r || lib)) return { ...base, no: k + 1, title: t.title, artist: artists(t), artist_id: t.artists && t.artists[0] && t.artists[0].id != null ? 'lib-' + t.artists[0].id : null, album: t.album && t.album.title, album_id: t.album && t.album.id != null ? 'lib-' + t.album.id : null, duration_ms: t.duration_ms, track_id: t.id, id: trackPageId(t) };
            if (r) return { ...base, no: k + 1, title: itemName({ title: r.title, site: r.site }), artist: r.artist, image: r.image, duration_ms: r.duration_ms, track_id: lib ? r.track_id : null, uri: r.stream_uri || null, page: r.page || it.uri, id: idOf(it.uri) };
            return { no: k + 1, title: pfPending ? 'Loading…' : itemName({ title: null, site: null }), pending: pfPending, page: it.uri, id: idOf(it.uri) };
          });
          const localSongs = (d.local_tracks || []).map((t, k) => ({ no: k + 1, title: t.title, artist: artists(t), artist_id: t.artists && t.artists[0] && t.artists[0].id != null ? 'lib-' + t.artists[0].id : null, album: t.album && t.album.title, album_id: t.album && t.album.id != null ? 'lib-' + t.album.id : null, duration_ms: t.duration_ms, track_id: t.id, id: trackPageId(t) }));
          const total = [...songs, ...localSongs].reduce((a, t) => a + (t.duration_ms || 0), 0);
          const songCount = songs.filter((t) => !t.card).length; const cardCount = songs.length - songCount;
          const empty = (t) => `<p class="muted small">${t}</p>`;
          // Summary bar: "12 songs · 8 Library · 3 TIDAL · 1 unavailable", each part a filter.
          const counts = new Map(); songs.forEach((t) => { const key = t.unavailable ? 'none' : t.plays ? t.plays.service : null; if (key) counts.set(key, (counts.get(key) || 0) + 1); });
          const filt = state.plFilter && state.plFilter.id === plId ? state.plFilter.key : null;
          // While the play-from answer is on its way, its chip row keeps its place, so the songs don't jump (second review #20)
          const summaryBar = pfPending && songs.length ? '<div class="chips plsummary" aria-hidden="true"><span class="chip skeleton chip-sk"></span><span class="chip skeleton chip-sk"></span></div>' : pf && songs.length ? `<div class="chips plsummary" role="group" aria-label="Filter by source"><button class="chip ${filt ? '' : 'on'}" data-pfilter="" aria-pressed="${!filt}">${songCount} song${songCount === 1 ? '' : 's'}</button>${[...counts.entries()].sort((a, b) => (a[0] === 'none') - (b[0] === 'none') || b[1] - a[1]).map(([key, n]) => `<button class="chip ${filt === key ? 'on' : ''} ${key === 'none' ? 'warnchip' : ''}" data-pfilter="${esc(key)}" aria-pressed="${filt === key}">${key === 'none' ? '' : svcBadge(key)}${n} ${key === 'none' ? 'unavailable' : esc(srcName(key))}</button>`).join('')}</div>` : '';
          const ownOrder = pf && pf.own_order;
          // Only what can play here: services not connected aren't part of the answer (second review #16)
          const orderText = pf ? pf.order.order.filter((k) => { const sv = (pf.order.services || []).find((x) => x.id === k); return !sv || sv.connected; }).map(srcName).join(' › ') || 'Your library' : '';
          main.innerHTML = backBtn('playlists', 'Playlists') + `<div class="hero">${mosaic(d.summary.covers, d.summary.name)}<div><div class="eyebrow">Playlist · ${esc(d.summary.visibility)} · ${d.summary.synced ? 'synced to your homeserver' : 'on this node'}</div><h1>${esc(d.summary.name)}</h1><p class="sub">${countOf(songCount + localSongs.length, 'songs')}${cardCount ? ` · ${countOf(cardCount, 'links')}` : ''}${total ? ' · ' + mins(total) : ''}</p>
              <div class="actions" style="margin-top:12px"><button class="btn primary" id="plplay">${icon('play')} Play</button><button class="btn" id="plshuffle">${icon('shuffle')} Shuffle</button><button class="btn" id="plzone">${icon('zone')} Play on…</button><button class="btn ghost" id="plmore" title="More">${icon('more')}</button><button class="btn danger" id="pldel">${icon('trash')} Delete</button></div></div></div>`
            + `<section class="section" id="plsongs"><div class="section-head"><h3>Songs</h3>${pf ? `<button class="link" id="plorderopen" title="Change where songs play from">Play from: ${esc(orderText)}</button>` : ''}</div>${summaryBar}${songs.length ? songRows(songs, { numbers: 'index', rowClass: (t) => `${t.unavailable ? 'unavail' : ''}${filt && (t.unavailable ? 'none' : t.plays && t.plays.service) !== filt ? ' filtered' : ''}`, extra: (t, k) => `${sourceChip(t, k)}<span class="row-actions q-actions"><button class="icon-btn" data-pup="${k}" title="Move up" ${k === 0 ? 'disabled' : ''}>${icon('chevron', 'ic up')}</button><button class="icon-btn" data-pdown="${k}" title="Move down" ${k === songs.length - 1 ? 'disabled' : ''}>${icon('chevron')}</button><button class="icon-btn" data-rm="${k}" title="Remove">${icon('close')}</button></span>` }) : emptyState('playlist', 'This playlist is empty', 'Add songs from any ⋯ menu, or import a file.')}</section>`
            + (localSongs.length ? `<section class="section" id="pllocal"><div class="section-head"><h3>On this node only</h3><span class="muted small">${countOf(localSongs.length, 'songs')}</span></div><p class="muted small">Not identified yet, so they stay on this node and aren't synced. Pick which song each one is (the ? in its menu) and it joins the playlist on your homeserver.</p>${songRows(localSongs, { numbers: 'index', extra: (t, k) => `<button class="icon-btn" data-lrm="${k}" title="Remove from this playlist" aria-label="Remove ${esc(t.title)} from this playlist">${icon('close')}</button>` })}</section>` : '')
            + (pf ? `<section class="section" id="plorder"><div class="section-head"><h3>Play from</h3><span class="muted small">${ownOrder ? 'this playlist’s own order' : 'your node’s order (Settings)'}</span></div><p class="muted small">Each song plays from the first source here that has it. A pinned song keeps its own source.</p>${orderList(pf.order)}<label class="check small" style="margin-top:10px"><input type="checkbox" id="plownorder" ${ownOrder ? '' : 'checked'}> Use my node’s order</label></section>` : '')
            + `<section class="section" id="plpubky"><div class="section-head"><h3>On Pubky</h3>${d.summary.visibility === 'public' ? '<span class="pill ok">shared</span>' : ''}</div>${d.summary.visibility === 'public'
              ? `<p class="muted small">${d.summary.synced ? 'Shared: anyone with the link can open it and play it from their own services. Your pins go along as suggestions.' : 'Shared on the next sync with your homeserver.'}</p>${state.pubkyUser ? `<div class="toolbar"><code class="sharelink">${esc(`pubky://${state.pubkyUser.replace(/^pubky/, '')}/pub/social/v1/posts/${d.summary.id}`)}</code><button class="btn sm" id="plcopylink">${icon('external')} Copy link</button><button class="btn sm ghost" id="plunshare">Make private</button></div>` : `<div class="toolbar"><button class="btn sm ghost" id="plunshare">Make private</button></div>`}`
              : `<p class="muted small">${d.summary.synced ? 'Saved privately on your homeserver.' : 'Private; it lives on this node until the next sync.'} Sharing puts a public copy on your homeserver: the songs and their order, never audio.</p><div class="toolbar"><button class="btn sm" id="plshare">${icon('upload')} Share on Pubky</button></div>`}</section>`
            + `<section class="section"><div class="section-head"><h3>Description</h3><button class="link" id="pldesc">${d.playlist.description ? 'Edit' : 'Add one'}</button></div>${d.playlist.description ? `<p>${esc(d.playlist.description)}</p>` : '<p class="muted small">A few words about this playlist, shown with it when you share it.</p>'}</section>`
            + `<section class="section"><div class="section-head"><h3>About</h3></div><dl class="kv"><dt>Songs</dt><dd>${songs.length} · ${d.tracks.filter(Boolean).length} in this library</dd><dt>Length</dt><dd>${total ? mins(total) : '—'}</dd><dt>Stored</dt><dd>${d.summary.synced ? 'your homeserver (private)' : 'this node'}</dd></dl></section>`;
          $('[data-back]', main).onclick = () => goBack('playlists');
          // Phones hide a row's Move up / Move down / Remove buttons (no room next to the title):
          // the row's ⋯ menu offers them and presses the same buttons
          const rowActs = (k, row) => (row && $('.q-actions', row) && getComputedStyle($('.q-actions', row)).display === 'none' ? [['pup', 'Move up', 'upload'], ['pdown', 'Move down', 'download'], ['rm', 'Remove from playlist', 'trash']].filter(([a]) => { const b = $(`[data-${a}]`, row); return b && !b.disabled; }).map(([a, label, ic]) => ({ label, icon: ic, run: () => $(`[data-${a}]`, row).click() })) : []);
          bindSongRows($('#plsongs'), songs, { rowExtra: rowActs });
          // Playlist rows can also be removed together (highest index first, so positions hold)
          enableSelect($('#plsongs'), songs, { remove: async (idx) => { for (const k of idx) await api('DELETE', `/playlists/${d.summary.id}/items/${k}`); state.plPf = null; toast(`Removed ${countOf(idx.length, 'songs')}`); render(); } });
          if (localSongs.length) bindSongRows($('#pllocal'), localSongs, {});
          $$('[data-lrm]', main).forEach((b) => b.onclick = async (e) => { e.stopPropagation(); try { await api('DELETE', `/playlists/${d.summary.id}/local/${b.dataset.lrm}`); render(); } catch (er) { toast(er.message, 'error'); } });
          // Cards (web pages, posts) play nowhere: Play and Shuffle play the songs around them
          const playable = (list) => list.filter((t) => !t.unavailable && !t.card);
          const playAll = (shuffle, anchorEl) => { const list = [...playable(songs), ...localSongs]; return playSongs(shuffle ? [...list].sort(() => Math.random() - 0.5) : list, 0, 'replace', null, anchorEl); };
          $('#plplay').onclick = (e) => (isBrowser() ? playAll(false, e.currentTarget) : playPlaylist(d.summary.id));
          stickyHead(d.summary.name, (el) => (isBrowser() ? playAll(false, el) : playPlaylist(d.summary.id)));
          $('#plshuffle').onclick = (e) => playAll(true, e.currentTarget);
          $('#plzone').onclick = (e) => showMenu(e.currentTarget, [{ title: 'Play on' }, ...zoneList().map((z) => ({ label: z.name, icon: 'zone', run: () => { state.zone = z.id; renderZoneName(); renderBar(); if (isBrowser()) playAll(false); else playPlaylist(d.summary.id); } }))]);
          // Name and description (second review: a playlist couldn't be renamed or described)
          const editField = async (key, label, value) => {
            const r = await choiceDialog({ title: key === 'name' ? 'Rename playlist' : 'Playlist description', field: { label, value: value || '' }, choices: [{ label: 'Save', value: 'save', primary: true }] });
            if (!r || (key === 'name' && !r.text)) return;
            try { await api('PUT', `/playlists/${d.summary.id}`, { [key]: r.text }); state.playlists = await api('GET', '/playlists'); toast(key === 'name' ? `Renamed to “${r.text}”` : 'Description saved'); render(); } catch (er) { toast(er.message, 'error'); }
          };
          $('#pldesc').onclick = () => editField('description', 'Description', d.playlist.description);
          $('#plmore').onclick = (e) => itemMenu(e.currentTarget, { title: d.summary.name, songs, path: `/playlist/${encodeURIComponent(d.summary.id)}`, extra: [{ label: 'Rename…', icon: 'notes', run: () => editField('name', 'Name', d.summary.name) }, { label: d.playlist.description ? 'Edit description…' : 'Add a description…', icon: 'notes', run: () => editField('description', 'Description', d.playlist.description) }] });
          $('#pldel').onclick = async () => { if (!$('#pldel').dataset.armed) { $('#pldel').dataset.armed = '1'; $('#pldel').innerHTML = `${icon('trash')} Really delete?`; setTimeout(() => { if ($('#pldel')) { delete $('#pldel').dataset.armed; $('#pldel').innerHTML = `${icon('trash')} Delete`; } }, 3000); return; } try { await api('DELETE', `/playlists/${d.summary.id}`); toast('Playlist deleted'); setView('playlists'); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-rm]', main).forEach((b) => b.onclick = async (e) => { e.stopPropagation(); try { await api('DELETE', `/playlists/${d.summary.id}/items/${b.dataset.rm}`); state.plPf = null; render(); } catch (er) { toast(er.message, 'error'); } });
          const pmove = async (i2, j2) => { const uris = d.playlist.items.map((it) => it.uri); if (j2 < 0 || j2 >= uris.length) return; const [u] = uris.splice(i2, 1); uris.splice(j2, 0, u); try { await api('PUT', `/playlists/${d.summary.id}`, { uris }); state.plPf = null; render(); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-pup]', main).forEach((b) => b.onclick = (e) => { e.stopPropagation(); pmove(Number(b.dataset.pup), Number(b.dataset.pup) - 1); });
          $$('[data-pdown]', main).forEach((b) => b.onclick = (e) => { e.stopPropagation(); pmove(Number(b.dataset.pdown), Number(b.dataset.pdown) + 1); });
          // Source chip → every source this song has here, plus Auto; picking one pins it.
          const pin = async (k, body, msg) => { try { await api('PUT', `/playlists/${d.summary.id}/items/${k}/source`, body); state.plPf = null; toast(msg); render(); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-psrc]', main).forEach((b) => b.onclick = (e) => {
            e.stopPropagation(); const k = Number(b.dataset.psrc); const t = songs[k]; const pinnedNow = t.pinned || t.pinLost;
            const opts = rankSvc([...(t.sources || [])], (x) => x.service).sort((a, b) => (b.available ? 1 : 0) - (a.available ? 1 : 0)).map((x) => ({ label: x.name, note: x.available ? x.label.replace(/^[^·]*·\s*/, '') || null : x.mode === 'preview' ? '30 s previews only' : 'not connected', svc: x.service, on: t.pinned && x.chosen, disabled: !x.available, run: () => pin(k, { service: x.service, uri: x.url || null }, `Pinned to ${x.name}`) }));
            showMenu(b, [{ title: t.unavailable ? t.unavailable : 'Play this song from' }, { label: 'Auto (follow the playlist)', icon: 'shuffle', on: !pinnedNow, note: t.plays && !pinnedNow ? `now: ${srcName(t.plays.service)}` : null, run: () => pin(k, { service: null }, 'Follows the playlist again') }, ...opts,
              ...(t.connect && t.connect.length ? [{ label: `Connect ${t.connect.map((c) => SERVICE_NAMES[c] || c).join(' or ')}`, icon: 'plus', run: () => setView('sources') }] : []),
              ...(t.id ? [{ label: 'Find another version', icon: 'disc', run: () => setView('item', { kind: 'song', id: t.id }) }] : [])]);
          });
          const setVis = async (pub) => { try { await api('PUT', `/playlists/${d.summary.id}/visibility`, { public: pub }); toast(pub ? 'Shared: it goes public on the next sync' : 'Private again: the public copy is removed on the next sync'); render(); } catch (e) { toast(e.message, 'error'); } };
          // Going public asks first, and says what happens without a Pubky sign-in (second review)
          const sh = $('#plshare'); if (sh) sh.onclick = async () => {
            const who = await pubkyUser();
            const go = await choiceDialog({ title: `Share “${d.summary.name}”?`, text: who
              ? 'A public copy goes to your homeserver with the next sync (in a few minutes): the songs and their order, never audio. Anyone with the link can open it, and people on Pubky can find it. You can make it private again at any time.'
              : 'You aren\'t signed in to Pubky yet, so there is nowhere to share it from. Sign in with Pubky Ring first (Settings › My accounts); the playlist stays on this node until then.',
              choices: who ? [{ label: 'Share', value: 'share', primary: true }] : [{ label: 'Open Accounts', value: 'accounts', primary: true }] });
            if (go === 'share') setVis(true); else if (go === 'accounts') setView('settings', 'accounts');
          };
          const us = $('#plunshare'); if (us) us.onclick = () => setVis(false);
          const cl = $('#plcopylink'); if (cl) cl.onclick = async () => { const t = $('#plpubky .sharelink').textContent; try { await navigator.clipboard.writeText(t); toast('Link copied'); } catch { toast(t); } };
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
          // One tab per service with its on/off switch, laid out like Settings (STEWARD-UX-SOURCES.md).
          // Off hides a source everywhere; its tab stays (dimmed) so it can be switched on again.
          try { state.sources = await api('GET', '/sources'); applySourcesOff(); } catch (e) { state.sources = state.sources || []; }
          if (state.view !== 'sources') return;
          const srcs = state.sources;
          const phone = !!(window.matchMedia && matchMedia('(max-width: 720px)').matches); // the side tabs' CSS is by width until the Settings step
          const known = (k) => k === 'library' || srcs.some((x) => x.id === k);
          const asked = state.viewArg && known(state.viewArg.source) ? state.viewArg.source : null;
          const active = asked || (phone ? null : (known(state.sourceId) ? state.sourceId : 'library'));
          if (active) state.sourceId = active;
          const cur = active === 'library' ? { id: 'library', name: 'Your library', description: '', status: 'ready', enabled: true } : srcs.find((x) => x.id === active) || null;
          const help = (h) => (h ? `<p class="set-help">${h}</p>` : '');
          const row = (label, h, ctl, cls = '') => `<div class="set-row ${cls}"><div class="set-label"><div class="set-name">${label}</div>${help(h)}</div>${ctl ? `<div class="set-ctl">${ctl}</div>` : ''}</div>`;
          const block = (label, h, body) => `<div class="set-row set-block">${label || h ? `<div class="set-label">${label ? `<div class="set-name">${label}</div>` : ''}${help(h)}</div>` : ''}<div class="set-body">${body}</div></div>`;
          const group = (title, body) => `<div class="set-group">${title ? `<h3 class="set-h">${title}</h3>` : ''}<div class="set-rows">${body}</div></div>`;
          const mark = (x) => `<span class="src-mark" style="background:hsl(${hue(x.id)} 45% 34%)" aria-hidden="true">${esc((x.name || x.id).replace(/^Your /, '').slice(0, 1).toUpperCase())}</span>`;
          const srcSwitch = (x, where) => `<input type="checkbox" class="switch" role="switch" data-src-on="${esc(x.id)}" ${x.enabled ? 'checked' : ''} aria-label="${esc(x.name)}${where}">`;
          const tabRow = (x) => { const st = srcState(x); return `<div class="src-tabrow ${x.enabled ? '' : 'off'}"><button class="set-tab ${active === x.id ? 'on' : ''}" id="srctab-${esc(x.id)}" ${active === x.id ? 'aria-current="page"' : ''} data-srctab="${esc(x.id)}">${mark(x)}<span class="src-tname"><span class="src-t1">${esc(x.name)}</span>${x.sign_in ? `<span class="src-tsub" title="${esc(x.sign_in)}">${esc(x.sign_in)}</span>` : ''}</span><span class="src-dot ${st.cls}" title="${esc(st.label)}"></span><span class="sr-only">${esc(st.label)}</span>${icon('chevron', 'ic chev')}</button>${x.id === 'library' ? '<span class="src-noswitch" aria-hidden="true"></span>' : srcSwitch(x, ' on')}</div>`; };
          const lib = { id: 'library', name: 'Your library', status: 'ready', enabled: true };
          // Working services first, in the owner's play order (STEWARD-UX-PLAYER-SOURCES §1)
          await loadSvcRank(); if (state.view !== 'sources') return;
          const on = rankSvc(srcs.filter((x) => x.enabled), (x) => x.id); const off = srcs.filter((x) => !x.enabled);
          const nav = `<nav class="set-nav src-nav" aria-label="Sources">${[lib, ...on].map(tabRow).join('')}${off.length ? (off.length > 2 ? `<details class="src-offgroup" ${off.some((x) => x.id === active) ? 'open' : ''}><summary>Turned off · ${off.length}</summary>${off.map(tabRow).join('')}</details>` : off.map(tabRow).join('')) : ''}</nav>`;
          const panelHead = (x) => `${phone ? `<button class="btn ghost set-back" data-src-back>${icon('back')} Sources</button>` : ''}<h2 class="set-title">${esc(x.name)}</h2>${x.description ? `<p class="muted small src-desc">${esc(x.description)}</p>` : ''}`;
          // TIDAL: one request carries the sign-in ways and both status views (STEWARD-CREDENTIALS-REUSE.md)
          let tidal = null; let tidalSi = null; if (active === 'tidal') { try { tidalSi = await api('GET', '/sources/tidal/signin'); tidal = tidalSi.details.status || null; } catch { tidalSi = null; tidal = null; } if (state.view !== 'sources') return; }
          let sub = null; let subPls = []; if (active === 'subsonic') { try { sub = await api('GET', '/sources/subsonic/server'); if (sub.connected) subPls = await api('GET', '/sources/subsonic/playlists').catch(() => []); } catch { sub = null; } if (state.view !== 'sources') return; }
          const subAccount = active !== 'subsonic' ? '' : sub && sub.connected
            ? `<div class="toolbar"><span class="pill ok account">Connected · ${esc(sub.name || 'your server')}${sub.version ? ' ' + esc(sub.version) : ''} · ${esc(sub.user || '')}@${esc(sub.url || '')}</span><button class="btn ghost" id="subforget">Forget this server</button></div>`
              + `<section class="section"><div class="section-head"><h3>Its playlists</h3><span class="muted small">import one as a private Pubky playlist (songs need a MusicBrainz id or ISRC on the server)</span></div>${subPls.length ? `<div class="list">${subPls.map((p) => `<div class="list-row"><div class="grow"><div class="name" title="${esc(p.name)}">${esc(p.name)}</div><div class="sub">${p.songs} song${p.songs === 1 ? '' : 's'}</div></div><button class="btn sm" data-subpl="${esc(p.id)}" aria-label="Import ${esc(p.name)}">${icon('download')} Import</button></div>`).join('')}</div>` : '<p class="muted small">No playlists on the server.</p>'}</section>`
            : `<form class="card subform" id="subform"><h3>Connect your server</h3><p class="muted small">Navidrome, Airsonic, Gonic, Ampache, LMS or another Pubky Music node. The password stays on this node; requests are signed with a fresh token each time.</p><label class="field"><span>Server address</span><input class="input" name="url" placeholder="https://music.example.com" autocomplete="url" required></label><label class="field"><span>User</span><input class="input" name="user" autocomplete="username" required></label><label class="field"><span>Password</span><input class="input" name="password" type="password" autocomplete="current-password" required></label><div class="toolbar"><button class="btn primary" id="subconnect" type="submit">${icon('zone')} Connect</button></div></form>`;
          let jf = null; let jfPls = []; if (active === 'jellyfin') { try { jf = await api('GET', '/sources/jellyfin/server'); if (jf.connected) jfPls = await api('GET', '/sources/jellyfin/playlists').catch(() => []); } catch { jf = null; } if (state.view !== 'sources') return; }
          const jfAccount = active !== 'jellyfin' ? '' : jf && jf.connected
            ? `<div class="toolbar"><span class="pill ok account">Signed in · ${esc(jf.server_name || 'Jellyfin')}${jf.version ? ' ' + esc(jf.version) : ''} · ${esc(jf.user || '')}</span><button class="btn ghost" id="jfout">Sign out</button></div>`
              + `<section class="section"><div class="section-head"><h3>Its playlists</h3><span class="muted small">import one as a private Pubky playlist (songs need a MusicBrainz recording id in Jellyfin)</span></div>${jfPls.length ? `<div class="list">${jfPls.map((p) => `<div class="list-row"><div class="grow"><div class="name" title="${esc(p.name)}">${esc(p.name)}</div><div class="sub">${p.songs} song${p.songs === 1 ? '' : 's'}</div></div><button class="btn sm" data-jfpl="${esc(p.id)}" aria-label="Import ${esc(p.name)}">${icon('download')} Import</button></div>`).join('')}</div>` : '<p class="muted small">No music playlists.</p>'}</section>`
            : `<form class="card subform" id="jfform"><h3>Sign in to your Jellyfin</h3><p class="muted small">Also works with Emby. The password is used once to sign in; the node keeps only the access token Jellyfin gives it.</p><label class="field"><span>Server address</span><input class="input" name="url" placeholder="https://jellyfin.example.com" autocomplete="url" required></label><label class="field"><span>User</span><input class="input" name="user" autocomplete="username" required></label><label class="field"><span>Password</span><input class="input" name="password" type="password" autocomplete="current-password"></label><div class="toolbar"><button class="btn primary" id="jfin" type="submit">${icon('zone')} Sign in</button></div></form>`;
          let jam = null; if (active === 'jamendo') { try { jam = await api('GET', '/sources/jamendo/status'); } catch { jam = null; } if (state.view !== 'sources') return; }
          const jamAccount = active !== 'jamendo' ? '' : !jam || !jam.configured ? `<div class="notice">This build of the node has no built-in Jamendo app. Add one in Settings › This node › Service apps.</div>`
            : jam.signed_in ? `<div class="toolbar"><span class="pill ok account">Signed in${jam.name ? ' · ' + esc(jam.name) : ''}</span><button class="btn" id="jamimport">${icon('heart')} Add my Jamendo likes to Favourites</button><button class="btn ghost" id="jamout">Sign out</button></div>`
            : !jam.can_sign_in ? `<p class="muted small">Search and full-length playback need no account. Signing in to bring your Jamendo likes over needs your own Jamendo app with its secret in the node config (<code>[jamendo]</code>).</p>`
            : `<div class="toolbar"><button class="btn" id="jamin">${icon('zone')} Sign in to Jamendo</button><span class="muted small">Optional: search and full-length playback need no account. Signing in brings your likes and favourites to this node. The Jamendo app's redirect URL must be on the same domain as <code>${esc(jam.redirect_uri)}</code>.</span></div>`;
          let spot = null; if (active === 'spotify') { try { spot = await api('GET', '/sources/spotify/status'); } catch { spot = null; } if (state.view !== 'sources') return; }
          const account = active === 'jamendo' ? jamAccount : active === 'subsonic' ? subAccount : active === 'jellyfin' ? jfAccount : '';

          const body = !cur ? '' : cur.id === 'library'
            ? group('', row('Music folders, scans and matches', 'Your own files. Folders, scans, tags and MusicBrainz matches are in Your library.', '<button class="btn" data-view-go="library" data-view-arg="node">Open Your library › Node library</button>')
              + row('Play from', 'Which source plays a song first when several have it.', '<button class="btn ghost" data-goto-settings="playback">Open Settings › Zones and outputs</button>'))
            : group('', row(cur.enabled ? 'On' : 'Turned off', cur.enabled ? `Shown in search, on Home, on song and album pages and in the play order.` : `Hidden from search, Home, song and album pages and the play order. Its sign-in and settings are kept.`, srcSwitch(cur, ' on'))
                + (() => { const st = srcState({ ...cur, enabled: true }); return row('Status', esc(st.long), `<span class="pill ${st.cls === 'ok' ? 'ok' : st.cls === 'bad' ? 'bad' : ''}">${esc(st.label)}</span>`); })()
                + (cur.features ? row('What it does', (cur.features.length ? cur.features.map((f) => FEATURE_WORDS[f] || f).join(' · ') : 'Nothing yet') + (cur.stage === 'unofficial' ? '. Unofficial: a private API the service has not approved; it can stop working at any time.' : cur.stage === 'beta' ? '. New here; tell us what breaks.' : '.'), cur.stage && cur.stage !== 'stable' ? `<span class="pill ${cur.stage === 'unofficial' ? 'warn' : ''}">${esc(cur.stage === 'unofficial' ? 'Unofficial' : 'Beta')}</span>` : '') : ''))
              + (account ? group('Account', block('', '', account)) : '')
              + (cur.id === 'tidal' ? '<div id="tidalsignin"></div>' : '')
              + (cur.id === 'qobuz' ? '<div id="signin-qobuz"></div>' : '')
              + (cur.id === 'spotify' ? '<div id="signin-spotify"></div>' : '')
              + (cur.id === 'youtube' ? '<div id="signin-youtube"></div>' : '')
              + (cur.enabled && (!cur.features || cur.features.includes('search')) && cur.status !== 'needs_config' && cur.status !== 'browser' && !/^not configured/.test(cur.status) && (cur.id !== 'qobuz' || cur.status === 'signed in') && (cur.id !== 'soundcloud' || /^(ready|signed in)/.test(cur.status)) && (cur.id !== 'bandcamp' || /^(ready|signed in)/.test(cur.status)) ? group('Use', row(`Search ${esc(cur.name)}`, `Opens the one search, scoped to ${esc(cur.name)}.`, `<button class="btn" id="ssearchall">${icon('search')} Search ${esc(cur.name)}</button>`)) : '')
              + (cur.id === 'youtube' ? '<div id="ytzones"></div>' : '')
              + (cur.id === 'qobuz' ? '<div id="qobuzmode"></div>' : '')
              + (cur.id === 'bandcamp' ? '<div id="bcmode"></div>' : '')
              + (cur.id === 'spotify' ? '<div id="spotsoloist"></div><div id="splibrespot"></div>' : '')
              + (cur.id === 'soundcloud' ? '<div id="scmode"></div>' : '')
              + '<div id="srcmine"></div>';
          main.innerHTML = (phone && active ? '' : pageHead('Sources')) + `<div class="settings ${active ? 'has-panel' : 'no-panel'}">${nav}<div class="set-body-wrap">${cur ? `<section class="set-panel" id="src-${esc(cur.id)}" aria-labelledby="srctab-${esc(cur.id)}">${panelHead(cur)}${body}</section>` : ''}</div></div>`;
          $$('[data-srctab]', main).forEach((t) => t.onclick = () => setView('sources', { source: t.dataset.srctab }, phone ? 'push' : 'replace'));
          $$('[data-src-back]', main).forEach((b) => b.onclick = () => goBack('sources'));
          // The Sign in chooser of each source that has one (STEWARD-CREDENTIALS-REUSE.md)
          signinChooser(tidalSigninCfg($('#tidalsignin')), tidalSi);
          signinChooser(qobuzSigninCfg($('#signin-qobuz')));
          signinChooser(spotifySigninCfg($('#signin-spotify')));
          signinChooser(youtubeSigninCfg($('#signin-youtube')));
          // YouTube › On your speakers (unofficial): yt-dlp as an external program (STEWARD-DUAL-MODE.md)
          const yz = $('#ytzones');
          const fillYtZones = async () => {
            if (!yz || !yz.isConnected) return;
            let u; try { u = await api('GET', '/sources/youtube/unofficial'); } catch (e) { yz.innerHTML = ''; return; }
            if (!yz.isConnected) return;
            const y = u.ytdlp;
            const rows = [row('On your speakers (unofficial)', esc(u.warning), `<input type="checkbox" class="switch" role="switch" id="yzon" ${u.enabled ? 'checked' : ''} aria-label="YouTube on your speakers (unofficial)">`)];
            if (u.enabled || y) {
              rows.push(y
                ? row('yt-dlp', `${esc(y.version ? 'Version ' + y.version : 'Found')}: ${y.from === 'downloaded' ? 'downloaded by this node' : y.from === 'config' ? 'set in the node config' : 'installed on this machine'} (<code>${esc(y.path)}</code>).`, y.from === 'downloaded' ? '<button class="btn ghost sm" id="yzupdate">Update</button><button class="btn ghost sm" id="yzremove">Remove</button>' : '')
                : row('yt-dlp', u.can_download ? 'Needed to play YouTube on your speakers. The node downloads it from yt-dlp’s official GitHub releases and checks it against the release’s checksums.' : 'Needed to play YouTube on your speakers. Install yt-dlp on this machine, then reload this page.', u.can_download ? '<button class="btn primary sm" id="yzget">Download yt-dlp</button>' : ''));
              if (u.health) rows.push(row('Health', esc(u.health.ok ? `Working: ${u.health.detail}.` : `Last try: ${u.health.detail}`), `<span class="pill ${u.health.ok ? 'ok' : 'bad'}">${u.health.ok ? 'OK' : 'Problem'}</span>`));
            }
            yz.innerHTML = group('On your speakers', rows.join(''));
            $('#yzon').onchange = async (e) => { try { await api('PUT', '/sources/youtube/unofficial', { enabled: e.target.checked }); toast(e.target.checked ? 'YouTube can play on your speakers' : 'YouTube plays only in This browser'); } catch (er) { toast(er.message, 'error'); } render(); };
            const get = async (b, msg) => { b.disabled = true; b.textContent = 'Downloading…'; try { await api('POST', '/sources/youtube/unofficial/ytdlp'); toast(msg); } catch (er) { toast(er.message, 'error'); } render(); };
            const yg = $('#yzget'); if (yg) yg.onclick = () => get(yg, 'yt-dlp downloaded');
            const yu = $('#yzupdate'); if (yu) yu.onclick = () => get(yu, 'yt-dlp updated');
            const yr = $('#yzremove'); if (yr) yr.onclick = async () => { try { await api('DELETE', '/sources/youtube/unofficial/ytdlp'); toast('yt-dlp removed'); } catch (er) { toast(er.message, 'error'); } render(); };
          };
          fillYtZones();
          // Qobuz (unofficial-only): opt-in, warned, the owner's own app and sign-in (STEWARD-DUAL-MODE.md)
          const qm = $('#qobuzmode');
          const fillQobuz = async () => {
            if (!qm || !qm.isConnected) return;
            let u; try { u = await api('GET', '/sources/qobuz/unofficial'); } catch (e) { qm.innerHTML = ''; return; }
            if (!qm.isConnected) return;
            const broke = u.health && !u.health.ok;
            const rows = [row('Qobuz (unofficial)', esc(u.warning), `<input type="checkbox" class="switch" role="switch" id="qbon" ${u.enabled ? 'checked' : ''} aria-label="Qobuz (unofficial)">`)];
            if (broke) rows.push(`<div class="notice warn" role="status">Switched itself off: ${esc(u.health.detail)}. Qobuz links still open on qobuz.com.<div class="actions"><button class="btn sm" id="qbretry">Try again</button></div></div>`);
            if (u.enabled || u.signed_in || u.app_from) {
              rows.push(row('Quality', 'Max plays up to 24-bit/192 kHz where Qobuz has it and your plan allows.', `<select class="input" id="qbq" aria-label="Qobuz quality"><option value="max" ${u.quality === 'max' ? 'selected' : ''}>Max (up to 24-bit/192 kHz)</option><option value="cd" ${u.quality === 'cd' ? 'selected' : ''}>CD (16-bit/44.1 kHz)</option><option value="mp3" ${u.quality === 'mp3' ? 'selected' : ''}>MP3 (320 kbps)</option></select>`));
              if (u.health && u.health.ok && u.signed_in) rows.push(row('Health', `Working: ${esc(u.health.detail)}.`, '<span class="pill ok">OK</span>'));
            }
            qm.innerHTML = group('Unofficial mode', rows.join(''));
            const put = async (body, ok) => { try { await api('PUT', '/sources/qobuz/unofficial', body); if (ok) toast(ok); } catch (e) { toast(e.message, 'error'); } render(); };
            $('#qbon').onchange = (e) => put({ enabled: e.target.checked }, e.target.checked ? 'Qobuz is on' : 'Qobuz is off');
            const qr = $('#qbretry'); if (qr) qr.onclick = () => put({ enabled: true }, 'Trying Qobuz again');
            const qq = $('#qbq'); if (qq) qq.onchange = () => put({ quality: qq.value }, 'Quality saved');
          };
          fillQobuz();
          const bcm = $('#bcmode');
          const fillBandcamp = async () => {
            if (!bcm || !bcm.isConnected) return;
            let u; try { u = await api('GET', '/sources/bandcamp/unofficial'); } catch (e) { bcm.innerHTML = ''; return; }
            if (!bcm.isConnected) return;
            const FMT = { flac: 'FLAC', alac: 'ALAC', 'aiff-lossless': 'AIFF', wav: 'WAV', 'mp3-320': 'MP3 320', 'mp3-v0': 'MP3 V0', 'aac-hi': 'AAC', vorbis: 'Ogg Vorbis' };
            const rows = [row('Bandcamp (unofficial)', esc(u.warning), `<input type="checkbox" class="switch" role="switch" id="bcon" ${u.enabled ? 'checked' : ''} aria-label="Bandcamp (unofficial)">`)];
            if (u.health && !u.health.ok) rows.push(`<div class="notice warn" role="status">Switched itself off: ${esc(u.health.detail)}. Bandcamp links still open on bandcamp.com.<div class="actions"><button class="btn sm" id="bcretry">Try again</button></div></div>`);
            if (u.enabled) {
              rows.push(u.signed_in
                ? row('Signed in', `Your Bandcamp collection${u.user ? ' (' + esc(u.user) + ')' : ''}. The sign-in stays on the node and never comes back.`, '<button class="btn ghost sm" id="bcout">Sign out</button>')
                : `<form class="set-row set-block" id="bcin"><div class="set-label"><div class="set-name">Your purchases</div>${help('To download what you bought: sign in on bandcamp.com in your browser, open its cookies for bandcamp.com and copy the value of the one named identity. The node checks it once with Bandcamp and keeps it like a sign-in. Search and streams work without it.')}</div><div class="set-body"><div class="toolbar"><input class="input" name="identity" type="password" placeholder="identity cookie value" autocomplete="off" spellcheck="false" aria-label="Bandcamp identity cookie" required><button class="btn" type="submit">Save</button></div></div></form>`);
              rows.push(row('Download format', `Bought music downloads into <code class="path">${esc(u.folder)}</code>, a library folder. When a release lacks this format, the best one it has is used.`, `<select class="input" id="bcfmt" aria-label="Bandcamp download format">${u.formats.map((f) => `<option value="${esc(f)}" ${f === u.format ? 'selected' : ''}>${esc(FMT[f] || f)}</option>`).join('')}</select>`));
              rows.push(row('Streams', 'What Bandcamp’s own player plays: MP3 128 kbit/s, full tracks where the artist allows it.', ''));
              if (u.health && u.health.ok) rows.push(row('Health', `Working: ${esc(u.health.detail)}.`, '<span class="pill ok">OK</span>'));
            }
            bcm.innerHTML = group('Unofficial mode', rows.join(''));
            const put = async (body, ok) => { try { await api('PUT', '/sources/bandcamp/unofficial', body); if (ok) toast(ok); } catch (e) { toast(e.message, 'error'); } render(); };
            $('#bcon').onchange = (e) => put({ enabled: e.target.checked }, e.target.checked ? 'Bandcamp is on' : 'Bandcamp is off');
            const br = $('#bcretry'); if (br) br.onclick = () => put({ enabled: true }, 'Trying Bandcamp again');
            const bi = $('#bcin'); if (bi) bi.onsubmit = (e) => { e.preventDefault(); put({ identity: String(new FormData(bi).get('identity') || '').trim() }, 'Signed in to Bandcamp'); };
            const bo = $('#bcout'); if (bo) bo.onclick = () => put({ identity: '' }, 'Signed out of Bandcamp');
            const bf = $('#bcfmt'); if (bf) bf.onchange = () => put({ format: bf.value }, 'Download format saved');
          };
          fillBandcamp();
          if (cur.id === 'bandcamp' && cur.enabled && cur.status === 'signed in') {
            // Your purchases, each downloadable into the library.
            const box = document.createElement('div'); box.id = 'bcmine'; box.innerHTML = '<p class="muted small">Loading your Bandcamp collection…</p>'; ($('#srcmine') || main).appendChild(box);
            const draw = async () => {
              try {
                const [items, jobs] = await Promise.all([api('GET', '/sources/bandcamp/collection'), api('GET', '/sources/bandcamp/downloads')]);
                if (state.view !== 'sources' || state.sourceId !== 'bandcamp' || !box.isConnected) return;
                const job = Object.fromEntries(jobs.map((j) => [j.id, j]));
                const act = (p) => { const j = job[p.id]; if (j && !['done', 'failed'].includes(j.state)) return `<span class="muted small">${esc(j.state === 'downloading' && j.total ? `${Math.round(100 * j.bytes / j.total)}%` : j.state)}</span>`; if (j && j.state === 'failed') return `<span class="pill bad" title="${esc(j.error || '')}">Failed</span><button class="btn ghost sm" data-bcdl="${esc(p.id)}">Retry</button>`; if (p.downloaded || (j && j.state === 'done')) return '<span class="pill ok">In your library</span>'; return p.downloadable ? `<button class="btn sm" data-bcdl="${esc(p.id)}">${icon('download')} Download</button>` : '<span class="muted small">no download</span>'; };
                box.innerHTML = `<section class="section"><div class="section-head"><h3>Your purchases</h3><span class="muted small">${countOf(items.length, 'items')}</span></div>${items.length ? `<div class="list">${items.map((p) => `<div class="list-row ${p.item ? 'clickable' : ''}" ${p.item ? `data-sitem="${esc(p.item)}"` : ''}>${p.image ? `<img class="thumb" src="${esc(p.image)}" alt="" loading="lazy">` : '<div class="thumb"></div>'}<div class="grow"><div class="name">${esc(p.title)}</div><div class="sub">${esc([p.artist, p.kind].filter(Boolean).join(' · '))}</div></div>${act(p)}</div>`).join('')}</div>` : '<p class="muted">Nothing bought on Bandcamp yet.</p>'}</section>`;
                $$('[data-sitem]', box).forEach((b) => b.onclick = (e) => { if (e.target.closest('button')) return; setView('source-item', { source: 'bandcamp', id: b.dataset.sitem }); });
                $$('[data-bcdl]', box).forEach((b) => b.onclick = async (e) => { e.stopPropagation(); b.disabled = true; try { await api('POST', `/sources/bandcamp/collection/${encodeURIComponent(b.dataset.bcdl)}/download`); toast('Downloading into your library'); } catch (er) { toast(er.message, 'error'); } draw(); });
                if (jobs.some((j) => !['done', 'failed'].includes(j.state))) setTimeout(draw, 2000);
              } catch (e) { box.innerHTML = `<p class="muted small">Bandcamp collection: ${esc(e.message)}</p>`; }
            };
            draw();
          }
          const slo = $('#spotsoloist');
          const fillSoloist = async () => {
            if (!slo || !slo.isConnected) return;
            let u; try { u = await api('GET', '/sources/spotify/soloist'); } catch (e) { slo.innerHTML = ''; return; }
            if (!slo.isConnected) return;
            const b = u.soloist;
            const rows = [row('Spotify Soloist', esc(u.note), `<input type="checkbox" class="switch" role="switch" id="slon" ${u.enabled ? 'checked' : ''} ${u.supported ? '' : 'disabled'} aria-label="Spotify Soloist">`)];
            if (!u.supported) rows.push('<div class="notice">Spotify Soloist runs on Linux and macOS only, so this node can’t start it.</div>');
            else {
              if (u.health && !u.health.ok) rows.push(`<div class="notice warn" role="status">${esc(u.health.detail)}<div class="actions"><button class="btn sm" id="slretry">Try again</button></div></div>`);
              if (u.enabled && u.waiting_for) rows.push(`<div class="notice" role="status">Switched on, waiting for ${esc(u.waiting_for)}.</div>`);
              const stateText = !u.running ? (u.enabled ? 'Not running.' : 'Off.') : u.paired === false ? `Running as “${esc(u.device_name)}”. Nobody has picked it yet: open a Spotify app on this network, choose this device from the device picker and press play.` : u.paired ? `Running as “${esc(u.device_name)}”, paired with a Spotify account.` : `Starting as “${esc(u.device_name)}”…`;
              rows.push(row('Status', stateText, u.running ? '<span class="pill ok">Running</span><button class="btn ghost sm" id="slrestart">Restart</button>' : u.enabled ? '<span class="pill">Waiting</span>' : '<span class="pill">Off</span>'));
              if (u.running && u.now && u.now.title) {
                const n = u.now; const who = [n.artists.join(', '), n.album].filter(Boolean).join(' · ');
                rows.push(row(n.status === 'paused' ? 'Paused' : n.status === 'playing' ? 'Playing on this machine' : 'Loaded', `${esc(n.title)}${who ? ' · ' + esc(who) : ''}${n.volume != null ? ` · volume ${n.volume}` : ''}`, `<button class="btn ghost sm" id="slprev" aria-label="Previous">${icon('prev')}</button><button class="btn sm" id="sltoggle">${n.status === 'playing' ? 'Pause' : 'Play'}</button><button class="btn ghost sm" id="slnext" aria-label="Next">${icon('next')}</button>`));
              }
              rows.push(`<form class="set-row set-block" id="slkey"><div class="set-label"><div class="set-name">Your Soloist API key</div>${help('Each person makes their own key, with a Spotify Premium account, at developer.spotify.com › Dashboard › Spotify Soloist API Key (accept Spotify’s terms, then Generate). The node keeps it like a sign-in and never shows it again. Soloist takes the key only on its command line, so other users of this machine can see it while Soloist runs.')}</div><div class="set-body"><div class="toolbar">${u.key_set ? `<span class="pill ok">Saved</span><button type="button" class="btn ghost sm" id="slkeychange">Change</button><button type="button" class="btn ghost sm" id="slkeyforget">Forget</button>` : ''}<input class="input" name="key" type="password" placeholder="Soloist API key" autocomplete="off" spellcheck="false" aria-label="Spotify Soloist API key" ${u.key_set ? 'hidden' : 'required'}><button class="btn" type="submit" ${u.key_set ? 'hidden' : ''}>Save</button></div></div></form>`);
              rows.push(`<form class="set-row set-block" id="sldev"><div class="set-label"><div class="set-name">Device</div>${help('The name Spotify apps show for this machine, and, with PipeWire, which output Soloist uses (a node name or id; empty means the system default).')}</div><div class="set-body"><div class="toolbar"><input class="input" name="name" placeholder="Device name" maxlength="64" autocomplete="off" aria-label="Spotify Connect device name" value="${esc(u.device_name)}"><input class="input" name="pw" placeholder="PipeWire output (optional)" autocomplete="off" spellcheck="false" aria-label="PipeWire output" value="${esc(u.pipewire_device || '')}"><button class="btn" type="submit">Save</button></div></div></form>`);
              rows.push(b
                ? row('Soloist', `${esc(b.version ? b.version : 'Found')}: ${b.from === 'downloaded' ? `downloaded by this node${u.downloaded_at ? ' on ' + new Date(u.downloaded_at * 1000).toLocaleDateString() : ''}` : b.from === 'config' ? 'set in the node config' : 'installed on this machine'} (<code>${esc(b.path)}</code>). Spotify’s builds stop working 90 days after they were made.`, b.from === 'downloaded' ? '<button class="btn ghost sm" id="slupdate">Update</button><button class="btn ghost sm" id="slremove">Remove</button>' : '')
                : row('Soloist', u.can_download ? 'The node downloads Spotify’s build for this machine from Spotify’s own servers, when you ask. Spotify publishes no checksums; the node checks that the download runs. By using it you accept Spotify’s Terms and Conditions of Use.' : 'Spotify offers no build for this machine here. Install Soloist yourself and point <code>[spotify] soloist</code> in the node config at it, then reload this page.', u.can_download ? '<button class="btn primary sm" id="slget">Download Soloist</button>' : ''));
            }
            slo.innerHTML = group('Play on this machine', rows.join(''));
            const put = async (body, ok) => { try { await api('PUT', '/sources/spotify/soloist', body); if (ok) toast(ok); } catch (e) { toast(e.message, 'error'); } render(); };
            $('#slon').onchange = (e) => put({ enabled: e.target.checked }, e.target.checked ? 'Spotify Soloist is on' : 'Spotify Soloist is off');
            const sr = $('#slretry'); if (sr) sr.onclick = () => put({ enabled: true }, 'Starting Spotify Soloist again');
            const srs = $('#slrestart'); if (srs) srs.onclick = () => put({ enabled: true }, 'Restarting Spotify Soloist');
            const kf = $('#slkey'); if (kf) kf.onsubmit = (e) => { e.preventDefault(); put({ api_key: String(new FormData(kf).get('key') || '').trim() }, 'Soloist API key saved'); };
            const kc = $('#slkeychange'); if (kc) kc.onclick = () => { kf.querySelectorAll('[hidden]').forEach((x) => { x.hidden = false; }); kc.hidden = true; kf.querySelector('input').focus(); };
            const kg = $('#slkeyforget'); if (kg) kg.onclick = () => put({ api_key: '' }, 'Soloist API key forgotten');
            const df = $('#sldev'); if (df) df.onsubmit = (e) => { e.preventDefault(); const f = new FormData(df); put({ device_name: String(f.get('name') || '').trim(), pipewire_device: String(f.get('pw') || '').trim() }, 'Device saved'); };
            const get = async (btn, msg) => { btn.disabled = true; btn.textContent = 'Downloading…'; try { await api('POST', '/sources/spotify/soloist/binary'); toast(msg); } catch (er) { toast(er.message, 'error'); } render(); };
            const sg = $('#slget'); if (sg) sg.onclick = () => get(sg, 'Soloist downloaded');
            const su = $('#slupdate'); if (su) su.onclick = () => get(su, 'Soloist updated');
            const sv = $('#slremove'); if (sv) sv.onclick = async () => { try { await api('DELETE', '/sources/spotify/soloist/binary'); toast('Soloist removed'); } catch (er) { toast(er.message, 'error'); } render(); };
            const ctl = async (command) => { try { await api('POST', '/sources/spotify/soloist/control', { command }); } catch (er) { toast(er.message, 'error'); } render(); };
            const st = $('#sltoggle'); if (st) st.onclick = () => ctl(u.now && u.now.status === 'playing' ? 'pause' : 'play');
            const sp = $('#slprev'); if (sp) sp.onclick = () => ctl('prev');
            const sn = $('#slnext'); if (sn) sn.onclick = () => ctl('next');
          };
          fillSoloist();
          const lso = $('#splibrespot');
          const fillLibrespot = async () => {
            if (!lso || !lso.isConnected) return;
            let u; try { u = await api('GET', '/sources/spotify/librespot'); } catch (e) { lso.innerHTML = ''; return; }
            if (!lso.isConnected) return;
            const b = u.librespot;
            const rows = [row('librespot (unofficial)', `${esc(u.warning)} Unlike Soloist, it can play into one of the node’s zones.`, `<input type="checkbox" class="switch" role="switch" id="lson" ${u.enabled ? 'checked' : ''} ${u.supported ? '' : 'disabled'} aria-label="librespot (unofficial)">`)];
            if (u.health && !u.health.ok) rows.push(`<div class="notice warn" role="status">${esc(u.health.detail)}<div class="actions"><button class="btn sm" id="lsretry">Try again</button></div></div>`);
            if (u.enabled && u.waiting_for) rows.push(`<div class="notice" role="status">Switched on, waiting for ${esc(u.waiting_for)}.</div>`);
            if (u.enabled || b) {
              rows.push(row('librespot', b ? `${esc(b.version || 'Found')}: ${b.from === 'config' ? 'set in the node config' : b.from === 'data' ? 'in the node’s bin folder' : 'installed on this machine'} (<code>${esc(b.path)}</code>).` : 'Not installed. librespot publishes no ready-made builds: install it with <code>cargo install librespot</code>, your distribution’s package or Homebrew, then reload this page (or point <code>[spotify] librespot</code> in the node config at it).', `<a class="btn ghost sm" href="https://github.com/librespot-org/librespot" target="_blank" rel="noopener">About librespot</a>`));
              const zoneOpts = u.zones.map((z) => `<option value="zone:${esc(z.id)}" ${u.output === 'zone' && u.zone_id === z.id ? 'selected' : ''}>Zone: ${esc(z.name)}</option>`).join('');
              rows.push(`<form class="set-row set-block" id="lsdev"><div class="set-label"><div class="set-name">Device and output</div>${help('The name Spotify apps show, and where the sound goes: this machine’s own sound output, or one of the node’s zones (librespot then pipes its audio into the node, and the zone starts, pauses and stops with it).')}</div><div class="set-body"><div class="toolbar"><input class="input" name="name" maxlength="64" placeholder="Device name" autocomplete="off" aria-label="librespot device name" value="${esc(u.device_name)}"><select class="input" name="out" aria-label="librespot output"><option value="local" ${u.output === 'local' ? 'selected' : ''}>This machine’s sound</option>${zoneOpts}</select><button class="btn" type="submit">Save</button></div></div></form>`);
              rows.push(row('Status', !u.running ? (u.enabled ? 'Not running.' : 'Off.') : `Running as “${esc(u.device_name)}”${u.user ? `, picked by ${esc(u.user)}` : '. Pick it in a Spotify app on this network'}.${u.now && u.now.title ? ` ${u.now.status === 'paused' ? 'Paused' : 'Playing'}: ${esc(u.now.title)}${u.now.artists && u.now.artists.length ? ' · ' + esc(u.now.artists.join(', ')) : ''}.` : ''}`, u.running ? '<span class="pill ok">Running</span>' : `<span class="pill">${u.enabled ? 'Waiting' : 'Off'}</span>`));
            }
            lso.innerHTML = group('librespot', rows.join(''));
            const put = async (body, ok) => { try { await api('PUT', '/sources/spotify/librespot', body); if (ok) toast(ok); } catch (e) { toast(e.message, 'error'); } render(); };
            $('#lson').onchange = (e) => put({ enabled: e.target.checked }, e.target.checked ? 'librespot is on' : 'librespot is off');
            const lr = $('#lsretry'); if (lr) lr.onclick = () => put({ enabled: true }, 'Starting librespot again');
            const ld = $('#lsdev'); if (ld) ld.onsubmit = (e) => { e.preventDefault(); const f = new FormData(ld); const out = String(f.get('out') || 'local'); put({ device_name: String(f.get('name') || '').trim(), output: out === 'local' ? 'local' : 'zone', zone_id: out.startsWith('zone:') ? out.slice(5) : '' }, 'librespot saved'); };
          };
          fillLibrespot();
          const scm = $('#scmode');
          const fillSoundCloud = async () => {
            if (!scm || !scm.isConnected) return;
            let u; try { u = await api('GET', '/sources/soundcloud/unofficial'); } catch (e) { scm.innerHTML = ''; return; }
            if (!scm.isConnected) return;
            const broke = u.health && !u.health.ok;
            const rows = [row('SoundCloud (unofficial)', esc(u.warning), `<input type="checkbox" class="switch" role="switch" id="scon" ${u.enabled ? 'checked' : ''} aria-label="SoundCloud (unofficial)">`)];
            if (broke) rows.push(`<div class="notice warn" role="status">Switched itself off: ${esc(u.health.detail)}. SoundCloud links still open on soundcloud.com.<div class="actions"><button class="btn sm" id="scretry">Try again</button></div></div>`);
            if (u.client_from) rows.push(row('Client ID', `<code class="path">${esc(u.client_id || '')}</code> · ${u.client_from === 'config' ? 'set in the node config.' : 'set here. SoundCloud can stop accepting a client at any time; then enter another one.'}`, `<button class="btn ghost sm" id="scclientchange">Change</button>${u.client_from === 'settings' ? '<button class="btn ghost sm" id="scclientforget">Forget</button>' : ''}`));
            rows.push(`<form class="set-row set-block" id="scclient" ${u.client_from ? 'hidden' : ''}><div class="set-label"><div class="set-name">Client ID</div>${help('The node has no built-in SoundCloud client and reads none off soundcloud.com: enter a SoundCloud web client ID you are allowed to use. Until you do, SoundCloud stays off.')}</div><div class="set-body"><div class="toolbar"><input class="input" name="id" placeholder="Client ID" autocomplete="off" spellcheck="false" aria-label="SoundCloud client ID" required><button class="btn" type="submit">Save</button></div></div></form>`);
            if (u.client_from) {
              rows.push(u.signed_in
                ? row('Signed in', `Your likes and playlists come from SoundCloud account ${esc(u.user || '')}. The token stays on the node and never comes back.`, '<button class="btn ghost sm" id="sctokenforget">Sign out</button>')
                : `<form class="set-row set-block" id="sctoken"><div class="set-label"><div class="set-name">Your likes and playlists</div>${help('Optional: paste your own SoundCloud sign-in token (the one your browser uses while you are signed in). The node checks it once with SoundCloud and keeps it like a sign-in; it never shows it again. Without it, search and playback still work.')}</div><div class="set-body"><div class="toolbar"><input class="input" name="token" type="password" placeholder="Sign-in token (optional)" autocomplete="off" spellcheck="false" aria-label="SoundCloud sign-in token" required><button class="btn" type="submit">Save</button></div></div></form>`);
              rows.push(row('Streams', 'MP3 as SoundCloud serves it, progressive or stitched from HLS. Tracks SoundCloud encrypts (Go+) are skipped; a track SoundCloud only snips plays its 30-second preview.', ''));
              if (u.health && u.health.ok) rows.push(row('Health', `Working: ${esc(u.health.detail)}.`, '<span class="pill ok">OK</span>'));
            }
            scm.innerHTML = group('Unofficial mode', rows.join(''));
            const put = async (body, ok) => { try { await api('PUT', '/sources/soundcloud/unofficial', body); if (ok) toast(ok); } catch (e) { toast(e.message, 'error'); } render(); };
            $('#scon').onchange = (e) => put({ enabled: e.target.checked }, e.target.checked ? 'SoundCloud is on' : 'SoundCloud is off');
            const sr = $('#scretry'); if (sr) sr.onclick = () => put({ enabled: true }, 'Trying SoundCloud again');
            const cf = $('#scclient'); if (cf) cf.onsubmit = (e) => { e.preventDefault(); put({ client_id: String(new FormData(cf).get('id') || '').trim() }, 'SoundCloud client saved'); };
            const cc = $('#scclientchange'); if (cc) cc.onclick = () => { cf.hidden = false; cf.querySelector('input').focus(); };
            const cg = $('#scclientforget'); if (cg) cg.onclick = () => put({ client_id: '' }, 'SoundCloud client forgotten');
            const tf = $('#sctoken'); if (tf) tf.onsubmit = (e) => { e.preventDefault(); put({ token: String(new FormData(tf).get('token') || '').trim() }, 'Signed in to SoundCloud'); };
            const tg = $('#sctokenforget'); if (tg) tg.onclick = () => put({ token: '' }, 'Signed out of SoundCloud');
          };
          fillSoundCloud();
          if (cur.id === 'soundcloud' && cur.enabled && cur.status === 'signed in') {
            // Your likes and playlists, with imports into favourites and Pubky playlists.
            const box = document.createElement('div'); box.id = 'scmine'; box.innerHTML = '<p class="muted small">Loading your SoundCloud likes…</p>'; ($('#srcmine') || main).appendChild(box);
            (async () => {
              try {
                const [likes, pls] = await Promise.all([api('GET', '/sources/soundcloud/likes'), api('GET', '/sources/soundcloud/playlists')]);
                if (state.view !== 'sources' || state.sourceId !== 'soundcloud' || !box.isConnected) return;
                const hit = (h) => `<div class="list-row clickable" data-sitem="${esc(h.id)}">${h.image ? `<img class="thumb" src="${esc(h.image)}" alt="" loading="lazy">` : '<div class="thumb"></div>'}<div class="grow"><div class="name">${esc(h.title)}</div><div class="sub">${esc([h.creator, h.year].filter(Boolean).join(' · '))}</div></div><span class="muted small">${esc(h.kind === 'item' ? 'set' : h.kind)}</span></div>`;
                box.innerHTML = `<section class="section"><div class="section-head"><h3>Your likes</h3><span class="muted small">${countOf(likes.tracks.length, 'tracks')} · ${countOf(likes.playlists.length, 'sets')}</span><button class="btn sm" id="scfav" title="Favourite them here too, by their SoundCloud link">${icon('heart')} Import as favourites</button></div>${likes.tracks.length || likes.playlists.length ? `<div class="list">${likes.playlists.slice(0, 20).map(hit).join('')}${likes.tracks.slice(0, 50).map(hit).join('')}</div>` : '<p class="muted">Nothing liked on SoundCloud yet.</p>'}</section>
                  <section class="section"><div class="section-head"><h3>Your playlists</h3><span class="muted small">import one as a private Pubky playlist</span></div>${pls.length ? `<div class="list">${pls.map((p) => `<div class="list-row"><div class="grow"><div class="name">${esc(p.name)}</div><div class="sub">${countOf(p.items, 'tracks')}${p.description ? ' · ' + esc(p.description) : ''}</div></div><button class="btn sm" data-scpl="${esc(p.id)}">${icon('download')} Import</button></div>`).join('')}</div>` : '<p class="muted">No sets of your own on SoundCloud.</p>'}</section>`;
                $$('[data-sitem]', box).forEach((b) => b.onclick = () => setView('source-item', { source: 'soundcloud', id: b.dataset.sitem }));
                $('#scfav').onclick = async () => { try { const r = await api('POST', '/sources/soundcloud/likes/import'); toast(`${r.imported} favourites imported${r.skipped ? `, ${r.skipped} skipped` : ''}`); loadFavourites().catch(() => {}); } catch (e) { toast(e.message, 'error'); } };
                $$('[data-scpl]', box).forEach((b) => b.onclick = () => importPlaylist(b, 'soundcloud', b.dataset.scpl));
              } catch (e) { box.innerHTML = `<p class="muted small">SoundCloud likes: ${esc(e.message)}</p>`; }
            })();
          }
          $$('[data-goto-settings]', main).forEach((b) => b.onclick = () => setView('settings', b.dataset.gotoSettings));
          $$('[data-view-go]', main).forEach((b) => b.onclick = () => setView(b.dataset.viewGo, b.dataset.viewArg || null));
          $$('[data-src-on]', main).forEach((sw) => sw.onchange = async () => {
            const id = sw.dataset.srcOn; const want = sw.checked;
            try { await api('PUT', `/sources/${encodeURIComponent(id)}/enabled`, { enabled: want }); applySourcesOff(); homeCache.clear(); toast(`${srcName(id)} ${want ? 'is on' : 'is off: hidden everywhere until you switch it on'}`); render(); } catch (e) { sw.checked = !want; toast(e.message, 'error'); }
          });
          const sf = $('#subform'); if (sf) sf.onsubmit = async (e) => { e.preventDefault(); const f = new FormData(sf); const b = $('#subconnect'); b.disabled = true; try { const v = await api('PUT', '/sources/subsonic/server', { url: f.get('url'), user: f.get('user'), password: f.get('password') }); state.sources = null; toast(`Connected to ${v.name || 'your server'}`); render(); } catch (er) { toast(er.message, 'error'); b.disabled = false; } };
          const jff = $('#jfform'); if (jff) jff.onsubmit = async (e) => { e.preventDefault(); const f = new FormData(jff); const b = $('#jfin'); b.disabled = true; try { const v = await api('PUT', '/sources/jellyfin/server', { url: f.get('url'), user: f.get('user'), password: f.get('password') || '' }); state.sources = null; toast(`Signed in to ${v.server_name || 'Jellyfin'}`); render(); } catch (er) { toast(er.message, 'error'); b.disabled = false; } };
          const jfo = $('#jfout'); if (jfo) jfo.onclick = async () => { try { await api('DELETE', '/sources/jellyfin/server'); state.sources = null; toast('Signed out of Jellyfin'); render(); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-jfpl]', main).forEach((b) => b.onclick = async () => { b.disabled = true; try { const r = await api('POST', `/sources/jellyfin/playlists/${encodeURIComponent(b.dataset.jfpl)}/import`); state.playlists = await api('GET', '/playlists'); toast(`Imported “${r.playlist.summary.name}”: ${countOf(r.imported, 'songs')}${r.skipped ? `, ${r.skipped} without an id skipped` : ''}`); } catch (e) { toast(e.message, 'error'); } b.disabled = false; });
          const sfg = $('#subforget'); if (sfg) sfg.onclick = async () => { try { await api('DELETE', '/sources/subsonic/server'); state.sources = null; toast('Server forgotten'); render(); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-subpl]', main).forEach((b) => b.onclick = async () => { b.disabled = true; try { const r = await api('POST', `/sources/subsonic/playlists/${encodeURIComponent(b.dataset.subpl)}/import`); state.playlists = await api('GET', '/playlists'); toast(`Imported “${r.playlist.summary.name}”: ${countOf(r.imported, 'songs')}${r.skipped ? `, ${r.skipped} without an id skipped` : ''}`); } catch (e) { toast(e.message, 'error'); } b.disabled = false; });
          const ji = $('#jamin'); if (ji) ji.onclick = async () => { try { const r = await api('POST', '/sources/jamendo/login'); location.href = r.url; } catch (e) { toast(e.message, 'error'); } };
          const jo = $('#jamout'); if (jo) jo.onclick = async () => { try { await api('POST', '/sources/jamendo/logout'); toast('Jamendo signed out'); render(); } catch (e) { toast(e.message, 'error'); } };
          const jm = $('#jamimport'); if (jm) jm.onclick = async () => { jm.disabled = true; try { const r = await api('POST', '/sources/jamendo/collection/import'); toast(`${r.imported} Jamendo song${r.imported === 1 ? '' : 's'} added to Favourites`); } catch (e) { toast(e.message, 'error'); } jm.disabled = false; };
          if (spot && spot.signed_in && cur.enabled) {
            const box = document.createElement('div'); box.innerHTML = '<p class="muted small">Loading your Spotify library…</p>'; ($('#srcmine') || main).appendChild(box);
            (async () => {
              try {
                const [col, pls, devs] = await Promise.all([api('GET', '/sources/spotify/collection'), api('GET', '/sources/spotify/playlists'), api('GET', '/sources/spotify/devices').catch(() => [])]);
                if (state.view !== 'sources' || state.sourceId !== 'spotify' || !box.isConnected) return;
                const albums = col.items.filter((h) => h.kind === 'album'); const tracks = col.items.filter((h) => h.kind === 'track');
                const hit = (h) => `<div class="list-row clickable" data-sitem="${esc(h.id)}">${h.image ? `<img class="thumb" src="${esc(h.image)}" alt="" loading="lazy">` : '<div class="thumb"></div>'}<div class="grow"><div class="name">${esc(h.title)}${h.library_track_id || h.library_album_id ? ' <span class="pill ok">in library</span>' : ''}</div><div class="sub">${esc([h.creator, h.year].filter(Boolean).join(' · '))}</div></div><button class="icon-btn" data-splay="${esc(h.id)}" title="Play on Spotify">${icon('play')}</button></div>`;
                box.innerHTML = `<section class="section"><div class="section-head"><h3>Your devices</h3><span class="muted small">Spotify Connect</span></div>${devs.length ? `<div class="chips">${devs.map((d) => `<span class="chip ${d.is_active ? 'on' : ''}">${esc(d.name)} <span class="muted">${esc(d.type)}${d.is_active ? ' · active' : ''}</span></span>`).join('')}</div>` : '<p class="muted small">No Spotify device online. Open Spotify on a phone, computer or speaker and it appears here.</p>'}</section>
                  <section class="section"><div class="section-head"><h3>Your library</h3><span class="muted small">${countOf(tracks.length, 'tracks')} · ${countOf(albums.length, 'albums')}</span><button class="btn sm" id="spotfav" title="Favourite them here too, by ISRC where Spotify has one">${icon('heart')} Import as favourites</button></div>${albums.length || tracks.length ? `<div class="list">${albums.slice(0, 20).map(hit).join('')}${tracks.slice(0, 50).map(hit).join('')}</div>` : '<p class="muted">Nothing saved on Spotify yet.</p>'}</section>
                  <section class="section"><div class="section-head"><h3>Your playlists</h3><span class="muted small">import one as a private Pubky playlist</span></div>${pls.length ? `<div class="list">${pls.map((p) => `<div class="list-row">${p.image ? `<img class="thumb" src="${esc(p.image)}" alt="" loading="lazy">` : ''}<div class="grow"><div class="name">${esc(p.name)}</div><div class="sub">${countOf(p.items, 'tracks')}${p.description ? ' · ' + esc(p.description) : ''}</div></div><button class="btn sm" data-spl="${esc(p.id)}">${icon('download')} Import</button></div>`).join('')}</div>` : '<p class="muted">No playlists on Spotify.</p>'}</section>`;
                $$('[data-sitem]', box).forEach((b) => b.onclick = () => setView('source-item', { source: 'spotify', id: b.dataset.sitem }));
                $$('[data-splay]', box).forEach((b) => b.onclick = (e) => { e.stopPropagation(); spotifyPlay([b.dataset.splay], b); });
                $('#spotfav').onclick = async () => { try { const r = await api('POST', '/sources/spotify/collection/import'); toast(`${r.imported} favourites imported${r.skipped ? `, ${r.skipped} skipped` : ''}`); loadFavourites().catch(() => {}); } catch (e) { toast(e.message, 'error'); } };
                $$('[data-spl]', box).forEach((b) => b.onclick = () => importPlaylist(b, 'spotify', b.dataset.spl));
              } catch (e) { box.innerHTML = `<p class="muted small">Spotify library: ${esc(e.message)}</p>`; }
            })();
          }
          if (tidal && tidal.signed_in && cur.enabled) {
            // Your collection and playlists, with imports into the node's favourites and Pubky playlists.
            const box = document.createElement('div'); box.id = 'tidalmine'; box.innerHTML = '<p class="muted small">Loading your TIDAL collection…</p>'; ($('#srcmine') || main).appendChild(box);
            (async () => {
              try {
                const [col, pls] = await Promise.all([api('GET', '/sources/tidal/collection'), api('GET', '/sources/tidal/playlists')]);
                if (state.view !== 'sources' || state.sourceId !== 'tidal' || !box.isConnected) return;
                const hit = (h) => `<div class="list-row clickable" data-sitem="${esc(h.id)}">${h.image ? `<img class="thumb" src="${esc(h.image)}" alt="" loading="lazy">` : '<div class="thumb"></div>'}<div class="grow"><div class="name">${esc(h.title)}</div><div class="sub">${esc([h.creator, h.year].filter(Boolean).join(' · '))}</div></div><span class="muted small">${esc(h.kind)}</span></div>`;
                box.innerHTML = `<section class="section"><div class="section-head"><h3>Your collection</h3><span class="muted small">${countOf(col.tracks.length, 'tracks')} · ${countOf(col.albums.length, 'albums')}</span><button class="btn sm" id="tidalfav" title="Favourite them here too: by ISRC where TIDAL has one, else by the TIDAL link">${icon('heart')} Import as favourites</button></div>${col.albums.length || col.tracks.length ? `<div class="list">${col.albums.slice(0, 20).map(hit).join('')}${col.tracks.slice(0, 50).map(hit).join('')}</div>` : '<p class="muted">Nothing in your TIDAL collection yet.</p>'}</section>
                  <section class="section"><div class="section-head"><h3>Your playlists</h3><span class="muted small">import one as a private Pubky playlist</span></div>${pls.length ? `<div class="list">${pls.map((p) => `<div class="list-row"><div class="grow"><div class="name">${esc(p.name)}</div><div class="sub">${countOf(p.items, 'tracks')}${p.description ? ' · ' + esc(p.description) : ''}</div></div><button class="btn sm" data-tpl="${esc(p.id)}">${icon('download')} Import</button></div>`).join('')}</div>` : '<p class="muted">No playlists of your own on TIDAL.</p>'}</section>`;
                $$('[data-sitem]', box).forEach((b) => b.onclick = () => setView('source-item', { source: 'tidal', id: b.dataset.sitem }));
                $('#tidalfav').onclick = async () => { try { const r = await api('POST', '/sources/tidal/collection/import'); toast(`${r.imported} favourites imported${r.skipped ? `, ${r.skipped} skipped` : ''}`); loadFavourites().catch(() => {}); } catch (e) { toast(e.message, 'error'); } };
                $$('[data-tpl]', box).forEach((b) => b.onclick = () => importPlaylist(b, 'tidal', b.dataset.tpl));
              } catch (e) { box.innerHTML = `<p class="muted small">TIDAL collection: ${esc(e.message)}</p>`; }
            })();
          }
          const ssa = $('#ssearchall'); if (ssa) ssa.onclick = () => { state.searchScope = cur.id; search.placeholder = `Search ${cur.name}…`; search.focus(); search.select(); if (search.value.trim()) runSearch(); else { const box = search.closest('.search') || search; box.classList.remove('nudge'); void box.offsetWidth; box.classList.add('nudge'); } };
          break;
        }
        case 'source-artist': {
          loading();
          const { source, id } = state.viewArg;
          const a = await api('GET', `/sources/${encodeURIComponent(source)}/artists/${encodeURIComponent(id)}`);
          if (state.view !== 'source-artist') return;
          main.innerHTML = backBtn('search', 'Back') + `<div class="artist-hero"><div class="portrait" style="background:hsl(${hue(a.name)} 35% 28%)">${a.image ? `<img src="${esc(a.image)}" alt="" data-err="remove">` : esc(initials(a.name))}</div><div class="grow"><div class="page-head"><div><div class="eyebrow">Artist · ${esc(source.toUpperCase())}</div><h1>${esc(a.name)}</h1><div class="muted">${countOf(a.albums.length, 'albums')} · <a href="${esc(webUrl(a.url))}" target="_blank" rel="noopener">open</a>${a.library_artist_id ? ` · <button class="link" data-artist="${a.library_artist_id}">in your library</button>` : ''}</div></div></div></div></div>` + (a.albums.length ? `<div class="grid">${a.albums.map((h) => `<button class="tile" data-sitem="${esc(h.id)}" title="${esc(h.title)}"><div class="art">${h.image ? `<img loading="lazy" alt="" src="${esc(h.image)}">` : ''}${coverBadge(source, true)}${h.library_album_id ? '<span class="src-badge lib">in library</span>' : ''}</div><div class="t">${esc(h.title)}</div><div class="a">${esc([h.year].filter(Boolean).join(' · '))}</div>${coverFrom(source)}</button>`).join('')}</div>` : '<p class="muted">No albums listed.</p>');
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
          main.innerHTML = backBtn('sources', srcName) + `<div class="artist-hero"><div class="portrait" style="border-radius:12px;background:hsl(${hue(it.title)} 35% 28%)">${it.image ? `<img src="${esc(it.image)}" alt="" data-err="remove">` : esc(initials(it.title))}</div><div class="grow"><div class="page-head"><div><div class="eyebrow">${esc(srcName)}</div><h1>${esc(it.title)}</h1><div class="muted">${esc([it.creator, it.year, it.tracks.length + (it.tracks.length === 1 ? ' track' : ' tracks')].filter(Boolean).join(' · '))} · <a href="${esc(webUrl(it.url))}" target="_blank" rel="noopener">open</a>${it.licence ? (/^https?:/.test(it.licence) ? ` · <a href="${esc(webUrl(it.licence))}" target="_blank" rel="noopener">licence</a>` : ` · ${esc(it.licence)}`) : ''}</div></div><div class="actions"><button class="btn primary" id="aplay">${icon('play')} Play</button><button class="btn" id="aadd">${icon('plus')} Add to queue</button><button class="btn" id="amore" title="Add to playlist">${icon('playlist')} Playlist</button>${it.library_album_id ? `<button class="btn ghost" id="ainlib" title="You already have this album">${icon('disc')} In your library</button>` : ''}${canHeart ? `<button class="btn heart ${state.favUris.has(it.url) ? 'on' : ''}" id="aheart">${icon(state.favUris.has(it.url) ? 'heart-fill' : 'heart')} ${state.favUris.has(it.url) ? 'Favourited' : 'Favourite'}</button>` : ''}</div></div>${it.description ? `<p class="bio">${esc(it.description.slice(0, 600))}${it.description.length > 600 ? '…' : ''}</p>` : ''}</div></div>`
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
          const row = (st, isSaved) => `<div class="list-row clickable" data-st-row="${esc(st.id)}"><button class="icon-btn" data-st-play="${esc(st.id)}" title="Play">${icon('play')}</button><div class="avatar" style="background:hsl(${hue(st.name)} 35% 28%);overflow:hidden">${st.favicon ? `<img src="${esc(st.favicon)}" alt="" style="width:100%;height:100%;object-fit:cover" data-err="remove">` : esc(initials(st.name))}</div><div class="grow"><div class="name">${esc(st.name)}</div><div class="sub">${esc([st.country, st.codec ? st.codec + (st.bitrate ? ' ' + st.bitrate + ' kbps' : '') : '', (st.tags || []).slice(0, 4).join(', ')].filter(Boolean).join(' · '))}</div></div>${isSaved ? `<button class="btn sm ghost" data-st-del="${esc(st.id)}">Remove</button>` : saved.some((x) => x.url === st.url) ? '<span class="pill ok">saved</span>' : `<button class="btn sm" data-st-save="${esc(st.id)}">Save</button>`}${webUrl(st.homepage) ? `<a class="icon-btn" href="${esc(webUrl(st.homepage))}" target="_blank" rel="noopener" title="Homepage">${icon('external')}</a>` : ''}</div>`;
          const results = state.radioResults || { q: '', stations: [] };
          main.innerHTML = pageHead('Radio', 'internet radio via radio-browser.info')
            + `<div class="toolbar"><input class="input" id="rq" placeholder="Search stations by name (jazz, FIP, BBC 6…)" value="${esc(results.q)}" style="width:360px"><button class="btn primary" id="rsearch">${icon('search')} Search</button><span class="muted small">Streams play on any zone; lossy codecs show as Lossy in the signal path.</span></div>`
            + (saved.length ? `<section class="section"><div class="section-head"><h3>Saved stations</h3></div><div class="list">${saved.map((st) => row(st, true)).join('')}</div></section>` : '')
            + (results.q ? `<section class="section"><div class="section-head"><h3>Results for “${esc(results.q)}”</h3></div>${results.stations.length ? `<div class="list">${results.stations.map((st) => row(st, false)).join('')}</div>` : '<p class="muted">No stations matched.</p>'}</section>` : (saved.length ? '' : emptyState('radio', 'No stations yet', 'Search the directory above and save the ones you like.')));
          const all = [...saved, ...results.stations];
          const byId = (id) => all.find((x) => x.id === id);
          const doSearch = async () => { const q = $('#rq').value.trim(); if (!q) return; $('#rsearch').disabled = true; try { const r = await api('GET', '/radio/search?limit=40&q=' + encodeURIComponent(q)); state.radioResults = { q, stations: r.stations }; render(); } catch (e) { toast(e.message, 'error'); $('#rsearch').disabled = false; } };
          $('#rsearch').onclick = doSearch; $('#rq').onkeydown = (e) => { if (e.key === 'Enter') doSearch(); }; needsText($('#rq'), $('#rsearch'));
          $$('[data-st-play]', main).forEach((b) => b.onclick = () => playStation(byId(b.dataset.stPlay)));
          // The whole row plays the station, not only the small play button (fresh-eyes review)
          $$('[data-st-row]', main).forEach((r) => r.onclick = (e) => { if (e.target.closest('button, a, input')) return; playStation(byId(r.dataset.stRow)); });
          $$('[data-st-save]', main).forEach((b) => b.onclick = async () => { try { await api('PUT', '/radio/stations', byId(b.dataset.stSave)); toast('Station saved'); render(); } catch (e) { toast(e.message, 'error'); } });
          $$('[data-st-del]', main).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/radio/stations/${encodeURIComponent(b.dataset.stDel)}`); render(); } catch (e) { toast(e.message, 'error'); } });
          break;
        }
        case 'item': {
          await pubkyUser();
          const { kind, id } = state.viewArg;
          loading();
          let pg;
          try { [pg] = await Promise.all([api('GET', `/item/${kind}/${encodeURIComponent(id)}`), loadSvcRank()]); }
          catch (e) { main.innerHTML = backBtn(kind === 'artist' ? 'artists' : 'albums', kind === 'artist' ? 'Artists' : 'Albums') + `<div class="notice">${esc(e.message)}</div>`; break; }
          if (state.view !== 'item' || state.viewArg.id !== id) return;
          // The node leaves empty lists out of its answer.
          for (const k of ['songs', 'albums', 'versions', 'also_on', 'about', 'open_on', 'connect']) pg[k] = pg[k] || [];

          state.itemTitle = pg.title || `Untitled on ${pg.source}`; state.itemSection = pg.section;
          $$('.nav-item').forEach((b) => b.classList.toggle('on', b.dataset.view === pg.section));
          const fallback = { artist: pg.artist && pg.artist.name, album: kind === 'album' ? pg.title : '', image: pg.image };
          const songs = pg.songs || [];
          const target = pg.target;
          // What comments are about: the song's own link, else this item's (a web link only)
          const commentUri = [pg.song_uri, target && target.uri, pg.link].find((u) => /^https?:\/\//.test(u || '')) || '';
          const qs = new URLSearchParams(Object.entries(target).map(([k, v]) => [k, String(v)])).toString();
          // Tags go on the song (every source) or on exactly this source's link (STEWARD-UX-INSTANCES 3)
          const songRef = target.track_id != null || target.album_id != null || target.artist_id != null ? target : pg.song_uri ? { uri: pg.song_uri } : target;
          const linkRef = pg.link && pg.link !== songRef.uri ? { uri: pg.link } : null;
          const hereName = pg.source === 'Your library' ? 'your library' : pg.source;
          const refQs = (r) => new URLSearchParams(Object.entries(r).map(([k, v]) => [k, String(v)])).toString();
          const [myTags, linkTags, myReview] = isListener() ? [null, [], undefined] : await Promise.all([
            // An item with no identity yet (needs_match): the "match it first" message, no editor
            api('GET', `/tags?${refQs(songRef)}`).then((r) => (r.needs_match ? null : r.tags)).catch(() => null),
            linkRef ? api('GET', `/tags?${refQs(linkRef)}`).then((r) => r.tags).catch(() => []) : Promise.resolve([]),
            api('GET', `/reviews?${qs}`).then((r) => (r.needs_match ? undefined : (r.reviews || [])[0] || null)).catch(() => undefined),
          ]);
          if (state.view !== 'item' || state.viewArg.id !== id) return;
          const favKey = pg.uri; const fav = !!(state.favUris && ((favKey && state.favUris.has(favKey)) || (target.track_id != null && state.favUris.has(`track:${target.track_id}`)) || (target.album_id != null && state.favUris.has(`album:${target.album_id}`)) || (target.artist_id != null && state.favUris.has(`artist:${target.artist_id}`))));
          const sectionName = { albums: 'Albums', artists: 'Artists', tracks: 'Tracks', home: 'Home', playlists: 'Playlists' }[pg.section] || 'Home';
          const pf = pg.plays_from;
          const where = pf ? (pf.service === 'library' ? 'your library' : (SERVICE_NAMES[pf.service] || pf.service) + (pf.mode === 'preview' ? ' (previews)' : '')) : null;
          // Honest about both: where this page is from, and how it plays here (STEWARD-UX-INSTANCES 7)
          const ownSvc = pg.source_id || (kind && String(id).startsWith('lib-') ? 'library' : 'link');
          const isGlobal = ownSvc === 'global';
          const ownName = ownSvc === 'library' ? 'Your library' : pg.source;
          // The cover's badge: an album or song page says where it is from; an artist has no cover
          const heroBadge = kind === 'artist' ? '' : coverBadge(ownSvc);
          // What Play will do, from the node's one decision (plays_from + plays_note)
          const playsHere = !pf ? (pg.connect.length ? `plays on ${pg.connect.map((c) => SERVICE_NAMES[c] || c).join(', ')}` : '') : pf.service === ownSvc ? (pf.mode === 'preview' ? '30 s preview' : '') : pf.mode === 'connect' ? `plays on your ${SERVICE_NAMES[pf.service] || pf.service} device` : `plays from ${where}${isGlobal ? '' : ' here'}`;
          const eyebrow = [`${{ song: 'Song', album: 'Album', artist: 'Artist' }[kind]}${isGlobal ? '' : ownSvc === 'library' ? ' in your library' : ` on ${pg.source}`}`, pg.plays_note && !isGlobal ? `${pg.plays_note.charAt(0).toLowerCase()}${pg.plays_note.slice(1)}` : '', playsHere].filter(Boolean).join(' · ');
          const by = [pg.artist ? (pg.artist.id ? `<a class="lnk" id="iartist">${esc(pg.artist.name)}</a>` : `<a class="lnk" id="iartistq">${esc(pg.artist.name)}</a>`) : '', pg.album ? (pg.album.id ? `<a class="lnk" id="ialbum">${esc(pg.album.name)}</a>` : esc(pg.album.name)) : '', pg.year].filter(Boolean).join(' · ');
          const empty = (t) => `<p class="muted small">${t}</p>`;
          const songsTitle = kind === 'artist' ? 'Top songs' : kind === 'song' ? 'Song' : 'Songs';
          const songsInfo = songs.length ? `${songs.length} song${songs.length === 1 ? '' : 's'}${pg.total_ms ? ' · ' + mins(pg.total_ms) : ''}` : '';
          const instName = (v) => (v.source === 'library' ? 'Your library' : SERVICE_NAMES[v.source] || String(v.label || v.source).split(' · ')[0]);
          // The source line (STEWARD-UX-PLAYER-SOURCES §1): a quiet line under the title. This page's
          // own source first; then the same recording on services that are on and working, in the
          // owner's play order; the rest behind "more". Services switched off are left out.
          const rk = state.svcRank && !state.svcRank.failed ? state.svcRank : null;
          const alsoSvc = (v) => srcSvc(v.source);
          const alsoAll = rankSvc(pg.also_on.map((v, k) => ({ v, k })).filter(({ v }) => !(rk && rk.off.has(alsoSvc(v)))), ({ v }) => alsoSvc(v));
          const alsoFirst = rk ? alsoAll.filter(({ v }) => rk.healthy.has(alsoSvc(v))) : alsoAll;
          const alsoMore = rk ? alsoAll.filter(({ v }) => !rk.healthy.has(alsoSvc(v))) : [];
          const alsoItem = ({ v, k }) => `<span class="src">${svcBadge(alsoSvc(v))}${v.page ? `<button type="button" class="lnk" data-also="${k}" title="${esc(v.label)}">${esc(instName(v))}</button>` : `<span>${esc(instName(v))}</span>`}${v.uri || v.source === 'library' || /^(tidal|deezer)$/.test(v.source) || ytOf(v.url) ? `<button class="chiplink" data-aplay="${k}" title="Play from ${esc(instName(v))}" aria-label="Play from ${esc(instName(v))}">${icon('play')}</button>` : ''}${webUrl(v.url) ? `<a class="chiplink" href="${esc(webUrl(v.url))}" target="_blank" rel="noopener" title="Open on ${esc(instName(v))}" aria-label="Open on ${esc(instName(v))}">${icon('external')}</a>` : ''}</span>`;
          // Artists and albums (TASK-pinakes-social-ux F6): "Also on" is a row of service icons next to
          // Play/Shuffle, the working services first. One opens the service's page here when the node
          // plays from it (on and working: Play is there), else the service's site (every link has a
          // page here, so `page` alone says nothing). Songs keep the source line with its play buttons.
          const iconAlso = kind === 'artist' || kind === 'album';
          const playsHereFrom = (v) => !!v.page && (rk ? rk.healthy.has(alsoSvc(v)) : alsoSvc(v) === 'library');
          const alsoIcon = ({ v, k }) => { const name = instName(v); return playsHereFrom(v) || (v.page && !webUrl(v.url)) ? `<button type="button" class="icon-btn also-ico" data-also="${k}" title="${esc(name)}" aria-label="Open on ${esc(name)}">${svcBadge(alsoSvc(v))}</button>` : webUrl(v.url) ? `<a class="icon-btn also-ico" href="${esc(webUrl(v.url))}" target="_blank" rel="noopener" title="${esc(name)}" aria-label="Open on ${esc(name)} (opens a new tab)">${svcBadge(alsoSvc(v))}</a>` : ''; };
          const alsoIcons = iconAlso && alsoAll.length ? `<span class="also-icons" role="group" aria-label="Also on">${alsoAll.map(alsoIcon).join('')}</span>` : '';
          const [lineFirst, lineMore] = iconAlso ? [[], []] : [alsoFirst, alsoMore];
          const ownItem = `<span class="src own" title="This page is ${esc(ownName)}'s">${svcBadge(ownSvc === 'library' ? 'library' : ownSvc)}<span>${esc(ownName)}</span>${webUrl(pg.link) ? `<a class="chiplink" href="${esc(webUrl(pg.link))}" target="_blank" rel="noopener" title="Open on ${esc(ownName)}" aria-label="Open on ${esc(ownName)}">${icon('external')}</a>` : ''}</span>`;
          const alsoLine = (isGlobal && !lineFirst.length && !lineMore.length) ? '' : `<div class="srcline" id="ialso">${isGlobal ? '<span class="muted">On</span>' : `<span class="muted">From</span>${ownItem}`}${!isGlobal && lineFirst.length ? '<span class="muted">· also</span>' : ''}${lineFirst.map(alsoItem).join('')}${lineMore.length ? `${!isGlobal && !lineFirst.length ? '<span class="muted">·</span>' : ''}<button type="button" class="lnk srcmore" data-srcmore aria-label="Show ${lineMore.length} more source${lineMore.length === 1 ? '' : 's'}">${lineFirst.length || !isGlobal ? `${lineMore.length} more` : `${lineMore.length} services`}</button><span class="srcrest" hidden>${lineMore.map(alsoItem).join('')}</span>` : ''}</div>`
            + (pg.source_id !== 'global' && pg.song_uri ? `<p class="small to-global"><a class="lnk" id="iglobal">All about this ${kind} →</a> <span class="muted">every source, versions and tags</span></p>` : '');
          const versionChip = (v, k) => `<span class="chip">${svcBadge(v.source === 'library' ? 'library' : v.source)} ${esc(v.label)}${v.uri || /^(tidal|deezer)$/.test(v.source) || v.source === 'library' || ytOf(v.url) ? ` <button class="chiplink" data-vplay="${k}" title="Play this version">${icon('play')}</button>` : ''}${webUrl(v.url) ? ` <a class="chiplink" href="${esc(webUrl(v.url))}" target="_blank" rel="noopener" title="Open">${icon('external')}</a>` : ''}</span>`;
          // Deep link with no history: back goes to the parent (song → album → artist → section).
          const parent = kind === 'song' && pg.album && pg.album.id ? { kind: 'album', ...pg.album } : kind !== 'artist' && pg.artist && pg.artist.id ? { kind: 'artist', ...pg.artist } : null;
          const crumbs = kind === 'artist' ? [] : [pg.artist && pg.artist.id ? ['artist', pg.artist] : null, kind === 'song' && pg.album && pg.album.id ? ['album', pg.album] : null].filter(Boolean);
          const crumbBar = crumbs.length ? `<nav class="crumbs" aria-label="Breadcrumb"><ol>${crumbs.map(([k, l]) => `<li><button class="lnk" data-crumb-kind="${k}" data-crumb-id="${esc(l.id)}">${esc(l.name)}</button></li>`).join('')}<li aria-current="page">${esc(state.itemTitle || '')}</li></ol></nav>` : '';
          if (isListener()) pg.about = pg.about.filter((f) => f.label !== 'File');
          // About (F6): the article's opening (Wikipedia in the reader's language, or TheAudioDB) with
          // its link and licence, the picture's Commons credit, then the facts. With an article it
          // sits under the header; without one it stays at the end of the page. A function: when open
          // sources answer later (about_pending) it is redrawn in place.
          const aboutTop = !!(pg.about_text && pg.about_text.text); // artist, album and song (F5: the same About everywhere)
          const aboutHtml = (p) => {
            const at = p.about_text;
            const clamp = !!(at && at.text);
            const blurbText = (at && at.text) || p.blurb;
            return `<section class="section" id="iabout"><div class="section-head"><h3>About</h3></div>${blurbText ? `<p class="blurb${clamp ? ' clamp' : ''}" id="iblurb"${at && at.lang ? ` lang="${esc(at.lang)}"` : ''}>${esc(blurbText)}</p>${clamp ? '<button type="button" class="lnk small" id="iblurbmore" aria-controls="iblurb" aria-expanded="false" hidden>More</button>' : ''}` : ''}${aboutCredit(p)}${p.about.length ? `<dl class="kv">${p.about.map((f) => `<dt>${esc(f.label)}</dt><dd>${webUrl(f.href) ? `<a href="${esc(webUrl(f.href))}" target="_blank" rel="noopener">${esc(f.value)}</a>` : esc(f.value)}</dd>`).join('')}</dl>` : blurbText ? '' : empty('Nothing more is known yet.')}</section>`;
          };
          main.innerHTML = `<div class="pagebar">${backBtn(parent ? `item:${parent.kind}:${parent.id}` : pg.section, parent ? parent.name : sectionName)}${crumbBar}</div>`
            + (pg.reading ? `<p class="muted small reading">Reading this link on Pinakes… versions and tags appear in a moment.</p>` : '')
            + (pg.pending ? `<p class="muted small reading" role="status"><span class="spin"></span> Getting the songs and albums${pg.plays_from && pg.plays_from.service ? ` from ${esc(SERVICE_NAMES[pg.plays_from.service] || pg.plays_from.service)}` : ''}…</p>` : '')
            + `<div class="hero">${pg.image ? `<div class="art${kind === 'artist' ? ' round' : ''}"><img src="${esc(imgSrc(pg.image))}" alt="" style="width:100%;border-radius:${kind === 'artist' ? '50%' : '12px'}" data-err="remove">${heroBadge}</div>` : `<div class="art placeholder monogram" style="background:linear-gradient(135deg, hsl(${hue(state.itemTitle)} 35% 34%), hsl(${hue(state.itemTitle)} 35% 20%));border-radius:${kind === 'artist' ? '50%' : '12px'}"><span aria-hidden="true">${esc(initials(state.itemTitle))}</span>${heroBadge}</div>`}<div><div class="eyebrow">${esc(eyebrow)}</div><h1>${esc(state.itemTitle)}</h1><p class="sub">${by}</p>${alsoLine}
              <div class="actions" style="margin-top:12px"><button class="btn primary" id="iplay">${icon('play')} Play</button>${kind !== 'song' ? `<button class="btn" id="ishuffle">${icon('shuffle')} Shuffle</button>` : ''}<button class="btn" id="izone">${icon('zone')} Play on…</button><button class="btn icon-only ${fav ? 'on' : ''}" id="ifav" title="${fav ? 'Remove from favourites' : 'Add to favourites'}" ${favKey || target.track_id || target.album_id || target.artist_id ? '' : 'disabled'}>${icon(fav ? 'heart-fill' : 'heart')}</button><button class="btn ghost" id="imore" title="More">${icon('more')}</button>${alsoIcons}</div>
              </div></div>`
            + (aboutTop ? aboutHtml(pg) : '')
            + (kind === 'song' ? '' : `<section class="section" id="isongs"><div class="section-head"><h3>${songsTitle}</h3><span class="muted small">${esc(songsInfo)}</span></div>${songs.length ? songRows(songs, { showAlbum: kind !== 'album', covers: kind !== 'album', ...(kind === 'artist' ? { numbers: 'index' } : {}) }) : empty(pg.connect.length ? `Not on your services · <a data-view="sources">Connect ${esc(pg.connect.map((c) => SERVICE_NAMES[c] || c).join(' or '))}</a>` : 'No connected service lists songs for this yet.')}</section>`)
            + (kind === 'artist' || (kind === 'song' && pg.albums.length) ? `<section class="section" id="ialbums"><div class="section-head"><h3>${kind === 'song' ? 'On the album' : 'Albums'}</h3><span class="muted small">${pg.albums.length || ''}</span></div>${pg.albums.length ? itemTileGrid(pg.albums, 'album') : empty(kind === 'song' ? 'The album is not known for this link yet.' : 'No albums found on your services yet.')}</section>` : '')
            + '<div id="iconcerts-slot"></div>'
            + (kind === 'song' && pg.versions.length ? `<section class="section" id="iversions"><div class="section-head"><h3>Versions</h3><span class="muted small">other recordings: live, remastered, edits</span></div><div class="list compact">${pg.versions.map((v, k) => `<div class="list-row">${svcBadge(v.source === 'library' ? 'library' : v.source)}<div class="grow"><div class="name">${v.page ? `<a class="lnk" data-vpage="${k}">${esc(v.label.split(' · ')[0])}</a>` : esc(v.label.split(' · ')[0])}</div><div class="sub">${esc(v.label.split(' · ').slice(1).join(' · '))}</div></div><button class="icon-btn" data-vplay="${k}" title="Play this version" aria-label="Play this version">${icon('play')}</button></div>`).join('')}</div></section>` : '')
            + '<div id="isimilar-slot"></div>'
            + `<section class="section" id="itags"><div class="section-head"><h3>Tags</h3>${pg.on_pubky && pg.on_pubky.taggers ? `<button class="faces muted small" data-taggers="${pg.on_pubky.taggers}">${pg.on_pubky.taggers} ${pg.on_pubky.taggers === 1 ? 'person' : 'people'} tagged this on Pubky</button>` : ''}</div>${resignNotice()}${myTags ? scopedTagRow(myTags, linkTags, linkRef ? pg.source : null, kind, (pg.on_pubky && pg.on_pubky.tags) || []) : `${pg.on_pubky && pg.on_pubky.tags.length ? `<div class="chips">${pg.on_pubky.tags.map(([l, c]) => `<button class="chip" data-tag-open="${esc(l)}">#${esc(l)} <span class="muted">${c}</span></button>`).join('')}</div>` : ''}${empty(`Tagging works once this ${kind} is matched to MusicBrainz, the open music database, so other apps know what it is.`)}`}${myReview !== undefined ? reviewRow(myReview) : ''}</section>`
            + (commentUri ? `<section class="section" id="icomments" data-curi="${esc(commentUri)}"><div class="section-head"><h3>Comments</h3></div><div class="comments-body"><div class="skeleton" style="height:48px"></div></div></section>` : '')
            + (aboutTop ? '' : aboutHtml(pg));
          $('[data-back]', main).onclick = () => goBack(pg.section);
          // The article's opening is clamped to a few lines; More shows it all. Whether it is cut off
          // is measured again when its width or the text size changes (zoom, rotation, a late font)
          const bindBlurb = () => { const bl = $('#iblurb', main); const bm = $('#iblurbmore', main);
            if (bl && bm) {
              const sync = () => { if (bl.classList.contains('clamp')) bm.hidden = !(bl.scrollHeight > bl.clientHeight + 2); };
              bm.onclick = () => { const open = bl.classList.toggle('clamp') === false; bm.textContent = open ? 'Less' : 'More'; bm.setAttribute('aria-expanded', String(open)); };
              sync(); if (window.ResizeObserver) new ResizeObserver(sync).observe(bl);
            } };
          bindBlurb();
          // One way back: on wide screens the breadcrumb stands in for Back when Back goes to a crumb
          { const bk = $('.pagebar > [data-back]', main);
            const same = bk && $$('[data-crumb-id]', main).find((c) => bk.textContent.trim().endsWith(c.textContent.trim()));
            if (same) { bk.classList.add('dupback'); same.dataset.asBack = '1'; } }
          const ig = $('#iglobal'); if (ig) ig.onclick = () => setView('item', { kind, id: idOf(pg.song_uri) });
          if ($('#isongs')) { bindSongRows($('#isongs'), songs, { plays_from: pf, fallback }); enableSelect($('#isongs'), songs, { plays_from: pf, fallback }); }
          if (pg.albums.length) bindItemTiles($('#ialbums'), pg.albums, 'album');
          // Related items and concerts come after the page (each outside call has a time budget)
          api('GET', `/item/${kind}/${encodeURIComponent(id)}/extras`).then((ex) => {
            if (state.view !== 'item' || state.viewArg.id !== id) return;
            pg.related = ex.related || []; pg.concerts = ex.concerts || [];
            // Similar (TASK-pinakes-social-ux A3): the library's own first (shared genres, then the
            // same artist; playable), Last.fm's similar artists on an artist page, then what people
            // tagged alike on Pubky. One section; each part only when it has something.
            const ss = $('#isimilar-slot'); if (ss) (async () => {
              let fm = null;
              if (kind === 'artist' && target.artist_id != null) { try { fm = await api('GET', `/library/artists/${target.artist_id}/similar`); } catch { fm = null; } }
              if (!ss.isConnected) return;
              const mine = ex.similar || []; const fmArtists = (fm && fm.artists) || [];
              const mineSongs = kind === 'song' ? mine.map((h) => ({ ...h, artist: h.subtitle })) : [];
              const mineTiles = kind === 'song' ? [] : mine.map((h) => ({ ...h, year: h.subtitle || h.year || '' }));
              const part = (label, body, id) => `<div class="simpart" id="${id}"><h4 class="simhead muted small">${label}</h4>${body}</div>`;
              const parts = [
                mine.length ? part('In your library', kind === 'song' ? songRows(mineSongs, { showAlbum: true, covers: true }) : itemTileGrid(mineTiles, kind), 'isimlib') : '',
                fmArtists.length ? part('From Last.fm', `<div class="chips">${fmArtists.map((a) => a.library_artist_id != null ? `<button class="chip" data-simlib="${a.library_artist_id}">${esc(a.name)}</button>` : `<button class="chip" data-simq="${esc(a.name)}" title="Not in your library: search">${esc(a.name)}</button>`).join('')}</div>`, 'isimfm') : '',
                pg.related.length ? part('Tagged alike on Pubky', itemTileGrid(pg.related.map(asTile), 'album'), 'irelated') : '',
              ].filter(Boolean);
              if (!parts.length) { ss.remove(); return; }
              ss.outerHTML = `<section class="section" id="isimilar"><div class="section-head"><h3>Similar</h3></div>${parts.join('')}</section>`;
              const lib = $('#isimlib', main);
              if (lib && kind === 'song') bindSongRows(lib, mineSongs, {}); else if (lib) bindItemTiles(lib, mineTiles, kind);
              $$('[data-simlib]', main).forEach((b) => b.onclick = () => setView('item', { kind: 'artist', id: 'lib-' + b.dataset.simlib }));
              $$('[data-simq]', main).forEach((b) => b.onclick = () => { search.value = b.dataset.simq; runSearch(); });
              if (pg.related.length) bindHome($('#irelated', main), { items: pg.related, tags: [] });
            })();
            const cs = $('#iconcerts-slot'); if (cs) { cs.outerHTML = (pg.concerts && pg.concerts.length ? `<section class="section" id="iconcerts"><div class="section-head"><h3>Upcoming concerts</h3></div><div class="list">${pg.concerts.map((c, k) => `<div class="list-row clickable" data-iconcert="${k}"><div class="grow"><div class="name">${esc(itemName(c))}</div><div class="sub">${esc([c.description, c.site].filter(Boolean).join(' · '))}</div></div>${icon('external')}</div>`).join('')}</div></section>` : ''); }
            $$('[data-iconcert]', main).forEach((r) => r.onclick = () => { const c = pg.concerts[Number(r.dataset.iconcert)]; openWeb(c.page || c.uri); });
          }).catch(() => { /* extras are a bonus */ });
          if (myTags) bindScopedTags(songRef, linkRef);
          if (myReview !== undefined) bindReviewRow(target, myReview);
          $$('[data-view="sources"]', main).forEach((a) => a.onclick = () => setView('sources'));
          const ia = $('#iartist'); if (ia) ia.onclick = () => setView('item', { kind: 'artist', id: pg.artist.id });
          const iq = $('#iartistq'); if (iq) iq.onclick = () => { search.value = pg.artist.name; runSearch(); };
          const ial = $('#ialbum'); if (ial) ial.onclick = () => setView('item', { kind: 'album', id: pg.album.id });
          const playAll = (shuffle, how = 'replace', anchorEl) => { if (!songs.length) return discoverPlay({ uri: pg.uri || '', page: pg.open_on[0] ? pg.open_on[0].url : pg.uri, title: pg.title, artist: fallback.artist, image: pg.image }, null, anchorEl); const list = shuffle ? [...songs].sort(() => Math.random() - 0.5) : songs; return playSongs(list, 0, how, pf, anchorEl, fallback); };
          $('#iplay').onclick = (e) => playAll(false, 'replace', e.currentTarget);
          stickyHead(state.itemTitle, (el) => playAll(false, 'replace', el));
          const sh = $('#ishuffle'); if (sh) sh.onclick = (e) => playAll(true, 'replace', e.currentTarget);
          $('#izone').onclick = (e) => showMenu(e.currentTarget, [{ title: 'Play on' }, ...zoneList().map((z) => ({ label: z.name, icon: 'zone', run: () => { state.zone = z.id; try { localStorage.setItem('pm.zone', z.id); } catch { /* */ } renderZoneName(); renderBar(); playAll(false); } }))]);
          $('#ifav').onclick = async () => {
            // A file not matched yet: "Which song is this?" first, then the heart
            try { await setFavourite(target, !fav); render(); } catch (er) { toast(er.message, 'error'); }
          };
          const extra = [...(commentUri ? [{ label: 'Post about this…', icon: 'notes', run: () => postAbout(commentUri, pg.title) }] : [])];
          $('#imore').onclick = (e) => itemMenu(e.currentTarget, { title: pg.title, songs, plays_from: pf, fallback, artistId: pg.artist && pg.artist.id, artistName: pg.artist && pg.artist.name, albumId: kind === 'album' ? null : pg.album && pg.album.id, openOn: pg.open_on, path: `/${kind}/${id}`, uri: pg.uri, extra });
          const playVersion = async (v, b) => { if (v.source === 'library') { if (v.uri && v.uri.startsWith('track:')) { const t = await api('GET', `/library/tracks/${v.uri.slice(6)}`); return playTracks([t], true); } return setView('item', { kind: 'album', id: 'lib-' + v.id }); } if (v.uri) return playSourceItems([qOf({ uri: v.uri, title: pg.title }, fallback)], true); if (ytOf(v.url)) return playSourceItems([qOf({ uri: `youtube:video:${ytOf(v.url)}`, title: pg.title }, fallback)], true); return discoverPlay({ uri: v.url, page: v.url, title: pg.title, artist: fallback.artist }, null, b); };
          $$('[data-vplay]', main).forEach((b) => b.onclick = () => playVersion(pg.versions[Number(b.dataset.vplay)], b));
          $$('[data-aplay]', main).forEach((b) => b.onclick = () => playVersion(pg.also_on[Number(b.dataset.aplay)], b));
          $$('[data-also]', main).forEach((a) => a.onclick = () => setView('item', { kind, id: pg.also_on[Number(a.dataset.also)].page }));
          const smore = $('[data-srcmore]', main); if (smore) smore.onclick = () => { const rest = smore.nextElementSibling; rest.hidden = false; smore.remove(); const first = $('a[href], button', rest); if (first) first.focus(); };
          $$('[data-vpage]', main).forEach((a) => a.onclick = () => setView('item', { kind, id: pg.versions[Number(a.dataset.vpage)].page }));
          $$('[data-tag-open]', $('#itags')).forEach((b) => b.onclick = () => setView('discover', { tag: b.dataset.tagOpen }));
          const csec = $('#icomments'); if (csec) mountComments(csec);
          if (pg.on_pubky && pg.on_pubky.faces) fillTagFaces($('#itags'), pg.on_pubky.faces, new Map(pg.on_pubky.tags));
          // Not while someone types a comment: the render would take the box away under them
          if (pg.reading || pg.pending) { const tries = (state.viewArg.tries || 0) + 1; const again = () => { if (state.view !== 'item' || state.viewArg.id !== id) return; if (commentTyping()) { setTimeout(again, 3000); return; } state.viewArg.tries = tries; render(); }; if (tries <= 12) setTimeout(again, pg.reading ? 4000 : 3000); }
          // About from open sources comes in the background: ask for the page again and redraw only
          // About and the portrait in place (the page stays put: scroll, focus, a half-typed tag).
          // When it brings other sources ("Also on"), the page is drawn once more at the end.
          else if (pg.about_pending) {
            const here = () => state.view === 'item' && state.viewArg.id === id;
            const poll = (tries) => setTimeout(async () => {
              if (!here()) return;
              let np; try { np = await api('GET', `/item/${kind}/${encodeURIComponent(id)}`); } catch { return; }
              if (!here()) return;
              if (np.about_pending) { if (tries < 12) poll(tries + 1); return; }
              const sources = (p) => JSON.stringify([(p.also_on || []).map((v) => v.url), (p.open_on || []).map((v) => v.url)]);
              if (sources(np) !== sources(pg)) { const redraw = () => { if (!here()) return; if (commentTyping()) { setTimeout(redraw, 3000); return; } render(); }; redraw(); return; }
              if (isListener()) np.about = (np.about || []).filter((f) => f.label !== 'File');
              const sec = $('#iabout', main); if (sec) { sec.outerHTML = aboutHtml({ ...np, about: np.about || [] }); bindBlurb(); }
              const ph = np.image && !pg.image && $('.hero .art.placeholder', main);
              if (ph) ph.outerHTML = `<div class="art${kind === 'artist' ? ' round' : ''}"><img src="${esc(imgSrc(np.image))}" alt="" style="width:100%;border-radius:${kind === 'artist' ? '50%' : '12px'}" data-err="remove"></div>`;
              Object.assign(pg, { about: np.about, blurb: np.blurb, about_text: np.about_text, image_credit: np.image_credit, image: np.image || pg.image, about_pending: false });
            }, 3000);
            poll(1);
          }
          break;
        }
        case 'queue': {
          await refreshQueue(false);
          if (state.view !== 'queue') return;
          const z = currentZone(); const q = isBrowser() ? state.browserQueue : state.queue;
          main.innerHTML = pageHead('Queue', z ? z.name : '', (isBrowser() ? '' : `<button class="btn ${z && z.radio ? 'primary' : ''}" id="qradio" title="When the queue ends, keep playing similar music from the library">${icon('radio')} Radio ${z && z.radio ? 'on' : 'off'}</button>`) + (q.length > 1 ? `<button class="btn" id="qshuffle" title="Shuffle what comes next; the playing track stays">${icon('shuffle')} Shuffle</button>` : '') + (q.length && zoneList().filter((o) => o.id !== state.zone).length ? `<button class="btn" id="qtransfer" title="Move this queue, the song where it is, repeat and radio to another device">${icon('zone')} Move to…</button>` : '') + (q.length ? `<button class="btn" id="qsave" title="A new private playlist from this queue">${icon('playlist')} Save as playlist</button>` : '') + (q.length && !isBrowser() ? `<button class="btn" id="qclear">${icon('trash')} Clear</button>` : ''))
            + (q.length ? `<div class="list queue">${q.map((it, i) => `<div class="list-row ${z && z.queue_index === i ? 'cur' : ''}" data-qrow="${i}"><span class="qgrip" data-qgrip="${i}" role="button" tabindex="0" aria-label="Drag to reorder (or Alt+↑/↓)" title="Drag to reorder">⋮⋮</span><button class="icon-btn" data-qi="${i}" title="Play from here">${z && z.queue_index === i ? icon('play') : `<span class="num">${i + 1}</span>`}</button><div class="grow"><div class="name" ${z && z.queue_index === i ? 'style="color:var(--accent-strong)"' : ''}>${esc(itemName(it))}</div><div class="sub">${esc([it.artist, it.album].filter(Boolean).join(' · '))}</div></div><span class="muted small">${it.duration_ms ? fmt(it.duration_ms) : ''}</span><span class="row-actions q-actions"><button class="icon-btn" data-qup="${i}" title="Move up" ${i === 0 ? 'disabled' : ''}>${icon('chevron', 'ic up')}</button><button class="icon-btn" data-qdown="${i}" title="Move down" ${i === q.length - 1 ? 'disabled' : ''}>${icon('chevron')}</button><button class="icon-btn" data-qrm="${i}" title="Remove">${icon('close')}</button></span></div>`).join('')}</div>` : emptyState('queue', 'The queue is empty', 'Play an album or add tracks with the row actions.'));
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
          const qt = $('#qtransfer'); if (qt) qt.onclick = () => showMenu(qt, [{ title: 'Move to' }, ...zoneList().filter((o) => o.id !== state.zone).map((o) => ({ label: o.name, icon: 'zone', note: deviceWords(o), run: () => moveTo(o.id) }))]);
          const qsv = $('#qsave'); if (qsv) qsv.onclick = async () => {
            const body = isBrowser() ? { uris: state.browserQueue.map((it) => it.uri), name: 'Queue · this browser' } : { zone: state.zone };
            try { const r = await api('POST', '/playlists/from-queue', body); state.playlists = await api('GET', '/playlists'); toast(r.skipped.length ? `Saved “${r.playlist.summary.name}”; ${countOf(r.skipped.length, 'songs')} left out: not matched to a song yet (press the ? next to a song to pick which one it is)` : `Saved “${r.playlist.summary.name}”`); } catch (e) { toast(e.message, 'error'); }
          };
          const qc = $('#qclear'); if (qc) qc.onclick = async () => { try { await api('DELETE', `/zones/${state.zone}/queue`); refreshQueue(); } catch (e) { toast(e.message, 'error'); } };
          break;
        }
        case 'zones': {
          const outs = isListener() ? [] : state.outputs || (state.outputs = await api('GET', '/outputs').catch(() => []));
          if (state.view !== 'zones') return;
          const capsOf = (z) => (outs.find((o) => o.id === z.output) || {}).caps || null;
          const zs = zoneList();
          main.innerHTML = pageHead('Zones', `${zs.length} · multi-room overview`, `<button class="btn" id="zgroup">${icon('plus')} Group outputs…</button>`) + `<div class="cards">${zs.map((z) => {
            const np = z.now_playing; const it = np && np.item; const caps = capsOf(z); const hv = caps && caps.hw_volume && caps.hw_volume.kind === 'db' ? caps.hw_volume : null;
            const members = np && np.members && np.members.length ? `<span class="pill">${np.members.length} outputs</span>` : '';
            return `<div class="card zone-card ${z.id === state.zone ? 'on' : ''}" data-zcard="${esc(z.id)}">
              <div class="zc-head">${icon('zone')}<h3>${esc(z.name)}</h3>${members}${z.transport === 'stopped' && z.error ? '<span class="pill bad">error</span>' : `<span class="pill ${z.transport === 'playing' ? 'ok' : ''}">${esc(z.transport)}</span>`}</div>
              <div class="zc-now">${it ? itemArt(it, 'sm', npSvc(np)) : '<div class="np-art placeholder"></div>'}<div class="zc-text"><div class="name">${it ? esc(np.stream_title || it.title || it.uri) : '<span class="muted">Nothing playing</span>'}</div><div class="sub">${it ? esc([it.artist, it.album].filter(Boolean).join(' · ')) : esc(z.id === BROWSER ? 'Streams the files to this browser' : z.output)}</div>${!it && z.error && z.transport === 'stopped' ? `<div class="sub warn-text" role="alert">Couldn’t play: ${esc(z.error)}</div>` : ''}</div></div>
              <div class="zc-controls">
                <button class="icon-btn" data-zt="previous" title="Previous">${icon('prev')}</button>
                <button class="icon-btn big" data-zt="toggle" title="${z.transport === 'playing' ? 'Pause' : 'Play'}">${icon(z.transport === 'playing' ? 'pause' : 'play')}</button>
                <button class="icon-btn" data-zt="next" title="Next">${icon('next')}</button>
                ${hv ? `<input type="range" data-zvol="${esc(z.id)}" min="${hv.min}" max="${hv.max}" step="${hv.step || 1}" value="${z.volume_db != null ? z.volume_db : hv.max}" aria-label="Volume of ${esc(z.name)}">` : z.id === BROWSER ? `<input type="range" data-zvol="${BROWSER}" min="-60" max="0" value="${Math.round(20 * Math.log10(audio.volume || 1))}" aria-label="Browser volume">` : '<span class="muted small">Volume: set on the device</span>'}
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
          $('#zgroup').onclick = () => setView('settings', 'playback');
          break;
        }
        case 'add-music': {
          // The old page: Your library with "Add music" open
          state.libAsk = true;
          return setView('library', null, 'replace');
        }
        case 'library': {
          const owner = isOwner();
          const tab = !owner ? 'mine' : state.viewArg === 'node' || state.viewArg === 'import' ? state.viewArg : 'mine';
          const tabsHtml = `<div class="chips libtabs" role="tablist" aria-label="Your library">${[['mine', 'My music'], ...(owner ? [['node', 'Node library'], ['import', 'Import']] : [])].map(([k, l]) => `<button class="chip ${tab === k ? 'on' : ''}" role="tab" aria-selected="${tab === k}" aria-controls="libpanel" id="libtab-${k}" data-libtab="${k}">${l}</button>`).join('')}</div>`;
          const head = pageHead('Your library', null, owner ? `<button class="btn primary" id="libadd">${icon('plus')} Add music</button>` : '') + tabsHtml;
          if (tab === 'mine') {
            if (!owner) {
              main.innerHTML = head + libPanel(tab, '<div class="card"><p>Your own music lives on your Pubky homeserver: the Pubky Music app keeps it there and plays it here when this node lets you.</p><p class="muted small">This node’s songs, albums and playlists are under Albums, Artists, Songs and Playlists.</p></div>');
              break;
            }
            const [st, pk, ns] = await Promise.all([api('GET', '/storage').catch(() => null), api('GET', '/pubky').catch(() => null), api('GET', '/node/settings').catch(() => ({}))]);
            const mine = pk && pk.user ? await api('GET', '/me/music').catch((er) => ({ error: er.message })) : null;
            if (state.view !== 'library' || (state.viewArg && state.viewArg !== 'mine')) return;
            const hs = (st && st.homeserver) || { mode: 'none' };
            const sv = st && st.saving;
            const room = hs.quota_mb && hs.quota_mb !== 'unlimited' && hs.used_bytes != null ? ` of ${fmtBytes(hs.quota_mb * 1e6)}` : '';
            const used = mine && !mine.error ? `${countOf(mine.count, 'files')} · ${fileSize(mine.bytes)}${room} used` : '';
            // Saving: a failed save offers Try again; otherwise Save now (a full sync, off while the node needs re-approval)
            const saveBtn = !sv || !pk || (pk.sync && pk.sync.running) || sv.state === 'saving' ? ''
              : sv.state === 'error' ? ' · <button class="lnk" id="stsave">Try again</button>'
                : ` · <button class="lnk" id="sync" ${pk.needs_resign ? `disabled title="${esc(pk.notice || RESIGN_TEXT)}"` : ''}>Save now</button>`;
            const status = pk && pk.user ? `${resignNotice(pk)}<div class="libstatus" role="status"><span id="libsaved">${savedWords(sv, pk)}</span>${saveBtn}${used ? ` · <span class="muted">${used}</span>` : ''}</div>` : '';
            const upOpen = state.libUpload || up.files.length > 0;
            const upload = `<section class="section card addm" id="libupload" ${upOpen ? '' : 'hidden'} aria-labelledby="libuph"><div class="section-head"><h3 id="libuph">${icon('upload')} Upload files</h3><button class="icon-btn" id="libupclose" aria-label="Close upload" title="Close">${icon('close')}</button></div>`
              + '<p class="muted small">To your homeserver, kept privately (never public), then added to your library.</p>'
              + (pk && pk.user ? `<div class="dropzone" id="updrop" tabindex="0" role="button" aria-label="Drop music files or folders here, or press Enter to choose files">${icon('upload')}<p><strong>Drop music files or whole folders here</strong></p><p class="muted small">FLAC, MP3, AAC, ALAC, Ogg, Opus, WAV, AIFF, DSD…</p><div class="row-btns"><button class="btn primary" id="uppick">Choose files</button><button class="btn" id="uppickdir">Choose a folder</button></div><input type="file" id="upfiles" multiple accept="audio/*,.flac,.dsf,.dff,.ape,.wv,.mka" hidden><input type="file" id="updir" webkitdirectory multiple hidden></div><div id="upbox"></div>` : '<p class="muted small">Sign in with Pubky first: the files are kept on your homeserver.</p>')
              + (hs.mode !== 'none' ? '<details class="set-adv"><summary>Or copy a folder from the node’s machine</summary><form class="toolbar" id="impform"><label class="sr-only" for="imppath">Folder to copy</label><input class="input" id="imppath" placeholder="/mnt/usb/Music" autocomplete="off" spellcheck="false"><button class="btn" id="impgo">Check</button></form><div id="impplan"></div></details>' : '')
              + '</section>';
            let body;
            if (!pk || !pk.user) body = '<div class="card"><p>Sign in with Pubky to keep your music, playlists and favourites on your homeserver.</p><button class="btn primary" data-goto-accounts>Sign in</button></div>';
            else if (mine && mine.error) body = `<div class="notice warn">${esc(mine.error)}</div>`;
            else {
              const songs = mine.files.filter((f) => f.song);
              const loose = mine.files.filter((f) => !f.song);
              const cover = (f) => (f.song.album_id ? `<img class="thumb" src="${esc(imgSrc(`/v1/library/albums/${f.song.album_id}/cover`))}" alt="" loading="lazy" data-err="hide">` : '<span class="thumb"></span>');
              const songRow = (f, k) => `<div class="list-row mrow" data-mrow="${k}">${cover(f)}<button class="icon-btn" data-mplay="${f.song.id}" aria-label="Play ${esc(f.song.title)}" title="Play">${icon('play')}</button><div class="grow"><div class="name">${esc(f.song.title)}${f.song.edited ? ' <span class="pill" title="Tags edited on this node">edited</span>' : ''}</div><div class="sub">${esc([f.song.artist, f.song.album].filter(Boolean).join(' · '))}</div></div><span class="muted small">${f.song.duration_ms ? fmt(f.song.duration_ms) : ''}</span><span class="muted small msize">${fileSize(f.size)}</span><button class="icon-btn" data-mmore="${k}" aria-label="More for ${esc(f.song.title)}" title="More">${icon('more')}</button></div>`;
              const looseRow = (f, k) => `<div class="list-row mrow"><div class="grow"><div class="name">${esc(f.name)}</div><div class="sub">${f.removed ? 'Taken out of the library: Node library brings it back.' : 'Not in the library yet: it is read with the next scan.'}</div></div><span class="muted small msize">${fileSize(f.size)}</span><button class="btn sm ghost" data-mdel="${k}" aria-label="Delete ${esc(f.name)} from your homeserver">Delete</button></div>`;
              body = (songs.length ? `<section class="section"><div class="section-head"><h3>On your homeserver</h3><span class="muted small">${countOf(songs.length, 'songs')}</span></div><div class="list">${songs.map((f) => songRow(f, mine.files.indexOf(f))).join('')}</div></section>` : `<div class="card"><p>No music on your homeserver yet.</p><button class="btn primary" data-libup>${icon('upload')} Upload files</button></div>`)
                + (loose.length ? `<section class="section"><div class="section-head"><h3>${loose.every((f) => f.removed) ? 'Not in the library' : 'Being added'}</h3></div><div class="list">${loose.map((f) => looseRow(f, mine.files.indexOf(f))).join('')}</div></section>` : '')
                + sGroup('Tag my library', sBlock('', 'One public tag on every identified song, so the music index matches your songs across services (saved to your homeserver with your other changes).', '<div class="toolbar"><input class="input" id="bulklabel" value="library" aria-label="Tag label"><button class="btn" id="bulktag">Tag</button></div><p class="muted small" id="bulkout" aria-live="polite"></p>'))
                + (hs.mode !== 'bundled' ? `<details class="set-adv"><summary>Copy between this node and your homeserver</summary>${sRow('Copy this node’s library to your homeserver', 'Files already there are skipped.', `<button class="btn ghost" id="pkcopyall">${icon('upload')} Copy</button>`)}${sRow('Bring files here', 'Files on your homeserver this node doesn’t have yet.', `<button class="btn ghost" id="pkmirror">${icon('download')} Bring them</button>`)}${sRow('Copy speed', 'Copies share your internet upload with this node\u2019s pages and the music it streams to you away from home. Gentler leaves more room.', `<select class="input" id="copyrate" aria-label="Copy speed">${[[1000000, 'Gentle (1 MB/s)'], [2500000, 'Normal (2.5 MB/s)'], [0, 'As fast as possible']].map(([v, l]) => `<option value="${v}" ${Number(ns.copy_rate ?? 2500000) === v ? 'selected' : ''}>${l}</option>`).join('')}</select>`)}</details>` : '');
            }
            main.innerHTML = head + libPanel(tab, status + upload + body);
            // Files: play, edit, download, delete
            const files = (mine && mine.files) || [];
            $$('[data-mplay]', main).forEach((b) => b.onclick = async () => { b.classList.add('starting'); try { await playTracks([await api('GET', `/library/tracks/${b.dataset.mplay}`)], true); } catch (er) { toast(er.message, 'error'); } finally { b.classList.remove('starting'); } });
            const delFile = async (f) => {
              const ok = await choiceDialog({ title: 'Delete from your homeserver?', text: `“${f.song ? f.song.title : f.name}” is deleted from your homeserver${hs.mode === 'bundled' ? ' and leaves the library' : ''}. This can’t be undone.`, choices: [{ label: 'Delete', value: true, primary: true }] });
              if (!ok) return;
              try { await api('DELETE', '/pubky/files', { paths: [f.path] }); toast('Deleted'); render(); } catch (er) { toast(er.message, 'error'); }
            };
            $$('[data-mdel]', main).forEach((b) => b.onclick = () => delFile(files[Number(b.dataset.mdel)]));
            $$('[data-mmore]', main).forEach((b) => b.onclick = () => { const f = files[Number(b.dataset.mmore)]; showMenu(b, [{ title: f.song.title }, { label: 'Edit tags…', icon: 'notes', run: () => editTrackTags(f.song.id) }, { label: 'Download', icon: 'download', run: () => { location.href = `/v1/library/tracks/${f.song.id}/download${tokenQ()}`; } }, ...(f.song.album_id ? [{ label: 'Go to album', icon: 'disc', run: () => setView('item', { kind: 'album', id: `lib-${f.song.album_id}` }) }] : []), { label: 'Delete from my homeserver', icon: 'trash', run: () => delFile(f) }]); });
            const sts = $('#stsave'); if (sts) sts.onclick = async () => { sts.disabled = true; try { await api('POST', '/storage/save'); toast('Saving to your homeserver'); } catch (er) { toast(er.message, 'error'); } render(); };
            const sy = $('#sync'); if (sy) sy.onclick = async () => { sy.disabled = true; try { syncToast(await api('POST', '/pubky/sync')); } catch (er) { toast(er.message, 'error'); } render(); };
            bindResign(main);
            if ((sv && sv.state === 'saving') || (pk && pk.sync && pk.sync.running)) { clearTimeout(state.savePoll); state.savePoll = setTimeout(() => { if (state.view === 'library' && (state.viewArg || 'mine') === 'mine') render(); }, 3000); }
            $$('[data-goto-accounts]', main).forEach((b) => b.onclick = () => setView('settings', 'accounts'));
            const cr = $('#copyrate'); if (cr) cr.onchange = async () => { try { await api('PUT', '/node/settings', { copy_rate: Number(cr.value) }); toast('Copy speed saved'); } catch (e) { toast(e.message, 'error'); } };
        const bt = $('#bulktag'); if (bt) bt.onclick = async () => {
          const label = $('#bulklabel').value.trim(); if (!label) return; bt.disabled = true;
          try { const r = await api('POST', '/tags/bulk', { label }); $('#bulkout').textContent = `#${r.label}: ${countOf(r.tagged, 'tracks')} tagged, ${r.already} already had it, ${r.skipped_no_identity} skipped (no MusicBrainz id or ISRC). Sync with Pubky to publish.`; }
          catch (e) { toast(e.message, 'error'); }
          bt.disabled = false;
        };
        const ca = $('#pkcopyall'); if (ca) ca.onclick = () => startUpload({ all: true });
        const mi = $('#pkmirror'); if (mi) mi.onclick = async () => { mi.disabled = true; await bringFiles(); mi.disabled = false; };
            // Upload: the drop zone, choosing files or a folder, and copying a folder on the machine
            const showUpload = () => { const s2 = $('#libupload'); s2.hidden = false; s2.scrollIntoView({ block: 'start' }); const z = $('#updrop'); if (z) z.focus(); };
            $$('[data-libup]', main).forEach((b) => b.onclick = showUpload);
            $('#libupclose').onclick = () => { state.libUpload = false; $('#libupload').hidden = true; };
            const drop = $('#updrop');
            if (drop) {
              $('#uppick').onclick = (ev) => { ev.stopPropagation(); $('#upfiles').click(); };
              $('#uppickdir').onclick = (ev) => { ev.stopPropagation(); $('#updir').click(); };
              drop.onkeydown = (ev) => { if (ev.key === 'Enter' && ev.target === drop) $('#upfiles').click(); };
              $('#upfiles').onchange = (ev) => { addUploads(ev.target.files); ev.target.value = ''; };
              $('#updir').onchange = (ev) => { addUploads(ev.target.files); ev.target.value = ''; };
              drop.ondragover = (ev) => { ev.preventDefault(); drop.classList.add('over'); };
              drop.ondragleave = () => drop.classList.remove('over');
              drop.ondrop = async (ev) => { ev.preventDefault(); drop.classList.remove('over'); addUploads(await filesFromDrop(ev.dataTransfer)); };
              drawUploads();
            }
            const imp = $('#impform');
            if (imp) imp.onsubmit = async (ev) => {
              ev.preventDefault(); const path = $('#imppath').value.trim(); if (!path) return;
              const box = $('#impplan'); box.innerHTML = '<p class="small" role="status"><span class="spin"></span> Looking at the folder…</p>';
              let plan; try { plan = await api('POST', '/library/imports', { path, dry_run: true }); } catch (er) { box.innerHTML = `<p class="small joberr">${esc(er.message)}</p>`; return; }
              if (!plan.total) { box.innerHTML = '<p class="small">No music files in that folder.</p>'; return; }
              box.innerHTML = `<p class="small">${countOf(plan.total, 'music files')}, ${fmtBytes(plan.bytes_total)}. Files already on your homeserver are skipped while it runs.</p><div class="row-btns"><button class="btn primary" id="impstart">Copy them</button></div>`;
              $('#impstart').onclick = async () => { try { const j = await api('POST', '/library/imports', { path }); watchJob(j.id); box.innerHTML = '<p class="small">Copying. Follow it in the job chip by the player; you can leave this page.</p>'; } catch (er) { toast(er.message, 'error'); } };
            };
          } else if (tab === 'node') {
            const [info, folders, shares, drv, ns, hidden] = await Promise.all([
              api('GET', '/info'), api('GET', '/library/folders').catch(() => []), api('GET', '/library/webdav').catch(() => []),
              api('GET', '/library/drives').catch(() => ({ providers: [], roots: [] })), api('GET', '/node/settings').catch(() => ({})), api('GET', '/library/hidden').catch(() => []),
            ]);
            if (state.view !== 'library' || state.viewArg !== 'node') return;
            state.info = info;
            const lsc = info.last_scan;
            const scanLine = info.scanning ? `<span class="spin"></span> Scanning… ${countOf(info.library.tracks, 'songs')} so far`
              : `${countOf(info.library.tracks, 'songs')} in the library${lsc ? ` · last scan ${ago(lsc.finished_at)}${lsc.error_count ? ` · ${countOf(lsc.error_count, 'files')} could not be read` : ''}` : ''}`;
            main.innerHTML = head + libPanel(tab, `<div class="libstatus" role="status">${scanLine}</div><p class="muted small">Edit a song’s tags or take it out of the library from its ⋯ menu, an album from the album’s ⋯ menu. Edits stay on this node; files are never changed.</p>`
              + nodeLibraryHtml({ info, folders, ns, shares, drv })
              + (hidden.length ? sGroup('Taken out of the library', hidden.map((h, k) => sRow(`<code class="path">${esc(h.path.split('/').slice(-2).join('/'))}</code>`, `Taken out ${ago(h.hidden_at)}; the file is still there.`, `<button class="btn sm ghost" data-unhide="${k}">Bring back</button>`)).join('')) : ''));
            bindNodeLibrary();
            $$('[data-unhide]', main).forEach((b) => b.onclick = async () => { b.disabled = true; try { const r = await api('POST', '/library/hidden/restore', { path: hidden[Number(b.dataset.unhide)].path }); toast(r.scan_started ? 'Back: reading it again' : 'Back with the next scan'); watchScan({ scanning: !!r.scan_started }); render(); } catch (er) { toast(er.message, 'error'); b.disabled = false; } });
            if (info.scanning) { clearTimeout(state.libPoll); state.libPoll = setTimeout(() => { if (state.view === 'library' && state.viewArg === 'node') render(); }, 3000); }
          } else {
            main.innerHTML = head + libPanel(tab, importHtml());
            bindImport();
          }
          $$('[data-libtab]', main).forEach((b) => b.onclick = () => { if (document.activeElement === b) state.libTabFocus = true; setView('library', b.dataset.libtab === 'mine' ? null : b.dataset.libtab, 'replace'); });
          $$('[data-view-go]', main).forEach((b) => b.onclick = () => setView(b.dataset.viewGo));
          const la = $('#libadd'); if (la) la.onclick = addMusicDialog;
          if (state.libAsk) { state.libAsk = false; if (owner) addMusicDialog(); }
          // Tabs: arrow keys move between them; the chosen tab keeps the keyboard after the page draws
          const tabBtns = $$('[data-libtab]', main);
          tabBtns.forEach((b, k) => { b.tabIndex = b.classList.contains('on') ? 0 : -1; b.onkeydown = (ev) => { if (ev.key !== 'ArrowRight' && ev.key !== 'ArrowLeft') return; ev.preventDefault(); const n = tabBtns[(k + (ev.key === 'ArrowRight' ? 1 : tabBtns.length - 1)) % tabBtns.length]; state.libTabFocus = true; n.click(); }; });
          if (state.libTabFocus) { state.libTabFocus = false; const on = $('[data-libtab].on', main); if (on) on.focus(); }
          break;
        }
        case 'downloads': {
          // What this device keeps (STEWARD-DOWNLOADS.md): offline, from the app's local server
          if (!isOffline()) { main.innerHTML = pageHead('Downloads', null) + '<p class="muted">Songs kept for offline listening live in the Pubky Music app on your phone or laptop.</p>'; break; }
          let d; try { d = await api('GET', '/offline'); } catch (e) { main.innerHTML = pageHead('Downloads', null) + `<div class="notice">${esc(e.message)}</div>`; break; }
          if (state.view !== 'downloads') return;
          const byId = new Map(d.tracks.map((t) => [t.id, t]));
          const songsOf = (ids) => ids.map((i) => byId.get(i)).filter(Boolean);
          const facts = [d.synced_at ? `Last synced ${ago(d.synced_at)}` : '', fileSize(d.bytes), d.queued ? `${countOf(d.queued, 'plays and hearts')} to send when your node is back` : ''].filter(Boolean).join(' · ');
          const head = pageHead('Downloads', d.tracks.length || null) + `<p class="muted small dl-note" role="status">Offline: playing what this device keeps. ${esc(facts)}</p>`;
          if (!d.tracks.length) { main.innerHTML = head + '<div class="empty">' + icon('download') + '<h3>Nothing on this device yet</h3><p>When your node is back, keep albums and playlists here to play them without it.</p></div>'; break; }
          const lists = {};
          const albumOpen = d.albums.find((a) => a.id === state.dlAlbum);
          const albumTile = (a) => `<button class="tile" data-dlalbum="${a.id}" title="${esc(a.title)}">${art(a.id, a.title, '', 'library', true)}<div class="t">${esc(a.title)}</div><div class="a">${countOf(a.track_ids.length, 'songs')}</div>${coverFrom('library')}</button>`;
          let html = head;
          if (albumOpen) {
            lists.album = songsOf(albumOpen.track_ids);
            html += `<section class="section"><div class="section-head"><h3>${esc(albumOpen.title)}</h3><div class="actions"><button class="btn sm primary" data-dlplay="album">${icon('play')} Play</button><button class="btn sm ghost" data-dlclose>All downloads</button></div></div>${trackRows(lists.album, { showAlbum: false, numbers: 'track', key: 'album' })}</section>`;
          }
          if (d.albums.length) html += `<section class="section"><div class="section-head"><h3>Albums <span class="muted small">${d.albums.length}</span></h3></div><div class="grid">${d.albums.map(albumTile).join('')}</div></section>`;
          const pls = d.playlists.filter((p) => p.track_ids.length);
          if (pls.length) html += `<section class="section"><div class="section-head"><h3>Playlists</h3></div><div class="list">${pls.map((p, k) => `<div class="list-row"><div class="grow"><div class="name">${esc(p.name || 'Playlist')}</div><div class="sub">${p.track_ids.length === p.total ? countOf(p.total, 'songs') : `${p.track_ids.length} of ${p.total} songs on this device`}</div></div><button class="btn sm" data-dlpl="${k}">${icon('play')} Play</button></div>`).join('')}</div></section>`;
          const favs = songsOf(d.favourites.track_ids || []);
          if (favs.length) { lists.fav = favs; html += `<section class="section"><div class="section-head"><h3>Favourites</h3><button class="btn sm ghost" data-dlplay="fav">${icon('play')} Play</button></div>${trackRows(favs, { key: 'fav' })}</section>`; }
          lists.all = [...d.tracks].sort((a, b) => String(a.title).localeCompare(String(b.title)));
          html += `<section class="section"><div class="section-head"><h3>All songs <span class="muted small">${d.tracks.length}</span></h3><button class="btn sm ghost" data-dlplay="all">${icon('shuffle')} Shuffle</button></div>${trackRows(lists.all, { key: 'all' })}</section>`;
          main.innerHTML = html;
          bindTrackRows(main, lists);
          $$('[data-dlalbum]', main).forEach((b) => b.onclick = () => { state.dlAlbum = Number(b.dataset.dlalbum); render(); main.scrollTo && main.scrollTo(0, 0); });
          const dc = $('[data-dlclose]', main); if (dc) dc.onclick = () => { state.dlAlbum = null; render(); };
          $$('[data-dlplay]', main).forEach((b) => b.onclick = () => { const k = b.dataset.dlplay; const ts = k === 'all' ? [...lists.all].sort(() => Math.random() - 0.5) : lists[k]; if (ts && ts.length) playTracks(ts, true); });
          $$('[data-dlpl]', main).forEach((b) => b.onclick = () => playTracks(songsOf(pls[Number(b.dataset.dlpl)].track_ids), true));
          break;
        }
        case 'settings': {
          if (isOffline()) {
            const help = (h) => (h ? `<p class="set-help">${h}</p>` : '');
            const row = (label, h, ctl) => `<div class="set-row"><div class="set-label"><div class="set-name">${label}</div>${help(h)}</div>${ctl ? `<div class="set-ctl">${ctl}</div>` : ''}</div>`;
            const group = (title, body) => `<div class="set-group"><h3 class="set-h">${title}</h3><div class="set-rows">${body}</div></div>`;
            let d = null; try { d = await api('GET', '/offline'); } catch { d = null; }
            if (state.view !== 'settings') return;
            main.innerHTML = pageHead('Settings', null)
              + group('This device', row('Offline', `Your node is out of reach, so this app plays what it keeps. It reconnects by itself.${d && d.synced_at ? ` Last synced ${esc(ago(d.synced_at))}.` : ''}`, '')
                + (d ? row('Kept here', `${countOf(d.tracks.length, 'songs')} · ${esc(fileSize(d.bytes))}${d.queued ? ` · ${countOf(d.queued, 'plays and hearts')} waiting to be sent` : ''}`, '<button class="btn" data-view-go="downloads">Downloads</button>') : ''))
              + group('Theme', row('Light or dark', 'Or follow this device.', `<div class="chips themeswitch" role="radiogroup" aria-label="Theme">${[['system', 'System'], ['light', 'Light'], ['dark', 'Dark']].map(([v, l]) => `<button class="chip scope ${themePref() === v ? 'on' : ''}" role="radio" aria-checked="${themePref() === v}" data-theme-pick="${v}">${l}</button>`).join('')}</div>`));
            $$('[data-theme-pick]', main).forEach((b) => b.onclick = () => { setTheme(b.dataset.themePick); $$('[data-theme-pick]', main).forEach((x) => { const on = x === b; x.classList.toggle('on', on); x.setAttribute('aria-checked', String(on)); }); });
            $$('[data-view-go]', main).forEach((b) => b.onclick = () => setView(b.dataset.viewGo));
            break;
          }
          if (isListener()) {
            const me = state.me; const info = state.info || await api('GET', '/info');
            const [who] = await loadProfiles([me.key]);
            const help = (h) => (h ? `<p class="set-help">${h}</p>` : '');
            const row = (label, h, ctl) => `<div class="set-row"><div class="set-label"><div class="set-name">${label}</div>${help(h)}</div>${ctl ? `<div class="set-ctl">${ctl}</div>` : ''}</div>`;
            const group = (title, body) => `<div class="set-group"><h3 class="set-h">${title}</h3><div class="set-rows">${body}</div></div>`;
            const zonesText = me.zones && me.zones.length ? me.zones.map((z) => (state.zones[z] ? state.zones[z].name : z)).join(', ') : 'any of its zones';
            main.innerHTML = pageHead('Settings', null)
              + group('You', row(esc((who && who.name) || 'Signed in with Pubky Ring'), keyHtml(me.key), '<button class="btn" id="lsignout">Sign out of this device</button>')
                + row(`On ${esc(info.name)}`, `You can play its music in this browser and on ${esc(zonesText)}. Favourites, playlists and the node’s settings belong to whoever runs it; your own come in a later version.`, ''))
              + group('Theme', row('Light or dark', 'Or follow this device.', `<div class="chips themeswitch" role="radiogroup" aria-label="Theme">${[['system', 'System'], ['light', 'Light'], ['dark', 'Dark']].map(([v, l]) => `<button class="chip scope ${themePref() === v ? 'on' : ''}" role="radio" aria-checked="${themePref() === v}" data-theme-pick="${v}">${icon(v === 'dark' ? 'moon' : v === 'light' ? 'sun' : 'settings')} ${l}</button>`).join('')}</div>`)) + layoutGroup(group, row) + colourGroup(group, row);
            $$('[data-theme-pick]', main).forEach((b) => b.onclick = () => { setTheme(b.dataset.themePick); $$('[data-theme-pick]', main).forEach((x) => { const on = x === b; x.classList.toggle('on', on); x.setAttribute('aria-checked', String(on)); }); }); bindLayoutPick();
            $('#lsignout').onclick = async () => { try { await api('DELETE', '/me/token'); } catch { /* signed out either way */ } state.token = ''; try { localStorage.removeItem('pm.token'); } catch { /* no storage */ } location.reload(); };
            break;
          }
          const info = await api('GET', '/info'); state.info = info;
          const outs = await api('GET', '/outputs');
          const ns = await api('GET', '/node/settings').catch(() => ({ contact: '', discovery: false }));
          // The homeserver bundled with the node (deploy/bundle) and its Cloudflare Tunnel, when there is one
          const bundled = await api('GET', '/homeserver/bundled').catch(() => null);
          const tunnel = await api('GET', '/remote/tunnel').catch(() => null);
          const hsHere = !!(bundled && bundled.configured && bundled.reachable);
          let pk = null; try { pk = await api('GET', '/pubky'); state.pubkyStatus = pk; state.pubkyUser = pk.user || null; } catch { /* no pubky config */ }
          const fresh = (c) => c && Date.now() - c.at < 30000;
          const [nodeMap, prefs, apps, nodeHome] = await Promise.all([
            api('GET', '/node').catch(() => null),
            fresh(state.prefsCache) ? state.prefsCache.v : (() => { const gen = state.prefsGen || 0; return api('GET', '/me/prefs').then((v) => { if (gen === (state.prefsGen || 0)) state.prefsCache = { at: Date.now(), v }; return v; }).catch(() => null); })(), // an answer older than a change isn't kept
            api('GET', '/node/apps').catch(() => null),
            api('GET', '/home/layout').catch(() => null),
          ]);
          if (state.view !== 'settings') return;
          const saved = Object.fromEntries(((nodeMap && nodeMap.groups) || []).map((g) => [g.id, g.saved]));
          // One topic per tab, settings as rows (STEWARD-UX-SETTINGS.md). Every panel is in the page
          // (their controls stay wired); only the chosen one shows. On a phone: the list, then a tab.
          const tabs = sideTabs({ view: 'settings', title: 'Settings', tabs: settingsTabs({ saved, bundled: !!(bundled && bundled.configured), tunnel: !!(tunnel && tunnel.available) }), prefix: 'set', ids: { backup: 'backupcard', import: 'roonmove' }, remembered: state.settingsTab });
          const { phone, active } = tabs;
          if (active) state.settingsTab = active;
          const help = (h) => (h ? `<p class="set-help">${h}</p>` : '');
          const row = (label, h, ctl, cls = '') => `<div class="set-row ${cls}"><div class="set-label"><div class="set-name">${label}</div>${help(h)}</div>${ctl ? `<div class="set-ctl">${ctl}</div>` : ''}</div>`;
          const block = (label, h, body) => `<div class="set-row set-block">${label || h ? `<div class="set-label">${label ? `<div class="set-name">${label}</div>` : ''}${help(h)}</div>` : ''}<div class="set-body">${body}</div></div>`;
          const group = (title, body) => `<div class="set-group">${title ? `<h3 class="set-h">${title}</h3>` : ''}<div class="set-rows">${body}</div></div>`;
          const panel = (k, html) => tabs.panel(k, html);
          state.devOutputs = !!ns.dev_outputs;
          const shownOuts = outs.filter((o) => state.devOutputs || !devOutput(o.id));
          const outList = shownOuts.length ? `<div class="set-list">${shownOuts.map((o) => `<div class="set-item"><div class="grow"><div class="name">${esc(o.name)}</div><div class="sub">${esc(o.id)}</div></div><span class="muted small caps">${o.caps.max_pcm_bits}-bit · ${o.caps.pcm_rates.map((r) => (r / 1000).toFixed(1).replace('.0', '') + 'k').join(' ')}${o.caps.dsd ? ' · DSD' : ''}</span></div>`).join('')}</div>` : '<p class="muted small">No outputs found.</p>';
          const zoneRows = zoneList().map((z) => row(esc(z.name), esc(z.output || ''), `<span class="pill ${z.transport === 'playing' ? 'ok' : ''}">${esc(z.transport)}</span>${z.id !== BROWSER ? `<button class="btn sm ${state.dspOpen === z.id ? 'primary' : 'ghost'}" data-dsp="${esc(z.id)}">DSP</button>` : ''}${String(z.id).startsWith('group:') ? `<button class="btn sm ghost" data-zone-del="${esc(z.id)}">Remove</button>` : ''}`)).join('');
          const playback = group('Play from', block('Where songs play from', 'The first source here that has a song plays it. A playlist can have its own order; a pinned song keeps its source.', '<div id="gordercard"><div id="gorder"><div class="skeleton" style="height:120px"></div></div></div>')
            + row('Prefer full tracks', 'On a service\u2019s song page, Play plays that service, even a 30 s preview. With this on, a preview gives way to the full track from your Play from order.', `<label><input type="checkbox" id="preferfull" ${ns.prefer_full ? 'checked' : ''}> On</label>`))
            + group('Speakers and outputs', block('', 'What this node can play to, found on your network. A Raspberry Pi or computer next to a DAC: run squeezelite on it with this node as its server (squeezelite -s and this node\u2019s address). Network speakers (UPnP, Sonos, HEOS, Chromecast, AirPlay, BluOS) are found by themselves; MPD and Snapcast are set in the node\u2019s config. The computer or phone the Pubky Music app runs on: the app adds its own player, which plays only for this node.', `${outList}<div class="toolbar" style="margin-top:10px"><button class="btn" id="rescan" title="Look again for speakers, streamers and devices on your network">Look for speakers again</button><button class="btn ghost" id="netscan" title="Ask every UPnP and Cast device on the network and explain what answers">${icon('search')} Explain what is on the network</button></div><div id="netscanout"></div>`)
              + block('Add a renderer by address', 'For a UPnP speaker or streamer the network scan doesn’t find: its IP address or its description URL. For a Linn: its IP address, MAC address and serial number, with spaces between.', `<div class="toolbar"><input class="input" id="rendaddr" placeholder="192.168.1.20" aria-label="Renderer address"><button class="btn" id="rendadd">${icon('plus')} Add renderer</button></div><div id="rendsaved" class="muted small"></div>`))
            + group('Zones', zoneRows + '<div id="dspbox"></div>'
              + block('Group outputs', 'Pick two or more of one kind to play as one zone, in sync; the first one checked leads. Sonos, HEOS and BluOS speakers group with their own kind, squeezelite players with each other, and so do the app\u2019s players on a DAC.', shownOuts.filter(groupKind).length > 1 ? `<div id="groupouts">${GROUP_KINDS.map(([k, label]) => { const os = shownOuts.filter((o) => groupKind(o) === k); return os.length ? `<div class="groupkind"><div class="small muted">${label}</div><div class="chips">${os.map((o) => `<label class="chip"><input type="checkbox" value="${esc(o.id)}" style="margin:0 6px 0 0"> ${esc(o.name)}</label>`).join('')}</div></div>` : ''; }).join('')}</div><p class="small muted" id="groupsync" role="status"></p><div class="toolbar" style="margin-top:10px"><input class="input" id="groupname" placeholder="Group name (Everywhere)" aria-label="Group name"><button class="btn" id="groupmake">${icon('plus')} Create group</button></div>` : '<p class="muted small">No two speakers of one kind yet.</p>'))
            + group('Subsonic apps', row('Jukebox zone', 'The zone Subsonic apps (Symfonium, Feishin…) control with their jukebox mode.', `<select class="input" id="jukeboxzone" aria-label="Subsonic jukebox zone"><option value="" ${ns.jukebox_zone ? '' : 'selected'}>First zone by name</option>${zoneList().filter((z) => z.id !== BROWSER).map((z) => `<option value="${esc(z.id)}" ${ns.jukebox_zone === z.id ? 'selected' : ''}>${esc(z.name)}</option>`).join('')}</select>`));
          const accounts = group('Pubky', pk ? (
              (!pk.user && hsHere ? row('Create your Pubky account on this node\u2019s homeserver', 'Your playlists, favourites, tags and files stay on this machine. Pubky Ring creates the account here and signs this node in.', '<button class="btn primary" id="pkhscreate">Create account here</button>') : '')
              + row(pk.user ? 'Signed in' : hsHere ? 'Use an existing homeserver' : 'Not signed in', pk.user ? keyHtml(pk.user) : hsHere ? 'Already have a Pubky account somewhere else? Sign in with Pubky Ring instead.' : 'Sign in with the Pubky Ring app to keep your playlists, favourites and tags in your own Pubky storage.', pk.user ? `<button class="btn ${pk.needs_resign ? 'primary' : 'ghost'}" id="pklogin" title="Approve this node again in Pubky Ring (replaces the current session)">Sign in again</button>` : `<button class="btn ${hsHere ? 'ghost' : 'primary'}" id="pklogin">Sign in with Pubky</button>`)
              + (pk.needs_resign ? `<div class="notice warn resign" role="status" id="pkresign"><span>${esc(pk.notice || RESIGN_TEXT)}</span></div>` : '')
              + '<div id="pkurl"></div>'
              + (pk.user ? row('Your music', 'Your files on the homeserver, saving and tagging are in Your library.', '<button class="btn ghost" data-view-go="library">Open Your library</button>') : row('Saving', 'Starts once you sign in: then your playlists, favourites and tags are kept on your homeserver.', ''))
            ) : row('Pubky', 'Pubky is not set up on this node.', ''))
            + group('ListenBrainz', block('', '', '<div id="lbcard"><div id="lbbody"><div class="skeleton" style="height:80px"></div></div></div>'))
            + group('Last.fm', block('', '', '<div id="lfcard"><div id="lfbody"><div class="skeleton" style="height:80px"></div></div></div>'))
            + group('Streaming services', row('TIDAL, Spotify, Deezer, Jamendo, your own servers', 'Sign in to them on Sources.', '<button class="btn ghost" data-goto-sources>Open Sources</button>'));
          const discover = group('Contact', block('Your email or website', 'The music index (Pinakes), MusicBrainz and Wikipedia ask each app for a way to reach whoever runs it. It goes only to them, and turns on what people on Pubky play.', `<div class="toolbar"><input class="input" id="contact" placeholder="you@example.org or https://your.site" value="${esc(ns.contact)}" aria-label="Your email or website"><button class="btn primary" id="contactsave">Save</button><span class="pill ${ns.discovery ? 'ok' : 'warn'}">${ns.discovery ? 'Discover on' : 'Discover off'}</span></div>`))
            + group('Concerts', block('Your city', 'For concerts near you on Home. Looked up once on OpenStreetMap; it stays on this node.', '<div class="toolbar"><input class="input" id="loccity" placeholder="City" aria-label="Your city"><button class="btn" id="locsave">Save</button><button class="btn ghost" id="locclear">Clear</button><span class="muted small" id="loccur"></span></div>'));
          const backup = group('Download a backup', block('', 'One file with everything needed to rebuild this node: matches and corrections, playlists, favourites, tags, play history, queues and settings. Never your music files.', `<form id="bkform" class="stack"><label class="small" for="bkpass">Passphrase for your sign-ins (TIDAL, Spotify, ListenBrainz, Pubky…)</label><input class="input" id="bkpass" type="password" autocomplete="new-password" placeholder="Leave empty to leave sign-ins out"><input class="input" id="bkpass2" type="password" autocomplete="new-password" placeholder="The same passphrase again" aria-label="Repeat the passphrase" hidden><p class="muted small">Without it you can restore everything else and sign in to those services again. Nothing opens the sign-ins without it.</p><div class="actions"><button class="btn primary" id="bkdown" type="submit">${icon('download')} Download a backup</button></div></form>`))
            + group('Every day or week, to a folder', block('', '', '<div id="bksched"><div class="skeleton" style="height:60px"></div></div>'))
            + group('Restore', block('', 'Replaces this node’s library and settings with the backup’s. Your music files are not touched.', `<label class="btn file-btn">${icon('upload')} Choose a backup file<input type="file" id="rsfile" accept=".pmbackup,.zip"></label><div id="rsout"></div>`));
          const appearance = group('Theme', row('Light or dark', 'Or follow this device.', `<div class="chips themeswitch" role="radiogroup" aria-label="Theme">${[['system', 'System'], ['light', 'Light'], ['dark', 'Dark']].map(([v, l]) => `<button class="chip scope ${themePref() === v ? 'on' : ''}" role="radio" aria-checked="${themePref() === v}" data-theme-pick="${v}">${icon(v === 'dark' ? 'moon' : v === 'light' ? 'sun' : 'settings')} ${l}</button>`).join('')}</div>`)) + layoutGroup(group, row) + colourGroup(group, row);
          const about = group('This node', row('Name', '', esc(info.name)) + row('Version', '', esc(info.version)) + row('Library', '', `${countOf(info.library.tracks, 'tracks')} · ${countOf(info.library.albums, 'albums')} · ${countOf(info.library.artists, 'artists')}`) + row('Files', '', `${info.library.files}${info.library.missing_files ? ` · <span class="warn-text">${info.library.missing_files} missing</span>` : ''}`))
            + group('Access', row('This browser', 'Changes only the token this browser uses to reach the node; the node’s token stays as it is.', '<button class="btn ghost" id="logout">Use a different token in this browser</button>')
              + (ns.token_replaceable
                ? block('The node’s token', 'A new token for the node: the old one stops working at once, so every app, browser and Subsonic app needs the new one. People you invited keep their own sign-ins.', '<div class="actions"><button class="btn" id="tokennew">Make a new token</button></div><div id="tokenout" aria-live="polite"></div>')
                : row('The node’s token', 'Set in the node’s config (<code>[api] token</code> in node.toml, <code>PM_TOKEN</code> in Docker): it is changed there.', `<a class="btn ghost sm" href="${TOKEN_GUIDE}" target="_blank" rel="noopener">${icon('external')} How to change it</a>`)))
            + group('For developers', row('Developer outputs', 'Show the Null output (it plays to nowhere, for testing) in the zone lists.', `<label><input type="checkbox" id="devouts" ${ns.dev_outputs ? 'checked' : ''}> Show</label>`))
            + group('Keyboard', block('', '', '<p class="muted small">Shortcuts: <kbd>/</kbd> search · <kbd>Space</kbd> play/pause · <kbd>←</kbd> <kbd>→</kbd> previous/next · <kbd>N</kbd> now playing · <kbd>Esc</kbd> close</p>'));
          // You › Home: whose layout applies; edit it on Home itself
          const applies = (a) => (a === 'yours' ? 'Yours' : 'The node’s default');
          const homeL = prefs && prefs.home_layout;
          const homeTab = group('Your Home', row('Rows on Home', `${homeL ? `${applies(homeL.applies)}. ` : ''}Which rows show and in what order; change them with Edit Home on Home.`, `<button class="btn ghost" id="sethomeedit">${icon('settings')} Edit Home</button>${homeL && homeL.applies === 'yours' ? ' <button class="btn ghost" id="sethomereset">Use the node’s default</button>' : ''}`)
            + row('Saved on', prefs ? (prefs.saved_on === 'homeserver' ? 'Your homeserver, so it follows you to every node and device.' : 'This node for now; it goes to your homeserver once it can be reached.') : '', ''));
          // This node › Home for everyone: the default, which Edit Home no longer writes (#123 follow-up).
          // Yours is what Home holds right now when it has loaded (a save may still be on its way), else the prefs answer.
          const mine = state.homeLayout || (homeL && homeL.value) || null;
          const shape = (l) => JSON.stringify([l.order || [], [...(l.hidden || [])].sort()]);
          const same = (x, y) => !!x && !!y && shape(x) === shape(y);
          const rowsOf = (l) => { if (!l) return 'Couldn’t read the node’s default'; const o = (l.order || []).length; const h = (l.hidden || []).length; if (!o && !h) return 'The built-in rows, in the built-in order'; return `${o ? `${countOf(o, 'rows')} arranged` : 'The built-in order'}${h ? `, ${countOf(h, 'rows')} hidden` : ''}`; };
          const canSetDefault = !!mine && (!homeL || homeL.applies === 'yours') && !same(mine, nodeHome); // only a layout of your own; a copy of the default may be old
          const whyDefault = !mine ? 'Your Home couldn’t be read.' : canSetDefault ? 'Copies your Home’s rows and order to the node; your own stays yours.' : homeL && homeL.applies !== 'yours' ? 'You haven’t arranged your own Home yet; use Edit Home on Home first.' : 'Your Home already is the default.';
          const nodeHomeTab = group('The default Home', row('Rows on Home for everyone', `${rowsOf(nodeHome)}. What Home shows to everyone who hasn’t arranged their own.`, '')
            + row('Make yours the default', `<span id="sethomedefault-why">${whyDefault}</span>`, `<button class="btn ghost" id="sethomedefault" aria-describedby="sethomedefault-why" ${canSetDefault ? '' : 'disabled'}>${icon('home')} Make my Home the default for everyone</button>`))
            + group('Get started', row('First steps on Home', ns.first_steps_hidden ? 'Hidden for everyone on this node.' : 'Shown on Home until its steps are done or you hide them.', `<label><input type="checkbox" id="firststeps" ${ns.first_steps_hidden ? '' : 'checked'}> Show</label>`));
          // You › Playback quality: your choice per service over the node's
          const qEntries = Object.entries((prefs && prefs.quality) || {});
          const qualityTab = group('Per service', qEntries.length
            ? qEntries.map(([src, q]) => row(esc(SERVICE_NAMES[src] || srcName(src)), applies(q.applies), `<span class="pill">${esc(String(q.value == null ? '—' : q.value))}</span>${q.applies === 'yours' ? ` <button class="btn ghost sm" data-qreset="${esc(src)}">Use the node’s</button>` : ''}`)).join('')
            : block('', 'Each service plays at the quality set on its page in Sources. Your own choice per service, over the node’s, comes here with the services’ quality choices.', '<button class="btn ghost" data-goto-sources>Open Sources</button>'));
          // This node › Service apps: the apps the admin sets once for everyone
          // Built in, or the admin's own app (Advanced); the service's limits in plain words
          const appFrom = (a) => a.from === 'settings' ? 'Your own app' : a.from === 'builtin' ? 'Built in' : a.from === 'config' ? 'Your own app, set in node.toml' : a.from ? 'Set' : '';
          const appAdvanced = (a) => !a.set ? '' : `<details class="small app-adv"><summary>Advanced: use your own app</summary><form class="toolbar" data-appform="${esc(a.id)}">${(a.fields || []).map((f) => `<label class="sr-only" for="app-${esc(a.id)}-${esc(f.name)}">${esc(a.name)} ${esc(f.label)}</label><input id="app-${esc(a.id)}-${esc(f.name)}" name="${esc(f.name)}" class="input" type="${f.secret ? 'password' : 'text'}" autocomplete="off" spellcheck="false" placeholder="${esc(f.label)}${f.optional ? ' (if your app has one)' : ''}" ${f.optional ? '' : 'required'}>`).join('')}<button class="btn primary">Save</button></form>${(a.redirects || []).length ? `<p class="muted small">Your app lists ${a.redirects.length > 1 ? 'one of these redirect addresses (the second only with a secret)' : 'this redirect address'}:</p><ul class="app-redirects">${a.redirects.map((r) => `<li><code class="path">${esc(r)}</code></li>`).join('')}</ul>` : ''}${webUrl(a.get_one) ? `<p class="muted small"><a href="${esc(webUrl(a.get_one))}" target="_blank" rel="noopener">${icon('external')} Where to make one</a></p>` : ''}</details>`;
          const appRow = (a) => `<div class="set-row"><div class="set-label"><div class="set-name">${esc(a.name)}</div>${help([appFrom(a), a.note ? esc(a.note) : ''].filter(Boolean).join(' · '))}${a.limits ? help(esc(a.limits)) : ''}${appAdvanced(a)}</div><div class="set-ctl">${a.configured ? `<span class="pill ok">${a.from === 'builtin' ? 'Built in' : 'Set'}</span>` : '<span class="pill">Not set up</span>'}${a.from === 'settings' && a.set && (a.fields || []).length ? ` <button class="btn ghost sm" data-appreset="${esc(a.id)}">${a.builtin ? 'Use built-in' : 'Remove'}</button>` : ''}</div></div>`;
          const appsTab = group('Set once, for everyone on this node', apps && apps.length ? apps.map(appRow).join('') : block('', 'No list from this node.', ''));
          // This homeserver: the bundled one
          const hsTab = bundled && bundled.configured ? group('The homeserver', row('Address', esc(bundled.icann_domain || ''), bundled.pubky_address ? `<code class="small">${esc(bundled.pubky_address)}</code>` : '')
            + row('Status', '', bundled.reachable ? `<span class="pill ok">Running${bundled.version ? ` · ${esc(bundled.version)}` : ''}</span>` : `<span class="pill bad">Not reachable</span>${bundled.error ? ` <span class="muted small">${esc(bundled.error)}</span>` : ''}`)
            + row('People', '', `${countOf(bundled.users || 0, 'accounts')}${bundled.disabled_users ? ` · ${bundled.disabled_users} turned off` : ''}`)
            + row('Invites', '', countOf(bundled.unused_invites || 0, 'unused invites'))
            + row('Storage', '', bundled.disk_used_mb != null ? `${bundled.disk_used_mb} MB used` : '—')) : '';
          main.innerHTML = tabs.page(`${panel('home', homeTab)}${panel('quality', qualityTab)}${panel('apps', appsTab)}${panel('nodehome', nodeHomeTab)}${hsTab ? panel('homeserver', hsTab) : ''}${panel('playback', playback)}${panel('accounts', accounts)}${panel('people', '<div id="peoplebox"><div class="skeleton" style="height:160px"></div></div>')}${tunnel && tunnel.available ? panel('remote', '<div id="remotebox"><div class="skeleton" style="height:160px"></div></div>') : ''}${panel('discover', discover)}${panel('backup', backup)}${panel('appearance', appearance)}${panel('about', about)}`);
          tabs.wire(main, (k) => { state.settingsTab = k; });
          $$('[data-goto-sources]', main).forEach((b) => b.onclick = () => setView('sources'));
          $$('[data-view-go]', main).forEach((b) => b.onclick = () => setView(b.dataset.viewGo));
          const he = $('#sethomeedit'); if (he) he.onclick = () => { state.homeEdit = true; setView('home'); };
          // Service apps: Save sends the fields to the app's own route; Use built-in sends its first field empty
          const appSend = async (a, body, msg) => { const [method, path] = a.set.split(' '); await api(method, path.replace(/^\/v1/, ''), body); toast(msg); render(); };
          $$('[data-appform]').forEach((f) => { f.onsubmit = async (e) => { e.preventDefault(); const a = (apps || []).find((x) => x.id === f.dataset.appform); if (!a) return; const body = {}; for (const fl of a.fields || []) { const v = (f.elements[fl.name] && f.elements[fl.name].value || '').trim(); if (v || !fl.optional) body[fl.name] = v; } const b = f.querySelector('button'); b.disabled = true; try { await appSend(a, body, `${a.name}: your own app is in use`); } catch (err) { b.disabled = false; toast(err.message, 'error'); } }; });
          $$('[data-appreset]').forEach((b) => { b.onclick = async () => { const a = (apps || []).find((x) => x.id === b.dataset.appreset); if (!a) return; b.disabled = true; try { await appSend(a, { [a.fields[0].name]: '' }, a.builtin ? `${a.name}: the built-in app is in use` : `${a.name}: your app is removed`); } catch (err) { b.disabled = false; toast(err.message, 'error'); } }; });
          const hd = $('#sethomedefault'); if (hd) hd.onclick = async () => { hd.disabled = true; try { await api('PUT', '/home/layout', { order: mine.order || [], hidden: mine.hidden || [] }); toast('Your Home is the default for everyone now'); render(); } catch (e) { hd.disabled = false; toast(e.message, 'error'); } };
          const hr = $('#sethomereset'); if (hr) hr.onclick = async () => { try { await api('PUT', '/me/prefs', { home_layout: null }); state.homeLayout = null; state.homeSaved = null; toast('Home follows the node’s default'); render(); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-qreset]', main).forEach((b) => b.onclick = async () => { try { await api('PUT', '/me/prefs', { quality: { [b.dataset.qreset]: null } }); toast('Back to the node’s quality'); render(); } catch (e) { toast(e.message, 'error'); } });
          const pff = $('#preferfull'); if (pff) pff.onchange = async () => { try { await api('PUT', '/node/settings', { prefer_full: pff.checked }); toast(pff.checked ? 'Previews give way to full tracks from your Play from order' : 'A song page plays its own service, previews included'); } catch (e) { toast(e.message, 'error'); pff.checked = !pff.checked; } };
          const fst = $('#firststeps'); if (fst) fst.onchange = async () => { try { await api('PUT', '/node/settings', { first_steps_hidden: !fst.checked }); toast(fst.checked ? 'First steps show on Home again' : 'First steps are hidden for everyone'); render(); } catch (e) { toast(e.message, 'error'); fst.checked = !fst.checked; } };
          const dvo = $('#devouts'); if (dvo) dvo.onchange = async () => { try { await api('PUT', '/node/settings', { dev_outputs: dvo.checked }); state.devOutputs = dvo.checked; toast(dvo.checked ? 'Developer outputs are shown' : 'Developer outputs are hidden'); renderZoneName(); } catch (e) { toast(e.message, 'error'); dvo.checked = !dvo.checked; } };
          // The first sync after a sign-in runs in the background: follow it here
          if (pk && pk.sync && pk.sync.running) { clearTimeout(state.syncPoll); state.syncPoll = setTimeout(() => { if (state.view === 'settings' && state.settingsTab === 'accounts') render(); }, 2000); }
          $$('[data-zone-del]', main).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/zones/${encodeURIComponent(b.dataset.zoneDel)}`); toast('Group removed'); if (state.zone === b.dataset.zoneDel) state.zone = BROWSER; render(); } catch (e) { toast(e.message, 'error'); } });
          // What the outputs picked become, and which can't join them (no sample rate in common)
          const groupBox = $('#groupouts');
          const order = []; // the order they were checked in: the first leads
          if (groupBox) {
            const byId = Object.fromEntries(shownOuts.map((o) => [o.id, o]));
            const update = () => {
              const picked = order.map((id) => byId[id]);
              const rates = picked.length ? picked.reduce((r, o) => r.filter((x) => o.caps.pcm_rates.includes(x)), picked[0].caps.pcm_rates) : null;
              const kind = picked.length ? groupKind(picked[0]) : null;
              $$('input', groupBox).forEach((i) => { const o = byId[i.value]; const other = !i.checked && !!kind && groupKind(o) !== kind; const off = other || (!i.checked && (kind === 'node' || kind === 'net') && !o.caps.pcm_rates.some((x) => rates.includes(x))); i.disabled = off; i.closest('label').title = other ? 'Another kind: speakers group with their own kind' : off ? 'Shares no sample rate with the ones picked' : ''; });
              $('#groupsync').textContent = groupSync(picked);
            };
            $$('input', groupBox).forEach((i) => i.onchange = () => { const at = order.indexOf(i.value); if (i.checked && at < 0) order.push(i.value); if (!i.checked && at >= 0) order.splice(at, 1); update(); });
          }
          const gm = $('#groupmake'); if (gm) gm.onclick = async () => { const ids = [...order]; const name = $('#groupname').value.trim() || 'Group'; if (ids.length < 2) { toast('Pick at least two outputs', 'error'); return; } try { const z = await api('POST', '/zones', { name, outputs: ids }); state.zones[z.id] = z; toast(`Group “${name}” created`); render(); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-dsp]', main).forEach((b) => b.onclick = () => { state.dspOpen = state.dspOpen === b.dataset.dsp ? null : b.dataset.dsp; render(); });
          if (state.dspOpen && state.zones[state.dspOpen]) {
            try { const cfg = await api('GET', `/zones/${encodeURIComponent(state.dspOpen)}/dsp`); $('#dspbox').innerHTML = dspEditor(state.zones[state.dspOpen], cfg); bindDspEditor(state.dspOpen, cfg); } catch (e) { $('#dspbox').innerHTML = `<div class="notice warn">${esc(e.message)}</div>`; }
          }
          const saveContact = async () => { const c = $('#contact').value.trim(); if (!c) return; try { await api('PUT', '/node/settings', { contact: c }); state.discoveryOff = false; homeCache.clear(); toast('Contact saved; Discover is on'); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#contactsave').onclick = saveContact; $('#contact').onkeydown = (e) => { if (e.key === 'Enter') saveContact(); };
          api('GET', '/home/location').then((l) => { const c = $('#loccur'); if (c) c.textContent = l ? `Now: ${l.label || `${l.lat.toFixed(2)}, ${l.lon.toFixed(2)}`}` : 'Not set'; }).catch(() => {});
          $('#locsave').onclick = async () => { const city = $('#loccity').value.trim(); if (!city) return; try { const l = await api('PUT', '/home/location', { city }); toast(`Concerts near ${l.label}`); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#locclear').onclick = async () => { try { await api('PUT', '/home/location', {}); toast('City cleared'); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#rescan').onclick = async () => { try { await api('POST', '/outputs/rescan'); toast('Outputs rescanned'); render(); } catch (e) { toast(e.message, 'error'); } };
          const showSaved = async () => { try { const r = await api('GET', '/outputs/discovery?quick=1'); const box = $('#rendsaved'); if (!box) return; box.innerHTML = r.saved_addresses.length ? 'Added by address: ' + r.saved_addresses.map((a) => `<code>${esc(a)}</code> <button class="link" data-rendrm="${esc(a)}">forget</button>`).join(' · ') : ''; $$('[data-rendrm]', box).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/outputs/upnp/${encodeURIComponent(b.dataset.rendrm)}`); toast('Address forgotten (its zone stays until restart)'); showSaved(); } catch (e) { toast(e.message, 'error'); } }); } catch { /* older node */ } };
          showSaved();
          needsText($('#rendaddr'), $('#rendadd'));
          const ra = $('#rendadd'); if (ra) ra.onclick = async () => {
            const address = $('#rendaddr').value.trim(); if (!address) return; ra.disabled = true;
            try { const outs = await api('POST', '/outputs/upnp', { address }); toast(`Renderer added · ${outs.length} outputs`); $('#rendaddr').value = ''; render(); } catch (e) { toast(e.message, 'error'); }
            ra.disabled = false;
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
          const tn = $('#tokennew'); if (tn) tn.onclick = async () => {
            const go = await choiceDialog({ title: 'Make a new token?', text: 'The old token stops working at once. Every app, browser and Subsonic app then needs the new one; this browser switches to it now.', choices: [{ label: 'Make a new token', value: true, primary: true }] });
            if (!go) return;
            tn.disabled = true;
            try {
              const r = await api('POST', '/node/token');
              // Every later call (and a reconnect of the event socket) uses it
              state.token = r.token; try { localStorage.setItem('pm.token', r.token); } catch { /* this tab keeps it */ }
              $('#tokenout').innerHTML = `<p class="small" style="margin-top:10px">The new token, shown only now. Copy it to your apps; the page then reloads, so its streams and covers use it too.</p><div class="toolbar"><code class="path">${esc(r.token)}</code><button class="btn sm primary" id="tokendone">Copy and reload</button></div>`;
              $('#tokendone').onclick = async () => { try { await navigator.clipboard.writeText(r.token); } catch { /* copied by hand */ } location.reload(); };
            } catch (e) { toast(e.message, 'error'); tn.disabled = false; }
          };
          const jz = $('#jukeboxzone'); if (jz) jz.onchange = async () => { try { await api('PUT', '/node/settings', { jukebox_zone: jz.value }); toast(jz.value ? 'Jukebox zone saved' : 'Jukebox follows the first zone'); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-theme-pick]', main).forEach((b) => b.onclick = () => { setTheme(b.dataset.themePick); $$('[data-theme-pick]', main).forEach((x) => { const on = x === b; x.classList.toggle('on', on); x.setAttribute('aria-checked', String(on)); }); }); bindLayoutPick();
          const loadLf = () => signinChooser(lastfmSigninCfg($('#lfbody')));
          loadLf();
          const loadLb = async () => {
            const box = $('#lbbody'); if (!box) return;
            let lb; try { lb = await api('GET', '/listenbrainz'); } catch (e) { box.innerHTML = `<p class="muted small">${esc(e.message)}</p>`; return; }
            if (!lb.connected) {
              box.innerHTML = `<p class="muted small">Send what you play to ListenBrainz, bring your listens here, and get its recommendations. Your user token is on <a href="https://listenbrainz.org/settings/" target="_blank" rel="noopener">listenbrainz.org/settings</a>; it stays on this node.</p><form class="row" id="lbform"><input class="input" name="token" type="password" placeholder="User token" autocomplete="off" style="flex:1;min-width:200px" required><button class="btn primary" id="lbconnect" type="submit">Connect</button></form>`;
              $('#lbform').onsubmit = async (e) => { e.preventDefault(); const b = $('#lbconnect'); b.disabled = true; try { await api('PUT', '/listenbrainz', { token: new FormData($('#lbform')).get('token') }); toast('ListenBrainz connected'); loadLb(); } catch (er) { toast(er.message, 'error'); b.disabled = false; } };
              return;
            }
            let pls = []; try { pls = await api('GET', '/listenbrainz/playlists'); } catch { /* none */ }
            box.innerHTML = `<div class="toolbar"><span class="pill ok account">Connected · ${esc(lb.user || '')}</span>${lb.pending ? `<span class="pill warn" title="ListenBrainz did not take them yet; they go with the next listen">${lb.pending} waiting</span>` : ''}</div>`
              + `<label class="check small"><input type="checkbox" id="lbsubmit" ${lb.submit ? 'checked' : ''}> Send what I play here</label>`
              + `<div class="toolbar" style="margin-top:8px"><button class="btn sm" id="lbimport">${icon('download')} Bring my listens here</button><button class="btn sm ghost" id="lbout">Disconnect</button></div>`
              + (pls.length ? `<p class="muted small" style="margin-top:12px">Made for you</p><div class="list">${pls.slice(0, 6).map((p) => `<div class="list-row"><div class="grow"><div class="name" title="${esc(p.name)}">${esc(p.name)}</div><div class="sub">${p.songs ? p.songs + ' songs' : 'ListenBrainz playlist'}</div></div><button class="btn sm" data-lbpl="${esc(p.id)}" aria-label="Import ${esc(p.name)}">${icon('download')} Import</button></div>`).join('')}</div>` : '');
            $('#lbsubmit').onchange = async (e) => { try { await api('PUT', '/listenbrainz', { submit: e.target.checked }); toast(e.target.checked ? 'Plays go to ListenBrainz' : 'Plays stay on this node'); } catch (er) { toast(er.message, 'error'); } };
            $('#lbimport').onclick = async (e) => { e.currentTarget.disabled = true; try { const r = await api('POST', '/listenbrainz/import'); toast(`${r.imported} listen${r.imported === 1 ? '' : 's'} added to your history${r.skipped ? `, ${r.skipped} without a MusicBrainz id skipped` : ''}`); } catch (er) { toast(er.message, 'error'); } loadLb(); };
            $('#lbout').onclick = async () => { try { await api('DELETE', '/listenbrainz'); toast('ListenBrainz disconnected'); loadLb(); } catch (er) { toast(er.message, 'error'); } };
            $$('[data-lbpl]', box).forEach((b) => b.onclick = async () => { b.disabled = true; try { const r = await api('POST', `/listenbrainz/playlists/${b.dataset.lbpl}/import`); state.playlists = await api('GET', '/playlists'); toast(`Imported “${r.playlist.summary.name}”: ${countOf(r.imported, 'songs')}`); } catch (er) { toast(er.message, 'error'); } b.disabled = false; });
          };
          loadLb();
          // ---- backup and restore (steward next-4, item 1)
          const tb = $('#tobackup'); if (tb) tb.onclick = () => $('#backupcard').scrollIntoView({ behavior: 'smooth', block: 'start' });
          const authH = () => (state.token ? { authorization: 'Bearer ' + state.token } : {});
          const saveBlob = (blob, name) => { const a = document.createElement('a'); a.href = URL.createObjectURL(blob); a.download = name; document.body.appendChild(a); a.click(); setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 1000); };
          $('#bkpass').oninput = () => { $('#bkpass2').hidden = !$('#bkpass').value; };
          $('#bkform').onsubmit = async (e) => {
            e.preventDefault();
            const p = $('#bkpass').value; if (p && p !== $('#bkpass2').value) { toast('The two passphrases differ', 'error'); return; }
            const b = $('#bkdown'); b.disabled = true;
            try {
              const res = await fetch('/v1/backup', { method: 'POST', headers: { ...authH(), ...(p ? { 'x-backup-passphrase': p } : {}) } });
              if (!res.ok) throw new Error((await res.json().catch(() => ({}))).error || res.status);
              const name = (/filename="([^"]+)"/.exec(res.headers.get('content-disposition') || '') || [])[1] || 'pubky-music.pmbackup';
              saveBlob(await res.blob(), name); toast(p ? 'Backup saved, sign-ins locked with your passphrase' : 'Backup saved (without sign-ins)');
            } catch (er) { toast(er.message, 'error'); }
            b.disabled = false;
          };
          const when = (t) => (t ? new Date(t * 1000).toLocaleString() : 'never');
          const nOf = (n, one, many) => `${n} ${n === 1 ? one : many}`;
          const countsLine = (c) => `${nOf(c.tracks, 'track', 'tracks')} (${c.identified} identified) · ${nOf(c.playlists, 'playlist', 'playlists')} · ${nOf(c.favourites, 'favourite', 'favourites')} · ${nOf(c.tags, 'tag', 'tags')} · ${nOf(c.plays, 'play', 'plays')}`;
          const loadSched = async () => {
            const box = $('#bksched'); if (!box) return;
            let sc; try { sc = await api('GET', '/backup/schedule'); } catch (er) { box.innerHTML = `<p class="muted small">${esc(er.message)}</p>`; return; }
            box.innerHTML = `<p class="muted small">A folder on the node, best on another disk or a NAS. The newest backups are kept, older ones removed.</p>
              <div class="toolbar"><input class="input" id="bkfolder" placeholder="/path/to/backups" value="${esc(sc.folder)}" aria-label="Backup folder on the node"></div>
              <div class="toolbar"><label class="small">Every <select class="input" id="bkevery" aria-label="How often">${[[24, 'day'], [168, 'week']].map(([h, l]) => `<option value="${h}" ${sc.every_hours === h ? 'selected' : ''}>${l}</option>`).join('')}</select></label><label class="small">Keep <input class="input" id="bkkeep" type="number" min="1" max="365" value="${sc.keep}" style="width:80px" aria-label="How many to keep"></label></div>
              <div class="toolbar"><input class="input" id="bkspass" type="password" autocomplete="new-password" placeholder="${sc.with_signins ? 'Passphrase saved: type a new one to change it' : 'Passphrase for sign-ins (optional)'}" aria-label="Passphrase for scheduled backups"></div>
              <div class="actions"><button class="btn" id="bksave">Save</button>${sc.folder ? '<button class="btn ghost" id="bknow">Back up now</button><button class="btn ghost" id="bkoff">Turn off</button>' : ''}</div>
              ${sc.folder ? `<p class="small ${sc.last_error ? 'warn-text' : 'muted'}" role="status">${sc.last_error ? `The last backup failed: ${esc(sc.last_error)}` : `Last backup: ${esc(when(sc.last_at))}${sc.last_file ? ` · ${esc(sc.last_file)}` : ''}`} · ${sc.files.length} in the folder · ${sc.with_signins ? 'with sign-ins' : 'without sign-ins'}</p>` : '<p class="muted small">Off.</p>'}`;
            const put = async (body, msg) => { try { await api('PUT', '/backup/schedule', body); toast(msg); loadSched(); } catch (er) { toast(er.message, 'error'); } };
            $('#bksave').onclick = () => { const f = $('#bkfolder').value.trim(); if (!f) { toast('Type the folder on the node', 'error'); return; } const pp = $('#bkspass').value; put({ folder: f, every_hours: Number($('#bkevery').value), keep: Number($('#bkkeep').value) || 7, ...(pp ? { passphrase: pp } : {}) }, 'Backups scheduled'); };
            const now = $('#bknow'); if (now) now.onclick = async () => { now.disabled = true; try { const r = await api('POST', '/backup/schedule/run'); toast(`Backup written: ${r.last_file}`); loadSched(); } catch (er) { toast(er.message, 'error'); now.disabled = false; } };
            const off = $('#bkoff'); if (off) off.onclick = () => put({ folder: '', passphrase: '' }, 'Scheduled backups off');
          };
          loadSched();
          $('#rsfile').onchange = async () => {
            const f = $('#rsfile').files[0]; const out = $('#rsout'); if (!f) return;
            const bytes = await f.arrayBuffer();
            out.innerHTML = '<div class="skeleton" style="height:60px"></div>';
            let info; try { const res = await fetch('/v1/restore?check=true', { method: 'POST', headers: { 'content-type': 'application/octet-stream', ...authH() }, body: bytes }); info = await res.json(); if (!res.ok) throw new Error(info.error || res.status); } catch (er) { out.innerHTML = `<div class="notice warn">${esc(er.message)}</div>`; return; }
            const c = info.counts;
            out.innerHTML = `<div class="notice" style="margin-top:10px"><p><b>${esc(f.name)}</b>: from “${esc(info.from_node)}”, ${esc(when(info.made_at))}.</p><p class="small">${countsLine(c)}</p>
              ${info.locked_signins ? '<label class="small" for="rspass">Its sign-ins are locked with a passphrase</label><input class="input" id="rspass" type="password" autocomplete="current-password" placeholder="Passphrase"><label class="check small"><input type="checkbox" id="rsskip"> Restore without the sign-ins</label>' : (info.left_out.length ? `<p class="small muted">Not in this backup: ${esc(info.left_out.join(', '))}. Sign in to those again afterwards.</p>` : '')}
              <div class="actions"><button class="btn primary" id="rsgo">Restore this backup</button></div></div><div id="rsreport"></div>`;
            $('#rsgo').onclick = async () => {
              const b = $('#rsgo');
              if (!b.dataset.armed) { b.dataset.armed = '1'; b.textContent = 'Replace this node’s library?'; setTimeout(() => { if (b.isConnected) { delete b.dataset.armed; b.textContent = 'Restore this backup'; } }, 4000); return; }
              b.disabled = true; b.textContent = 'Restoring…';
              const pass = $('#rspass') ? $('#rspass').value : ''; const skip = $('#rsskip') && $('#rsskip').checked;
              try {
                const res = await fetch(`/v1/restore${skip ? '?skip_secrets=true' : ''}`, { method: 'POST', headers: { 'content-type': 'application/octet-stream', ...authH(), ...(pass ? { 'x-backup-passphrase': pass } : {}) }, body: bytes });
                const r = await res.json(); if (!res.ok) throw new Error(r.error || res.status);
                const rc = r.restored;
                $('#rsreport').innerHTML = `<div class="notice ok-notice" role="status" style="margin-top:10px"><p><b>Restored.</b> ${countsLine(rc)}.${r.rescan_started ? ' Checking your music folders now; unchanged files keep their matches.' : ''}</p>
                  ${r.signed_in.length ? `<p class="small">Signed in again: ${esc(r.signed_in.join(', '))}.</p>` : ''}${r.sign_in_again.length ? `<p class="small">Sign in again: ${esc(r.sign_in_again.join(', '))}.</p>` : ''}${r.notes.map((n) => `<p class="small muted">${esc(n)}</p>`).join('')}
                  ${r.missing_folders.map((m, k) => `<div class="toolbar" style="margin-top:8px"><span class="small">Not on this computer: <code>${esc(m)}</code>. Where is that music now?</span><input class="input" data-mvto="${k}" placeholder="/path/to/the/music" aria-label="New place of ${esc(m)}"><button class="btn sm" data-mv="${k}">Use this folder</button></div>`).join('')}</div>`;
                $$('[data-mv]', out).forEach((mb) => mb.onclick = async () => { const k = Number(mb.dataset.mv); const to = $(`[data-mvto="${k}"]`, out).value.trim(); if (!to) return; try { const m = await api('POST', '/library/folders/move', { from: r.missing_folders[k], to }); toast(`${m.moved_files} files found in their new place`); mb.closest('.toolbar').remove(); } catch (er) { toast(er.message, 'error'); } });
                b.textContent = 'Restored'; state.pubkyUser = undefined; state.albums = null; homeCache.clear(); toast('Backup restored');
              } catch (er) { toast(er.message, 'error'); b.disabled = false; b.textContent = 'Restore this backup'; delete b.dataset.armed; }
            };
          };
          if (state.focusRestore) { state.focusRestore = false; $('#backupcard').scrollIntoView({ block: 'start' }); $('#rsfile').focus(); }
          const loadOrder = async () => { const box = $('#gorder'); if (!box) return; try { const o = await api('GET', '/play-order'); box.innerHTML = orderList(o); bindOrderList(box, o.order, async (order) => { try { await api('PUT', '/play-order', { order }); state.plPf = null; loadOrder(); } catch (e) { toast(e.message, 'error'); } }); } catch (e) { box.innerHTML = `<p class="muted small">${esc(e.message)}</p>`; } };
          loadOrder();
          const sy = $('#sync'); if (sy) sy.onclick = async () => { try { const r = await api('POST', '/pubky/sync'); syncToast(r); render(); } catch (e) { toast(e.message, 'error'); } };
          // Settings › People: invites to this node's homeserver and its music, and who has what
          const ACCESS = { homeserver_music: 'Homeserver + music', homeserver: 'Homeserver only', music: 'Music only' };
          const quotaText = (q) => (q == null ? 'the default quota' : q === 'unlimited' || q === 'Unlimited' ? 'no limit' : `${q} MB`);
          const inviteQr = (inv, hsKey) => (inv.url ? `<div class="notice qrbox" style="margin-top:10px">${inv.qr_svg ? `<div class="qr">${inv.qr_svg}</div>` : ''}<div><p>Send this to ${inv.name ? esc(inv.name) : 'them'}: they scan it with Pubky Ring (or open the link on their phone), and Ring creates their account on this node’s homeserver.</p><p class="muted small">Older Ring versions can’t open it; they can sign up by hand with the code <code>${esc(inv.token || '')}</code>${hsKey ? ` and this homeserver’s key <code>${esc(hsKey)}</code>` : ''}.</p><a class="btn sm" href="${esc(ringUrl(inv.url))}">Open in Pubky Ring</a> <button class="btn sm ghost" data-copylink="${esc(inv.url)}">Copy link</button></div></div>` : '');
          const loadPeople = async () => {
            const box = $('#peoplebox'); if (!box) return;
            let r; try { r = await api('GET', '/people'); } catch (e) { box.innerHTML = `<p class="muted small">${esc(e.message)}</p>`; return; }
            const zl = await api('GET', '/people/zones').catch(() => ({ zones: [] }));
            const parties = await api('GET', '/party').catch(() => []);
            if (!box.isConnected) return;
            const hs = r.homeserver || {};
            const onHs = !!(hs.configured && hs.reachable);
            const kinds = onHs ? ['homeserver_music', 'homeserver', 'music'] : ['music'];
            const open = new Set(zl.zones || []);
            const zones = zoneList().filter((z) => z.id !== BROWSER);
            const person = (p) => {
              const bits = [ACCESS[p.access] || p.access];
              if (p.access !== 'music') bits.push(p.homeserver_enabled === false ? '<span class="warn-text">homeserver account off (can read, can’t write)</span>' : `${p.used_bytes != null ? `uses ${fmtBytes(p.used_bytes)} of ` : ''}${quotaText(p.storage_quota_mb)}`);
              if (p.music_enabled && p.sessions) bits.push(`signed in on ${countOf(p.sessions, 'devices')}`);
              const ctl = `<label class="check small"><input type="checkbox" data-pmusic="${esc(p.key)}" ${p.music_enabled ? 'checked' : ''}> Music</label>`
                + (p.access !== 'music' && onHs ? `<label class="check small"><input type="checkbox" data-phs="${esc(p.key)}" ${p.homeserver_enabled === false ? '' : 'checked'}> Homeserver</label><input class="input sm" type="number" min="1" style="width:9em" data-pquota="${esc(p.key)}" placeholder="Quota, MB" aria-label="Storage quota in MB for ${esc(p.name || shortKey(p.key))} (empty: no limit)" value="${typeof p.storage_quota_mb === 'number' ? p.storage_quota_mb : ''}">` : '');
              return row(p.name ? esc(p.name) : esc(shortKey(p.key)), `${p.name ? keyHtml(p.key) + ' · ' : ''}${bits.join(' · ')}`, ctl);
            };
            const invite = (inv) => row(inv.name ? esc(inv.name) : 'Unnamed invite', `${ACCESS[inv.access] || inv.access} · made ${new Date(inv.created_at * 1000).toLocaleDateString()}${inv.url ? `<details class="small"><summary>Show the invite again</summary>${inviteQr(inv, hs.public_key)}</details>` : ''}`, `<button class="btn sm ghost" data-pinv-del="${esc(inv.id)}" title="An invite that was sent stays usable on the homeserver; this only takes it off the list">Forget</button>`);
            box.innerHTML = group('This node’s homeserver', hs.configured
                ? row(onHs ? `Pubky homeserver ${esc(hs.version || '')}` : 'Not reachable', onHs ? `${countOf(hs.users || 0, 'accounts')}${hs.disabled_users ? ` (${hs.disabled_users} turned off)` : ''} · ${fmtBytes((hs.disk_used_mb || 0) * 1e6)} used · whoever runs it (you) can read every account’s files, <code>/priv</code> included, so tell the people you invite.` : esc(hs.error || 'The node can’t reach its homeserver’s admin API.'), '')
                : row('No homeserver on this node', 'Installed without its own homeserver (the bundled install has one). You can still let someone with a Pubky account elsewhere play this node’s music.', ''))
              + group('Invite someone', block('', 'Homeserver only: a Pubky account here, with a storage quota. Homeserver + music: the same, and they can sign in to this node’s music with Ring. Music only: someone whose Pubky account is elsewhere, by their key. Their own playlists and favourites live on their account.', `<div class="toolbar"><select class="input" id="pinvaccess" aria-label="What they get">${kinds.map((k) => `<option value="${k}">${ACCESS[k]}</option>`).join('')}</select><input class="input" id="pinvname" placeholder="Name (optional)" aria-label="Their name"><input class="input" id="pinvquota" type="number" min="1" placeholder="Quota, MB (empty: no limit)" aria-label="Storage quota in MB" ${onHs ? '' : 'hidden'}><input class="input" id="pinvkey" placeholder="Their Pubky key" aria-label="Their Pubky key" ${onHs ? 'hidden' : ''}><button class="btn primary" id="pinvgo">${icon('plus')} Create invite</button></div><div id="pinvout" aria-live="polite"></div>`))
              + (r.invites.length ? group('Not used yet', r.invites.map(invite).join('')) : '')
              + group('People', r.people.length ? r.people.map(person).join('') : row('Nobody yet', 'People you invite show here once they have signed up.', ''))
              + group('Party', (parties.map((pt) => row(esc(pt.name), `${pt.zone_name ? `On ${esc(pt.zone_name)} · ` : ''}until ${new Date(pt.expires_at * 1000).toLocaleString([], { weekday: 'short', hour: '2-digit', minute: '2-digit' })} · ${countOf(pt.guests.length, 'guests')} · ${countOf(pt.added, 'songs')} added · ${pt.adds_per_hour} per guest per hour, ${pt.waiting_per_guest} waiting at once<details class="small"><summary>Show the guest link</summary><div class="notice qrbox" style="margin-top:10px">${pt.qr_svg ? `<div class="qr">${pt.qr_svg}</div>` : ''}<div><p>Guests scan this or open the link: they give a name and add songs from the library to this zone. No account needed.</p><p><code>${esc(pt.url)}</code></p><button class="btn sm ghost" data-copylink="${esc(pt.url)}">Copy link</button></div></div></details>`, `<button class="btn sm ghost" data-party-end="${esc(pt.id)}">End party</button>`)).join(''))
                + (zones.length ? block(parties.length ? 'Start another party' : 'Start a party', 'Anyone with the link adds songs from the library to one zone, without an account. Their songs play after the current one, in the order they came; the limits are checked by the node.', `<div class="toolbar"><select class="input" id="ptzone" aria-label="Party zone">${zones.map((z) => `<option value="${esc(z.id)}">${esc(z.name)}</option>`).join('')}</select><input class="input" id="ptname" placeholder="Name (optional)" aria-label="Party name"><select class="input" id="pthours" aria-label="How long the link works">${[[3, '3 hours'], [6, '6 hours'], [12, '12 hours'], [24, 'A day'], [72, '3 days']].map(([h, l]) => `<option value="${h}" ${h === 6 ? 'selected' : ''}>${l}</option>`).join('')}</select></div><div class="toolbar" style="margin-top:8px"><label class="small" for="ptadds">Songs per guest per hour</label><input class="input" id="ptadds" type="number" min="1" max="100" value="5" style="width:6em"><label class="small" for="ptwait">Waiting at once</label><input class="input" id="ptwait" type="number" min="1" max="50" value="3" style="width:6em"><button class="btn primary" id="ptgo">Start party</button></div>`) : row('Start a party', 'Needs a zone to play on.', '')))
              + group('Zones listeners may play on', block('', 'People with music access can play on these. None checked: every zone.', (zones.length ? `<div class="chips" id="pzones">${zones.map((z) => `<label class="chip"><input type="checkbox" value="${esc(z.id)}" style="margin:0 6px 0 0" ${open.has(z.id) ? 'checked' : ''}> ${esc(z.name)}</label>`).join('')}</div><div class="actions" style="margin-top:10px"><button class="btn" id="pzsave">Save zones</button></div>` : '<p class="muted small">No zones yet.</p>')));
            const acc = $('#pinvaccess', box);
            const fit = () => { const music = acc.value === 'music'; $('#pinvquota', box).hidden = music; $('#pinvkey', box).hidden = !music; };
            acc.onchange = fit; fit();
            $('#pinvgo', box).onclick = async () => {
              const body = { access: acc.value, name: $('#pinvname', box).value.trim() || undefined };
              if (acc.value === 'music') { body.key = $('#pinvkey', box).value.trim(); if (!body.key) { toast('Paste their Pubky key', 'error'); return; } } else { const q = Number($('#pinvquota', box).value); if (q > 0) body.storage_quota_mb = Math.round(q); }
              try {
                const v = await api('POST', '/people/invites', body);
                if (acc.value === 'music') { toast('Added: they can sign in with Pubky Ring now'); loadPeople(); return; }
                await loadPeople();
                const out = $('#pinvout'); if (out) { out.innerHTML = inviteQr(v, hs.public_key); out.scrollIntoView({ behavior: 'smooth', block: 'center' }); }
              } catch (e) { toast(e.message, 'error'); }
            };
            const patch = async (key, body, done) => { try { await api('PATCH', `/people/${encodeURIComponent(key)}`, body); toast(done); loadPeople(); } catch (e) { toast(e.message, 'error'); loadPeople(); } };
            $$('[data-pmusic]', box).forEach((c) => c.onchange = () => patch(c.dataset.pmusic, { music: c.checked }, c.checked ? 'Music access on' : 'Music access off: their devices are signed out'));
            $$('[data-phs]', box).forEach((c) => c.onchange = () => patch(c.dataset.phs, { homeserver_enabled: c.checked }, c.checked ? 'Homeserver account on' : 'Homeserver account off: they can read and delete, not write'));
            $$('[data-pquota]', box).forEach((i) => i.onchange = () => { const q = Number(i.value); patch(i.dataset.pquota, { storage_quota_mb: q > 0 ? Math.round(q) : 'unlimited' }, q > 0 ? `Quota set to ${Math.round(q)} MB` : 'No storage limit'); });
            $$('[data-pinv-del]', box).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/people/invites/${encodeURIComponent(b.dataset.pinvDel)}`); toast('Invite taken off the list'); loadPeople(); } catch (e) { toast(e.message, 'error'); } });
            const ptg = $('#ptgo', box); if (ptg) ptg.onclick = async () => {
              const body = { zone: $('#ptzone', box).value, name: $('#ptname', box).value.trim() || undefined, hours: Number($('#pthours', box).value), adds_per_hour: Number($('#ptadds', box).value) || 5, waiting_per_guest: Number($('#ptwait', box).value) || 3 };
              try { await api('POST', '/party', body); toast('Party started: share the guest link'); await loadPeople(); const last = $$('#peoplebox details').filter((x) => x.querySelector('[data-copylink*="/party/"]')).pop(); if (last) { last.open = true; last.scrollIntoView({ behavior: 'smooth', block: 'center' }); } } catch (e) { toast(e.message, 'error'); }
            };
            $$('[data-party-end]', box).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/party/${encodeURIComponent(b.dataset.partyEnd)}`); toast('Party ended: the guest link no longer works'); loadPeople(); } catch (e) { toast(e.message, 'error'); } });
            const pzs = $('#pzsave', box); if (pzs) pzs.onclick = async () => { const zones = $$('#pzones input:checked', box).map((i) => i.value); try { await api('PUT', '/people/zones', { zones }); toast(zones.length ? `Listeners may play on ${countOf(zones.length, 'zones')}` : 'Listeners may play on every zone'); } catch (e) { toast(e.message, 'error'); } };
          };
          // Settings › Remote access: the bundled install's Cloudflare Tunnel
          const loadRemote = async (check = false) => {
            const box = $('#remotebox'); if (!box) return;
            if (check) { const b = $('#rtcheck', box); if (b) { b.disabled = true; b.innerHTML = '<span class="spin"></span> Checking…'; } }
            let t; try { t = await api('GET', `/remote/tunnel${check ? '?check=1' : ''}`); } catch (e) { box.innerHTML = `<p class="muted small">${esc(e.message)}</p>`; return; }
            if (!box.isConnected) return;
            const yes = (v, good, bad) => (v == null ? '' : v ? `<span class="pill ok">${good}</span>` : `<span class="pill warn">${bad}</span>`);
            box.innerHTML = group('Cloudflare Tunnel', (t.configured
                ? row('Homeserver', `<code>${esc(t.homeserver_domain || '')}</code>${t.published === false ? ' · not published yet' : t.published ? ' · published: Pubky apps find it at this address' : ''}`, yes(t.homeserver_reachable, 'Reachable', 'Not reachable'))
                  + (t.music_hostname ? row('This node', `<code>${esc(t.music_hostname)}</code>`, yes(t.music_reachable, 'Reachable', 'Not reachable')) : '')
                  + (t.restart_needed ? `<div class="notice warn" role="status">Restart the homeserver once so it publishes the address. ${t.restart_hint ? esc(t.restart_hint) : `On the machine, in <code>deploy/bundle</code>: <code>${esc(t.restart_command || '')}</code> <button class="btn sm ghost" data-copylink="${esc(t.restart_command || '')}">Copy</button>`}</div>` : '')
                  + row('Check from outside', 'Opens both addresses over HTTPS, as someone outside your network would.', '<button class="btn" id="rtcheck">Check now</button>')
                : row('Not set up', 'Reach this node and its homeserver from anywhere, with no open ports or static IP.', ''))
              + block(t.configured ? 'Change the tunnel' : 'Set up', 'In Cloudflare Zero Trust › Networks › Tunnels, create a tunnel and give it two public hostnames: one to <code>http://localhost:6286</code> (the homeserver), one to <code>http://localhost:7788</code> (this node). Paste the tunnel’s token and the hostnames here. With the compose bundle, start cloudflared once: <code>docker compose --profile tunnel up -d</code> (the bare-metal install starts it by itself).', `<form class="stack" id="rtform"><input class="input" id="rttoken" type="password" autocomplete="off" placeholder="${t.configured ? 'Tunnel token (saved; paste to replace)' : 'Tunnel token'}" aria-label="Cloudflare tunnel token"><input class="input" id="rths" placeholder="Homeserver hostname, e.g. pubky.example.org" aria-label="Homeserver hostname" value="${esc(t.homeserver_domain || '')}"><input class="input" id="rtmusic" placeholder="This node’s hostname (optional), e.g. music.example.org" aria-label="This node's hostname" value="${esc(t.music_hostname || '')}"><div class="actions"><button class="btn primary" id="rtsave" type="submit">Save tunnel</button>${t.configured ? '<button class="btn ghost" id="rtdel" type="button">Remove tunnel</button>' : ''}</div></form>`))
              + group('Other ways', row('Your own domain', 'A static IP, ports 80, 443 and 6287 forwarded, and Caddy in front: see “With its own Pubky homeserver” in the deploy guide (docs/deploy.md).', ''));
            const ck = $('#rtcheck', box); if (ck) ck.onclick = () => loadRemote(true);
            $('#rtform', box).onsubmit = async (e) => {
              e.preventDefault();
              const token = $('#rttoken', box).value.trim(); const homeserver_domain = $('#rths', box).value.trim(); const music_hostname = $('#rtmusic', box).value.trim() || undefined;
              if (!token) { toast('Paste the tunnel’s token', 'error'); return; }
              if (!homeserver_domain) { toast('Enter the homeserver’s hostname', 'error'); return; }
              try { await api('PUT', '/remote/tunnel', { token, homeserver_domain, music_hostname }); toast('Tunnel saved'); loadRemote(); } catch (err) { toast(err.message, 'error'); }
            };
            const del = $('#rtdel', box); if (del) del.onclick = async () => { try { await api('DELETE', '/remote/tunnel'); toast('Tunnel removed'); loadRemote(); } catch (e) { toast(e.message, 'error'); } };
          };
          // Pubky Ring: a QR and link, then wait for the approval (a sign-in, or creating the owner's
          // account on the bundled homeserver, which signs the node in the same way)
          const ringFlow = async (path, what) => {
            try {
              const r = await api('POST', path);
              $('#pkurl').innerHTML = `<div class="notice qrbox" style="margin-top:12px">${r.qr_svg ? `<div class="qr">${r.qr_svg}</div>` : ''}<div><p>Scan with Pubky Ring, or open the link on this device and approve${what}. This page updates by itself.</p><p class="muted small">Pubky Ring is the free app (iPhone and Android, from your app store) that holds your Pubky identity; this node never sees your keys.</p><a class="btn sm" href="${esc(ringUrl(r.url))}">Open in Pubky Ring</a> <button class="btn sm ghost" id="pkcopy">Copy link</button></div></div>`;
              $('#pkurl').scrollIntoView({ behavior: 'smooth', block: 'center' }); // the QR, not below the fold
              const pc = $('#pkcopy'); if (pc) pc.onclick = () => { navigator.clipboard && navigator.clipboard.writeText(r.url).then(() => toast('Link copied')).catch(() => toast(r.url)); };
              const t = setInterval(async () => { try { const st = await api('GET', '/pubky/login'); if (st.status === 'signed_in') { clearInterval(t); state.pubkyUser = undefined; toast('Signed in as ' + shortKey(st.user)); render(); } } catch (e) { clearInterval(t); toast(e.message, 'error'); } }, 2000);
            } catch (e) { toast(e.message, 'error'); }
          };
          const pl = $('#pklogin'); if (pl) pl.onclick = () => ringFlow('/pubky/login', '');
          const hc = $('#pkhscreate'); if (hc) hc.onclick = () => ringFlow('/homeserver/bundled/owner-signup', ': it creates your account on this node\u2019s homeserver');
          if (state.autoCreate && hc) { state.autoCreate = false; hc.click(); hc.scrollIntoView({ block: 'center' }); }
          loadPeople(); loadRemote();
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

      $$('[data-crumb-kind]', main).forEach((b) => b.onclick = () => (b.dataset.asBack ? $('.pagebar > [data-back]', main).click() : setView('item', { kind: b.dataset.crumbKind, id: b.dataset.crumbId })));
      bindTrackRows(main, lists);
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
  const nodeLabel = () => (state.info ? `${state.info.name} · ${countOf(state.info.library.tracks, 'tracks')}` : 'Connected');
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
    // Short retries while it matters: someone waits on a held change, or the node has only just gone
    // (a restart is back within seconds); the long backoff after that spares a node that is really off
    const urgent = link.waiters.length > 0 || (link.downSince && Date.now() - link.downSince < 20000);
    const d = Math.min(urgent ? 1500 : 30000, 500 * 2 ** Math.min(link.attempt, 10));
    const wait = Math.round(d / 2 + Math.random() * d / 2);
    link.attempt = urgent ? Math.min(link.attempt + 1, 2) : link.attempt + 1; // after the fast phase: 2 → 4 → 8 → 16 → 30 s
    link.retryAt = Date.now() + wait;
    const conn = $('#conn'); if (conn) conn.dataset.retryAt = String(link.retryAt); // when the next retry is due (for tests)
    link.retry = setTimeout(() => { link.retry = null; clearRetryAt(); reconnect(); }, wait);
  };
  const goneDown = () => {
    link.up = false; clearInterval(link.ping); link.awaiting = false;
    if (!link.downSince) link.downSince = Date.now();
    scheduleRetry(); showDown();
  };
  // Drop the socket without waiting for its close handshake (a dead peer may never answer).
  const dropSocket = () => { const ws = link.ws; link.ws = null; if (ws) { ws.onclose = ws.onmessage = ws.onerror = null; try { ws.close(); } catch {} } };
  const clearRetryAt = () => { const c = $('#conn'); if (c) delete c.dataset.retryAt; };
  const retryNow = () => { clearTimeout(link.retry); link.retry = null; clearRetryAt(); link.attempt = 0; reconnect(); };
  let reconnectGen = 0; // only the latest attempt may report its result
  const reconnect = async () => {
    if (!link.loaded) return connect();
    const gen = ++reconnectGen;
    let info = null; try { info = await api('GET', '/info'); } catch { if (gen === reconnectGen && !link.up) goneDown(); return; }
    if (link.up) return; // a success from an older attempt still counts
    state.info = info;
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
    setConn('ok', nodeLabel()); checkHealth(); if (isOwner()) findJob(); if (state.browserIndex < 0 && state.browserQueue.length) restoreBrowserQueue(); if (isOwner()) api('GET', '/node/settings').then((ns) => { state.devOutputs = !!ns.dev_outputs; }).catch(() => {});
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
  // A run's identified songs by method: " (9 by album, 3 by fingerprint)"
  const idHow = (by) => { const n = (k) => (by || {})[k] || 0; const parts = [[n('album_fit'), 'by album'], [n('fingerprint') + n('fingerprint+release'), 'by fingerprint']].filter(([x]) => x); return parts.length ? ` (${parts.map(([x, how]) => `${x} ${how}`).join(', ')})` : ''; };
  const renderIdentify = async () => {
    const box = $('#idbox'); if (!box) return;
    let st; try { st = await api('GET', '/library/identify'); } catch (e) { box.textContent = e.message; return; }
    const c = st.counts; const p = st.progress;
    const pct = c.total ? Math.round((c.identified / c.total) * 100) : 0;
    box.innerHTML = `<div class="idbar"><span style="width:${pct}%"></span></div>
      <p class="small">${c.identified} of ${c.total} identified${c.suggested ? ` · <a data-view="identify">${c.suggested} to confirm</a>` : ''}${c.unknown ? ` · ${c.unknown} not found` : ''}${c.unchecked ? ` · ${c.unchecked} not checked yet` : ''}</p>
      ${c.identified && c.by ? `<p class="small muted" id="idby">${[[c.by.tags, 'by their tags'], [c.by.album, 'by their folder as an album'], [c.by.fingerprint, 'by how they sound'], [c.by.you, 'by you']].filter(([n]) => n).map(([n, how]) => `${n} ${how}`).join(' · ')}</p>` : ''}
      ${p.running ? `<p class="small">Working: ${p.done} of ${p.total}…</p>` : (p.finished_at ? `<p class="small muted">Last run: ${p.identified} identified${idHow(p.identified_by)}, ${p.suggested} to confirm, ${p.unknown} not found${p.errors ? `, ${p.errors} errors` : ''}.</p>` : '')}
      ${p.acoustid_refused ? `<p class="small warn-text" role="status">AcoustID refused the key (${esc(p.acoustid_refused)}), so songs were matched by title and artist only. Enter a new key${st.acoustid_own ? ' or remove this one' : ''}.</p>` : ''}
      ${st.acoustid && !st.acoustid_own && !p.acoustid_refused ? '' : `<form class="set-group" id="acidform"><p class="small muted">${st.acoustid_own && !p.acoustid_refused ? 'Matching by how songs sound uses your AcoustID key.' : 'Songs are matched by their title and artist. Matching by how they sound (AcoustID) finds more. It needs a free key: sign in at <a href="https://acoustid.org/new-application" target="_blank" rel="noopener">acoustid.org/new-application</a>, register an application, and paste its key here.'}</p><div class="row" style="gap:8px"><input class="input" name="key" id="acidkey" placeholder="${st.acoustid_own ? 'A new key' : 'AcoustID application key'}" autocomplete="off" aria-label="AcoustID key"><button class="btn sm" id="acidsave">Save key</button>${st.acoustid_own ? '<button class="btn ghost sm" type="button" id="acidclear">Remove</button>' : ''}</div></form>`}
      <label class="row small" style="gap:8px; margin:8px 0"><input type="checkbox" id="idwrite" ${st.write_ids ? 'checked' : ''}> Write IDs into my files <span class="muted">(MusicBrainz recording and release IDs; each file's tags are backed up first; off by default)</span></label>
      <div class="actions"><button class="btn primary" id="idrun" ${p.running ? 'disabled' : ''}>${c.unchecked ? 'Identify' : 'Identify again'}</button>${c.suggested ? '<button class="btn" data-view="identify">Review matches</button>' : ''}${st.write_ids ? '<button class="btn" id="idwritenow" title="Write the IDs found so far into the files">Write now</button>' : ''}</div>`;
    $$('[data-view="identify"]', box).forEach((b) => b.onclick = () => setView('identify'));
    const acf = $('#acidform'); if (acf) {
      const saveKey = async (key) => { try { await api('PUT', '/library/identify/settings', { acoustid_key: key }); toast(key ? 'AcoustID key saved: the next Identify also listens to the songs' : 'AcoustID key removed'); renderIdentify(); } catch (e) { toast(e.message, 'error'); } };
      needsText($('#acidkey'), $('#acidsave'));
      acf.onsubmit = (e) => { e.preventDefault(); const k = $('#acidkey').value.trim(); if (k) saveKey(k); };
      const clr = $('#acidclear'); if (clr) clr.onclick = () => saveKey('');
    }
    const wr = $('#idwrite'); if (wr) wr.onchange = async () => { try { await api('PUT', '/library/identify/settings', { write_ids: wr.checked }); toast(wr.checked ? 'IDs will be written into your files' : 'Your files stay as they are'); renderIdentify(); } catch (e) { toast(e.message, 'error'); wr.checked = !wr.checked; } };
    const wn = $('#idwritenow'); if (wn) wn.onclick = async () => { wn.disabled = true; try { const r = await api('POST', '/library/identify/write'); toast(`${r.written} file${r.written === 1 ? '' : 's'} updated${r.skipped.length ? `, ${r.skipped.length} left as they were (${r.skipped[0].reason})` : ''}`); } catch (e) { toast(e.message, 'error'); } wn.disabled = false; };
    const run = $('#idrun'); if (run) run.onclick = async () => { try { await api('POST', '/library/identify', { retry: !c.unchecked }); renderIdentify(); } catch (e) { toast(e.message, 'error'); } };
    clearTimeout(idTimer);
    if (p.running) idTimer = setTimeout(renderIdentify, 1500);
  };
  const watchScan = (info) => {
    const was = state.wasScanning; state.wasScanning = !!(info && info.scanning);
    showScanning(!!(info && info.scanning));
    clearTimeout(scanTimer);
    // A scan that ends says so, with what it could not read (fresh-eyes review)
    if (was && info && !info.scanning && info.library) {
      const ls = info.last_scan; const n = info.library.tracks;
      toast(`Library up to date: ${n.toLocaleString()} ${n === 1 ? 'track' : 'tracks'}${ls && ls.error_count ? `. ${ls.error_count} ${ls.error_count === 1 ? 'file' : 'files'} could not be read (Your library › Node library)` : ''}`, ls && ls.error_count ? 'error' : '');
      state.albums = null;
      if (state.view === 'settings' || state.view === 'albums' || state.view === 'tracks') render();
    }
    if (info && info.scanning) scanTimer = setTimeout(async () => { try { const i = await api('GET', '/info'); state.info = i; setConn(true, `${i.name} · ${countOf(i.library.tracks, 'tracks')}`); watchScan(i); } catch { showScanning(false); } }, 2000);
  };
  const connect = async () => {
    try {
      state.info = await api('GET', '/info');
      document.body.classList.toggle('offline', isOffline());
      if (isOffline()) {
        // The app's local server: no zones, no events; the app switches back when the node is reachable
        setConn(true, `Offline · ${countOf(state.info.library.tracks, 'songs')} on this device`);
        try { state.me = await api('GET', '/me'); } catch { state.me = { role: 'owner' }; }
        state.sources = []; state.zones = {}; state.zone = BROWSER;
        loadFavourites().catch(() => { state.favUris = new Set(); });
        renderZoneName(); renderBar();
        link.loaded = true;
        if (!OFFLINE_VIEWS.has(state.view)) setView('downloads', undefined, 'replace'); else render();
        return;
      }
      setConn(true, `${state.info.name} · ${countOf(state.info.library.tracks, 'tracks')}`);
      watchScan(state.info);
      try { state.me = await api('GET', '/me'); } catch { state.me = { role: 'owner' }; }
      document.body.classList.toggle('listener', isListener());
      if (isOwner()) {
        wireLive();
        loadFavourites().catch(() => {});
        api('GET', '/sources').then((l) => { state.sources = l; applySourcesOff(); }).catch(() => {});
      } else { state.favUris = new Set(); state.sources = []; }
      const zones = await api('GET', '/zones');
      state.zones = Object.fromEntries(zones.map((z) => [z.id, z]));
      ensureZone(); renderZoneName(); renderBar(); refreshQueue(false); refreshAlong();
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
    clearTimeout(link.retry); link.retry = null; clearRetryAt();
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
      if (ev.type === 'zone_event' && ev.event && ev.event.type === 'error' && state.starting && ev.zone === state.starting.zone) startFail(ev.event.message, true);
      if (ev.type === 'zone') {
        const old = state.zones[ev.state.id]; state.zones[ev.state.id] = ev.state;
        if (state.starting && ev.state.id === state.starting.zone) {
          const fresh = state.starting.oldItem == null || (ev.state.now_playing && ev.state.now_playing.item.id !== state.starting.oldItem);
          if (state.starting.sent !== false && ((ev.state.transport === 'playing' && fresh) || (state.starting.what === 'move' && ev.state.transport !== 'stopped'))) startDone(true);
          // Only an error that is new fails the start: the engine keeps the last error until a track starts
          else if (ev.state.transport === 'stopped' && ev.state.error && ev.state.error !== (old && old.error)) startFail(ev.state.error);
        }
        // A start that failed says so until the zone starts playing (from stopped or paused) something
        // other than the song it was already playing: that song going on, or the next one of its queue, is not that start
        if (ev.state.transport === 'playing' && (!old || old.transport !== 'playing') && state.startError && state.startError.zone === ev.state.id && !(ev.state.now_playing && ev.state.now_playing.item.id === state.startError.still)) state.startError = null;
        if (!old) { renderZoneName(); refreshAlong(); }
        if (ev.state.id === state.zone && (!old || old.queue_version !== ev.state.queue_version)) refreshQueue();
        if (ev.state.id === state.zone && old && old.now_playing && ev.state.now_playing && old.now_playing.item.id !== ev.state.now_playing.item.id && !overlay.hidden) renderNowPlaying();
        if (state.view === 'zones' && (!old || old.transport !== ev.state.transport || old.error !== ev.state.error || (old.now_playing && old.now_playing.item.id) !== (ev.state.now_playing && ev.state.now_playing.item.id))) render();
      }
      // A zone that could not play (or seek) says why, once, for the zone this page plays to
      else if (ev.type === 'zone_event' && ev.event && ev.event.type === 'error' && ev.zone === state.zone) {
        const zn = state.zones[ev.zone]; toast(`${zn ? zn.name : 'This zone'}: ${ev.event.message || 'something went wrong'}`, 'error');
      }
      else if (ev.type === 'zone_removed') {
        // A group of yours went: rebuilt (follow it) or ended (back to your own zone). Ask first,
        // then fall back as for any zone that goes
        const yours = ev.zone === state.zone && (state.along || state.hosting || state.alongHome);
        delete state.zones[ev.zone];
        if (yours) refreshAlong().finally(() => { ensureZone(); renderZoneName(); renderBar(); refreshQueue(); });
        else { ensureZone(); renderZoneName(); refreshAlong(); }
      }
      else if (ev.type === 'listening_stopped') { if (state.liveShow) state.liveShow({ share: false }); toast(`Live is off: your homeserver refused the post (${ev.error})`, 'error'); }
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
