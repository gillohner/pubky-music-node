// Provider marks (same glyphs as the web UI's app.js) and a tiny toggle for menus/popovers.
const MARKS = {
  tidal: '<path d="M4 8l4-4 4 4-4 4zm8 0l4-4 4 4-4 4zm-4 8l4-4 4 4-4 4z"/>',
  spotify: '<path d="M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm4.4 14.4a.7.7 0 0 1-1 .3c-2.6-1.6-5.9-2-9.8-1.1a.7.7 0 0 1-.3-1.4c4.2-1 7.9-.5 10.8 1.3.3.2.4.6.3.9zm1.2-2.8a.9.9 0 0 1-1.2.3c-3-1.8-7.5-2.4-11-1.3a.9.9 0 0 1-.5-1.7c4-1.2 9-.6 12.4 1.5.4.3.5.8.3 1.2zm.1-2.9C14.1 8.6 8.3 8.4 4.9 9.4a1 1 0 0 1-.6-2c3.9-1.2 10.4-1 14.5 1.5a1 1 0 0 1-1.1 1.8z"/>',
  deezer: '<path d="M2 15h4v3H2zm5 0h4v3H7zm5 0h4v3h-4zm5 0h4v3h-4zM7 11h4v3H7zm5 0h4v3h-4zm5 0h4v3h-4zm-5-4h4v3h-4zm5 0h4v3h-4zm0-4h4v3h-4z"/>',
  youtube: '<path d="M21.6 7.2a2.5 2.5 0 0 0-1.8-1.8C18.3 5 12 5 12 5s-6.3 0-7.8.4A2.5 2.5 0 0 0 2.4 7.2 26 26 0 0 0 2 12a26 26 0 0 0 .4 4.8 2.5 2.5 0 0 0 1.8 1.8C5.7 19 12 19 12 19s6.3 0 7.8-.4a2.5 2.5 0 0 0 1.8-1.8A26 26 0 0 0 22 12a26 26 0 0 0-.4-4.8zM10 15V9l5.2 3z"/>',
  qobuz: '<path d="M12 3a8 8 0 1 0 4.9 14.3l2.4 2.4 1.4-1.4-2.4-2.4A8 8 0 0 0 12 3zm0 2.5a5.5 5.5 0 1 1 0 11 5.5 5.5 0 0 1 0-11z"/>',
  apple: '<path d="M16 3v11.5a3.5 3.5 0 1 1-2-3.2V7.2l-5 1.4v7.9A3.5 3.5 0 1 1 7 13.3V5.3z"/>',
  local: '<path d="M4 4h16v3H4zm0 5h16v3H4zm0 5h10v3H4zm12 0h4v3h-4z"/>',
  archive: '<path d="M3 20h18v2H3zM4 9h2v9H4zm4 0h2v9H8zm4 0h2v9h-2zm4 0h2v9h-2zm4 0h1v9h-1zM2 7l10-5 10 5v1H2z"/>',
  mb: '<path d="M12 2l9 5v10l-9 5-9-5V7zm-1 4.5L6 9v6l5 2.5zm2 0v11l5-2.5V9z"/>',
};
document.querySelectorAll('.mark[data-s]').forEach((el) => {
  const g = MARKS[el.dataset.s] || MARKS.local;
  el.classList.add(el.dataset.s);
  el.insertAdjacentHTML('afterbegin', `<svg viewBox="0 0 24 24" aria-hidden="true">${g}</svg>`);
});
document.querySelectorAll('[data-toggle]').forEach((b) => b.addEventListener('click', (e) => {
  e.stopPropagation();
  const t = document.getElementById(b.dataset.toggle);
  if (t) t.hidden = !t.hidden;
}));
document.querySelectorAll('.scope').forEach((g) => g.addEventListener('click', (e) => { const b = e.target.closest('button'); if (!b) return; g.querySelectorAll('button').forEach((x) => x.classList.toggle('on', x === b)); }));
