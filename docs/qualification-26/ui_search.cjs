// Runs the real page (setup.html + catalog.js + ui.js) in jsdom against the real Radio Browser API and a stub device.
const { JSDOM } = require('jsdom');
const fs = require('fs'), path = require('path');
const src = path.resolve(__dirname, '../../app/src');
const queries = process.argv.slice(2);
const state = { catalog:{version:1,selected:1,stations:[{id:1,name:'Station test',url:'http://example.org/s',logo:''}]}, online:true, ssid:'x', ip:'192.168.1.17', networks:[], scanning:false, setup:false, reading:true, ready:true, restarting:false, wifi_message:'', wifi_status:0 };
(async () => {
  for (const q of queries) {
    const posts = [];
    const html = fs.readFileSync(path.join(src,'setup.html'),'utf8').replace('<!--S-->','')
      .replace(/<script src="\/catalog.js" defer><\/script><script src="\/ui.js" defer><\/script>/,'');
    const dom = new JSDOM(html, { url:'http://streambewi.test/', runScripts:'outside-only', pretendToBeVisual:true });
    const w = dom.window;
    w.HTMLDialogElement.prototype.showModal = function(){ this.open = true; };
    w.HTMLDialogElement.prototype.close = function(){ this.open = false; };
    w.fetch = async (url, o={}) => {
      url = String(url);
      if (url.startsWith('https://') && url.includes('.api.radio-browser.info/')) {
        const r = await fetch(url, {signal:o.signal}); const t = await r.text();
        return { ok:r.ok, status:r.status, text:async()=>t, json:async()=>JSON.parse(t) };
      }
      const body = o.body ? String(o.body) : null; if (o.method === 'POST') posts.push([url, body]);
      const json = url.startsWith('/api/state') ? state : { ok:true, restart:false };
      return { ok:true, status:200, text:async()=>JSON.stringify(json), json:async()=>json };
    };
    w.eval(fs.readFileSync(path.join(src,'catalog.js'),'utf8'));
    w.eval(fs.readFileSync(path.join(src,'ui.js'),'utf8'));
    await new Promise(r => setTimeout(r, 300));
    w.document.getElementById('query').value = q;
    w.document.getElementById('search-form').dispatchEvent(new w.Event('submit', {cancelable:true, bubbles:true}));
    for (let i = 0; i < 80; i++) { await new Promise(r => setTimeout(r, 250)); const m = w.document.getElementById('search-message').textContent; if (m && !m.startsWith('On cherche')) break; }
    const msg = w.document.getElementById('search-message').textContent;
    const rows = [...w.document.querySelectorAll('.result-row')].map(r => r.querySelector('.row-name').textContent);
    console.log(JSON.stringify({ query:q, message:msg, results:rows }));
    if (process.env.ADD && rows.length) {
      const idx = Number(process.env.ADD) || 0;
      w.document.querySelectorAll('.result-row')[idx].querySelector('button').click();
      await new Promise(r => setTimeout(r, 500));
      console.log(JSON.stringify({ added:rows[idx], posts }));
    }
    w.close();
  }
})();
