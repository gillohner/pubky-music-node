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
  }

  async function reconnect(nodeUrl) {
    show('connect', false);
    show('saved');
    show('retry', false);
    show('other', false);
    clearErr();
    $('saved-msg').textContent = 'Connecting to ' + nodeUrl + '…';
    try {
      await invoke('open_saved');
    } catch (e) {
      $('saved-msg').textContent = String(e) + '.';
      show('retry');
      show('other');
    }
  }

  $('retry').onclick = async () => {
    const s = await invoke('saved');
    if (s) reconnect(s.node_url); else connectView();
  };
  $('other').onclick = async () => { await invoke('forget'); connectView(); };

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

  invoke('saved').then((s) => (s ? reconnect(s.node_url) : connectView())).catch(fail);
})();
