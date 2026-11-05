// SPDX-License-Identifier: AGPL-3.0-or-later
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
import { gzipSync } from 'node:zlib';

const origin = process.env.LOOKLAB_PERF_ORIGIN ?? 'http://localhost:8082';
const out = process.env.LOOKLAB_PERF_OUT ?? '/tmp/looklab-first-photo';
const dist = process.env.LOOKLAB_PERF_DIST ?? '/tmp/looklab-release-dist';
const { chromium } = await import(pathToFileURL(process.env.PLAYWRIGHT_MODULE).href);
await mkdir(out, { recursive: true });
const browser = await chromium.launch({ headless: true, args: ['--no-sandbox'] });
try {
    const setup = await browser.newPage();
    await setup.goto(`${origin}/match?camera=fujifilm/x-t5&look=sunlit-negative`);
    const share = setup.getByRole('link', { name: 'Share link', exact: true });
    await share.waitFor({ timeout: 120_000 });
    const match = await share.getAttribute('href');
    await setup.close();
    const results = [];
    for (const [name, path] of [['Home', '/'], ['Look', '/looks/sunlit-negative'], ['Compare', '/compare?a=sunlit-negative&b=cold-steel'], ['Match result', match]]) {
        const context = await browser.newContext({ viewport: { width: 1365, height: 900 } });
        await context.addInitScript(() => {
            const timings = { wasm: [], canvases: [], longtasks: [] };
            window.looklabPhotoTimings = timings;
            if (!PerformanceObserver.supportedEntryTypes.includes('longtask')) {
                throw new Error('Chromium longtask PerformanceObserver support is required');
            }
            const recordLongtasks = entries => {
                timings.longtasks.push(...entries.map(entry => ({ start: entry.startTime, duration: entry.duration })));
            };
            const observer = new PerformanceObserver(list => recordLongtasks(list.getEntries()));
            observer.observe({ type: 'longtask', buffered: true });
            window.looklabFlushLongtasks = () => recordLongtasks(observer.takeRecords());
            const instantiate = WebAssembly.instantiateStreaming;
            WebAssembly.instantiateStreaming = async function (...args) {
                const start = performance.now();
                const result = await instantiate.apply(this, args);
                timings.wasm.push({ start, end: performance.now() });
                return result;
            };
            const put = CanvasRenderingContext2D.prototype.putImageData;
            CanvasRenderingContext2D.prototype.putImageData = function (...args) {
                const result = put.apply(this, args);
                const canvas = this.canvas;
                const entry = { submitted: performance.now(), width: canvas.width, height: canvas.height, paint: null };
                requestAnimationFrame(() => requestAnimationFrame(() => {
                    if (canvas.closest('.viewer') && canvas.getBoundingClientRect().width > 0) {
                        entry.paint = performance.now();
                        timings.canvases.push(entry);
                    }
                }));
                return result;
            };
        });
        const page = await context.newPage();
        const cdp = await context.newCDPSession(page);
        await cdp.send('Network.clearBrowserCache');
        await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
        if (process.env.LOOKLAB_PERF_TRACE === '1') {
            await cdp.send('Tracing.start', { categories: 'v8.wasm,disabled-by-default-v8.wasm.detailed,blink.user_timing,devtools.timeline', transferMode: 'ReturnAsStream' });
        }
        await page.goto(`${origin}${path}`, { waitUntil: 'domcontentloaded' });
        await page.waitForFunction(() => window.looklabPhotoTimings.canvases.some(entry => entry.paint !== null), null, { timeout: 120_000 });
        await page.waitForFunction(() => window.looklabPhotoTimings.canvases.some(entry => entry.width >= 1000 && entry.paint !== null), null, { timeout: 120_000 });
        const result = await page.evaluate(() => {
            window.looklabFlushLongtasks();
            const timings = window.looklabPhotoTimings;
            const firstPhotoMs = timings.canvases.find(entry => entry.paint !== null).paint;
            const fullPhotoMs = timings.canvases.find(entry => entry.width >= 1000 && entry.paint !== null).paint;
            const entries = timings.longtasks.filter(entry => entry.start < fullPhotoMs && entry.start + entry.duration > firstPhotoMs);
            return {
                timings,
                firstPhotoMs,
                fullPhotoMs,
                upgradeLongtasks: {
                    interval: 'first-photo-paint-to-full-scene-paint',
                    startMs: firstPhotoMs,
                    endMs: fullPhotoMs,
                    durationMs: fullPhotoMs - firstPhotoMs,
                    count: entries.length,
                    maxDurationMs: entries.reduce((max, entry) => Math.max(max, entry.duration), 0),
                    sumDurationMs: entries.reduce((sum, entry) => sum + entry.duration, 0),
                    overlapDurationMs: entries.reduce((sum, entry) => sum + Math.min(fullPhotoMs, entry.start + entry.duration) - Math.max(firstPhotoMs, entry.start), 0),
                    entries,
                },
                navigation: performance.getEntriesByType('navigation')[0].toJSON(),
                resources: performance.getEntriesByType('resource').map(entry => entry.toJSON()),
                measures: performance.getEntriesByType('measure').map(entry => entry.toJSON()),
            };
        });
        if (name === 'Home') {
            const started = await page.evaluate(() => performance.now());
            await page.getByRole('button', { name: 'Sunlit Negative', exact: true }).click();
            await page.waitForFunction(start => window.looklabPhotoTimings.canvases.some(entry => entry.submitted >= start), started, { timeout: 120_000 });
            result.switchMeasures = await page.evaluate(start => performance.getEntriesByName('looklab.lut.apply').filter(entry => entry.startTime >= start).map(entry => entry.toJSON()), started);
        }
        results.push({ name, path, ...result });
        await page.screenshot({ path: `${out}/${name.replaceAll(' ', '-').toLowerCase()}.png` });
        if (process.env.LOOKLAB_PERF_TRACE === '1') {
            const completed = new Promise(resolve => cdp.once('Tracing.tracingComplete', resolve));
            await cdp.send('Tracing.end');
            const { stream } = await completed;
            const chunks = [];
            for (;;) {
                const chunk = await cdp.send('IO.read', { handle: stream });
                chunks.push(chunk.base64Encoded ? Buffer.from(chunk.data, 'base64') : Buffer.from(chunk.data));
                if (chunk.eof) break;
            }
            await cdp.send('IO.close', { handle: stream });
            await writeFile(`${out}/${name.replaceAll(' ', '-').toLowerCase()}-trace.json`, Buffer.concat(chunks));
        }
        await context.close();
    }
    const wasm = await readFile(`${dist}/looklab-site_bg.wasm`);
    const report = { origin, date: new Date().toISOString(), initialWasmBytes: wasm.length, initialWasmGzipBytes: gzipSync(wasm).length, results };
    await writeFile(`${out}/timings.json`, JSON.stringify(report, null, 2));
    console.log(JSON.stringify({ initialWasmBytes: report.initialWasmBytes, initialWasmGzipBytes: report.initialWasmGzipBytes, pages: results.map(result => ({ name: result.name, firstPhotoMs: result.firstPhotoMs, fullPhotoMs: result.fullPhotoMs, upgradeLongtasks: result.upgradeLongtasks, wasm: result.timings.wasm, measures: result.measures, switchMeasures: result.switchMeasures })) }, null, 2));
} finally {
    await browser.close();
}
