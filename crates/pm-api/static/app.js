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
    browserQueue: [],
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
  const toast = (msg, kind = '') => {
    const t = $('#toast');
    t.textContent = msg;
    t.className = `toast show ${kind}`;
    clearTimeout(toast.h);
    toast.h = setTimeout(() => t.classList.remove('show'), 3800);
  };
  const setFill = (el) => { const min = +el.min || 0; const max = +el.max || 1; el.style.setProperty('--fill', `${((el.value - min) / (max - min)) * 100}%`); };

  const api = async (method, path, body) => {
    const r = await fetch('/v1' + path, {
      method,
      headers: { 'content-type': 'application/json', ...(state.token ? { authorization: 'Bearer ' + state.token } : {}) },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (r.status === 401) { login(); throw new Error('The node wants an API token.'); }
    const text = await r.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { json = { error: text }; }
    if (!r.ok) throw new Error(json && json.error ? json.error : `HTTP ${r.status}`);
    return json;
  };

  const login = () => { const d = $('#login'); if (!d.open) { $('#tok').value = state.token; d.showModal(); } };
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
      now_playing: it ? { item: it, position_ms: audio.currentTime * 1000, duration_ms: (audio.duration || 0) * 1000, signal_path: null } : null,
    };
  };
  const currentZone = () => (isBrowser() ? browserState() : state.zones[state.zone]);
  const zoneList = () => [browserState(), ...Object.values(state.zones)];
  const ensureZone = () => { if (!isBrowser() && !state.zones[state.zone]) state.zone = BROWSER; };
  const renderZoneName = () => { const z = currentZone(); $('#zonename').textContent = z ? z.name : 'Zone'; };
  const browserPlay = (i) => {
    const it = state.browserQueue[i];
    if (!it) { audio.pause(); state.browserIndex = -1; renderBar(); return; }
    state.browserIndex = i;
    audio.src = it.stream ? it.stream : `/v1/library/tracks/${it.track_id}/file${tokenQ()}`;
    audio.play().catch((e) => toast('Browser playback: ' + e.message, 'error'));
    if (it.track_id != null) api('POST', '/library/history', { track_id: it.track_id, zone: 'browser' }).catch(() => {});
    renderBar();
    if (state.view === 'queue') render();
  };
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
      state.browserQueue = [{ id: Date.now(), uri: st.url, stream: st.url, title: st.name, artist: [st.country, (st.tags || []).slice(0, 3).join(', ')].filter(Boolean).join(' · '), album: 'Internet radio', duration_ms: 0 }];
      browserPlay(0); toast(`Tuning to ${st.name}`); return;
    }
    try { await api('POST', '/radio/play', { zone: state.zone, station: st }); toast(`Tuning to ${st.name}`); refreshQueue(); } catch (e) { toast(e.message, 'error'); }
  };
  const playPlaylist = async (id) => {
    try {
      if (isBrowser()) { const d = await api('GET', `/playlists/${id}`); playTracks(d.tracks.filter(Boolean), true); return; }
      const r = await api('POST', `/playlists/${id}/play`, { zone: state.zone });
      toast(`Queued ${r.queued} tracks${r.missing.length ? `, ${r.missing.length} not in the library` : ''}`);
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
        <span class="state">${z.transport === 'playing' ? '▶ playing' : ''}</span>
      </button>`).join('');
    $$('[data-zone]', zonePop).forEach((b) => b.onclick = () => {
      state.zone = b.dataset.zone; localStorage.setItem('pm.zone', state.zone);
      closePop(); renderZoneName(); renderBar(); refreshQueue();
    });
    zonePop.hidden = false; $('#scrim').hidden = false;
  };
  $('#scrim').onclick = () => { closePop(); $('#sidebar').classList.remove('open'); };
  $('#menu').onclick = () => { $('#sidebar').classList.toggle('open'); $('#scrim').hidden = !$('#sidebar').classList.contains('open'); };

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
    $('#np').innerHTML = `<div class="np-art">${art(albumIdOf(it), it.title || it.uri, 'sm')}</div><div class="np-text"><div class="np-title">${esc(live || it.title || it.uri)}</div><div class="np-sub">${esc(live ? [it.title, it.album].filter(Boolean).join(' · ') : [it.artist, it.album].filter(Boolean).join(' · '))}</div></div>`;
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
        <div class="np-top"><button class="btn ghost" id="npclose">${icon('back')} Back</button><span class="muted">Now playing on ${esc(z.name)}</span><span class="spacer"></span><button class="btn ghost" id="npq">${icon('queue')} Queue</button></div>
        <div class="np-hero">
          <div class="np-big">${art(albumId, it.title || it.uri)}</div>
          <div class="np-meta">
            <div class="eyebrow">${esc(np.stream_title ? 'Live · ' + (it.title || '') : it.album ? 'From ' + it.album : 'Track')}</div>
            <h1>${esc(np.stream_title || it.title || it.uri)}</h1>
            <div class="artist">${esc(np.stream_title ? (it.artist || '') : (it.artist || ''))}</div>
            <div class="np-progress"><input type="range" id="npseek" min="0" max="1000" value="${np.duration_ms ? Math.round((np.position_ms / np.duration_ms) * 1000) : 0}" aria-label="Seek"><div class="muted small" style="display:flex;justify-content:space-between"><span id="nppos">${fmt(np.position_ms)}</span><span>${fmt(np.duration_ms)}</span></div></div>
            <div class="path">
              <div class="path-head"><span class="qdot" style="width:10px;height:10px;border-radius:50%;background:${h ? colour[h] : 'var(--muted)'}"></span>${h ? LABEL[h] : 'Browser decode'}<span class="muted" style="font-weight:400;margin-left:auto">${np.signal_path ? esc(fmtFormat(np.source_format) + ' → ' + fmtFormat(np.output_format)) : ''}</span></div>
              ${np.signal_path ? np.signal_path.stages.map((s) => `<div class="path-row"><span class="pd" style="background:${colour[s.quality]}"></span><span class="pn">${esc(s.name)}</span><span class="pf">${esc(fmtFormat(s.in_format))} → ${esc(fmtFormat(s.out_format))}</span>${s.note ? `<span class="pnote">${esc(s.note)}</span>` : ''}</div>`).join('') : `<div class="path-row"><span class="pd" style="background:var(--muted)"></span><span class="pn">browser</span><span class="pf">the original file, decoded by this browser</span></div>`}
            </div>
            ${next.length ? `<div class="upnext"><h3>Up next</h3><div class="list">${next.map((n) => `<div class="list-row"><div class="grow"><div class="name">${esc(n.title || n.uri)}</div><div class="sub">${esc(n.artist || '')}</div></div><span class="muted small">${fmt(n.duration_ms)}</span></div>`).join('')}</div></div>` : ''}
          </div>
        </div>
      </div>`;
    const s = $('#npseek'); setFill(s);
    s.addEventListener('input', () => { s.dataset.dragging = '1'; setFill(s); $('#nppos').textContent = fmt((s.value / 1000) * (np.duration_ms || 0)); });
    s.addEventListener('change', () => { delete s.dataset.dragging; transport({ action: 'seek', position_ms: Math.round((s.value / 1000) * (np.duration_ms || 0)) }); });
    $('#npclose').onclick = () => toggleNowPlaying();
    $('#npq').onclick = () => { toggleNowPlaying(); setView('queue'); };
  };
  // Keep the overlay's progress moving without rebuilding it on every tick.
  setInterval(() => {
    if (overlay.hidden) return;
    const z = currentZone(); const np = z && z.now_playing; const s = $('#npseek');
    if (!np || !s || s.dataset.dragging) return;
    if (np.item.id !== (renderNowPlaying.lastItem || {}).id || np.stream_title !== renderNowPlaying.lastTitle) { renderNowPlaying.lastTitle = np.stream_title; renderNowPlaying(); }
    s.value = np.duration_ms ? Math.round((np.position_ms / np.duration_ms) * 1000) : 0; setFill(s);
    const p = $('#nppos'); if (p) p.textContent = fmt(np.position_ms);
    renderNowPlaying.lastItem = np.item;
  }, 500);

  // ---- views ---------------------------------------------------------------
  const main = $('#main');
  const setView = (v, arg) => {
    state.view = v; state.viewArg = arg;
    $$('.nav-item').forEach((b) => b.classList.toggle('on', b.dataset.view === v || (v === 'album' && b.dataset.view === 'albums') || (v === 'artist' && b.dataset.view === 'artists') || (v === 'genre' && b.dataset.view === 'genres') || (v === 'playlist' && b.dataset.view === 'playlists')));
    $('#sidebar').classList.remove('open'); $('#scrim').hidden = true; closePop();
    main.scrollTop = 0;
    render();
  };
  $$('.nav-item').forEach((b) => b.onclick = () => setView(b.dataset.view));

  const search = $('#search');
  let searchTimer = null;
  const runSearch = async () => {
    const q = search.value.trim();
    if (!q) { if (state.view === 'search') setView('home'); return; }
    try {
      const r = await api('GET', '/library/search?q=' + encodeURIComponent(q) + '&limit=200');
      if (search.value.trim() !== q) return;
      setView('search', { q, tracks: r.tracks, albums: r.albums || [], artists: r.artists || [], genres: r.genres || [] });
    } catch (e) { toast(e.message, 'error'); }
  };
  search.addEventListener('input', () => { clearTimeout(searchTimer); searchTimer = setTimeout(runSearch, 280); });
  search.addEventListener('keydown', (e) => { if (e.key === 'Enter') { clearTimeout(searchTimer); runSearch(); } if (e.key === 'Escape') { search.value = ''; search.blur(); if (state.view === 'search') setView('home'); } });

  document.addEventListener('keydown', (e) => {
    const typing = ['INPUT', 'SELECT', 'TEXTAREA'].includes(e.target.tagName) || e.target.isContentEditable;
    if (e.key === 'Escape') { if (!overlay.hidden) toggleNowPlaying(); closePop(); return; }
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
  const trackHasId = (t) => !!canonicalUrl(Array.isArray(t.canonical) ? t.canonical[0] : t.canonical);
  const trackRows = (tracks, { showAlbum = true, numbers = 'index', key = 'tr' } = {}) => {
    rememberTracks(tracks);
    const z = currentZone(); const np = z && z.now_playing;
    return `<div class="tracks" data-list="${key}">${tracks.map((t, i) => {
      const cur = np && np.item.uri === 'track:' + t.id;
      const fav = isFav(t); const can = trackHasId(t);
      return `<div class="track ${showAlbum ? '' : 'compact'} ${cur ? 'cur' : ''}" data-i="${i}">
        <div class="no"><span class="num">${numbers === 'track' ? (t.track_no ?? '') : i + 1}</span><button class="rowplay" data-act="play" title="Play from here">${icon('play')}</button></div>
        <div class="main-col"><div class="title">${esc(t.title)}</div><div class="by">${esc(artists(t))}</div></div>
        ${showAlbum ? `<div class="album-col">${t.album ? `<button data-act="album" data-album="${t.album.id}">${esc(t.album.title)}</button>` : ''}</div>` : ''}
        <div><span class="fmt">${esc(fmtFormat(t.file && t.file.format))}</span></div>
        <div class="dur">${fmt(t.duration_ms)}</div>
        <div class="row-actions">
          <button class="icon-btn heart ${fav ? 'on' : ''}" data-act="fav" ${can ? '' : 'disabled'} title="${can ? (fav ? 'Remove from favourites' : 'Add to favourites') : 'No MusicBrainz id or ISRC yet'}">${icon(fav ? 'heart-fill' : 'heart')}</button>
          <button class="icon-btn" data-act="next" title="Play next">${icon('playnext')}</button>
          <button class="icon-btn" data-act="add" title="Add to queue">${icon('plus')}</button>
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
      if (i < 0) return;
      if (act === 'play') playTracks(tracks.slice(i), true);
      else if (act === 'next') playTracks([tracks[i]], false, 'next');
      else if (act === 'add') playTracks([tracks[i]], false, 'end');
      else if (act === 'fav') {
        const on = btn.classList.contains('on');
        try { await setFavourite({ track_id: tracks[i].id }, !on); btn.classList.toggle('on', !on); btn.innerHTML = icon(!on ? 'heart-fill' : 'heart'); btn.title = !on ? 'Remove from favourites' : 'Add to favourites'; }
        catch (er) { toast(er.message, 'error'); }
      }
    }, { once: false });
  };
  const tile = (a) => `<button class="tile" data-album="${a.id}" title="${esc(a.title)}">${art(a.id, a.title)}<span class="tile-play" data-tileplay="${a.id}" role="button" title="Play album">${icon('play')}</span><div class="t">${esc(a.title)}</div><div class="a">${esc(a.album_artist || '')}${a.year ? ` · ${a.year}` : ''}</div></button>`;
  const tagRow = (tags, ref) => `<div class="tagrow" data-tagref='${esc(JSON.stringify(ref))}'><span class="eyebrow">Tags</span>${tags.map((t) => `<span class="chip tag"><button class="chiplink" data-tag-open="${esc(t.label)}" title="Open in Discover">#${esc(t.label)}</button><button class="chipx" data-tag-del="${esc(t.label)}" title="${t.synced ? 'Public on your homeserver · remove' : 'Not pushed yet · remove'}">×</button></span>`).join('')}<input class="taginput" id="tagadd" placeholder="Add a tag…" maxlength="20"><span class="muted small">Public pubky.app tags on the MusicBrainz URL; Discover reads them back via Pinakes.</span></div>`;
  const bindTagRow = (ref) => {
    const add = $('#tagadd'); if (!add) return;
    add.onkeydown = async (e) => { if (e.key !== 'Enter' || !add.value.trim()) return; try { await api('PUT', '/tags', { ...ref, label: add.value.trim() }); toast(`Tagged #${add.value.trim().toLowerCase()}`); render(); } catch (er) { toast(er.message, 'error'); } };
    $$('[data-tag-del]', main).forEach((b) => b.onclick = async () => { try { await api('DELETE', '/tags?' + new URLSearchParams({ ...Object.fromEntries(Object.entries(ref).map(([k, v]) => [k, String(v)])), label: b.dataset.tagDel }).toString()); render(); } catch (er) { toast(er.message, 'error'); } });
    $$('[data-tag-open]', main).forEach((b) => b.onclick = () => setView('discover', { tag: b.dataset.tagOpen }));
  };
  const reviewRow = (r) => `<div class="reviewrow"><span class="eyebrow">My rating</span><span class="stars" id="stars">${[1, 2, 3, 4, 5].map((n) => `<button class="star ${r && r.rating >= n ? 'on' : ''}" data-star="${n}" aria-label="${n} star${n === 1 ? '' : 's'}">${r && r.rating >= n ? '★' : '☆'}</button>`).join('')}</span><button class="btn sm ghost" id="notesbtn">${r && r.text ? 'Edit notes' : 'Add notes'}</button>${r && r.text ? `<div class="notes" id="notes">${esc(r.text)}</div>` : ''}<div class="notes-edit" id="notesedit" hidden><textarea class="input" id="notestext" rows="3" placeholder="Notes, kept privately on your homeserver">${esc(r ? r.text : '')}</textarea><div class="actions" style="margin-top:8px"><button class="btn primary sm" id="notessave">Save</button><button class="btn sm ghost" id="notescancel">Cancel</button></div></div></div>`;
  const bindReviewRow = (ref, r) => {
    const stars = $('#stars'); if (!stars) return;
    const save = async (rating, text) => { try { if (rating == null && !text) await api('DELETE', '/reviews?' + new URLSearchParams(Object.entries(ref).map(([k, v]) => [k, String(v)])).toString()); else await api('PUT', '/reviews', { ...ref, rating, text }); toast('Review saved'); render(); } catch (e) { toast(e.message, 'error'); } };
    $$('[data-star]', stars).forEach((b) => b.onclick = () => { const n = Number(b.dataset.star); const cur = r ? r.rating : null; save(cur === n ? null : n, r ? r.text : ''); });
    $('#notesbtn').onclick = () => { $('#notesedit').hidden = false; $('#notesbtn').hidden = true; if ($('#notes')) $('#notes').hidden = true; $('#notestext').focus(); };
    $('#notescancel').onclick = () => { $('#notesedit').hidden = true; $('#notesbtn').hidden = false; if ($('#notes')) $('#notes').hidden = false; };
    $('#notessave').onclick = () => save(r ? r.rating : null, $('#notestext').value.trim());
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
  const emptyState = (iconName, title, text, action) => `<div class="empty">${icon(iconName)}<h3>${esc(title)}</h3><p>${esc(text)}</p>${action ? `<button class="btn primary" data-empty-action="${action.view}">${esc(action.label)}</button>` : ''}</div>`;
  const pageHead = (title, count, actions = '') => `<div class="page-head"><h2>${esc(title)}${count != null ? `<span class="count">${esc(count)}</span>` : ''}</h2><div class="actions">${actions}</div></div>`;
  const backBtn = (view, label) => `<button class="back" data-back="${view}">${icon('back')} ${esc(label)}</button>`;
  const loading = () => { main.innerHTML = `<div class="skeleton" style="height:28px;width:220px"></div><div class="sk-grid">${'<div class="skeleton"></div>'.repeat(8)}</div>`; };

  const loadFavourites = async () => { const f = await api('GET', '/favourites'); state.favUris = new Set(f.uris || []); return f; };
  const setFavourite = async (ref, on) => {
    if (on) await api('PUT', '/favourites', ref);
    else await api('DELETE', '/favourites?' + new URLSearchParams(Object.entries(ref).map(([k, v]) => [k, String(v)])).toString());
    await loadFavourites();
    toast(on ? 'Added to favourites' : 'Removed from favourites');
  };
  const albumsAll = async () => { if (!state.albums) state.albums = await api('GET', '/library/albums?limit=1000'); return state.albums; };

  // ---- render --------------------------------------------------------------
  const render = async () => {
    const v = state.view;
    const lists = {};
    try {
      switch (v) {
        case 'home': {
          loading();
          const h = await api('GET', '/library/home?limit=12');
          if (state.view !== 'home') return;
          const most = h.most_played.map((p) => p.track);
          if (!h.stats.tracks) {
            main.innerHTML = pageHead('Home') + emptyState('disc', 'Your library is empty', 'Point the node at a music folder and start a scan; albums appear here as they are found.', { view: 'settings', label: 'Open settings' });
            break;
          }
          main.innerHTML = pageHead('Home', `${h.stats.albums} albums · ${h.stats.tracks} tracks · ${fmtLong(h.stats.total_duration_ms)}`)
            + strip('Favourites', h.favourite_albums, 'favourites')
            + strip('Recently added', h.recently_added, 'albums')
            + strip('Recently played', h.recently_played)
            + strip('Most played albums', h.most_played_albums)
            + (most.length ? `<section class="section"><div class="section-head"><h3>Most played tracks</h3></div>${trackRows(most, { key: 'most' })}</section>` : '')
            + `<section class="section"><div class="section-head"><h3>History</h3></div>${h.history.length ? `<div class="list">${h.history.map((p) => `<div class="list-row"><div class="avatar" style="background:hsl(${hue(p.track.title)} 35% 28%)">${esc(initials(p.track.title))}</div><div class="grow"><div class="name">${esc(p.track.title)}</div><div class="sub">${esc(artists(p.track))}</div></div><span class="muted small">${esc((state.zones[p.zone] || {}).name || (p.zone === 'browser' ? 'This browser' : p.zone) || '')} · ${ago(p.played_at)}</span></div>`).join('')}</div>` : `<p class="muted">Nothing played yet. Plays on every zone show up here.</p>`}</section>`;
          lists.most = most;
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
            + (f.other.length ? `<section class="section"><div class="section-head"><h3>Not in this library</h3></div><div class="list">${f.other.map((u) => `<div class="list-row"><div class="grow"><div class="name muted">${esc(u.replace(/^https?:\/\//, ''))}</div></div><a class="btn sm ghost" href="${esc(u)}" target="_blank" rel="noopener">${icon('external')} Open</a></div>`).join('')}</div></section>` : '');
          lists.fav = f.tracks;
          const fp = $('#favplay'); if (fp) fp.onclick = () => playTracks(f.tracks, true);
          break;
        }
        case 'albums': {
          loading();
          const albums = await albumsAll();
          if (state.view !== 'albums') return;
          main.innerHTML = pageHead('Albums', albums.length) + (albums.length ? albumGrid(albums) : emptyState('disc', 'No albums yet', 'Scan a folder from Settings to fill the library.', { view: 'settings', label: 'Open settings' }));
          break;
        }
        case 'album': {
          const id = state.viewArg;
          const [albums, tr] = await Promise.all([albumsAll(), api('GET', `/library/albums/${id}/tracks`)]);
          if (state.view !== 'album') return;
          const a = albums.find((x) => x.id === id) || (tr.tracks[0] && tr.tracks[0].album) || { id, title: 'Album' };
          const myTags = a.release_mbid ? (await api('GET', `/tags?album_id=${id}`).catch(() => ({ tags: [] }))).tags : [];
          const myReview = a.release_mbid ? ((await api('GET', `/reviews?album_id=${id}`).catch(() => ({ reviews: [] }))).reviews[0] || null) : null;
          const onPubky = a.release_mbid && !state.discoveryOff ? await api('GET', `/discover/uri?album_id=${id}`).catch((e) => { if (String(e.message).includes('discovery is off')) state.discoveryOff = true; return null; }) : null;
          const tracks = tr.tracks;
          const total = tracks.reduce((s, t) => s + (t.duration_ms || 0), 0);
          const relUrl = a.release_mbid ? 'https://musicbrainz.org/release/' + a.release_mbid : null;
          const fav = relUrl && state.favUris.has(relUrl);
          main.innerHTML = backBtn('albums', 'Albums') + `
            <div class="hero">${art(id, a.title)}<div>
              <div class="eyebrow">Album</div><h1>${esc(a.title)}</h1>
              <div class="sub">${esc(a.album_artist || artists(tracks[0] || {}))}</div>
              <div class="facts">${[a.year, `${tracks.length} tracks`, fmtLong(total), tracks[0] ? fmtFormat(tracks[0].file.format) : ''].filter(Boolean).join(' · ')}</div>
              <div class="actions"><button class="btn primary" id="playall">${icon('play')} Play</button><button class="btn" id="nextall">${icon('playnext')} Play next</button><button class="btn" id="addall">${icon('plus')} Add to queue</button><button class="btn heart ${fav ? 'on' : ''}" id="heart" ${relUrl ? '' : 'disabled'} title="${relUrl ? '' : 'No MusicBrainz release id yet'}">${icon(fav ? 'heart-fill' : 'heart')} ${fav ? 'Favourited' : 'Favourite'}</button></div>
              ${relUrl ? tagRow(myTags, { album_id: id }) : ''}
              ${relUrl ? reviewRow(myReview) : ''}
              ${onPubky ? `<div class="tagrow"><span class="eyebrow">On Pubky</span>${onPubky.indexed ? (onPubky.taggers ? `<span class="muted small">${onPubky.taggers} ${onPubky.taggers === 1 ? 'person' : 'people'}</span>` : '') + (onPubky.tags.length ? onPubky.tags.slice(0, 12).map(([l, n]) => `<button class="chip" data-tag-open="${esc(l)}">#${esc(l)} <span class="muted">${n}</span></button>`).join('') : '<span class="muted small">no tags yet</span>') : '<span class="muted small">not indexed yet</span>'}</div>` : ''}
            </div></div>` + trackRows(tracks, { showAlbum: false, numbers: 'track', key: 'album' });
          bindTagRow({ album_id: id });
          bindReviewRow({ album_id: id }, myReview);
          lists.album = tracks;
          $('#playall').onclick = () => playTracks(tracks, true);
          $('#nextall').onclick = () => playTracks(tracks, false, 'next');
          $('#addall').onclick = () => playTracks(tracks, false, 'end');
          $('#heart').onclick = async () => { try { await setFavourite({ album_id: id }, !fav); render(); } catch (er) { toast(er.message, 'error'); } };
          break;
        }
        case 'artists': {
          loading();
          state.artists = await api('GET', '/library/artists');
          if (state.view !== 'artists') return;
          main.innerHTML = pageHead('Artists', state.artists.length) + (state.artists.length ? `<div class="list">${state.artists.map((a) => `<div class="list-row clickable" data-artist="${a.id}"><div class="avatar" style="background:hsl(${hue(a.name)} 35% 28%)">${esc(initials(a.name))}</div><div class="grow"><div class="name">${esc(a.name)}</div><div class="sub">${a.albums} album${a.albums === 1 ? '' : 's'} · ${a.tracks} track${a.tracks === 1 ? '' : 's'}</div></div>${icon('chevron', 'ic chev')}</div>`).join('')}</div>` : emptyState('artist', 'No artists yet', 'Artists come from the tags of scanned files.'));
          break;
        }
        case 'artist': {
          const d = await api('GET', `/library/artists/${state.viewArg}`);
          if (state.view !== 'artist') return;
          const mbUrl = d.artist.mbid ? 'https://musicbrainz.org/artist/' + d.artist.mbid : null;
          const fav = mbUrl && state.favUris.has(mbUrl);
          const myTags = mbUrl ? (await api('GET', `/tags?artist_id=${d.artist.id}`).catch(() => ({ tags: [] }))).tags : [];
          const info = await api('GET', `/library/artists/${d.artist.id}/info`).catch(() => ({}));
          const links = [['Wikipedia', info.wikipedia], ['MusicBrainz', info.musicbrainz], ['Homepage', info.homepage]].filter(([, u]) => u);
          main.innerHTML = backBtn('artists', 'Artists') + `<div class="artist-hero"><div class="portrait" style="background:hsl(${hue(d.artist.name)} 35% 28%)">${info.image ? `<img src="/v1/library/artists/${d.artist.id}/image${tokenQ()}" alt="" onerror="this.remove()">` : esc(initials(d.artist.name))}</div><div class="grow"><div class="page-head"><div><div class="eyebrow">Artist</div><h1>${esc(d.artist.name)}</h1><div class="muted">${d.artist.albums} albums · ${d.artist.tracks} tracks${links.length ? ' · ' + links.map(([l, u]) => `<a href="${esc(u)}" target="_blank" rel="noopener">${l}</a>`).join(' · ') : ''}</div>${mbUrl ? tagRow(myTags, { artist_id: d.artist.id }) : ''}</div><div class="actions"><button class="btn primary" id="artplay">${icon('play')} Play all</button><button class="btn heart ${fav ? 'on' : ''}" id="heart" ${mbUrl ? '' : 'disabled'} title="${mbUrl ? '' : 'No MusicBrainz artist id yet'}">${icon(fav ? 'heart-fill' : 'heart')} Favourite</button></div></div>${info.bio ? `<p class="bio">${esc(info.bio)}</p>` : info.note ? `<p class="muted small">${esc(info.note)}</p>` : ''}</div></div>` + albumGrid(d.albums);
          bindTagRow({ artist_id: d.artist.id });
          $('#artplay').onclick = async () => { try { const r = await api('GET', `/library/artists/${state.viewArg}/tracks`); playTracks(r.tracks, true); } catch (er) { toast(er.message, 'error'); } };
          $('#heart').onclick = async () => { try { await setFavourite({ artist_id: d.artist.id }, !fav); render(); } catch (er) { toast(er.message, 'error'); } };
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
          const tracks = (await api('GET', '/library/tracks?limit=1000')).tracks;
          if (state.view !== 'tracks') return;
          main.innerHTML = pageHead('Tracks', tracks.length, tracks.length ? `<button class="btn" id="shuffle">Play all</button>` : '') + (tracks.length ? trackRows(tracks, { key: 'all' }) : emptyState('notes', 'No tracks yet', 'Scan a folder from Settings to fill the library.', { view: 'settings', label: 'Open settings' }));
          lists.all = tracks;
          const sh = $('#shuffle'); if (sh) sh.onclick = () => playTracks(tracks, true);
          break;
        }
        case 'search': {
          const { q, tracks, albums, artists, genres } = { tracks: [], albums: [], artists: [], genres: [], ...(state.viewArg || { q: '' }) };
          const none = !tracks.length && !albums.length && !artists.length && !genres.length;
          main.innerHTML = pageHead(`Results for “${q}”`, none ? null : `${tracks.length} tracks · ${albums.length} albums · ${artists.length} artists`)
            + (none ? emptyState('search', 'Nothing matched', 'Search looks at titles, artists, albums and genres in your library.') : '')
            + (albums.length ? `<section class="section"><div class="section-head"><h3>Albums</h3></div><div class="strip">${albums.map(tile).join('')}</div></section>` : '')
            + (artists.length || genres.length ? `<section class="section"><div class="section-head"><h3>${artists.length ? 'Artists' : ''}${artists.length && genres.length ? ' & ' : ''}${genres.length ? 'Genres' : ''}</h3></div><div class="chips">${artists.map((a) => `<button class="chip" data-artist="${a.id}">${icon('artist')} ${esc(a.name)}</button>`).join('')}${genres.map((g) => `<button class="chip" data-genre="${g.id}">#${esc(g.name)}</button>`).join('')}</div></section>` : '')
            + (tracks.length ? `<section class="section"><div class="section-head"><h3>Tracks</h3></div>${trackRows(tracks, { key: 'search' })}</section>` : '');
          lists.search = tracks;
          $$('[data-genre]', main).forEach((b) => b.onclick = () => setView('genre', Number(b.dataset.genre)));
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
          const d = await api('GET', `/playlists/${state.viewArg}`);
          if (state.view !== 'playlist') return;
          const present = d.tracks.filter(Boolean);
          const total = present.reduce((s, t) => s + (t.duration_ms || 0), 0);
          main.innerHTML = backBtn('playlists', 'Playlists') + `
            <div class="page-head"><div><div class="eyebrow">Playlist · ${esc(d.summary.visibility)} · ${d.summary.synced ? 'synced to homeserver' : 'local only'}</div><h1>${esc(d.summary.name)}</h1><div class="muted">${d.playlist.items.length} items · ${present.length} in this library · ${fmtLong(total)}</div></div>
            <div class="actions"><button class="btn primary" id="plplay">${icon('play')} Play</button><button class="btn danger" id="pldel">${icon('trash')} Delete</button></div></div>`
            + (d.playlist.items.length ? `<div class="list">${d.playlist.items.map((it, i) => { const t = d.tracks[i]; return `<div class="list-row"><span class="muted small" style="width:24px;text-align:right">${i + 1}</span>${t ? `<div class="grow"><div class="name">${esc(t.title)}</div><div class="sub">${esc(artists(t))}${t.album ? ' · ' + esc(t.album.title) : ''}</div></div><span class="fmt">${esc(fmtFormat(t.file.format))}</span><span class="muted small">${fmt(t.duration_ms)}</span>` : `<div class="grow"><div class="name muted">${esc(it.uri.replace(/^https?:\/\//, ''))}</div><div class="sub">not in this library</div></div>`}<button class="icon-btn" data-rm="${i}" title="Remove">${icon('close')}</button></div>`; }).join('')}</div>` : emptyState('playlist', 'This playlist is empty', 'Add tracks from any list with the queue menu, or import a file.'));
          $('#plplay').onclick = () => playPlaylist(d.summary.id);
          $('#pldel').onclick = async () => { if (!$('#pldel').dataset.armed) { $('#pldel').dataset.armed = '1'; $('#pldel').innerHTML = `${icon('trash')} Really delete?`; setTimeout(() => { if ($('#pldel')) { delete $('#pldel').dataset.armed; $('#pldel').innerHTML = `${icon('trash')} Delete`; } }, 3000); return; } try { await api('DELETE', `/playlists/${d.summary.id}`); toast('Playlist deleted'); setView('playlists'); } catch (e) { toast(e.message, 'error'); } };
          $$('[data-rm]', main).forEach((b) => b.onclick = async () => { try { await api('DELETE', `/playlists/${d.summary.id}/items/${b.dataset.rm}`); render(); } catch (e) { toast(e.message, 'error'); } });
          break;
        }
        case 'radio': {
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
        case 'discover': {
          loading();
          const tag = state.viewArg && state.viewArg.tag;
          let page;
          try { page = await api('GET', tag ? `/discover/tag/${encodeURIComponent(tag)}?limit=40` : '/discover/trends?window=7d&limit=40'); }
          catch (e) { main.innerHTML = pageHead('Discover') + `<div class="notice warn">${esc(e.message)}</div><p class="muted">Discover reads trending music from Pinakes. Set <code>contact</code> in the node's config so the request carries a contact address.</p>`; break; }
          if (state.view !== 'discover') return;
          main.innerHTML = (tag ? backBtn('discover', 'Trending') : '') + pageHead(tag ? `#${tag}` : 'Discover', tag ? 'tagged on Pubky, via Pinakes' : 'trending this week via Pinakes')
            + (page.facets.length ? `<div class="chips">${page.facets.slice(0, 30).map(([l, c]) => `<span class="chip">#${esc(l)} <span class="muted">${c}</span></span>`).join('')}</div>` : '')
            + (page.items.length ? `<div class="list">${page.items.map((it, i) => `<div class="list-row"><div class="avatar" style="background:hsl(${hue(it.title || it.uri)} 35% 28%)">${esc(initials(it.title || 'M'))}</div><div class="grow"><div class="name">${esc(it.title || (it.in_library && it.in_library.title) || it.uri)}</div><div class="sub">${esc(it.artist || '')}${it.taggers != null ? ` · ${it.taggers} taggers` : ''}</div></div>${it.in_library ? `<button class="btn sm" data-dplay="${i}">${icon('play')} In library</button>` : '<span class="pill">not in library</span>'}<a class="icon-btn" href="${esc(it.uri)}" target="_blank" rel="noopener" title="Open">${icon('external')}</a></div>`).join('')}</div>` : emptyState('compass', 'Nothing trending yet', 'Pinakes has not indexed music tags for this window.'));
          $$('[data-dplay]', main).forEach((b) => b.onclick = () => playTracks([page.items[Number(b.dataset.dplay)].in_library], true));
          break;
        }
        case 'queue': {
          await refreshQueue(false);
          if (state.view !== 'queue') return;
          const z = currentZone(); const q = isBrowser() ? state.browserQueue : state.queue;
          main.innerHTML = pageHead('Queue', z ? z.name : '', q.length && !isBrowser() ? `<button class="btn" id="qclear">${icon('trash')} Clear</button>` : '')
            + (q.length ? `<div class="list">${q.map((it, i) => `<div class="list-row clickable ${z && z.queue_index === i ? 'cur' : ''}" data-qi="${i}"><span class="muted small" style="width:24px;text-align:right">${z && z.queue_index === i ? '▶' : i + 1}</span><div class="grow"><div class="name" ${z && z.queue_index === i ? 'style="color:var(--accent-strong)"' : ''}>${esc(it.title || it.uri)}</div><div class="sub">${esc([it.artist, it.album].filter(Boolean).join(' · '))}</div></div><span class="muted small">${fmt(it.duration_ms)}</span></div>`).join('')}</div>` : emptyState('queue', 'The queue is empty', 'Play an album or add tracks with the row actions.'));
          $$('[data-qi]', main).forEach((b) => b.onclick = () => transport({ action: 'play', index: Number(b.dataset.qi) }));
          const qc = $('#qclear'); if (qc) qc.onclick = async () => { try { await api('DELETE', `/zones/${state.zone}/queue`); refreshQueue(); } catch (e) { toast(e.message, 'error'); } };
          break;
        }
        case 'settings': {
          const info = await api('GET', '/info'); state.info = info;
          const outs = await api('GET', '/outputs');
          const folders = await api('GET', '/library/folders').catch(() => []);
          const ns = await api('GET', '/node/settings').catch(() => ({ contact: '', discovery: false }));
          let pk = null; try { pk = await api('GET', '/pubky'); } catch { /* no pubky config */ }
          if (state.view !== 'settings') return;
          main.innerHTML = pageHead('Settings') + `
            <div class="cards">
              <div class="card"><h3>Node</h3><dl class="kv"><dt>Name</dt><dd>${esc(info.name)}</dd><dt>Version</dt><dd>${esc(info.version)}</dd><dt>Library</dt><dd>${info.library.tracks} tracks · ${info.library.albums} albums · ${info.library.artists} artists</dd><dt>Files</dt><dd>${info.library.files}${info.library.missing_files ? ` · <span style="color:var(--warn)">${info.library.missing_files} missing</span>` : ''}</dd></dl>
                <div class="actions" style="margin-top:14px"><button class="btn" id="scan">Scan library</button><button class="btn" id="rescan">Rescan outputs</button><button class="btn ghost" id="logout">Change token</button></div></div>
              <div class="card"><h3>Upstream contact</h3><p class="muted small">An email or URL sent in the node's User-Agent. Pinakes (Discover, On Pubky), MusicBrainz and Wikipedia ask for one.</p><div class="toolbar"><input class="input" id="contact" placeholder="you@example.org or https://your.site" value="${esc(ns.contact)}" style="width:280px"><button class="btn primary" id="contactsave">Save</button><span class="pill ${ns.discovery ? 'ok' : 'warn'}">${ns.discovery ? 'Discover on' : 'Discover off'}</span></div></div>
              <div class="card"><h3>Pubky</h3>${pk ? `<p class="muted">${pk.user ? 'Signed in as <code>' + esc(pk.user) + '</code>' : 'No session on the node yet'} · ${pk.unsynced} unsynced object${pk.unsynced === 1 ? '' : 's'}</p><div class="actions" style="margin-top:14px">${pk.user ? '' : `<button class="btn primary" id="pklogin">Sign in with Pubky Ring</button>`}<button class="btn" id="sync">Sync playlists and favourites</button></div><div id="pkurl"></div>` : '<p class="muted">Pubky is not configured on this node.</p>'}</div>
            </div>
            <section class="section"><div class="section-head"><h3>Library folders</h3></div><p class="muted small">Paths on the node. New folders are watched and scanned; removing one forgets its tracks.</p>
              ${folders.length ? `<div class="list">${folders.map(([p, present]) => `<div class="list-row"><div class="grow"><div class="name">${esc(p)}</div></div>${present ? '' : '<span class="pill warn">missing</span>'}<button class="btn sm ghost" data-folder-del="${esc(p)}">Remove</button></div>`).join('')}</div>` : '<p class="muted">No folders yet.</p>'}
              <div class="toolbar" style="margin-top:10px"><input class="input" id="folderadd" placeholder="/path/on/the/node/Music" style="width:360px"><button class="btn primary" id="folderbtn">${icon('plus')} Add folder</button></div></section>
            <section class="section"><div class="section-head"><h3>Outputs</h3></div>${outs.length ? `<div class="list">${outs.map((o) => `<div class="list-row"><div class="grow"><div class="name">${esc(o.name)}</div><div class="sub">${esc(o.id)}</div></div><span class="muted small">${o.caps.max_pcm_bits}-bit · ${o.caps.pcm_rates.map((r) => (r / 1000).toFixed(1).replace('.0', '') + 'k').join(' ')}${o.caps.dsd ? ' · DSD' : ''}</span></div>`).join('')}</div>` : '<p class="muted">No outputs found.</p>'}</section>
            <section class="section"><div class="section-head"><h3>Zones</h3></div><div class="list">${zoneList().map((z) => `<div class="list-row"><div class="grow"><div class="name">${esc(z.name)}</div><div class="sub">${esc(z.output || '')}</div></div><span class="pill ${z.transport === 'playing' ? 'ok' : ''}">${esc(z.transport)}</span></div>`).join('')}</div></section>
            <p class="muted small">Shortcuts: <kbd>/</kbd> search · <kbd>Space</kbd> play/pause · <kbd>←</kbd> <kbd>→</kbd> previous/next · <kbd>N</kbd> now playing · <kbd>Esc</kbd> close</p>`;
          $('#scan').onclick = async () => { try { await api('POST', '/library/scan', {}); toast('Scan started'); } catch (e) { toast(e.message, 'error'); } };
          const saveContact = async () => { const c = $('#contact').value.trim(); if (!c) return; try { await api('PUT', '/node/settings', { contact: c }); state.discoveryOff = false; toast('Contact saved; Discover is on'); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#contactsave').onclick = saveContact; $('#contact').onkeydown = (e) => { if (e.key === 'Enter') saveContact(); };
          const addFolder = async () => { const p = $('#folderadd').value.trim(); if (!p) return; try { const r = await api('POST', '/library/folders', { path: p }); toast(r.scan_started ? 'Folder added; scanning' : 'Folder added; scanned after the current scan'); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#folderbtn').onclick = addFolder; $('#folderadd').onkeydown = (e) => { if (e.key === 'Enter') addFolder(); };
          $$('[data-folder-del]', main).forEach((b) => b.onclick = async () => { if (!b.dataset.armed) { b.dataset.armed = '1'; b.textContent = 'Really remove?'; setTimeout(() => { if (b.isConnected) { delete b.dataset.armed; b.textContent = 'Remove'; } }, 3000); return; } try { const r = await api('DELETE', '/library/folders?path=' + encodeURIComponent(b.dataset.folderDel)); toast(`Folder removed · ${r.removed_tracks} tracks gone`); render(); } catch (e) { toast(e.message, 'error'); } });
          $('#rescan').onclick = async () => { try { await api('POST', '/outputs/rescan'); toast('Outputs rescanned'); render(); } catch (e) { toast(e.message, 'error'); } };
          $('#logout').onclick = () => login();
          const sy = $('#sync'); if (sy) sy.onclick = async () => { try { const r = await api('POST', '/pubky/sync'); toast(`Synced: ${r.pushed} pushed, ${r.pulled} pulled${r.errors.length ? `, ${r.errors.length} errors` : ''}`); render(); } catch (e) { toast(e.message, 'error'); } };
          const pl = $('#pklogin'); if (pl) pl.onclick = async () => {
            try {
              const r = await api('POST', '/pubky/login');
              $('#pkurl').innerHTML = `<div class="notice" style="margin-top:12px">Open Pubky Ring, paste this URL and approve. This page updates by itself.<br><code class="qr-note">${esc(r.url)}</code></div>`;
              const t = setInterval(async () => { try { const st = await api('GET', '/pubky/login'); if (st.status === 'signed_in') { clearInterval(t); toast('Signed in as ' + st.user); render(); } } catch (e) { clearInterval(t); toast(e.message, 'error'); } }, 2000);
            } catch (e) { toast(e.message, 'error'); }
          };
          break;
        }
        default:
          main.innerHTML = emptyState('compass', 'Unknown view', '');
      }
      bindTiles();
      $$('[data-artist]', main).forEach((el) => el.onclick = () => setView('artist', Number(el.dataset.artist)));
      $$('[data-back]', main).forEach((b) => b.onclick = () => setView(b.dataset.back));
      $$('[data-empty-action]', main).forEach((b) => b.onclick = () => setView(b.dataset.emptyAction));
      if (Object.keys(lists).length) bindTrackRows(main, lists);
    } catch (e) {
      main.innerHTML = `<div class="notice warn">${esc(e.message)}</div>`;
    }
  };

  // ---- connection + events -------------------------------------------------
  const setConn = (ok, text) => { const c = $('#conn'); c.className = 'node-status ' + (ok ? 'ok' : 'bad'); $('.node-text', c).textContent = text; c.title = text; };
  const connect = async () => {
    try {
      state.info = await api('GET', '/info');
      setConn(true, `${state.info.name} · ${state.info.library.tracks} tracks`);
      loadFavourites().catch(() => {});
      const zones = await api('GET', '/zones');
      state.zones = Object.fromEntries(zones.map((z) => [z.id, z]));
      ensureZone(); renderZoneName(); renderBar(); refreshQueue(false);
      setView(state.view);
      events();
    } catch (e) { setConn(false, e.message); main.innerHTML = `<div class="notice warn">Cannot reach the node: ${esc(e.message)}</div>`; }
  };
  const events = () => {
    if (state.ws) { state.ws.onclose = null; state.ws.close(); }
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${proto}://${location.host}/v1/events${tokenQ()}`);
    state.ws = ws;
    ws.onmessage = (m) => {
      const ev = JSON.parse(m.data);
      if (ev.type === 'snapshot') { state.zones = Object.fromEntries(ev.zones.map((z) => [z.id, z])); ensureZone(); renderZoneName(); }
      else if (ev.type === 'zone') {
        const old = state.zones[ev.state.id]; state.zones[ev.state.id] = ev.state;
        if (!old) renderZoneName();
        if (ev.state.id === state.zone && (!old || old.queue_version !== ev.state.queue_version)) refreshQueue();
        if (ev.state.id === state.zone && old && old.now_playing && ev.state.now_playing && old.now_playing.item.id !== ev.state.now_playing.item.id && !overlay.hidden) renderNowPlaying();
      }
      else if (ev.type === 'zone_removed') { delete state.zones[ev.zone]; ensureZone(); renderZoneName(); }
      else if (ev.type === 'library_changed') { state.albums = null; if (['albums', 'tracks', 'artists', 'home', 'favourites', 'album', 'artist', 'playlists', 'playlist'].includes(state.view)) render(); if (state.info) api('GET', '/info').then((i) => { state.info = i; setConn(true, `${i.name} · ${i.library.tracks} tracks`); }).catch(() => {}); }
      renderBar();
    };
    ws.onclose = () => { setConn(false, 'Reconnecting…'); setTimeout(events, 3000); };
    ws.onopen = () => { if (state.info) setConn(true, `${state.info.name} · ${state.info.library.tracks} tracks`); };
  };
  connect();
})();
