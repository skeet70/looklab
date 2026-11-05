// SPDX-License-Identifier: AGPL-3.0-or-later
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';

const origin = process.env.LOOKLAB_PERF_ORIGIN ?? 'http://localhost:8082';
const out = process.env.LOOKLAB_SCENE_CHECK_OUT ?? '/tmp/looklab-scene-worker-check';
const root = (process.env.LOOKLAB_SCENE_ROOT ?? await readFile(new URL('../site-root.txt', import.meta.url), 'utf8')).trim();
if (!/^[0-9a-f]{64}$/.test(root)) throw new Error('Expected a SHA-256 site root in site/site-root.txt or LOOKLAB_SCENE_ROOT');
const { chromium } = await import(pathToFileURL(process.env.PLAYWRIGHT_MODULE).href);
await mkdir(out, { recursive: true });
const browser = await chromium.launch({ headless: true, args: ['--no-sandbox'] });
try {
    const page = await browser.newPage();
    const harnessUrl = `${origin}/__scene-worker-check`;
    await page.route(harnessUrl, route => route.fulfill({ contentType: 'text/html', body: '<!doctype html><title>Scene worker parity</title>' }));
    await page.goto(harnessUrl);
    const evidence = await page.evaluate(async ({ root }) => {
        const digest = async bytes => Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bytes)), byte => byte.toString(16).padStart(2, '0')).join('');
        const fetchBytes = async path => {
            const response = await fetch(new URL(path, location.origin));
            if (!response.ok) throw new Error(`${path}: HTTP ${response.status}`);
            return new Uint8Array(await response.arrayBuffer());
        };
        const verified = async (path, expectedHash, expectedLength) => {
            const bytes = await fetchBytes(path);
            const actual = await digest(bytes);
            if (actual !== expectedHash || (expectedLength !== undefined && bytes.length !== expectedLength)) {
                throw new Error(`${path}: integrity mismatch, SHA-256 ${actual}, ${bytes.length} bytes`);
            }
            return bytes;
        };
        const decodeJson = bytes => JSON.parse(new TextDecoder().decode(bytes));
        const index = decodeJson(await verified('/data/index.json', root));
        if (index.scenes.length !== 9 || new Set(index.scenes.map(scene => scene.name)).size !== 9) {
            throw new Error(`Expected nine distinct published scenes, got ${index.scenes.length}`);
        }
        const look = index.looks.find(entry => entry.visibility === 'public' && entry.versions.some(version => version.status === 'published'));
        if (!look) throw new Error('No public published Look for LUT verification');
        const version = look.versions.find(entry => entry.status === 'published');
        const manifest = decodeJson(await verified(version.manifest, version.manifest_sha256, version.manifest_bytes));
        const lutEntry = manifest.artifacts.find(entry => entry.name === 'preview.lut33');
        if (!lutEntry) throw new Error('Published Look has no preview.lut33');
        const lutPath = new URL(lutEntry.name, new URL(version.manifest, `${location.origin}/`)).pathname;
        const lut = await verified(lutPath, lutEntry.sha256, lutEntry.bytes);
        const moduleUrl = `${location.origin}/generated/wasm/looklab_scene_module.wasm`;
        const moduleBytes = await fetchBytes(moduleUrl);
        const uiModuleUrl = `${location.origin}/looklab-site.js`;
        const ui = await import(uiModuleUrl);
        await ui.default();
        if (typeof ui.looklab_prepare_scene_reference !== 'function' || typeof ui.looklab_render_scene_reference !== 'function') {
            throw new Error('The served UI module has no Rust scene parity reference exports');
        }
        const worker = new Worker(`${location.origin}/workers/scene-worker.js`);
        let nextId = 0;
        const request = message => new Promise((resolve, reject) => {
            const cleanup = () => {
                clearTimeout(timer);
                worker.removeEventListener('message', onMessage);
                worker.removeEventListener('error', onError);
                worker.removeEventListener('messageerror', onMessageError);
            };
            const onMessage = event => {
                const reply = event.data;
                if (message.type === 'init' ? reply.type !== 'ready' && reply.type !== 'error' : reply.id !== message.id) return;
                cleanup();
                if (reply.type === 'error') reject(new Error(reply.message));
                else if (message.type === 'init' || reply.type === 'result') resolve(reply);
                else reject(new Error(`Unexpected worker reply: ${reply.type}`));
            };
            const onError = event => { cleanup(); reject(new Error(event.message)); };
            const onMessageError = () => { cleanup(); reject(new Error('Scene worker reply could not be deserialized')); };
            const timer = setTimeout(() => { cleanup(); reject(new Error('Scene worker request timed out')); }, 120_000);
            worker.addEventListener('message', onMessage);
            worker.addEventListener('error', onError);
            worker.addEventListener('messageerror', onMessageError);
            worker.postMessage(message, message.bytes ? [message.bytes.buffer] : []);
        });
        const callWorker = async (fn, bytes) => new Uint8Array((await request({ type: 'call', id: ++nextId, fn, bytes })).bytes);
        const assertSame = (label, expected, actual) => {
            if (expected.length !== actual.length) throw new Error(`${label}: byte lengths differ, main ${expected.length}, worker ${actual.length}`);
            for (let offset = 0; offset < expected.length; offset++) {
                if (expected[offset] !== actual[offset]) throw new Error(`${label}: byte ${offset} differs, main ${expected[offset]}, worker ${actual[offset]}`);
            }
        };
        const renderRequest = prepared => {
            const bytes = new Uint8Array(4 + prepared.length + lut.length);
            new DataView(bytes.buffer).setUint32(0, prepared.length, true);
            bytes.set(prepared, 4);
            bytes.set(lut, 4 + prepared.length);
            return bytes;
        };
        const scenes = [];
        try {
            await request({ type: 'init', url: moduleUrl });
            for (const scene of index.scenes) {
                try {
                    const png = await verified(scene.path, scene.sha256, scene.bytes);
                    const mainPrepared = ui.looklab_prepare_scene_reference(png);
                    const mainRgba = ui.looklab_render_scene_reference(png, lut);
                    const workerPrepared = await callWorker(1, png);
                    assertSame(`${scene.name} prepared`, mainPrepared, workerPrepared);
                    const header = new DataView(mainPrepared.buffer, mainPrepared.byteOffset, mainPrepared.byteLength);
                    const width = header.getUint32(0, true);
                    const height = header.getUint32(4, true);
                    if (width !== scene.width || height !== scene.height || mainPrepared.length !== 8 + width * height * 32) {
                        throw new Error(`${scene.name}: prepared dimensions or wire length differ from verified index`);
                    }
                    const workerRgba = await callWorker(2, renderRequest(workerPrepared));
                    assertSame(`${scene.name} RGBA`, mainRgba, workerRgba);
                    if (mainRgba.length !== width * height * 4) throw new Error(`${scene.name}: incorrect RGBA byte length`);
                    scenes.push({ name: scene.name, status: 'passed', width, height, pngSha256: scene.sha256, prepared: { bytes: mainPrepared.length, sha256: await digest(mainPrepared), byteIdentical: true }, rgba: { bytes: mainRgba.length, sha256: await digest(mainRgba), byteIdentical: true } });
                } catch (error) {
                    scenes.push({ name: scene.name, status: 'failed', error: String(error.message ?? error) });
                }
            }
        } finally {
            worker.terminate();
        }
        return { siteRootSha256: root, moduleUrl, moduleSha256: await digest(moduleBytes), reference: { moduleUrl: uiModuleUrl, prepared: 'looklab_prepare_scene_reference', rgba: 'looklab_render_scene_reference', description: 'UI-compiled Rust PNG decode, scene preparation and rendering in Chromium main context' }, lut: { look: look.slug, version: version.version, path: lutPath, sha256: lutEntry.sha256, bytes: lut.length }, expectedSceneCount: 9, scenes };
    }, { root });
    const report = { origin, date: new Date().toISOString(), browser: browser.version(), ...evidence };
    await writeFile(`${out}/parity.json`, JSON.stringify(report, null, 2));
    console.log(JSON.stringify(report, null, 2));
    const failures = report.scenes.filter(scene => scene.status !== 'passed');
    if (report.scenes.length !== 9 || failures.length) throw new Error(`Scene worker parity failed: ${failures.length} failures across ${report.scenes.length}/9 scenes; see ${out}/parity.json`);
} finally {
    await browser.close();
}
