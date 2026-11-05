// SPDX-License-Identifier: AGPL-3.0-or-later
// Browser checks: worker round trip, match/permalink, contribute upload with HAR, page screenshots.

import { copyFile, link, mkdir, readFile, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { fileURLToPath, pathToFileURL } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../..', import.meta.url));

const USAGE = `usage: node site/checks/e2e.mjs [options]

  --browser chromium|firefox   engine to drive (default chromium)
  --flow all|match|contribute|pages|flatness|propose|developers|compare|account
  --origin URL                 stub server origin (default http://localhost:8099)
  --out DIR                    artifact directory (default /tmp/looklab-e2e)
  --state DIR                  stub server state, must match --state (default /tmp/looklab-stub-state)
  --raw FILE --jpeg FILE       the contribution files
  --body ID --firmware V --sim KEY
  --controls "key=step,..."    control steps, comma separated
  --workers N                  worker count for the contribute run
  --verifier FILE              native looklab binary for contribution HAR checks
  --verify-module FILE         source-built prep module for HAR checks
  --count N                    files for the flatness run (default 100)
  --expected FILE              canonical permalink to compare against
  --status TOKEN               status token for the /status screenshot
  --permalink BASE64URL        pinned match result for the pages flow

needs the built site behind the stub server and Playwright from nix:

  cd site && nix develop -c env CARGO_TARGET_DIR=target-ui trunk build --release
  CARGO_TARGET_DIR=target-ui nix develop -c cargo run -j 6 -p looklab-web --features stub --example stub_server -- --port 8099 --state /tmp/looklab-e2e/stub-state
  CARGO_TARGET_DIR=target-ui nix develop -c cargo test -j 6 -p looklab-cli --test verify_payload reproduces_an_upload_and_passes -- --exact
  nix develop -c env \\
    PLAYWRIGHT_MODULE=$(nix eval --raw nixpkgs#playwright-driver)/index.js \\
    PLAYWRIGHT_BROWSERS_PATH=$(nix eval --raw nixpkgs#playwright-driver.browsers) \\
    node site/checks/e2e.mjs --browser chromium --flow all --out /tmp/looklab-e2e --state /tmp/looklab-e2e/stub-state`;

const options = {
  browser: 'chromium',
  flow: 'all',
  origin: 'http://localhost:8099',
  out: '/tmp/looklab-e2e',
  state: '/tmp/looklab-stub-state',
  raw: 'fixtures/cache/fujifilm-x-s10.raf',
  jpeg: 'fixtures/generated/fujifilm-x-s10.jpg',
  body: 'fujifilm/x-s10',
  firmware: '1.00',
  sim: 'provia',
  controls: '',
  workers: 1,
  verifier: 'target-ui/debug/looklab',
  'verify-module': 'target-ui/verify-payload-wasm/wasm32-unknown-unknown/release/looklab_prep_module.wasm',
  count: 100,
  expected: 'site/data/looks/synthetic-match/1.0.0/match.permalink',
  status: '',
  permalink: '',
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
  const value = process.argv[index];
  options[key] = ['workers', 'count'].includes(key) ? Number(value) : value;
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

await mkdir(options.out, { recursive: true });

function check(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
  console.log(`ok ${message}`);
}

function launchOptions() {
  if (options.browser !== 'firefox') {
    return {};
  }
  return {
    firefoxUserPrefs: {
      'security.webauth.webauthn_enable_softtoken': true,
      'security.webauth.webauthn_enable_usbtoken': false,
      'security.webauth.webauthn_enable_android_fido2': false,
    },
  };
}

async function addVirtualAuthenticator(page) {
  if (options.browser !== 'chromium') {
    return;
  }
  const session = await page.context().newCDPSession(page);
  await session.send('WebAuthn.enable');
  await session.send('Emulation.setFocusEmulationEnabled', { enabled: true });
  const { authenticatorId } = await session.send('WebAuthn.addVirtualAuthenticator', {
    options: {
      protocol: 'ctap2',
      ctap2Version: 'ctap2_1',
      transport: 'internal',
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  });
  await session.send('WebAuthn.setAutomaticPresenceSimulation', {
    authenticatorId,
    enabled: true,
  });
  return { cdp: session, authenticatorId };
}

async function open(viewport, harPath) {
  const browser = await engine.launch(launchOptions());
  const context = await browser.newContext({
    viewport,
    acceptDownloads: true,
    ...(harPath
      ? {
          recordHar: {
            path: harPath,
            content: 'embed',
            urlFilter: /\/api\/(nonce|passkey\/[^/?]+|session(?:\/[^/?]+)?|upload)|\/status\/[A-Za-z0-9_-]+$/,
          },
        }
      : {}),
  });
  const page = await context.newPage();
  await page.bringToFront();
  const bodies = new Map();
  page.on('request', (request) => {
    if (!/\/api\/|\/status\//.test(request.url())) {
      return;
    }
    const data = request.postDataBuffer();
    if (data) {
      bodies.set(`${request.method()} ${request.url()}`, data.toString('base64'));
    }
  });
  const problems = [];
  page.on('pageerror', (error) => problems.push(String(error)));
  page.on('console', (message) => {
    if (message.type() === 'error') {
      problems.push(message.text());
    }
  });
  const authenticator = await addVirtualAuthenticator(page);
  return { browser, context, page, problems, bodies, authenticator };
}

async function close(session) {
  await session.context.close();
  await session.browser.close();
}

async function match(size) {
  const session = await open(VIEWPORTS[size]);
  const { page, problems } = session;
  try {
    // Trunk reloads replace the document during concurrent UI edits.
    await page.routeWebSocket(/\/trunk\/ws(?:\?|$)/, socket => socket.close());
    if (options.browser === 'chromium') {
      const cdp = await session.context.newCDPSession(page);
      await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
      await session.context.grantPermissions(['clipboard-read', 'clipboard-write']);
    }
    await page.goto(`${options.origin}/match?camera=fujifilm/x-t5&look=sunlit-negative`);
    const recipe = page.getByRole('article').filter({ has: page.getByRole('heading', { name: 'Take this recipe shooting' }) });
    await recipe.or(page.getByRole('alert')).first().waitFor({ timeout: 120_000 });
    if (await page.getByRole('alert').count()) {
      throw new Error(`Recipe failed: ${await page.getByRole('alert').allTextContents()}`);
    }
    await recipe.waitFor({ timeout: 120_000 });
    await page.getByRole('heading', { name: 'Sunlit Negative on your Fujifilm X-T5', exact: true }).waitFor();
    await page.getByRole('slider', { name: 'Comparison divider' }).waitFor({ timeout: 120_000 });
    const settings = await recipe.locator('dl').innerText();
    const share = page.getByRole('link', { name: 'Share link', exact: true });
    const href = await share.getAttribute('href');
    const permalink = Buffer.from(href.split('/').pop(), 'base64url').toString('utf8');
    if (options.browser === 'chromium') {
      await page.getByRole('button', { name: 'Copy as text', exact: true }).focus();
      await page.keyboard.press('Enter');
      await page.getByRole('status').filter({ hasText: 'Settings copied' }).waitFor();
      const copied = await page.evaluate(() => navigator.clipboard.readText());
      check(copied.includes('Film simulation:'), `${size}: copied settings are readable`);
      const exposure = copied.split('\n').find(line => line.startsWith('Exposure compensation:'));
      check(/^Exposure compensation: [+-](?:\d+|\d+\/3) EV$/.test(exposure ?? ''), `${size}: exposure compensation uses exact thirds: ${exposure}`);
    }
    for (const colorScheme of ['dark', 'light']) {
      await page.emulateMedia({ colorScheme });
      check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${size}: match fits viewport`);
      await page.screenshot({ path: `${options.out}/match-${size}-${colorScheme}.png`, fullPage: true });
    }
    await share.click();
    await recipe.or(page.getByRole('alert')).first().waitFor({ timeout: 120_000 });
    if (await page.getByRole('alert').count()) {
      throw new Error(`Pinned recipe failed: ${await page.getByRole('alert').allTextContents()}`);
    }
    await recipe.waitFor({ timeout: 600_000 });
    await page.getByRole('slider', { name: 'Comparison divider' }).waitFor({ timeout: 120_000 });
    check(await recipe.locator('dl').innerText() === settings, `${size}: permalink reloads the same settings`);
    check(await page.getByRole('link', { name: 'Share link', exact: true }).getAttribute('href') === href, `${size}: pinned result has the same permalink`);
    await page.getByText('Check with your shots', { exact: true }).click();
    await page.locator('.match-verification input[type=file]').setInputFiles({ name: 'invalid.jpg', mimeType: 'image/jpeg', buffer: Buffer.from('not a JPEG') });
    await page.getByRole('alert').filter({ hasText: 'Choose JPEGs' }).waitFor();
    const jpeg = await readFile(options.jpeg);
    await page.locator('.match-verification .drop-zone').evaluate((zone, encoded) => {
      const transfer = new DataTransfer();
      transfer.items.add(new File([Uint8Array.from(atob(encoded), character => character.charCodeAt(0))], 'shot.jpg', { type: 'image/jpeg' }));
      zone.dispatchEvent(new DragEvent('drop', { dataTransfer: transfer, bubbles: true, cancelable: true }));
    }, jpeg.toString('base64'));
    await page.getByRole('alert').filter({ hasText: 'Add the original RAW files' }).waitFor({ timeout: 120_000 });
    await page.screenshot({ path: `${options.out}/match-verification-${size}.png`, fullPage: true });
    await page.goto(`${options.origin}/match`);
    const cameras = page.getByRole('combobox', { name: 'Your camera', exact: true });
    await cameras.waitFor();
    await cameras.focus();
    await page.keyboard.press('Home');
    await page.keyboard.press('ArrowDown');
    await page.keyboard.press('Enter');
    const firstNext = page.getByRole('button', { name: 'Next', exact: true });
    const cameraPicker = await cameras.boundingBox();
    check((await firstNext.boundingBox()).y > cameraPicker.y + cameraPicker.height, `${size}: wizard navigation follows camera controls`);
    await page.getByRole('button', { name: 'Next', exact: true }).focus();
    await page.keyboard.press('Enter');
    await page.getByRole('heading', { name: 'Which look?', exact: true }).waitFor();
    await page.getByRole('button', { name: 'Next', exact: true }).focus();
    await page.keyboard.press('Enter');
    await page.getByRole('heading', { name: 'How will you apply it?', exact: true }).waitFor();
    await page.screenshot({ path: `${options.out}/match-wizard-${size}.png`, fullPage: true });
    await page.getByRole('button', { name: 'Get my settings', exact: true }).focus();
    await page.keyboard.press('Enter');
    check(!(await page.getByRole('button', { name: 'Back', exact: true }).isEnabled()), `${size}: choices stay locked while computing a match`);
    await recipe.waitFor({ timeout: 600_000 });
    check(problems.length === 0, `${options.browser} ${size}: no page errors during match: ${problems.join('; ')}`);
    await page.getByRole('button', { name: 'Change choices', exact: true }).click();
    await page.getByRole('button', { name: 'Back', exact: true }).click();
    await page.getByRole('button', { name: 'Back', exact: true }).click();
    let cameraRequestReady;
    const cameraRequest = new Promise(resolve => { cameraRequestReady = resolve; });
    await page.route('**/data/bodies/fujifilm/x-m1.json', route => cameraRequestReady(route));
    const changeCamera = async model => {
      await cameras.fill(model);
      await page.keyboard.press('ArrowDown');
      await page.keyboard.press('Enter');
    };
    await changeCamera('X-M1');
    const pendingCamera = await cameraRequest;
    check(!(await page.getByRole('button', { name: 'Next', exact: true }).isEnabled()), `${size}: camera changes wait for the new settings`);
    await pendingCamera.fulfill({ status: 503, body: 'Unavailable for this browser check' });
    await page.getByRole('alert').filter({ hasText: 'Your camera settings could not load.' }).waitFor();
    check(!(await page.getByRole('button', { name: 'Next', exact: true }).isEnabled()), `${size}: camera load failure cannot submit old settings`);
    await page.screenshot({ path: `${options.out}/match-camera-error-${size}.png`, fullPage: true });
    await page.unroute('**/data/bodies/fujifilm/x-m1.json');
    await page.route(/\/scenes\/photo-.*-thumb\.png$/, route => route.fulfill({ status: 503, body: 'Unavailable for this browser check' }));
    await page.goto(`${options.origin}/match`);
    await cameras.waitFor();
    await changeCamera('X-T5');
    await page.getByRole('button', { name: 'Next', exact: true }).click();
    await page.getByRole('status').filter({ hasText: 'Look thumbnails could not load.' }).waitFor();
    await page.getByRole('button', { name: 'Choose a look', exact: true }).click();
    await page.getByRole('dialog', { name: 'Choose a look', exact: true }).getByRole('button', { name: /Cold Steel/ }).click();
    await page.getByText('Cold Steel', { exact: true }).filter({ visible: true }).waitFor();
    check(await page.getByRole('button', { name: 'Choose a look', exact: true }).getAttribute('aria-expanded') === 'false', `${size}: looks stay selectable without thumbnails`);
    await page.screenshot({ path: `${options.out}/match-thumbnail-error-${size}.png`, fullPage: true });
    return permalink;
  } catch (error) {
    console.error(`Match page at failure: ${await page.locator('body').innerText()}`);
    console.error(`Match browser diagnostics: ${problems.join('; ')}`);
    await page.screenshot({ path: `${options.out}/match-failed-${size}.png`, fullPage: true });
    throw error;
  } finally {
    await close(session);
  }
}

async function contribute(size) {
  const harPath = `${options.out}/session-${size}.har`;
  const session = await open(VIEWPORTS[size], harPath);
  const { page, problems } = session;
  await page.routeWebSocket('**/.well-known/trunk/ws', () => {});
  if (session.authenticator) {
    await session.authenticator.cdp.send('Network.enable');
    await session.authenticator.cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
  }
  const summary = {};
  const capture = async (name, fullPage = true) => {
    for (const colorScheme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme });
      await page.screenshot({ path: `${options.out}/${name}-${size}-${colorScheme}.png`, fullPage });
      check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth && [...document.querySelectorAll('dialog[open]')].every(dialog => dialog.scrollWidth <= dialog.clientWidth)), `${size} ${colorScheme}: ${name} fits viewport`);
    }
    await page.emulateMedia({ colorScheme: 'light' });
  };
  try {
    await page.goto(`${options.origin}/contribute`);
    await page.getByRole('heading', { name: 'Help build looks', exact: true }).waitFor();
    await capture('contribute-landing');
    const catalog = await (await page.request.get(`${options.origin}/data/index.json`)).json();
    const wantedLook = catalog.looks.find(look => look.visibility !== 'test_fixture');
    const wantedCamera = catalog.bodies.find(camera => camera.kind === 'camera');
    check(Boolean(wantedLook && wantedCamera), `${size}: wanted-shot fixture uses published names`);
    const wantedBounties = [0.01, 0.1, 0.5, 0.9].map((reduction, index) => ({
      look: wantedLook.slug,
      look_version: 1,
      body: wantedCamera.id,
      region: `h${index}/e3`,
      expected_variance_reduction: reduction,
      reason: { kind: 'unverified_region', sigma: 0.2 },
      status: 'open',
    }));
    await page.route('**/bounties.json', route => route.fulfill({ json: { bounties: wantedBounties } }));
    const guideNames = [
      'Share photos you already have',
      'Take a few wanted shots',
      'Re-render a few RAWs in camera',
      'Photograph a pattern on your phone',
      'Photograph a gradient',
      'Photograph a color chart',
    ];
    for (const [index, name] of guideNames.entries()) {
      const card = page.getByRole('button', { name, exact: true });
      await card.focus();
      await page.keyboard.press('Enter');
      const dialog = page.getByRole('dialog', { name, exact: true });
      await dialog.waitFor();
      check(await dialog.getByRole('list').first().getByRole('listitem').count() >= 3, `${size}: ${name} has numbered guidance`);
      if (index === 1) {
        await dialog.getByRole('article').nth(2).waitFor();
        check(await dialog.getByRole('article').count() === 3, `${size}: wanted guide shows the top three requests`);
        check(await dialog.getByRole('article').first().textContent().then(text => text.includes(wantedLook.name) && text.includes(`${wantedCamera.make} ${wantedCamera.model}`)), `${size}: wanted guide shows published look and camera names`);
      }
      if (index === 3) {
        const { createHash } = await import('node:crypto');
        const fixtureSeed = createHash('sha256').update('looklab-fixture-pattern-v1').digest('hex');
        const link = dialog.getByRole('link', { name: 'Open the measurement pattern on your phone', exact: true });
        const address = await dialog.getByRole('textbox', { name: 'Pattern URL', exact: true }).inputValue();
        const expected = new URL(`pattern/${fixtureSeed}`, `${options.origin}/`).href;
        check(address === expected, `${size}: phone link uses the public fixture and deployment prefix`);
        check(await link.evaluate(anchor => anchor.href) === address, `${size}: phone link and selectable URL agree`);
        await writeFile(`${options.out}/pattern-${size}.url`, `${address}\n`);
        await dialog.getByRole('img', { name: 'Scan to open the measurement pattern', exact: true }).screenshot({ path: `${options.out}/pattern-qr-${size}.png` });
        const [pattern] = await Promise.all([page.waitForEvent('popup'), link.click()]);
        try {
          if (options.browser === 'chromium') {
            const cdp = await page.context().newCDPSession(pattern);
            await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
          }
          await pattern.reload();
          const canvas = pattern.getByRole('button', { name: 'Measurement pattern. Tap or press Enter for fullscreen.', exact: true });
          await canvas.waitFor();
          check(pattern.url() === address, `${size}: phone link opens its pattern in a new tab`);
          const rendered = () => pattern.locator('canvas').evaluate(canvas => canvas.width === innerWidth && canvas.height === innerHeight && canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data.some((channel, index) => index % 4 !== 3 && channel !== 0));
          check(await rendered(), `${size}: phone pattern fills the viewport with rendered pixels`);
          check(!await pattern.locator('.site-header').isVisible() && !await pattern.locator('footer').isVisible(), `${size}: phone pattern has no site chrome`);
          for (const colorScheme of ['light', 'dark']) {
            await pattern.emulateMedia({ colorScheme });
            await pattern.screenshot({ path: `${options.out}/phone-pattern-${size}-${colorScheme}.png` });
          }
          if (await pattern.evaluate(() => document.fullscreenEnabled)) {
            await canvas.focus();
            await pattern.keyboard.press('Enter');
            await pattern.waitForFunction(() => document.fullscreenElement !== null);
            check(true, `${size}: pattern supports keyboard fullscreen`);
            await pattern.evaluate(() => document.exitFullscreen());
          }
          const other = new URL(address);
          other.pathname = other.pathname.replace(/[a-f0-9]{64}$/, 'ab'.repeat(32));
          await pattern.goto(other.href);
          await canvas.waitFor();
          check(await rendered(), `${size}: pattern route renders another 64-hex seed`);
        } finally {
          await pattern.close();
        }
      }
      await capture(`contribute-help-${index}`, false);
      await page.keyboard.press('Escape');
      await dialog.waitFor({ state: 'hidden' });
      check(await card.evaluate(button => button === document.activeElement), `${size}: ${name} returns keyboard focus`);
    }
    await page.getByRole('button', { name: guideNames[0], exact: true }).click();
    const [chooser] = await Promise.all([
      page.waitForEvent('filechooser'),
      page.getByRole('dialog', { name: guideNames[0], exact: true }).getByRole('button', { name: 'Choose photos', exact: true }).click(),
    ]);
    await chooser.setFiles([options.raw, options.jpeg]);
    await page.getByRole('heading', { name: /^\d+ pairs? found$/ }).waitFor({ timeout: 120_000 });
    await capture('contribute-1-drop');

    await page.fill('#body-id', options.body);
    await page.fill('#firmware', options.firmware);
    await page.fill('#sim', options.sim);
    await page.getByRole('checkbox', { name: 'Recipe contribution' }).check();
    await page.fill('#recipe-target', 'cold-steel');
    await page.fill('#recipe-target', 'sunlit-negative');
    await page.getByRole('checkbox', { name: 'Recipe contribution' }).uncheck();
    await page.click('summary:has-text("Technical settings editor")');
    if (options.controls) {
      await page.fill('#controls', options.controls.split(',').join('\n'));
    }
    await page.click('summary:has-text("Processing options")');
    await page.selectOption('#workers', String(options.workers));
    await capture('contribute-2-confirm');

    const started = Date.now();
    await page.click('button:has-text("Process locally")');
    await capture('contribute-processing');
    await page.waitForSelector('h2:has-text("Review what")', { timeout: 900_000 });
    summary.prep_millis = Date.now() - started;
    await capture('contribute-3-inspector');
    check((await page.locator('.payload-summary').textContent()).includes('No pixels, no location'), `${size}: upload summary explains privacy`);
    await page.locator('summary:has-text("See exactly")').focus();
    await page.keyboard.press('Enter');
    await capture('contribute-inspector-expanded');

    summary.timing = await page.evaluate(() => {
      const table = [...document.querySelectorAll('table.data-table')].find(
        (candidate) => candidate.querySelector('thead th')?.textContent === 'RAW',
      );
      return table
        ? [...table.querySelectorAll('tbody tr')].map((row) =>
            [...row.children].map((cell) => cell.textContent.trim()),
          )
        : [];
    });

    const [seedDownload] = await Promise.all([
      page.waitForEvent('download'),
      page.click('button:has-text("Export seed file")'),
    ]);
    summary.seeds = `${options.out}/seeds-${size}.json`;
    await seedDownload.saveAs(summary.seeds);
    const [payloadDownload] = await Promise.all([
      page.waitForEvent('download'),
      page.click('button:has-text("Download payload")'),
    ]);
    await payloadDownload.saveAs(`${options.out}/payload-${size}.json`);
    const beforeResume = await page.locator('.payload-summary').textContent();
    await page.reload();
    await page.waitForSelector('h2:has-text("Review what")');
    check(await page.locator('.payload-summary').textContent() === beforeResume, `${size}: local draft resumes prepared samples`);
    check(await page.locator('#recipe-target').inputValue() === 'sunlit-negative', `${size}: latest settings survive draft resume`);

    await page.bringToFront();
    await page.click('button:has-text("Continue to send")');
    await capture('contribute-send');
    await page.click('button:has-text("Send contribution")');
    await page.waitForSelector('dialog[open]');
    await page.click('dialog button:has-text("Create a profile")');
    await page.waitForSelector('text=Status link', { timeout: 300_000 });
    await capture('contribute-4-upload');
    check(await page.getByRole('button', { name: 'Back', exact: true }).isDisabled(), `${size}: completed upload cannot go back`);
    check(await page.getByRole('navigation', { name: 'Progress' }).getByRole('button', { name: 'Send', exact: true }).isDisabled(), `${size}: completed batch cannot reopen Send`);
    const link = await page.locator('a[href^="/status/"]').first().getAttribute('href');
    summary.status = link;
    check(Boolean(link), `${options.browser} ${size}: upload returned a status link`);
    check(problems.length === 0, `${options.browser} ${size}: no page errors during contribute`);

    await page.click('a[href^="/status/"]');
    await page.waitForSelector('h1:has-text("Your contributions")', { timeout: 60_000 });
    await page.waitForSelector('table.data-table', { timeout: 60_000 });
    await page.screenshot({ path: `${options.out}/status-${size}.png`, fullPage: true });
    summary.status_token = link.split('/').pop();
    return summary;
  } catch (error) {
    const state = await page
      .evaluate(() => ({
        notice: document.querySelector('.notice')?.textContent ?? null,
        error: document.querySelector('.error')?.textContent ?? null,
        focused: document.hasFocus(),
      }))
      .catch(() => ({}));
    await page.screenshot({ path: `${options.out}/contribute-failed-${size}.png`, fullPage: true }).catch(() => {});
    console.error(`${options.browser} ${size}: ${JSON.stringify(state)} ${problems.join(' | ')}`);
    throw error;
  } finally {
    await close(session);
    await patchHar(harPath, session.bodies);
  }
}

async function verifyContributionPayload(size, summary) {
  const log = `${options.out}/verify-payload-${size}.txt`;
  const state = `${options.out}/verify-state-${size}-${randomUUID()}.json`;
  const snapshot = `${options.out}/verification-data-${size}`;
  const response = await fetch(`${options.origin}/data/bodies/${options.body}.json`);
  if (!response.ok) throw new Error(`Cannot load verification camera: HTTP ${response.status}`);
  const { id, make, model, exif_aliases, kind, capabilities, status, mapping_status, schema } = await response.json();
  const entity = { id, make, model, exif_aliases, kind, capabilities, status, mapping_status };
  const filename = id.replace('/', '-');
  await mkdir(`${snapshot}/bodies`, { recursive: true });
  await writeFile(`${snapshot}/bodies/${filename}.body.yaml`, JSON.stringify(entity));
  await writeFile(`${snapshot}/bodies/${filename}.yaml`, schema);
  const args = [
    'verify-payload',
    '--seeds', summary.seeds,
    '--har', `${options.out}/session-${size}.har`,
    '--module', options['verify-module'],
    '--origin', options.origin,
    '--rp-id', new URL(options.origin).hostname,
    '--data', snapshot,
    '--kind', 'library',
    '--intent', 'native',
    '--pow-memory-kib', '8',
    '--pow-iterations', '1',
    '--pow-difficulty', '0',
    '--state', state,
    options.raw,
    options.jpeg,
  ];
  try {
    const output = execFileSync(options.verifier, args, { cwd: REPO_ROOT, encoding: 'utf8' });
    await writeFile(log, output);
    summary.verification = { log, state, snapshot };
    check(true, `${size}: restored upload passes verify-payload`);
  } catch (error) {
    const output = [error.stdout, error.stderr, error.message].filter(Boolean).map(String).join('\n');
    await writeFile(log, output);
    throw new Error(`${size}: restored upload failed verify-payload; see ${log}`, { cause: error });
  }
}

async function patchHar(path, bodies) {
  const har = JSON.parse(await readFile(path, 'utf8'));
  let patched = 0;
  for (const entry of har.log.entries) {
    const body = bodies.get(`${entry.request.method} ${entry.request.url}`);
    if (!body) {
      continue;
    }
    entry.request.postData = {
      mimeType: entry.request.postData?.mimeType ?? 'application/octet-stream',
      text: body,
      encoding: 'base64',
    };
    patched += 1;
  }
  await writeFile(path, JSON.stringify(har));
  console.log(`ok ${path}: ${patched} request bodies recorded`);
}

async function pages() {
  const seed = '32f39504de81195703a8418531a00420bec6efbd0edb4361ddc995f3e4640ee4';
  for (const [size, viewport] of Object.entries(VIEWPORTS)) {
    const session = await open(viewport);
    const { page } = session;
    if (session.authenticator) await session.authenticator.cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    try {
      await page.goto(`${options.origin}/help/passkeys`);
      await page.waitForSelector('h1');
      await page.screenshot({ path: `${options.out}/passkeys-${size}.png`, fullPage: true });

      await page.goto(`${options.origin}/pattern/${seed}`);
      await page.waitForSelector('canvas');
      await page.waitForTimeout(1000);
      await page.screenshot({ path: `${options.out}/pattern-${size}.png` });

      for (const route of ['/', '/looks', '/compare', '/match', '/cameras', '/contribute', '/contribute/wanted', '/me', '/developers', '/developers/pipelines', '/community', '/community/propose', '/community/log', '/review', '/about/how-it-works', '/about/privacy']) {
        await page.goto(`${options.origin}${route}`);
        if (route === '/review') await page.getByText('This area is for community moderators.', { exact: true }).waitFor();
        else await page.waitForSelector('h1');
        check(await page.locator('header.site-header').isVisible(), `${size}: ${route} has the shared shell`);
        check(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), `${size}: ${route} fits the viewport`);
      }
      await page.goto(`${options.origin}/looks`);
      await page.getByRole('button', { name: /^Simulated: Sunlit Negative/ }).click();
      await page.locator('main canvas').first().waitFor({ state: 'visible', timeout: 120_000 });
      check(await page.getByRole('slider', { name: 'Comparison divider' }).count() === 0, `${size}: a look opens as a single preview`);
      const overview = page.getByRole('tab', { name: 'Overview', exact: true });
      await overview.focus();
      await page.keyboard.press('ArrowRight');
      check(await page.getByRole('tab', { name: 'Measurements', exact: true }).getAttribute('aria-selected') === 'true', `${size}: arrow key opens measurements`);
      await page.keyboard.press('End');
      check(await page.getByRole('tab', { name: 'History', exact: true }).getAttribute('aria-selected') === 'true', `${size}: End opens history`);
      await page.keyboard.press('Home');
      check(await overview.getAttribute('aria-selected') === 'true', `${size}: Home returns to overview`);
      check(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), `${size}: look viewer fits the viewport`);
      await page.screenshot({ path: `${options.out}/look-viewer-${size}.png`, fullPage: true });
      const publishedVersion = await page.getByLabel('Published version').locator('option').first().getAttribute('value');
      await page.goto(`${options.origin}/looks/sunlit-negative?version=99.0.0`);
      await page.locator('main [role=alert]:visible').first().waitFor();
      check(await page.locator('main canvas:visible').evaluateAll(canvases => canvases.every(canvas => canvas.getContext('2d').getImageData(0, 0, 1, 1).data[3] === 0)), `${size}: an unavailable pinned version never shows the latest preview`);
      await page.getByRole('tab', { name: 'Measurements', exact: true }).click();
      await page.locator('[role=tabpanel] [role=alert]:visible').first().waitFor();
      await page.getByRole('tab', { name: 'History', exact: true }).click();
      await page.getByLabel('Published version').selectOption(publishedVersion);
      await page.waitForURL(url => url.searchParams.get('version') === publishedVersion);
      check(new URL(page.url()).searchParams.get('version') === publishedVersion, `${size}: History version selection updates the shareable URL`);
      await page.getByRole('tab', { name: 'Overview', exact: true }).click();
      await page.locator('main canvas:visible').first().waitFor();
      for (const theme of ['dark', 'light']) {
        await page.emulateMedia({ colorScheme: theme });
        for (const [name, route] of [['home', '/'], ['looks', '/looks'], ['look', '/looks/sunlit-negative'], ['cameras', '/cameras'], ['camera', '/cameras/fujifilm/x-t5']]) {
          await page.goto(`${options.origin}${route}`);
          await page.locator('main canvas').first().waitFor({ state: 'visible', timeout: 120_000 });
          for (let top = 0; top < await page.evaluate(() => document.documentElement.scrollHeight); top += viewport.height) {
            await page.evaluate(top => window.scrollTo(0, top), top);
            await page.waitForFunction(() => Array.from(document.querySelectorAll('main canvas')).filter(canvas => {
              const box = canvas.getBoundingClientRect();
              return box.width > 0 && box.height > 0 && box.bottom > 0 && box.top < innerHeight;
            }).every(canvas => canvas.getContext('2d').getImageData(0, 0, 1, 1).data[3] === 255), null, { timeout: 120_000 });
          }
          check(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), `${size}: ${theme} ${name} fits the viewport after photos load`);
          if (name === 'home' || name === 'looks') {
            check(await page.getByRole('button', { name: /Synthetic Match Case/ }).count() === 0, `${size}: ${name} excludes test fixture looks`);
          }
          if (name === 'home') {
            check(await page.getByRole('group', { name: 'Comparison view', exact: true }).count() === 0, `${size}: ${theme} home hides view-mode controls`);
            check(await page.getByRole('group', { name: 'Image surround', exact: true }).count() === 0, `${size}: ${theme} home hides surround controls`);
            check(await page.getByRole('slider', { name: 'Comparison divider', exact: true }).count() === 1, `${size}: ${theme} home retains the split divider`);
            const index = await (await page.request.get(`${options.origin}/data/index.json`)).json();
            const publicLooks = index.looks.filter(look => look.visibility === undefined || look.visibility === 'public');
            const baseline = index.reference_preview ? 'Neutral' : publicLooks[0].name;
            const initialLook = index.reference_preview ? publicLooks[0] : (publicLooks[1] ?? publicLooks[0]);
            const hero = page.getByRole('figure').filter({ has: page.getByRole('slider', { name: 'Comparison divider', exact: true }) });
            check(await hero.getByRole('img', { name: `Simulated: ${baseline}`, exact: true }).count() > 0, `${size}: ${theme} hero uses its published baseline or the older-index fallback`);
            check(await page.getByRole('button', { name: initialLook.name, exact: true }).getAttribute('aria-pressed') === 'true', `${size}: ${theme} hero starts with the expected right-hand look`);
            const chip = publicLooks.at(-1);
            await page.getByRole('button', { name: chip.name, exact: true }).click();
            await hero.getByRole('img', { name: `Simulated: ${chip.name}`, exact: true }).first().waitFor();
            check(await hero.getByRole('img', { name: `Simulated: ${baseline}`, exact: true }).count() > 0, `${size}: ${theme} switching chips retains the left baseline`);
            await page.getByRole('button', { name: initialLook.name, exact: true }).click();
            await hero.getByRole('img', { name: `Simulated: ${initialLook.name}`, exact: true }).first().waitFor();
          } else if (name === 'look') {
            check(await page.getByRole('group', { name: 'Image surround', exact: true }).count() === 1, `${size}: ${theme} look retains viewer controls`);
          }
          await page.screenshot({ path: `${options.out}/${name}-${theme}-${size}.png`, fullPage: true });
        }
      }
      for (const route of ['/bodies', '/mod', '/log', '/propose', '/passkeys', '/bounties', '/recipes', '/verify']) {
        await page.goto(`${options.origin}${route}`);
        await page.getByRole('heading', { name: 'Not found', exact: true }).waitFor();
      }
      await page.goto(`${options.origin}/`);
      const signIn = page.getByRole('button', { name: 'Sign in', exact: true });
      if (await signIn.count()) {
        await signIn.focus();
        await page.keyboard.press('Enter');
        await page.getByRole('dialog', { name: 'Sign in to looklab' }).waitFor();
        for (let step = 0; step < 8; step += 1) {
          check(await page.evaluate(() => Boolean(document.activeElement.closest('dialog'))), `${size}: Tab keeps focus in the sign-in dialog`);
          await page.keyboard.press('Tab');
        }
        await page.keyboard.press('Escape');
        check(!(await page.getByRole('dialog').isVisible()), `${size}: Escape closes sign-in`);
        check(await signIn.evaluate(element => document.activeElement === element), `${size}: sign-in restores focus`);
      }
      await page.screenshot({ path: `${options.out}/home-${size}.png`, fullPage: true });
      if (options.permalink) {
        await page.goto(`${options.origin}/match/r/${options.permalink}`);
        await page.waitForSelector('h1');
        await page.screenshot({ path: `${options.out}/match-result-${size}.png`, fullPage: true });
      }

      if (options.status) {
        await page.goto(`${options.origin}/status/${options.status}`);
        await page.screenshot({ path: `${options.out}/status-direct-${size}.png`, fullPage: true });
      }
      console.log(`ok ${options.browser} ${size}: pages captured`);
    } finally {
      await close(session);
    }
  }
}

async function flatness() {
  const directory = `${options.out}/many`;
  await mkdir(directory, { recursive: true });
  const files = [];
  for (let index = 0; index < options.count; index += 1) {
    const stem = `fujifilm-x-s10-${String(index).padStart(3, '0')}`;
    const raw = `${directory}/${stem}.raf`;
    const jpeg = `${directory}/${stem}.jpg`;
    await linkOrCopy(options.raw, raw);
    await linkOrCopy(options.jpeg, jpeg);
    files.push(raw, jpeg);
  }

  const session = await open(VIEWPORTS.desktop);
  const { page, problems } = session;
  try {
    await page.goto(`${options.origin}/contribute`);
    await page.setInputFiles('.drop-zone input[type=file]:not([webkitdirectory])', files);
    await page.getByRole('heading', { name: /^\d+ pairs? found$/ }).waitFor({ timeout: 600_000 });
    await page.fill('#body-id', options.body);
    await page.fill('#firmware', options.firmware);
    await page.fill('#sim', options.sim);
    await page.click('summary:has-text("Technical settings editor")');
    if (options.controls) {
      await page.fill('#controls', options.controls.split(',').join('\n'));
    }
    await page.click('summary:has-text("Processing options")');
    await page.selectOption('#workers', String(options.workers));
    await page.click('text=Process locally', { timeout: 600_000 });
    await page.waitForSelector('h2:has-text("Review what")', { timeout: 3_600_000 });
    await page.click('summary:has-text("See exactly")');
    await page.screenshot({ path: `${options.out}/flatness-${options.count}.png`, fullPage: true });

    const rows = await page.evaluate(() => {
      const table = [...document.querySelectorAll('table.data-table')].find(
        (candidate) => candidate.querySelector('thead th')?.textContent === 'RAW',
      );
      return table
        ? [...table.querySelectorAll('tbody tr')].map((row) =>
            [...row.children].map((cell) => cell.textContent.trim()),
          )
        : [];
    });
    const memory = rows.map((row) => Number.parseFloat(row[3]));
    const peak = Math.max(...memory);
    check(rows.length === options.count, `${options.count} files processed, ${rows.length} rows reported`);
    check(peak < 768, `peak worker memory ${peak} MiB stays under the 75% budget of 768 MiB`);
    check(problems.length === 0, `${options.browser}: no page errors during the ${options.count}-file run`);
    await writeFile(
      `${options.out}/flatness-${options.count}.json`,
      JSON.stringify({ rows, peak, min: Math.min(...memory) }, null, 2),
    );
    console.log(JSON.stringify({ rows, peak, min: Math.min(...memory) }, null, 2));
    return { rows: rows.length, peak, memory };
  } finally {
    await close(session);
  }
}

async function linkOrCopy(from, to) {
  try {
    await link(from, to);
  } catch {
    await copyFile(from, to);
  }
}

function validateSnapshot(directory) {
  const output = execFileSync(
    'cargo',
    ['run', '-q', '-p', 'looklab-cli', '--', 'validate-snapshot', directory],
    { cwd: REPO_ROOT, encoding: 'utf8', env: process.env },
  );
  console.log(output.trim());
  check(output.includes('valid snapshot'), `looklab validate-snapshot accepts ${directory}`);
}

async function propose(size) {
  const fixture = `${options.out}/fixture-${size}.raf`;
  await writeFile(fixture, `looklab fixture donation for ${size}\n`);
  const session = await open(VIEWPORTS[size]);
  const { page, problems } = session;
  try {
    if (session.authenticator) await session.authenticator.cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    await page.goto(`${options.origin}/review`);
    await page.getByText('This area is for community moderators.', { exact: true }).waitFor();
    check(await page.locator('header').getByRole('link', { name: /Review/ }).count() === 0, `${size}: visitor header has no review entry`);
    check(await page.locator('#mod-open-session').count() === 0, `${size}: visitor cannot open a moderator session`);
    await page.screenshot({ path: `${options.out}/review-visitor-${size}.png`, fullPage: true });
    await page.goto(`${options.origin}/community/propose`);
    await page.waitForSelector('#kind', { timeout: 60_000 });
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    const profileRequestPromise = page.waitForRequest((request) => new URL(request.url()).pathname === '/api/session', { timeout: 60_000 });
    await page.getByRole('button', { name: 'Create a profile', exact: true }).click();
    const profileRequest = await profileRequestPromise;
    const profileCredential = profileRequest.postDataJSON().credential_id;
    await page.locator('dialog').waitFor({ state: 'hidden', timeout: 60_000 });
    await page.selectOption('#kind', 'body');
    await page.setInputFiles('section[aria-labelledby="fixtures"] input[type=file]:not([webkitdirectory])', [fixture]);
    await page.waitForSelector('#donate-0', { timeout: 60_000 });
    await page.check('#donate-0');
    await page.screenshot({ path: `${options.out}/propose-${size}.png`, fullPage: true });
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.screenshot({ path: `${options.out}/propose-${size}-dark.png`, fullPage: true });
    await page.emulateMedia({ colorScheme: 'light' });
    check(await page.isChecked('#donate-0'), `${options.browser} ${size}: fixture consent is per file`);

    await page.click('#submit-proposal');
    await page.waitForSelector('#donation-confirm', { timeout: 60_000 });
    await page.screenshot({ path: `${options.out}/propose-confirm-${size}.png`, fullPage: true });
    check(
      (await page.locator('#donation-confirm').textContent()).includes(`fixture-${size}.raf`),
      `${options.browser} ${size}: the donation confirmation names the exact file`,
    );
    await page.click('#confirm-donation');
    await page.waitForSelector('#submitted-link', { timeout: 300_000 });
    const href = await page.locator('#submitted-link').getAttribute('href');
    check(Boolean(href), `${options.browser} ${size}: submission returned a proposal`);
    const id = href.split('/').pop();
    await page.click('#submitted-link');
    await page.waitForSelector('text=Evidence', { timeout: 60_000 });
    await page.screenshot({ path: `${options.out}/proposal-${size}.png`, fullPage: true });

    const grant = await page.request.post(`${options.origin}/api/__stub/moderator/${profileCredential}`);
    check(grant.ok(), `${size}: stub grants moderator to the test profile`);
    await page.locator('.account-menu summary').click();
    await page.getByRole('button', { name: 'Sign out', exact: true }).click();
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    await page.getByRole('button', { name: 'Continue with passkey', exact: true }).click();
    await page.locator('dialog').waitFor({ state: 'hidden', timeout: 60_000 });
    await page.goto(`${options.origin}/review`);
    await page.waitForSelector('#mod-open-session', { timeout: 60_000 });
    await page.route('**/api/mod/session', async (route) => {
      const response = await route.fetch();
      const reply = await response.json();
      await route.fulfill({ response, json: { ...reply, expires_in_seconds: 1 } });
    });
    await page.click('#mod-open-session');
    await page.waitForSelector('#mod-queue', { timeout: 60_000 });
    await page.waitForSelector('#mod-open-session', { timeout: 10_000 });
    check(await page.locator('#mod-queue').count() === 0, `${size}: expired moderator session returns to confirmation`);
    await page.unroute('**/api/mod/session');
    await page.click('#mod-open-session');
    await page.waitForSelector('#mod-queue', { timeout: 60_000 });
    await page.locator('[aria-label="Proposal queue, use j and k to navigate"]').focus();
    await page.keyboard.press('j');
    await page.waitForSelector('#mod-proposal', { timeout: 60_000 });
    await page.keyboard.press('k');
    await page.waitForSelector('#mod-proposal', { timeout: 60_000 });
    if (await page.locator('.review-back').isVisible()) await page.locator('.review-back').click();
    await page.click(`button[data-proposal="${id}"]`);
    await page.waitForSelector('#mod-proposal', { timeout: 60_000 });
    await page.screenshot({ path: `${options.out}/moderate-${size}.png`, fullPage: true });
    await page.emulateMedia({ colorScheme: 'dark' });
    await page.screenshot({ path: `${options.out}/moderate-${size}-dark.png`, fullPage: true });
    await page.emulateMedia({ colorScheme: 'light' });
    await page.focus('#mod-approve');
    await page.keyboard.press('Enter');
    await page.fill('#mod-reason', 'The payload validates and the donated fixture matches it.');
    check((await page.locator('#mod-log-preview').textContent()).includes(id), `${size}: decision preview identifies the proposal`);
    check((await page.locator('#mod-log-preview').textContent()).includes('approved'), `${size}: decision preview describes the decision`);
    await page.screenshot({ path: `${options.out}/decision-preview-${size}.png`, fullPage: true });
    await page.focus('#mod-confirm-decision');
    await page.keyboard.press('Enter');
    await page.waitForSelector('#mod-decision', { timeout: 300_000 });
    const version = await page.getAttribute('#mod-decision', 'data-snapshot-version');
    check(Boolean(version), `${options.browser} ${size}: approval wrote snapshot v${version}`);
    await page.screenshot({ path: `${options.out}/decision-${size}.png`, fullPage: true });
    await page.goto(`${options.origin}/community/log`);
    await page.locator('.activity-list').filter({ hasText: id }).first().waitFor({ timeout: 60_000 });
    check((await page.locator('main').textContent()).includes('The payload validates and the donated fixture matches it.'), `${size}: the public log contains the decision reason`);
    for (const colorScheme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme });
      for (const route of ['community', 'community/log']) {
        await page.goto(`${options.origin}/${route}`);
        await page.waitForSelector('.activity-list');
        if (route === 'community') {
          const activity = page.locator('.activity-list');
          check(!/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i.test(await activity.textContent()), `${size}: recent activity does not expose internal ids`);
          check(!/\d{4}-\d{2}-\d{2}T/.test(await activity.textContent()), `${size}: recent activity uses readable local dates`);
          check(await activity.getByRole('link', { name: /A moderator approved a proposal/ }).count() > 0, `${size}: decisions link to their proposals`);
        }
        await page.screenshot({ path: `${options.out}/${route.replaceAll('/', '-')}-${size}-${colorScheme}.png`, fullPage: true });
        check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${route} ${size} ${colorScheme}: no horizontal overflow`);
      }
    }

    const snapshotDir = `${options.state}/community/v${version}`;
    const exported = await readFile(`${snapshotDir}/fixtures/fixture-${size}.raf`, 'utf8');
    check(
      exported === `looklab fixture donation for ${size}\n`,
      `${options.browser} ${size}: the approved snapshot exports the donated file`,
    );
    const manifest = await readFile(`${snapshotDir}/fixtures/manifest.toml`, 'utf8');
    check(
      manifest.includes('committed = true'),
      `${options.browser} ${size}: the exported fixture is committed`,
    );
    check(
      !manifest.includes('source ='),
      `${options.browser} ${size}: the published manifest drops the private blob source`,
    );
    await writeFile(`${options.out}/proposal-errors-${size}.json`, JSON.stringify(problems, null, 2));
    check(problems.length === 0, `${options.browser} ${size}: no page errors during the proposal flow`);
    return { id, version };
  } finally {
    await close(session);
  }
}

async function developers() {
  const session = await open(VIEWPORTS.desktop);
  const { page } = session;
  try {
    const cdp = await page.context().newCDPSession(page);
    await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    await page.goto(`${options.origin}/developers`);
    await page.waitForSelector('#card-inspector input[type=file]', { state: 'attached', timeout: 120_000 });
    const vectors = `${REPO_ROOT}/data/card/conformance-v1`;
    const index = JSON.parse(await readFile(`${vectors}/index.json`, 'utf8'));
    for (const entry of index.files) {
      await page.locator('#card-inspector input[type=file]').setInputFiles(`${vectors}/${entry.path}`);
      await page.waitForFunction((name) => document.querySelector('#card-inspector .inspection-result p')?.textContent === name, entry.path.split('/').pop());
      const actual = await page.locator('[data-card-outcome]').getAttribute('data-card-outcome');
      check(actual === entry.outcome, `card ${entry.path}: ${entry.outcome}`);
    }
    await page.locator('#render-tag-checker summary').click();
    const tags = JSON.parse(await readFile(`${vectors}/render-tags.json`, 'utf8'));
    for (const vector of tags) {
      await page.locator('#render-tag-text').fill(vector.input);
      await page.getByRole('button', { name: 'Check render tag', exact: true }).click();
      await page.waitForSelector('[data-tag-outcome]:not([data-tag-outcome=checking])');
      const actual = await page.locator('[data-tag-outcome]').getAttribute('data-tag-outcome');
      check(vector.result.error ? actual !== 'valid' : actual === 'valid', `render tag ${vector.input}`);
      if (!vector.result.error) {
        const output = await page.locator('[data-tag-outcome]').textContent();
        for (const value of Object.values(vector.result)) check(output.includes(value), `render tag field ${value}`);
      }
    }
    await page.evaluate(() => {
      const original = File.prototype.arrayBuffer;
      globalThis.restoreFileReads = () => { File.prototype.arrayBuffer = original; };
      File.prototype.arrayBuffer = function () {
        if (!this.name.startsWith('delayed-')) return original.call(this);
        return new Promise((resolve, reject) => {
          globalThis.releaseFileRead = async () => {
            try { resolve(await original.call(this)); } catch (error) { reject(error); }
            await new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done)));
          };
        });
      };
    });
    await page.locator('#card-inspector input[type=file]').setInputFiles({
      name: 'delayed-card.llk', mimeType: 'application/octet-stream',
      buffer: await readFile(`${vectors}/valid/card-33.llk`),
    });
    await page.waitForFunction(() => typeof globalThis.releaseFileRead === 'function');
    await page.locator('#card-inspector input[type=file]').setInputFiles(`${vectors}/malformed/bad-magic.llk`);
    await page.waitForSelector('[data-card-outcome=rejected]');
    await page.evaluate(() => globalThis.releaseFileRead());
    check(await page.locator('[data-card-outcome]').getAttribute('data-card-outcome') === 'rejected', 'Late card read cannot replace the latest selection');
    await page.evaluate(() => { globalThis.releaseFileRead = null; });
    await page.locator('#render-tag-checker input[type=file]').setInputFiles({
      name: 'delayed-photo.jpg', mimeType: 'image/jpeg', buffer: Buffer.from([0xff, 0xd8, 0xff, 0xd9]),
    });
    await page.waitForFunction(() => typeof globalThis.releaseFileRead === 'function');
    await page.locator('#render-tag-text').fill('looklab-card/1 malformed');
    await page.getByRole('button', { name: 'Check render tag', exact: true }).click();
    await page.waitForSelector('[data-tag-outcome=malformed]');
    await page.evaluate(() => globalThis.releaseFileRead());
    check(await page.locator('[data-tag-outcome]').getAttribute('data-tag-outcome') === 'malformed', 'Late JPEG read cannot replace the latest render-tag input');
    await page.evaluate(() => globalThis.restoreFileReads());
    const [download] = await Promise.all([
      page.waitForEvent('download'),
      page.getByRole('link', { name: 'Download conformance vectors', exact: false }).click(),
    ]);
    await page.getByText('Download SHA-256', { exact: true }).click();
    const expectedArchiveHash = (await page.locator('.conformance-sha256').textContent()).trim();
    const { createHash } = await import('node:crypto');
    const archiveBytes = await readFile(await download.path());
    check(createHash('sha256').update(archiveBytes).digest('hex') === expectedArchiveHash, 'Downloaded conformance ZIP matches its published SHA-256');
    for (const [size, viewport] of Object.entries(VIEWPORTS)) {
      await page.setViewportSize(viewport);
      for (const colorScheme of ['dark', 'light']) {
        await page.emulateMedia({ colorScheme });
        for (const route of ['developers', 'developers/pipelines', 'about/how-it-works']) {
          await page.goto(`${options.origin}/${route}`);
          await page.waitForSelector('article.developers-page');
          check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${route}: no horizontal overflow at ${size}`);
          await page.screenshot({ path: `${options.out}/${route.replaceAll('/', '-')}-${size}-${colorScheme}.png`, fullPage: true });
        }
      }
    }
    const verificationPath = '**/api/cards/verification';
    await page.route(verificationPath, route => route.fulfill({
      json: { verifications: [{ firmware_id: 'test-firmware', firmware_version: '1.2.0', look: 'conformance-tone', look_version: '1.0.0', images: 24, median_de: 1.5, p95_de: 3.2 }] },
    }));
    for (const [size, viewport] of Object.entries(VIEWPORTS)) {
      await page.setViewportSize(viewport);
      await page.goto(`${options.origin}/developers`);
      await page.getByRole('rowheader', { name: 'test-firmware' }).waitFor();
      const listing = page.locator('#firmware-listings');
      check((await listing.textContent()).includes('1.2.0'), `${size}: firmware version appears`);
      check((await listing.textContent()).includes('24'), `${size}: verified photo count appears`);
      check((await listing.textContent()).includes('Nearly identical'), `${size}: measured closeness appears`);
      await listing.getByText('Measurements', { exact: true }).click();
      check((await listing.textContent()).includes('1.50'), `${size}: median measurement appears`);
      check((await listing.textContent()).includes('3.20'), `${size}: p95 measurement appears`);
      check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${size}: firmware table stays within page width`);
      await page.screenshot({ path: `${options.out}/developers-firmware-${size}.png`, fullPage: true });
    }
    await page.unroute(verificationPath);
    await page.route(verificationPath, route => route.fulfill({ json: { verifications: [] } }));
    await page.goto(`${options.origin}/developers`);
    await page.getByRole('heading', { name: 'No firmware listed yet', exact: true }).waitFor();
    await page.unroute(verificationPath);
    await page.route(verificationPath, route => route.fulfill({ status: 503, body: 'Unavailable' }));
    await page.goto(`${options.origin}/developers`);
    await page.locator('#firmware-listings [role=alert]').waitFor();
    check((await page.locator('#firmware-listings').textContent()).includes('Could not load listed firmware'), 'Unavailable firmware service shows an error, not an empty list');
    await page.unroute(verificationPath);
    return { cards: index.files.length, tags: tags.length };
  } finally {
    await close(session);
  }
}

async function compareViews() {
  const session = await open(VIEWPORTS.desktop);
  const { page } = session;
  const timing = [];
  try {
    if (options.browser === 'chromium') {
      const cdp = await page.context().newCDPSession(page);
      await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    }
    for (const [size, viewport] of Object.entries(VIEWPORTS)) {
      await page.setViewportSize(viewport);
      for (const colorScheme of ['dark', 'light']) {
        await page.emulateMedia({ colorScheme });
        await page.goto(`${options.origin}/compare?a=sunlit-negative&b=cold-steel`);
        const viewer = page.locator('.compare-page > .viewer');
        await viewer.waitFor({ timeout: 120_000 });
        const divider = viewer.getByRole('slider', { name: 'Comparison divider' });
        await divider.focus();
        await page.keyboard.press('Home');
        check(await divider.getAttribute('aria-valuenow') === '0', 'Compare divider Home');
        await page.keyboard.press('ArrowRight');
        check(await divider.getAttribute('aria-valuenow') === '1', 'Compare divider arrow key');
        await page.keyboard.press('End');
        check(await divider.getAttribute('aria-valuenow') === '100', 'Compare divider End');
        await page.keyboard.press('Home');
        const frame = await viewer.locator('.viewer-images').boundingBox();
        await page.mouse.move(frame.x + 1, frame.y + frame.height / 2);
        await page.mouse.down();
        await page.mouse.move(frame.x + frame.width / 2, frame.y + frame.height / 2);
        await page.mouse.up();
        check(Math.abs(Number(await divider.getAttribute('aria-valuenow')) - 50) < 3, 'Compare mouse drag');
        if (options.browser === 'chromium') {
          const cdp = await page.context().newCDPSession(page);
          const handle = await divider.boundingBox();
          const start = { x: handle.x + handle.width / 2, y: handle.y + handle.height / 2 };
          await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [start] });
          await cdp.send('Input.dispatchTouchEvent', { type: 'touchMove', touchPoints: [{ x: frame.x + frame.width * .7, y: start.y }] });
          await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] });
          check(Math.abs(Number(await divider.getAttribute('aria-valuenow')) - 70) < 3, 'Compare touch drag');
        }
        for (const [label, value] of [['Split', 'split'], ['Side by side', 'side-by-side'], ['Toggle', 'toggle'], ['Difference', 'difference']]) {
          await viewer.getByRole('button', { name: label, exact: true }).focus();
          await page.keyboard.press('Enter');
          await page.waitForFunction(view => new URL(location.href).searchParams.get('view') === view, value);
          check(new URL(page.url()).searchParams.get('view') === value, `Compare ${label} URL`);
          if (value === 'toggle') {
            await viewer.getByRole('button', { name: 'Switch look', exact: true }).click();
            check(await viewer.getByRole('img', { name: 'Simulated: Cold Steel', exact: true }).isVisible(), 'Compare toggle click shows second look');
            const hold = viewer.getByRole('button', { name: 'Hold to flip looks' });
            await hold.focus();
            await page.keyboard.down('Space');
            check(await viewer.getByRole('img', { name: 'Simulated: Sunlit Negative', exact: true }).isVisible(), 'Compare toggle hold flips the clicked look');
            await page.keyboard.up('Space');
            check(await viewer.getByRole('img', { name: 'Simulated: Cold Steel', exact: true }).isVisible(), 'Compare toggle release restores the clicked look');
          }
          if (value === 'difference') {
            check(await page.getByLabel('Difference legend').isVisible(), 'Compare difference legend');
            await viewer.locator('.difference-timing').filter({ hasText: 'Difference computed' }).waitFor({ timeout: 120_000 });
            timing.push({ size, colorScheme, text: await viewer.locator('.difference-timing').textContent() });
          }
          await page.getByRole('region', { name: 'Comparison summary', exact: true }).waitFor({ timeout: 120_000 });
          check(await page.locator('.compare-breakdown-row').count() === 5, 'Compare five-row summary');
          if (await page.getByText('Not enough measured data to rate this difference yet.', { exact: false }).count()) {
            check(await page.locator('.compare-summary meter').count() === 0, 'Compare unsupported data has no rating bars');
            check(await page.locator('.compare-summary .closeness').count() === 0, 'Compare unsupported data has no closeness label');
          }
          check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `Compare ${size} fits viewport`);
          await page.screenshot({ path: `${options.out}/compare-${value}-${size}-${colorScheme}.png`, fullPage: true });
          await page.reload();
          await page.locator('.compare-page > .viewer').waitFor({ timeout: 120_000 });
          check(await page.locator('.compare-page > .viewer').getByRole('button', { name: label, exact: true }).getAttribute('aria-pressed') === 'true', `Compare ${label} reload`);
        }
      }
    }
    return timing;
  } catch (error) {
    await page.screenshot({ path: `${options.out}/compare-failure.png`, fullPage: true });
    throw error;
  } finally {
    await close(session);
  }
}

async function account() {
  const session = await open(VIEWPORTS.desktop);
  const { page, context, browser: engineBrowser, problems } = session;
  const { cdp, authenticatorId } = session.authenticator;
  await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
  let second;
  try {
    await page.goto(`${options.origin}/me`);
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().focus();
    await page.keyboard.press('Enter');
    await page.getByRole('dialog').waitFor();
    await page.keyboard.press('Escape');
    check(await page.getByRole('dialog').count() === 0, 'Escape closes sign-in dialog');
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    for (const [size, viewport] of Object.entries(VIEWPORTS)) {
      await page.setViewportSize(viewport);
      for (const colorScheme of ['light', 'dark']) {
        await page.emulateMedia({ colorScheme });
        await page.getByRole('button', { name: 'Close dialog' }).focus();
        const focusOrder = [
          page.getByRole('button', { name: 'Continue with passkey', exact: true }),
          page.getByLabel('Display name (optional)'),
          page.getByRole('button', { name: 'Create a profile', exact: true }),
          page.getByRole('link', { name: 'How this works', exact: true }),
          page.getByRole('button', { name: 'Close dialog', exact: true }),
        ];
        for (const target of focusOrder) {
          await page.keyboard.press('Tab');
          check(await target.evaluate((element) => element === document.activeElement), 'Sign-in keyboard order follows returning sign-in, new profile, help, then Close');
        }
        check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `Sign-in dialog ${size} ${colorScheme}: no horizontal scroll`);
        await page.screenshot({ path: `${options.out}/account-dialog-${size}-${colorScheme}.png`, fullPage: true });
      }
    }
    await page.getByLabel('Display name (optional)').fill('River Photographer');
    const created = page.waitForResponse((response) => new URL(response.url()).pathname === '/api/session');
    const activation = page.waitForRequest((request) => request.method() === 'POST' && new URL(request.url()).pathname === '/api/session');
    await page.getByRole('button', { name: 'Create a profile', exact: true }).click();
    const firstProfile = await (await created).json();
    const activationRequest = (await activation).postDataJSON();
    check(Array.isArray(activationRequest.pow) && activationRequest.pow.length === 16, 'New profile activation sends the server-required work proof');
    await page.locator('dialog').waitFor({ state: 'hidden' });
    check(await page.locator('.account-menu summary').getAttribute('aria-label') === 'River Photographer', 'Optional display name reaches account menu');
    await page.locator('.account-menu summary').focus();
    await page.keyboard.press('Enter');
    check(await page.locator('.account-menu summary').getAttribute('aria-expanded') === 'true', 'Enter opens the account menu');
    await page.getByRole('link', { name: 'Your contributions', exact: true }).focus();
    await page.keyboard.press('Escape');
    check(await page.locator('.account-menu summary').getAttribute('aria-expanded') === 'false', 'Escape closes the account menu');
    check(await page.locator('.account-menu summary').evaluate((element) => element === document.activeElement), 'Escape returns focus to the account menu button');
    await page.keyboard.press('Space');
    check(await page.locator('.account-menu summary').getAttribute('aria-expanded') === 'true', 'Space opens the account menu');
    await page.keyboard.press('Escape');
    await page.locator('.account-menu summary').click();
    check(await page.getByRole('link', { name: 'Review', exact: true }).count() === 0, 'Ordinary profile has no Review link');
    await page.getByRole('button', { name: 'Sign out', exact: true }).click();
    await page.goto(`${options.origin}/status/${firstProfile.status_token}`);
    await page.waitForURL(`${options.origin}/me`);
    await page.getByRole('link', { name: 'Private status link to bookmark' }).waitFor();
    check(await page.locator('.account-menu summary').isVisible(), 'Private status link signs this browser in and opens /me');
    await page.locator('.account-menu summary').click();
    await page.getByRole('button', { name: 'Sign out', exact: true }).click();
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    const resumed = page.waitForResponse((response) => new URL(response.url()).pathname === '/api/session');
    await page.getByRole('button', { name: 'Continue with passkey', exact: true }).click();
    const resumedProfile = await (await resumed).json();
    await page.locator('dialog').waitFor({ state: 'hidden' });
    check((await page.request.get(`${options.origin}/status/${firstProfile.status_token}`)).ok(), 'Local sign-out preserves existing status link');
    const credentials = await cdp.send('WebAuthn.getCredentials', { authenticatorId });
    const credential = credentials.credentials[0];
    check(Boolean(credential), 'Profile stores a discoverable passkey');
    second = await engineBrowser.newContext({ viewport: VIEWPORTS.mobile });
    const other = await second.newPage();
    const otherCdp = await second.newCDPSession(other);
    await otherCdp.send('Network.setCacheDisabled', { cacheDisabled: true });
    await otherCdp.send('WebAuthn.enable');
    await otherCdp.send('Emulation.setFocusEmulationEnabled', { enabled: true });
    const otherAuthenticator = await otherCdp.send('WebAuthn.addVirtualAuthenticator', { options: { protocol: 'ctap2', transport: 'internal', hasResidentKey: true, hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true } });
    await otherCdp.send('WebAuthn.addCredential', { authenticatorId: otherAuthenticator.authenticatorId, credential });
    await other.goto(`${options.origin}/me`);
    await other.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    const otherResponse = other.waitForResponse((response) => new URL(response.url()).pathname === '/api/session');
    await other.getByRole('button', { name: 'Continue with passkey', exact: true }).click();
    const otherProfile = await (await otherResponse).json();
    await other.locator('dialog').waitFor({ state: 'hidden' });
    check((await page.request.get(`${options.origin}/status/${resumedProfile.status_token}`)).ok(), 'Second browser leaves first session signed in');
    check((await other.request.get(`${options.origin}/status/${otherProfile.status_token}`)).ok(), 'Second browser has its own valid session');
    // Virtual authenticators do not share signature counters.
    const updatedCredentials = await otherCdp.send('WebAuthn.getCredentials', { authenticatorId: otherAuthenticator.authenticatorId });
    await cdp.send('WebAuthn.removeCredential', { authenticatorId, credentialId: credential.credentialId });
    await cdp.send('WebAuthn.addCredential', { authenticatorId, credential: updatedCredentials.credentials[0] });
    const credentialId = Buffer.from(credential.credentialId, 'base64').toString('base64url');
    check((await page.request.post(`${options.origin}/api/__stub/moderator/${credentialId}`)).ok(), 'Stub moderator grant succeeds');
    await page.reload();
    await page.locator('.account-menu summary').waitFor();
    await page.locator('.account-menu summary').click();
    check(await page.getByRole('link', { name: 'Review', exact: true }).isVisible(), 'Reload refreshes newly granted roles without another passkey prompt');
    await page.locator('.account-menu summary').click();
    await page.locator('.account-menu summary').click();
    await page.getByRole('button', { name: 'Sign out', exact: true }).click();
    await page.getByRole('button', { name: 'Sign in', exact: true }).first().click();
    await page.getByRole('button', { name: 'Continue with passkey', exact: true }).click();
    await page.locator('dialog').waitFor({ state: 'hidden' });
    await page.locator('.account-menu summary').click();
    check(await page.getByRole('link', { name: 'Review', exact: true }).isVisible(), 'Moderator sees Review in account menu');
    for (const [size, viewport] of Object.entries(VIEWPORTS)) {
      await page.setViewportSize(viewport);
      for (const colorScheme of ['light', 'dark']) {
        await page.emulateMedia({ colorScheme });
        for (const route of ['me', 'help/passkeys', 'about/privacy']) {
          await page.goto(`${options.origin}/${route}`);
          await page.locator('h1').waitFor();
          check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${route} ${size} ${colorScheme}: no horizontal scroll`);
          await page.screenshot({ path: `${options.out}/account-${route.replaceAll('/', '-')}-${size}-${colorScheme}.png`, fullPage: true });
        }
      }
    }
    await page.locator('.account-menu summary').click();
    await page.getByRole('button', { name: 'Sign out everywhere', exact: true }).click();
    await page.locator('.header-account').waitFor();
    check(!(await other.request.get(`${options.origin}/status/${otherProfile.status_token}`)).ok(), 'Sign out everywhere revokes second browser');
    check(!(await page.request.get(`${options.origin}/status/${resumedProfile.status_token}`)).ok(), 'Sign out everywhere revokes first browser');
    await other.reload();
    await other.locator('.header-account').waitFor();
    check(problems.length === 0, `Account browser console: ${problems.join('; ')}`);
    return { created: true, signedInAgain: true, independentSessions: true, revokedEverywhere: true, moderatorMenu: true };
  } finally {
    await second?.close();
    await close(session);
  }
}

const result = { browser: options.browser, out: options.out };
if (options.flow === 'all' || options.flow === 'match') {
  result.permalink = await match('desktop');
  if (process.argv.includes('--expected') && options.expected) {
    const expected = (await readFile(options.expected, 'utf8')).trim();
    result.expected = expected;
    check(
      result.permalink === expected,
      `browser match equals the CLI permalink in ${options.expected}`,
    );
  }
  await match('mobile');
}
if (options.flow === 'all' || options.flow === 'contribute') {
  result.contribute = await contribute('desktop');
  await verifyContributionPayload('desktop', result.contribute);
  result.contribute_mobile = await contribute('mobile');
  await verifyContributionPayload('mobile', result.contribute_mobile);
}
if (options.flow === 'all' || options.flow === 'pages') {
  if (!options.permalink && result.permalink) {
    options.permalink = Buffer.from(result.permalink, 'utf8').toString('base64url');
  }
  await pages();
}
if (options.flow === 'all' || options.flow === 'propose') {
  result.proposals = [await propose('desktop'), await propose('mobile')];
  for (const entry of result.proposals) {
    validateSnapshot(`${options.state}/community/v${entry.version}`);
  }
}
if (options.flow === 'flatness') {
  result.flatness = await flatness();
}
if (options.flow === 'all' || options.flow === 'developers') {
  result.developers = await developers();
}
console.log(JSON.stringify(result, null, 2));
if (options.flow === 'compare') {
  console.log(JSON.stringify({ compare: await compareViews() }, null, 2));
}
if (options.flow === 'account') {
  console.log(JSON.stringify({ account: await account() }, null, 2));
}
