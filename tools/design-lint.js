// Design lint for the web UI (STEWARD-FIX-design-lint.md). No dependencies; CI runs it.
//   1. every <button> has a class from the component set;
//   2. no emoji or dingbats as icons (icons come from icon()); a line may opt out with
//      `lint-allow-glyph` when the glyph is a component's own styled text (the rating stars);
//   3. every class the UI uses has a rule in app.css.
//   node tools/design-lint.js [static dir]
const fs = require('fs');
const path = require('path');
const DIR = process.argv[2] || path.join(__dirname, '..', 'crates', 'pm-api', 'static');
const files = fs.readdirSync(DIR).filter((f) => /\.(js|html)$/.test(f)).map((f) => [f, fs.readFileSync(path.join(DIR, f), 'utf8')]);
const css = fs.readdirSync(DIR).filter((f) => f.endsWith('.css')).map((f) => fs.readFileSync(path.join(DIR, f), 'utf8')).join('\n');
const problems = [];
const where = (file, src, idx) => `${file}:${src.slice(0, idx).split('\n').length}`;

// The component set a button may belong to.
const BUTTON_CLASSES = ['btn', 'chip', 'icon-btn', 'chiplink', 'chipx', 'half', 'back', 'tile', 'link', 'nav-item', 'pop-item', 'tab', 'seg', 'row-btn', 'list-row', 'toast-x', 'close', 'crumb', 'zone-btn', 'out-btn', 'play-btn', 'sbtn', 'mini', 'qbtn', 'rowplay', 'idmark', 'card', 'quality', 'connbar-retry', 'lnk'];
// Classes set only by the browser or third parties, never styled by us.
const IGNORE_CLASSES = new Set(['ic']);

// Characters that are icons, not text: emoji, symbols and pictographs, dingbats, geometric
// shapes, misc symbols. Copy punctuation (· — … “ ” › ← → × ½ ⋯) is outside these ranges.
const ICONISH = /[\u{1F000}-\u{1FAFF}\u{2600}-\u{27BF}\u{25A0}-\u{25FF}\u{2B00}-\u{2BFF}\u{1F1E6}-\u{1F1FF}]/u;

// A token glued to an interpolation (`f${n}`) is a computed class, not checked here.
const classTokens = (value) => value.replace(/\S*\$\{[^}]*\}\S*/g, ' ').split(/\s+/).filter((t) => /^[a-zA-Z][\w-]*$/.test(t));
const used = new Map(); // class -> first place
const use = (c, at) => { if (!used.has(c)) used.set(c, at); };

for (const [file, src] of files) {
  // 1. buttons
  for (const m of src.matchAll(/<button\b([^>]*)>/g)) {
    const attrs = m[1];
    const cls = /class="([^"]*)"/.exec(attrs);
    const tokens = cls ? classTokens(cls[1]) : [];
    const dynamic = cls && /\$\{/.test(cls[1]);
    if (!tokens.some((t) => BUTTON_CLASSES.includes(t)) && !(dynamic && /\$\{[^}]*'(btn|chip|icon-btn)\b/.test(cls[1]))) {
      problems.push(`${where(file, src, m.index)}  button without a component class: <button${attrs.slice(0, 80)}>`);
    }
  }
  for (const m of src.matchAll(/createElement\('button'\)/g)) {
    const after = src.slice(m.index, m.index + 300);
    if (!/className\s*=|classList\.add/.test(after)) problems.push(`${where(file, src, m.index)}  created button without a class`);
  }
  // 2. icon glyphs
  src.split('\n').forEach((line, i) => {
    if (line.includes('lint-allow-glyph') || /^\s*(\/\/|\*|<!--)/.test(line)) return;
    const g = ICONISH.exec(line);
    if (g) problems.push(`${file}:${i + 1}  emoji or symbol used as an icon: ${JSON.stringify(g[0])} (use icon())`);
  });
  // 3. classes used
  for (const m of src.matchAll(/class="([^"]*)"/g)) classTokens(m[1]).forEach((c) => use(c, where(file, src, m.index)));
  for (const m of src.matchAll(/classList\.(add|toggle|remove)\(([^)]*)\)/g)) {
    // toggle(name, force): only the first argument is a class
    const names = [...m[2].matchAll(/'([\w-]+)'/g)].map((x) => x[1]);
    (m[1] === 'toggle' ? names.slice(0, 1) : names).forEach((c) => use(c, where(file, src, m.index)));
  }
  for (const m of src.matchAll(/className\s*=\s*'([^']*)'/g)) classTokens(m[1]).forEach((c) => use(c, where(file, src, m.index)));
  // ternaries inside class attributes: `${x ? 'on' : ''}`
  for (const m of src.matchAll(/class="[^"]*"/g)) {
    // only the branches of a ternary, not the strings it compares
    for (const e of m[0].matchAll(/\$\{([^}]*)\}/g)) for (const s of e[1].matchAll(/[?:]\s*'([a-zA-Z][\w-]*(?: [a-zA-Z][\w-]*)*)'/g)) s[1].split(' ').forEach((c) => use(c, where(file, src, m.index)));
  }
}
const hasRule = (c) => new RegExp(`\\.${c.replace(/[-]/g, '\\-')}(?![\\w-])`).test(css);
for (const [c, at] of [...used.entries()].sort()) {
  if (IGNORE_CLASSES.has(c) || hasRule(c)) continue;
  problems.push(`${at}  class "${c}" has no rule in app.css`);
}

if (problems.length) {
  for (const p of problems) console.log(p);
  console.log(`\n${problems.length} design problem${problems.length === 1 ? '' : 's'}`);
  process.exit(1);
}
console.log(`design lint: ok (${used.size} classes, every button classed, no icon glyphs)`);
