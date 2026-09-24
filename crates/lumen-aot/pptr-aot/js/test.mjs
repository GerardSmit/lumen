import puppeteer from 'puppeteer-core';
// The script the pptr-aot binary carries (scratchpad test.mjs, plus a per-OS default browser).
if (process.env.PPTR_IMPORT_ONLY) {
  // Startup measurement: the whole module graph is loaded, linked and evaluated by now.
  console.log('imported', typeof puppeteer.launch);
  process.exit(0);
}
const defaultBrowser = {
  win32: 'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  darwin: '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  linux: '/usr/bin/google-chrome',
}[process.platform];
const executablePath = process.env.PPTR_EXE || defaultBrowser;
const t0 = Date.now();
const lap = (label) => console.log(`[${String(Date.now() - t0).padStart(5)}ms] ${label}`);
const browser = await puppeteer.launch({ executablePath, headless: true, args: ['--no-first-run'] , pipe: !!process.env.PPTR_PIPE});
lap('launch ' + (await browser.version()));
const page = await browser.newPage();
lap('newPage');
await page.setContent(`<html><body><h1 id="t">Hello lumen</h1>
<form><input id="name"><button id="go" type="button" onclick="document.getElementById('out').textContent='hi '+document.getElementById('name').value">Go</button></form>
<div id="out"></div><ul><li>a</li><li>b</li><li>c</li></ul></body></html>`);
lap('setContent');
const v = await page.evaluate(() => ({ sum: 1 + 2, ua: typeof navigator.userAgent, arr: [1, 'x', null], title: document.querySelector('#t').textContent }));
console.log('evaluate:', JSON.stringify(v));
const arg = await page.evaluate((a, b) => a * b, 6, 7);
console.log('evaluate args:', arg);
const h1 = await page.$eval('#t', (el) => el.textContent);
console.log('$eval:', h1);
const lis = await page.$$eval('li', (els) => els.map((e) => e.textContent));
console.log('$$eval:', JSON.stringify(lis));
await page.type('#name', 'world');
await page.click('#go');
const out = await page.$eval('#out', (el) => el.textContent);
console.log('after click:', out);
lap('interact');
await page.goto('data:text/html,<title>data url</title><p>x</p>');
console.log('title:', await page.title());
lap('goto');
const shot = await page.screenshot({ path: 'shot.png' });
console.log('screenshot bytes:', shot.length > 1000, shot[0], shot[1], shot[2], shot[3]);
lap('screenshot');
const pdf = await page.pdf();
console.log('pdf magic:', Buffer.from(pdf).subarray(0, 5).toString());
lap('pdf');
await browser.close();
lap('closed');
