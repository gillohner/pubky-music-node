// The connect page: reconnect to the remembered node, or connect with
// Pubky Ring or by address. Commands live in src/lib.rs.
(() => {
  'use strict';
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const $ = (id) => document.getElementById(id);
  let ringUrl = '';

  const show = (id, on = true) => { $(id).hidden = !on; };
  const fail = (e) => { $('err').textContent = String(e); show('err'); $('err').scrollIntoView({ block: 'nearest' }); };
  const clearErr = () => show('err', false);

  function connectView() {
    show('saved', false);
    show('connect');
    searchLan();
  }

  // Nodes on the LAN (mDNS and SSDP). A node without a token connects at once; one with a token
  // fills the address form and asks for it.
  let searching = false;
  async function searchLan() {
    if (searching) return;
    searching = true;
    show('lan-again', false);
    $('lan-status').textContent = 'Looking for nodes…';
    show('lan-status');
    let found = [];
    try { found = await invoke('discover_nodes'); } catch (e) { found = []; }
    searching = false;
    show('lan-again');
    const list = $('lan-list');
    list.textContent = '';
    if (!found.length) {
      $('lan-status').textContent = 'None found. The node must be on the same network, with its API listening beyond localhost (for example 0.0.0.0:7788).';
      return;
    }
    show('lan-status', false);
    for (const n of found) {
      const li = document.createElement('li');
      const b = document.createElement('button');
      b.type = 'button';
      const name = document.createElement('span');
      name.textContent = n.name || n.url;
      const addr = document.createElement('span');
      addr.className = 'addr';
      addr.textContent = n.url.replace(/^https?:\/\//, '') + (n.token_required ? ' · needs a token' : '');
      b.append(name, addr);
      b.onclick = () => pickNode(n, b);
      li.append(b);
      list.append(li);
    }
  }

  async function pickNode(n, btn) {
    clearErr();
    if (n.token_required) {
      $('url').value = n.url;
      $('manual-box').open = true;
      show('tok-hint');
      $('tok').focus();
      $('manual-box').scrollIntoView({ block: 'nearest' });
      return;
    }
    btn.disabled = true;
    try { await invoke('connect_manual', { nodeUrl: n.url, token: '' }); } catch (e) { fail(e); } finally { btn.disabled = false; }
  }
  $('lan-again').onclick = () => searchLan();

  // The remembered node is tried again for about a minute before the page gives up: a phone
  // opening the app as Wi-Fi comes back, or a node still starting, answers a few seconds later.
  const RETRIES = [2, 3, 5, 5, 10, 10, 15, 15];
  let attempt = 0; // bumped by every new reconnect, so an older retry loop stops

  async function reconnect(nodeUrl) {
    const mine = ++attempt;
    show('connect', false);
    show('saved');
    show('retry', false);
    show('other', false);
    clearErr();
    $('saved-msg').textContent = 'Connecting to ' + nodeUrl + '…';
    for (let i = 0; ; i++) {
      try {
        await invoke('open_saved');
        return;
      } catch (e) {
        if (mine !== attempt) return;
        show('retry');
        show('other');
        // Only an unreachable node is worth waiting for; a refused token is not
        if (i >= RETRIES.length || !String(e).startsWith("Can't reach")) {
          $('saved-msg').textContent = String(e) + '.';
          return;
        }
        for (let left = RETRIES[i]; left > 0; left--) {
          $('saved-msg').textContent = String(e) + '. Trying again in ' + left + ' s…';
          await new Promise((r) => setTimeout(r, 1000));
          if (mine !== attempt) return;
        }
        $('saved-msg').textContent = 'Connecting to ' + nodeUrl + '…';
      }
    }
  }

  $('retry').onclick = async () => {
    const s = await invoke('saved');
    if (s) reconnect(s.node_url); else connectView();
  };
  $('other').onclick = async () => { attempt++; await invoke('forget'); connectView(); };

  listen('ring-url', (ev) => {
    ringUrl = ev.payload.url;
    $('qr').innerHTML = ev.payload.qr_svg;
    $('ring').textContent = 'Waiting for Pubky Ring…';
    show('ring-out');
  });

  $('ring').onclick = async () => {
    clearErr();
    $('ring').disabled = true;
    $('ring').textContent = 'Starting…';
    try {
      await invoke('ring_connect');
    } catch (e) {
      fail(e);
      show('ring-out', false);
    } finally {
      $('ring').disabled = false;
      $('ring').textContent = 'Sign in with Pubky Ring';
    }
  };
  $('ring-open').onclick = () => { if (ringUrl) invoke('open_ring', { url: ringUrl }).catch(fail); };

  $('manual').onsubmit = async (ev) => {
    ev.preventDefault();
    clearErr();
    const btn = ev.submitter || $('manual').querySelector('button');
    btn.disabled = true;
    try {
      await invoke('connect_manual', { nodeUrl: $('url').value, token: $('tok').value });
    } catch (e) {
      fail(e);
    } finally {
      btn.disabled = false;
    }
  };

  const start = () => invoke('saved').then((s) => (s ? reconnect(s.node_url) : connectView())).catch(fail);

  // A crash last time: show its report first, and reconnect only after Continue, so a crash on
  // reconnecting cannot loop (src/crash.rs).
  function showCrash(text) {
    $('crash-text').textContent = text;
    // The newest entry's title, in words above the details ("Java exception on thread …")
    const titles = text.split('\n').filter((l) => l.startsWith('== '));
    $('crash-first').textContent = (titles[titles.length - 1] || '').replace(/^== .*?[+-]\d{4}: /, '').replace(/^== /, '');
    show('crash');
    const copy = async () => {
      try { await navigator.clipboard.writeText(text); $('crash-copy').textContent = 'Copied'; return; } catch (e) { /* older WebViews */ }
      const r = document.createRange(); r.selectNodeContents($('crash-text'));
      const sel = getSelection(); sel.removeAllRanges(); sel.addRange(r);
      try { document.execCommand('copy'); $('crash-copy').textContent = 'Copied'; } catch (e) { $('crash-copy').textContent = 'Selected: copy it'; }
    };
    $('crash-copy').onclick = copy;
    $('crash-share').onclick = () => invoke('crash_share', { text }).catch(copy);
    $('crash-go').onclick = async () => { try { await invoke('crash_dismiss'); } catch (e) { /* carry on */ } show('crash', false); start(); };
  }

  invoke('crash_report').then((t) => (t ? showCrash(t) : start())).catch(start);
})();
