// SPDX-License-Identifier: AGPL-3.0-or-later
// Browser checks for docs/09 P5.9 (UX foundations) and P5.11 (Compare).
//
// Requires a running site with published sample data (stub server or production-shaped
// instance). Uses Playwright from nix:
//
//   nix develop -c env PLAYWRIGHT_MODULE=$(nix eval --raw nixpkgs#playwright-driver)/index.mjs \
//     PLAYWRIGHT_BROWSERS_PATH=$(nix eval --raw nixpkgs#playwright-driver.browsers) \
//     node site/checks/ux-foundations.mjs --origin http://localhost:8099

import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../..', import.meta.url));

const USAGE = `usage: node site/checks/ux-foundations.mjs [options]

  --browser chromium|firefox   engine to drive (default chromium)
  --origin URL                 site origin (default http://localhost:8099)
  --out DIR                    artifact directory (default /tmp/looklab-ux)
  --only color|vocabulary      run one section`;

const options = {
  browser: 'chromium',
  origin: 'http://localhost:8099',
  out: '/tmp/looklab-ux',
  only: '',
};

for (let index = 2; index < process.argv.length; index += 1) {
  const argument = process.argv[index];
  if (argument === '--help' || argument === '-h') {
    console.log(USAGE);
    process.exit(0);
  }
  const key = argument.replace(/^--/, '');
  if (!(key in options)) {
    throw new Error(`unknown option ${argument}\n\n${USAGE}`);
  }
  index += 1;
  options[key] = process.argv[index];
}

const playwrightModule = process.env.PLAYWRIGHT_MODULE;
const playwright = playwrightModule
  ? await import(pathToFileURL(playwrightModule).href)
  : await import('playwright-core');
const { chromium, firefox } = playwright.default ?? playwright;
const engine = options.browser === 'firefox' ? firefox : chromium;

const VIEWPORTS = {
  desktop: { width: 1280, height: 960 },
  mobile: { width: 375, height: 812 },
};

const failures = [];
const log = [];

function check(condition, message) {
  if (!condition) {
    failures.push(message);
    console.log(`FAIL ${message}`);
  } else {
    console.log(`ok ${message}`);
  }
}

function note(message) {
  log.push(message);
  console.log(message);
}

await mkdir(options.out, { recursive: true });

async function open(viewport) {
  const browser = await engine.launch();
  const context = await browser.newContext({ viewport });
  const page = await context.newPage();
  const problems = [];
  page.on('pageerror', error => problems.push(error.message));
  page.on('console', message => {
    if (message.type() === 'error') problems.push(message.text());
  });
  return { browser, context, page, problems };
}

async function close(session) {
  await session.context.close();
  await session.browser.close();
}

async function siteIndex() {
  const response = await fetch(`${options.origin}/data/index.json`);
  if (!response.ok) {
    throw new Error(`GET /data/index.json: HTTP ${response.status}`);
  }
  return response.json();
}

// docs/08 §7.1: every neutral is R = G = B; one accent marks interactive controls.
async function colorTokens() {
  const files = [`${REPO_ROOT}/site/styles.css`];
  for (const name of await readdir(`${REPO_ROOT}/site/styles`)) {
    if (name.endsWith('.css')) files.push(`${REPO_ROOT}/site/styles/${name}`);
  }
  const named = new Set(['black', 'white', 'gray', 'grey', 'silver', 'transparent', 'currentcolor', 'inherit', 'none']);
  const chromaticNames = /\b(red|orange|yellow|olive|lime|green|teal|aqua|cyan|blue|navy|fuchsia|magenta|purple|maroon|pink|brown|gold|crimson|indigo|violet)\b/gi;
  let checked = 0;
  let accentLiterals = 0;
  const offenders = [];
  const tokenCounts = new Map();
  let sourceText = '';
  for (const file of files) {
    const source = await readFile(file, 'utf8');
    sourceText += source;
    const lines = source.replace(/\/\*[\s\S]*?\*\//g, '').split('\n');
    for (const [index, line] of lines.entries()) {
      const accent = /--accent\s*:/.test(line);
      for (const match of line.matchAll(/#[0-9a-fA-F]{3,8}\b/g)) {
        const rgb = parseHex(match[0]);
        if (!rgb) continue;
        checked += 1;
        if (accent) {
          accentLiterals += 1;
          continue;
        }
        if (rgb.r !== rgb.g || rgb.g !== rgb.b) {
          offenders.push(`${file.split('/site/')[1]}:${index + 1} ${match[0]}`);
        }
      }
      for (const match of line.matchAll(/\brgba?\(\s*([^)]*)\)/gi)) {
        const rgb = parseRgb(match[1]);
        if (!rgb) continue;
        checked += 1;
        if (accent) {
          accentLiterals += 1;
          continue;
        }
        if (rgb.r !== rgb.g || rgb.g !== rgb.b) {
          offenders.push(`${file.split('/site/')[1]}:${index + 1} ${match[0]}`);
        }
      }
      for (const match of line.matchAll(/\bhsla?\(\s*([^)]*)\)/gi)) {
        const rgb = parseHsl(match[1]);
        if (!rgb) continue;
        checked += 1;
        if (accent) {
          accentLiterals += 1;
          continue;
        }
        if (rgb.r !== rgb.g || rgb.g !== rgb.b) {
          offenders.push(`${file.split('/site/')[1]}:${index + 1} ${match[0]}`);
        }
      }
      for (const match of line.matchAll(/\b(?:background|color|border|outline|box-shadow|fill|stroke)(?:-[a-z]+)?\s*:\s*([^;]+)/gi)) {
        for (const word of match[1].matchAll(chromaticNames)) {
          if (!named.has(word[0].toLowerCase())) {
            offenders.push(`${file.split('/site/')[1]}:${index + 1} ${word[0]}`);
          }
        }
      }
      for (const match of line.matchAll(/(--[a-z-]+)\s*:\s*(#[0-9a-fA-F]{3,8})\s*;/g)) {
        tokenCounts.set(match[1], (tokenCounts.get(match[1]) ?? 0) + 1);
      }
    }
  }
  check(/@media\s*\(prefers-color-scheme:\s*dark\)/.test(sourceText), 'dark theme block is declared');
  for (const token of ['--paper', '--panel', '--ink', '--muted', '--line', '--notice']) {
    check((tokenCounts.get(token) ?? 0) >= 2, `color token ${token} is declared for light and dark themes`);
  }
  check(accentLiterals >= 1, `one accent literal is exempt from the neutral rule (found ${accentLiterals})`);
  check(offenders.length === 0, `color tokens: ${checked} literals checked, every neutral has R = G = B${offenders.length ? `; offenders: ${offenders.join(', ')}` : ''}`);
  return { checked, accentLiterals, offenders };
}

function parseHex(text) {
  const digits = text.slice(1);
  const expand = value => value.length === 1 ? value + value : value;
  if (digits.length === 3 || digits.length === 4) {
    return { r: parseInt(expand(digits[0]), 16), g: parseInt(expand(digits[1]), 16), b: parseInt(expand(digits[2]), 16) };
  }
  if (digits.length === 6 || digits.length === 8) {
    return { r: parseInt(digits.slice(0, 2), 16), g: parseInt(digits.slice(2, 4), 16), b: parseInt(digits.slice(4, 6), 16) };
  }
  return null;
}

function parseRgb(body) {
  const values = body.replace(/\//g, ' ').split(/[\s,]+/).filter(Boolean).slice(0, 3);
  if (values.length < 3) return null;
  const channel = value => value.endsWith('%') ? Math.round(parseFloat(value) * 2.55) : Math.round(parseFloat(value));
  const rgb = values.map(channel);
  return rgb.some(Number.isNaN) ? null : { r: rgb[0], g: rgb[1], b: rgb[2] };
}

function parseHsl(body) {
  const values = body.replace(/\//g, ' ').split(/[\s,]+/).filter(Boolean);
  if (values.length < 3) return null;
  const hue = parseFloat(values[0]) / 360;
  const saturation = parseFloat(values[1]) / 100;
  const lightness = parseFloat(values[2]) / 100;
  if ([hue, saturation, lightness].some(Number.isNaN)) return null;
  if (saturation === 0) {
    const value = Math.round(lightness * 255);
    return { r: value, g: value, b: value };
  }
  const q = lightness < 0.5 ? lightness * (1 + saturation) : lightness + saturation - lightness * saturation;
  const p = 2 * lightness - q;
  const channel = offset => {
    let t = (hue + offset) % 1;
    if (t < 0) t += 1;
    if (t < 1 / 6) return p + (q - p) * 6 * t;
    if (t < 1 / 2) return q;
    if (t < 2 / 3) return p + (q - p) * (2 / 3 - t) * 6;
    return p;
  };
  return {
    r: Math.round(channel(1 / 3) * 255),
    g: Math.round(channel(0) * 255),
    b: Math.round(channel(-1 / 3) * 255),
  };
}

// docs/08 §0.3 right column. Inspect regions may show these; glance and engage may not.
const INTERNAL_TERMS = [
  { label: 'Body', pattern: /\bBody\b/ },
  { label: 'Epoch', pattern: /\bEpoch\b/ },
  { label: 'native_family Look', pattern: /\bnative_family\b/ },
  { label: 'lut_defined Look', pattern: /\blut_defined\b/ },
  { label: 'Realization', pattern: /\bRealization\b/ },
  { label: 'provisional', pattern: /\bprovisional\b/i },
  { label: 'ΔE (CAM16-UCS)', pattern: /ΔE\s*\(CAM16-UCS\)/ },
  { label: 'Tone T', pattern: /\bTone T\b/ },
  { label: 'neutral tint N', pattern: /\bneutral tint N\b/ },
  { label: 'chroma G', pattern: /\bchroma G\b/ },
  { label: 'hue R', pattern: /\bhue R\b/ },
  { label: 'lightness Λ', pattern: /lightness Λ/ },
  { label: 'Settings card', pattern: /\bSettings card\b/ },
  { label: 'Recipe', pattern: /\bRecipe\b/ },
  { label: 'Contribution ladder', pattern: /\bContribution ladder\b/i },
  { label: 'rung', pattern: /\brungs?\b/i },
  { label: 'Bounties', pattern: /\bBounties\b/ },
  { label: 'Contributor credential', pattern: /\bContributor credential\b/i },
  { label: 'passkey assertion', pattern: /\bpasskey assertion\b/i },
  { label: 'Status feed', pattern: /\bStatus feed\b/i },
  { label: 'capability token', pattern: /\bcapability token\b/i },
  { label: 'endorsed tier', pattern: /\bendorsed tier\b/i },
  { label: 'Moderation queue', pattern: /\bModeration queue\b/i },
];

const INSPECT_ROUTES = new Set(['/community/log']);

function visibleText() {
  const root = document.body;
  const skip = element => {
    for (let node = element; node && node !== root; node = node.parentElement) {
      if (node.hidden) return true;
      if (node.matches?.('pre, code, svg, canvas, .data-table, .plot, dialog:not([open])')) return true;
      if (node.matches?.('details:not([open])') && !node.querySelector(':scope > summary')?.contains(element)) return true;
    }
    return false;
  };
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const parts = [];
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if (!node.parentElement || skip(node.parentElement)) continue;
    const text = node.textContent.trim();
    if (text) parts.push(text);
  }
  return parts.join(' ');
}

async function load(page, route) {
  await page.goto(`${options.origin}${route}`);
  await page.waitForFunction(() => (document.querySelector('main')?.innerText.trim().length ?? 0) > 60, null, { timeout: 60_000 }).catch(() => {});
  await page.waitForTimeout(800);
}

async function vocabulary(routes) {
  const hits = [];
  for (const [size, viewport] of Object.entries(VIEWPORTS)) {
    const session = await open(viewport);
    const { page } = session;
    try {
      for (const route of routes) {
        await load(page, route);
        const text = await page.evaluate(visibleText);
        const found = INTERNAL_TERMS.filter(term => term.pattern.test(text));
        if (found.length) {
          for (const term of found) {
            const match = term.pattern.exec(text);
            const at = match ? match.index : 0;
            const context = text.slice(Math.max(0, at - 60), at + 60).replace(/\s+/g, ' ');
            hits.push({ size, route, term: term.label, context });
          }
        }
        const inspect = INSPECT_ROUTES.has(route);
        check(inspect || found.length === 0, `${size} ${route}: ${inspect ? 'inspect route, internal terms allowed' : 'no internal vocabulary'}${found.length ? ` (${found.map(term => term.label).join(', ')})` : ''}`);
      }
    } finally {
      await close(session);
    }
  }
  return hits;
}

async function focusInfo(page) {
  return page.evaluate(() => {
    const element = document.activeElement;
    const style = element ? getComputedStyle(element) : null;
    return {
      tag: element?.tagName,
      text: (element?.getAttribute('aria-label') || element?.textContent || '').trim().slice(0, 60),
      inMain: Boolean(element?.closest('main')),
      inDialog: Boolean(element?.closest('dialog')),
      outline: style ? `${style.outlineStyle} ${style.outlineWidth}` : '',
    };
  });
}

function escapeRegExp(text) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

async function keyboard() {
  const steps = [];
  const record = message => {
    steps.push(message);
    note(`keyboard ${message}`);
  };
  const desktop = await open(VIEWPORTS.desktop);
  const { page } = desktop;
  try {
    await page.goto(`${options.origin}/`);
    await page.waitForSelector('h1');
    await page.evaluate(() => document.body.focus());
    const order = [];
    for (let step = 0; step < 7; step += 1) {
      await page.keyboard.press('Tab');
      order.push(await focusInfo(page));
    }
    record(`tab order ${order.map(info => info.text).join(' -> ')}`);
    check(order.every(info => info.outline === 'solid 3px' || info.outline === 'solid 2px'), 'every focused control shows a visible outline');
    check(order.map(info => info.text).join('|') === 'Skip to content|looklab|Looks|Compare|Get the look|Contribute|Sign in', 'header tab order follows the site map');

    await page.goto(`${options.origin}/`);
    await page.waitForSelector('h1');
    await page.keyboard.press('Tab');
    await page.keyboard.press('Enter');
    await page.waitForTimeout(200);
    await page.keyboard.press('Tab');
    const skipped = await focusInfo(page);
    record(`skip link then Tab reaches ${skipped.tag} "${skipped.text}" inMain=${skipped.inMain}`);
    check(skipped.inMain, 'skip link moves keyboard focus into main');

    await page.goto(`${options.origin}/`);
    await page.waitForSelector('h1');
    await page.getByRole('link', { name: 'Compare', exact: true }).focus();
    await page.keyboard.press('Enter');
    await page.waitForURL(/\/compare/);
    record(`nav Compare then Enter reaches ${new URL(page.url()).pathname}`);
    check(new URL(page.url()).pathname.endsWith('/compare'), 'navigation works from the keyboard');

    await page.getByRole('button', { name: 'Sign in', exact: true }).focus();
    await page.keyboard.press('Enter');
    const dialog = page.getByRole('dialog', { name: 'Sign in to looklab' });
    await dialog.waitFor();
    record('sign-in dialog opened with Enter');
    for (let step = 0; step < 8; step += 1) {
      await page.keyboard.press('Tab');
      const info = await focusInfo(page);
      check(info.inDialog, 'Tab keeps focus in the sign-in dialog');
    }
    await page.keyboard.press('Escape');
    await page.waitForTimeout(200);
    const restored = await focusInfo(page);
    record(`Escape closes sign-in and returns focus to "${restored.text}"`);
    check(!(await page.getByRole('dialog', { name: 'Sign in to looklab' }).isVisible().catch(() => false)), 'Escape closes the sign-in dialog');
    check(restored.text === 'Sign in', 'Escape returns focus to the Sign in trigger');

    await page.goto(`${options.origin}/compare?a=${compare.a}&b=${compare.b}`);
    const viewer = page.locator('.compare-page > .viewer');
    await viewer.waitFor({ timeout: 120_000 });
    const divider = viewer.getByRole('slider', { name: 'Comparison divider' });
    await divider.focus();
    await page.keyboard.press('Home');
    check(await divider.getAttribute('aria-valuenow') === '0', 'divider Home sets 0');
    await page.keyboard.press('ArrowRight');
    check(await divider.getAttribute('aria-valuenow') === '1', 'divider ArrowRight moves by 1');
    await page.keyboard.press('End');
    check(await divider.getAttribute('aria-valuenow') === '100', 'divider End sets 100');
    record('divider keyboard: Home 0, ArrowRight 1, End 100');
    const frame = await viewer.locator('.viewer-images').boundingBox();
    await page.keyboard.press('Home');
    await page.mouse.move(frame.x + 1, frame.y + frame.height / 2);
    await page.mouse.down();
    await page.mouse.move(frame.x + frame.width / 2, frame.y + frame.height / 2);
    await page.mouse.up();
    const dragged = Number(await divider.getAttribute('aria-valuenow'));
    record(`divider mouse drag reaches ${dragged}`);
    check(Math.abs(dragged - 50) < 3, 'divider follows a mouse drag');

    for (const [label, value] of [['Side by side', 'side-by-side'], ['Toggle', 'toggle'], ['Difference', 'difference'], ['Split', 'split']]) {
      await viewer.getByRole('button', { name: label, exact: true }).focus();
      await page.keyboard.press('Enter');
      await page.waitForFunction(view => new URL(location.href).searchParams.get('view') === view, value);
      check(new URL(page.url()).searchParams.get('view') === value, `keyboard selects ${label}`);
    }
    record('view modes selectable from the keyboard: side by side, toggle, difference, split');

    await page.goto(`${options.origin}/compare?a=${compare.a}&b=${compare.b}`);
    await viewer.waitFor({ timeout: 120_000 });
    const trigger = page.locator('.compare-look-pickers .look-picker button').first();
    await trigger.focus();
    await page.keyboard.press('Enter');
    await page.locator('.look-picker-popover').first().waitFor();
    check(await page.locator('.look-picker-popover').first().isVisible(), 'Enter opens the look picker');
    await page.keyboard.press('Escape');
    await page.waitForTimeout(200);
    check(!(await page.locator('.look-picker-popover').first().isVisible()), 'Escape closes the look picker');
    check(await trigger.evaluate(element => element === document.activeElement), 'Escape returns focus to the look picker trigger');
    record('look picker opens with Enter and closes with Escape, returning focus');

    await page.goto(`${options.origin}/contribute`);
    await page.waitForSelector('h1');
    await page.waitForTimeout(2000);
    const card = page.locator('.help-card-button').first();
    await card.focus();
    await page.keyboard.press('Enter');
    const help = page.getByRole('dialog').first();
    await help.waitFor();
    record(`contribute help dialog "${await help.getAttribute('aria-label')}" opened with Enter`);
    for (let step = 0; step < 8; step += 1) {
      await page.keyboard.press('Tab');
      const info = await focusInfo(page);
      check(info.inDialog, 'Tab keeps focus in the contribute help dialog');
    }
    await page.keyboard.press('Escape');
    await page.waitForTimeout(200);
    check(!(await help.isVisible().catch(() => false)), 'Escape closes the contribute help dialog');
    check(await card.evaluate(element => element === document.activeElement), 'Escape returns focus to the help card');
    record('contribute help dialog traps Tab and returns focus on Escape');
  } finally {
    await close(desktop);
  }

  const mobile = await open(VIEWPORTS.mobile);
  try {
    const page = mobile.page;
    await page.goto(`${options.origin}/`);
    await page.waitForSelector('h1');
    await page.evaluate(() => document.body.focus());
    const order = [];
    for (let step = 0; step < 3; step += 1) {
      await page.keyboard.press('Tab');
      order.push(await focusInfo(page));
    }
    record(`375 px tab order ${order.map(info => info.text).join(' -> ')}`);
    check(order[2].text === 'Menu', '375 px header collapses to the Menu button');
    await page.keyboard.press('Enter');
    await page.waitForTimeout(200);
    check(await page.locator('.menu-toggle').getAttribute('aria-expanded') === 'true', 'Enter opens the 375 px menu');
    check(await page.locator('#main-navigation').isVisible(), 'opened menu shows the navigation');
    await page.keyboard.press('Tab');
    const link = await focusInfo(page);
    check(link.text === 'Looks', 'Tab moves from the Menu button into the navigation');
    await page.keyboard.press('Enter');
    await page.waitForURL(/\/looks/);
    record(`375 px menu navigates to ${new URL(page.url()).pathname}`);
    check(new URL(page.url()).pathname.endsWith('/looks'), '375 px menu navigation works from the keyboard');
  } finally {
    await close(mobile);
  }
  await writeFile(`${options.out}/keyboard.log`, `${steps.join('\n')}\n`);
  return steps;
}

async function settle(page) {
  await page.evaluate(async () => {
    const step = Math.max(200, Math.floor(innerHeight * 0.8));
    for (let top = 0; top < document.documentElement.scrollHeight; top += step) {
      scrollTo(0, top);
      await new Promise(resolve => setTimeout(resolve, 80));
    }
    scrollTo(0, 0);
  });
  await page.waitForTimeout(500);
}

function overflowReport() {
  const wide = [...document.querySelectorAll('body *')].filter(element => {
    const box = element.getBoundingClientRect();
    const style = getComputedStyle(element);
    return box.width > 0 && box.right > innerWidth + 1 && style.visibility !== 'hidden' && style.display !== 'none';
  }).slice(0, 5).map(element => `${element.tagName.toLowerCase()}.${String(element.className).slice(0, 40)} right=${Math.round(element.getBoundingClientRect().right)}`);
  return { scrollWidth: document.documentElement.scrollWidth, innerWidth, wide };
}

async function overflow(routes) {
  const rows = [];
  for (const [size, viewport] of Object.entries(VIEWPORTS)) {
    const session = await open(viewport);
    const { page } = session;
    try {
      for (const route of routes) {
        await load(page, route);
        await settle(page);
        const result = await page.evaluate(overflowReport);
        rows.push({ size, route, ...result });
        check(result.scrollWidth <= result.innerWidth, `${size} ${route}: no horizontal scroll (${result.scrollWidth}/${result.innerWidth})${result.wide.length ? `; ${result.wide.join(', ')}` : ''}`);
      }
      await page.goto(`${options.origin}/`);
      await page.getByRole('button', { name: 'Sign in', exact: true }).focus();
      await page.keyboard.press('Enter');
      await page.getByRole('dialog', { name: 'Sign in to looklab' }).waitFor();
      const dialog = await page.evaluate(() => {
        const element = document.querySelector('dialog[open]');
        return { scrollWidth: element?.scrollWidth ?? 0, clientWidth: element?.clientWidth ?? 0, document: document.documentElement.scrollWidth, innerWidth };
      });
      rows.push({ size, route: 'sign-in dialog', ...dialog });
      check(dialog.scrollWidth <= dialog.clientWidth && dialog.document <= dialog.innerWidth, `${size} sign-in dialog: no horizontal scroll`);
    } finally {
      await close(session);
    }
  }
  const lines = rows.map(row => `${row.size}\t${row.route}\t${row.scrollWidth}/${row.innerWidth}${row.scrollWidth <= row.innerWidth ? '' : ' OVERFLOW'}`);
  await writeFile(`${options.out}/overflow.txt`, `${lines.join('\n')}\n`);
  return rows;
}

async function compareModes() {
  const desktop = await open(VIEWPORTS.desktop);
  const { page } = desktop;
  try {
    const url = `${options.origin}/compare?a=${compare.a}&b=${compare.b}&scene=${compare.scene}&view=side-by-side`;
    await page.goto(url);
    const viewer = page.locator('.compare-page > .viewer');
    await viewer.waitFor({ timeout: 120_000 });
    await page.waitForTimeout(2000);
    const params = new URL(page.url()).searchParams;
    check(params.get('a') === compare.a && params.get('b') === compare.b && params.get('scene') === compare.scene && params.get('view') === 'side-by-side', 'Compare URL keeps a, b, scene and view');
    check(await page.locator('.viewer-controls button[aria-pressed=true]').textContent() === 'Side by side', 'Compare restores the view from the URL');
    check(await page.locator('.compare-scene-chip[aria-pressed=true]').count() === 1, 'Compare restores the scene from the URL');
    const summary = (await page.locator('.compare-summary-line p').textContent()).trim();
    check(summary.length > 20, `Compare summary sentence: ${summary}`);
    check(await page.locator('.compare-breakdown-row').count() === 5, 'Compare shows five breakdown rows');
    const rated = !summary.startsWith('Not enough');
    check(await page.locator('.compare-summary .closeness').count() === (rated ? 1 : 0), rated ? 'Compare shows the closeness label' : 'Compare withholds the closeness label without measurements');
    const disclosure = page.locator('details.disclosure').filter({ hasText: 'Show measurements' });
    check(await disclosure.evaluate(element => !element.open), 'Compare measurements start behind a closed disclosure');
    check(!(await page.locator('.compare-measurements-table').isVisible()), 'Compare measurement table is hidden before opening the disclosure');
    await disclosure.locator('summary').click();
    check(await disclosure.evaluate(element => element.open), 'Compare measurements disclosure opens');
    if (rated) {
      check(await page.locator('.compare-measurements-table').isVisible(), 'Compare measurement table is visible after opening');
    } else {
      check(!(await page.locator('.compare-measurements-table').isVisible()), 'Compare withholds the measurement table without data');
    }
    check(await page.locator('details.disclosure .plot').count() >= 3, 'Compare measurements include plots');
    await page.reload();
    await viewer.waitFor({ timeout: 120_000 });
    await page.waitForTimeout(1500);
    check(new URL(page.url()).searchParams.get('view') === 'side-by-side', 'Compare view survives a reload');

    const otherScene = compare.sceneAlt;
    if (otherScene) {
      await page.locator('.compare-scene-chip').filter({ hasText: otherScene.title }).click();
      await page.waitForFunction(name => new URL(location.href).searchParams.get('scene') === name, otherScene.name);
      await page.reload();
      await viewer.waitFor({ timeout: 120_000 });
      await page.waitForTimeout(1500);
      check(new URL(page.url()).searchParams.get('scene') === otherScene.name, 'Compare scene round-trips through the URL');
      check(await page.locator('.compare-scene-chip[aria-pressed=true]').textContent() === otherScene.title, 'Compare scene selection survives a reload');
    }

    await page.locator('details.disclosure summary').filter({ hasText: 'More options' }).click();
    await page.getByLabel('First look version').selectOption({ index: 0 });
    await page.waitForFunction(() => new URL(location.href).searchParams.has('av'));
    check(new URL(page.url()).searchParams.has('av'), 'Compare version selection writes av to the URL');
    await page.reload();
    await viewer.waitFor({ timeout: 120_000 });
    check(new URL(page.url()).searchParams.has('av'), 'Compare version pin survives a reload');

    const before = new URL(page.url()).searchParams;
    await page.locator('.compare-swap').click();
    await page.waitForFunction(([a, b]) => new URL(location.href).searchParams.get('a') === a && new URL(location.href).searchParams.get('b') === b, [before.get('b'), before.get('a')]);
    check(new URL(page.url()).searchParams.get('a') === before.get('b'), 'Compare swap exchanges the two looks in the URL');
    await page.reload();
    await viewer.waitFor({ timeout: 120_000 });
    check(new URL(page.url()).searchParams.get('a') === before.get('b'), 'Compare swap survives a reload');

    const other = compare.third;
    if (other) {
      await page.locator('.compare-look-pickers .look-picker button').first().click();
      const card = page.locator('.look-picker-popover').first().getByRole('button', { name: new RegExp(`^Simulated: ${escapeRegExp(other.name)}`) });
      await card.click();
      await page.waitForFunction(slug => new URL(location.href).searchParams.get('a') === slug, other.slug);
      check(!new URL(page.url()).searchParams.has('av'), 'switching a look clears that side\'s version pin');
    }

    for (const [label, value] of [['Split', 'split'], ['Side by side', 'side-by-side'], ['Toggle', 'toggle'], ['Difference', 'difference']]) {
      await page.goto(`${options.origin}/compare?a=${compare.a}&b=${compare.b}&scene=${compare.scene}`);
      await viewer.waitFor({ timeout: 120_000 });
      await page.locator('.viewer-controls button', { hasText: label }).click();
      await page.waitForFunction(view => new URL(location.href).searchParams.get('view') === view, value);
      await page.waitForTimeout(value === 'difference' ? 3000 : 800);
      await page.screenshot({ path: `${options.out}/compare-${value}-desktop.png`, fullPage: true });
    }
    return { summary };
  } finally {
    await close(desktop);
  }
}

async function compareMobileScreenshots() {
  const session = await open(VIEWPORTS.mobile);
  const { page } = session;
  try {
    for (const [label, value] of [['Split', 'split'], ['Side by side', 'side-by-side'], ['Toggle', 'toggle'], ['Difference', 'difference']]) {
      await page.goto(`${options.origin}/compare?a=${compare.a}&b=${compare.b}&scene=${compare.scene}`);
      await page.locator('.compare-page > .viewer').waitFor({ timeout: 120_000 });
      await page.locator('.viewer-controls button', { hasText: label }).click();
      await page.waitForFunction(view => new URL(location.href).searchParams.get('view') === view, value);
      await page.waitForTimeout(value === 'difference' ? 3000 : 800);
      const result = await page.evaluate(overflowReport);
      check(result.scrollWidth <= result.innerWidth, `mobile compare ${label}: no horizontal scroll (${result.scrollWidth}/${result.innerWidth})`);
      await page.screenshot({ path: `${options.out}/compare-${value}-mobile.png`, fullPage: true });
    }
  } finally {
    await close(session);
  }
}

const index = await siteIndex();
const publicLooks = (index.looks ?? []).filter(look => look.visibility === undefined || look.visibility === 'public');
if (publicLooks.length < 2) {
  throw new Error('needs at least two public looks in /data/index.json; run against the stub server or a seeded production instance');
}
const photoScenes = (index.scenes ?? []).filter(scene => scene.kind === 'photo');
if (!photoScenes.length) {
  throw new Error('needs sample photos in /data/index.json');
}
const camera = (index.bodies ?? []).find(body => body.kind === 'camera');
const scene = photoScenes[0];
const sceneAlt = photoScenes[1];
const compare = {
  a: publicLooks[0].slug,
  b: publicLooks[1].slug,
  third: publicLooks[2],
  scene: scene.name,
  sceneAlt: sceneAlt ? { name: sceneAlt.name, title: sceneAlt.title } : null,
};
const routes = [
  '/',
  '/looks',
  `/looks/${compare.a}`,
  '/cameras',
  ...(camera ? [`/cameras/${camera.id}`] : []),
  `/compare?a=${compare.a}&b=${compare.b}`,
  '/match',
  '/contribute',
  '/contribute/wanted',
  '/me',
  '/help/passkeys',
  '/community',
  '/community/propose',
  '/community/log',
  '/developers',
  '/developers/pipelines',
  '/about/how-it-works',
  '/about/privacy',
];

console.log(`origin ${options.origin}`);
console.log(`looks ${compare.a}, ${compare.b}${compare.third ? `, ${compare.third.slug}` : ''}; scene ${compare.scene}`);

await colorTokens();
if (options.only === 'color') {
  if (failures.length) process.exitCode = 1; else console.log('color checks passed');
} else {
  const hits = await vocabulary(routes);
  if (options.only === 'vocabulary') {
    await writeFile(`${options.out}/vocabulary.txt`, `${hits.map(hit => `${hit.size}\t${hit.route}\t${hit.term}\t${hit.context}`).join('\n')}\n`);
    if (failures.length) process.exitCode = 1; else console.log('vocabulary checks passed');
  } else {
    const keyboardSteps = await keyboard();
    const rows = await overflow(routes);
    const compareResult = await compareModes();
    await compareMobileScreenshots();

    await writeFile(`${options.out}/vocabulary.txt`, `${hits.map(hit => `${hit.size}\t${hit.route}\t${hit.term}\t${hit.context}`).join('\n')}\n`);
    await writeFile(`${options.out}/summary.json`, JSON.stringify({ failures, keyboardSteps: keyboardSteps.length, overflow: rows.length, compare: compareResult, hits }, null, 2));

    console.log(`artifacts ${options.out}`);
    if (failures.length) {
      console.error(`${failures.length} failure(s)`);
      process.exitCode = 1;
    } else {
      console.log('all checks passed');
    }
  }
}
