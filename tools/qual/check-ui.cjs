// Browser smoke test against mocked device APIs. No claim of hardware/network acceptance.
// Run: node tools/qual/check-ui.cjs (requires playwright + Chromium).
const { chromium } = require('playwright');
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const root=path.resolve(__dirname,'../../app/src');
(async()=>{
 const browser=await chromium.launch({headless:true,executablePath:process.env.CHROMIUM_EXECUTABLE});
 const page=await browser.newPage({viewport:{width:390,height:844},deviceScaleFactor:1});
 const errors=[];page.on('pageerror',e=>errors.push(e.message));
 let state={catalog:{version:1,selected:1,stations:[{id:1,name:'FIP',url:'http://example.org/fip',logo:''},{id:2,name:'France Inter',url:'http://example.org/inter',logo:''},{id:3,name:'France Culture',url:'http://example.org/culture',logo:''},{id:4,name:'Radio classique',url:'http://example.org/classique',logo:''}]},online:true,ssid:'Maison',ip:'192.168.1.42',networks:[{ssid:'Maison',strength:-40,secured:true},{ssid:'Livebox-Salon',strength:-60,secured:true}],scanning:false,setup:true,reading:true,ready:true,restarting:false,wifi_message:'',wifi_status:0,led:{available:false,saved:36,preview:null,applied:36,busy:false,error:null}};
 const actions=[];let debugUntil=0,debugLength=900000;
 await page.route('http://streambewi.test/**',async route=>{
  const u=new URL(route.request().url()),p=u.pathname;
  if(p==='/api/state')return route.fulfill({json:state});
  if(p==='/api/logs')return route.fulfill({json:{lines:[{l:'I',m:'streambewi::stream: <b>net</b>: got IP 192.168.1.42/24'}],capacity:32,seen:3,overwritten:0,withheld:1}});
  if(p==='/api/logs/config'){
   if(route.request().method()==='PUT'){actions.push(['PUT',route.request().postData()]);return route.fulfill({json:{ok:true,generation:1}});}
   return route.fulfill({json:{yaml:'log:\n  default_level: info\n',source:'default',load_error:false,writable:state.setup,max_bytes:1024}});
  }
  if(p.startsWith('/api/led/')){
   const v=Object.fromEntries(new URLSearchParams(route.request().postData()||'')),led=state.led;actions.push([p,v,Date.now()]);
   if(p.endsWith('/preview')){led.preview=Number(v.percent);led.applied=led.preview;}
   if(p.endsWith('/cancel')){led.preview=null;led.applied=led.saved;}
   if(p.endsWith('/save')){
    led.error=null;
    if(led.failNext){led.failNext=false;led.error='Écriture impossible : l’ancienne luminosité est conservée.';return route.fulfill({status:202,json:{ok:true,pending:true}});}
    led.busy=true;const percent=Number(v.percent);setTimeout(()=>{led.saved=percent;led.busy=false;led.preview=null;led.applied=percent;},700);
    return route.fulfill({status:202,json:{ok:true,pending:true}});
   }
   return route.fulfill({json:{ok:true}});
  }
  if(p==='/api/debug'){
   const m=route.request().method(),now=Date.now();
   if(m==='POST'){actions.push(['POST /api/debug',route.request().postData()]);if(!(debugUntil>now))debugUntil=now+debugLength;}
   if(m==='DELETE'){actions.push(['DELETE /api/debug']);debugUntil=0;}
   const on=debugUntil>now;
   return route.fulfill({json:{enabled:on,remaining_seconds:on?Math.ceil((debugUntil-now)/1000):0,duration_seconds:900,persistent:false,targets:on?['streambewi::stream']:[],partial:false}});
  }
  if(p==='/api/scan')return route.fulfill({status:202,body:'Recherche en cours'});
  if(p==='/wifi'&&!state.setup)return route.fulfill({status:403,body:'Refused outside the setup boot'});
  if(p==='/wifi'){ actions.push('wifi');state.wifi_message='Wi-Fi enregistré. Débranchez puis rebranchez StreamBeWI pour écouter la radio.';state.wifi_status=2;return route.fulfill({status:202,body:'En cours'}); }
  if(p.startsWith('/api/stations/')){
   const v=Object.fromEntries(new URLSearchParams(route.request().postData()));actions.push([p,v]);
   if(p.endsWith('/save'))state.catalog.stations.push({id:5,...v});
   if(p.endsWith('/select'))state.catalog.selected=Number(v.id);
   return route.fulfill({json:{ok:true,restart:false}});
  }
  // The page is one answer: setup.rs splices the three sources in where setup.html marks them.
  if(p==='/'){
   const src=f=>fs.readFileSync(path.join(root,f),'utf8');
   const body=src('setup.html').replace('<!--CSS-->',()=>src('ui.css')).replace('<!--S-->','').replace('<!--CATALOG-->',()=>src('catalog.js')).replace('<!--UI-->',()=>src('ui.js'));
   return route.fulfill({body,contentType:'text/html'});
  }
  return route.fulfill({status:404,body:'Not found'});
 });
 await page.route('https://*.api.radio-browser.info/**',route=>route.fulfill({json:[{name:'Radio test',url_resolved:'http://example.org/radio',favicon:'',codec:'MP3',hls:0},{name:'Radio TLS',url_resolved:'https://example.org/tls',favicon:'',codec:'MP3',hls:0},{name:'Radio AAC',url_resolved:'https://example.org/aac',favicon:'',codec:'AAC',hls:0},{name:'Radio HLS',url_resolved:'https://example.org/live.m3u8',favicon:'',codec:'UNKNOWN',hls:1}]}));
 await page.goto('http://streambewi.test/');
 await page.getByRole('button',{name:'Choisir FIP',exact:true}).waitFor();
 assert.equal(await page.locator('.station').count(),4);
 assert.equal(await page.locator('#play-label').textContent(),'En lecture');
 assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
 fs.mkdirSync('target/ui-check',{recursive:true});
 await page.screenshot({path:'target/ui-check/home-mobile.png',fullPage:true});
 await page.getByRole('button',{name:/Wi-Fi connecté/}).click();
 await page.getByRole('button',{name:/Livebox-Salon/}).click();
 assert.equal(await page.locator('#ssid').inputValue(),'Livebox-Salon');
 await page.locator('#password').fill('correct-password');await page.getByRole('button',{name:'Se connecter',exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('#wifi-result').textContent.includes('enregistré'));
 assert(actions.includes('wifi'));
 await page.screenshot({path:'target/ui-check/wifi-mobile.png',fullPage:true});
 await page.getByRole('button',{name:'Fermer le Wi-Fi',exact:true}).click();
 // Normal operation (not the setup boot): the Wi-Fi panel is read-only, the credentials form is gone.
 state.setup=false;await page.getByRole('button',{name:/Wi-Fi connecté/}).click();
 await page.waitForFunction(()=>document.querySelector('#wifi-form').hidden&&document.querySelector('#networks').hidden);
 assert((await page.locator('#wifi-current').textContent()).includes('StreamBeWI-Setup'));
 await page.getByRole('button',{name:'Fermer le Wi-Fi',exact:true}).click();
 await page.getByRole('button',{name:'Ouvrir les réglages'}).click();
 await page.locator('#query').fill('Radio test');await page.getByRole('button',{name:'Chercher',exact:true}).click();
 await page.getByRole('button',{name:'Ajouter',exact:true}).first().waitFor();
 // http and https MP3 are offered; AAC and HLS are counted, not offered as playable.
 assert.equal(await page.locator('.result-row').count(),2);
 assert((await page.locator('#search-message').textContent()).includes('2 autres stations trouvées dans un format non pris en charge'));
 await page.getByRole('button',{name:'Ajouter',exact:true}).nth(1).click();
 await page.getByRole('button',{name:'Ajoutée ✓',exact:true}).waitFor();
 assert(actions.some(a=>Array.isArray(a)&&a[0].endsWith('/save')&&a[1].url==='https://example.org/tls'));
 // Diagnostic: lines are text, the policy form exists only when the boot allows writing it.
 await page.locator('#diag summary').click();
 await page.waitForFunction(()=>document.querySelector('#diag-log').textContent.includes('got IP'));
 assert(await page.locator('#diag-log').textContent().then(t=>t.includes('<b>net</b>')));
 assert.equal(await page.locator('#diag-log b').count(),0);
 assert(await page.locator('#diag-form').isHidden());
 state.setup=true;await page.getByRole('button',{name:'Actualiser',exact:true}).click();
 await page.locator('#diag-form').waitFor({state:'visible'});
 await page.locator('#diag-yaml').fill('log:\n  default_level: warn\n');await page.getByRole('button',{name:'Appliquer',exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('#diag-result').textContent.includes('appliquée'));
 assert(actions.some(a=>a[0]==='PUT'&&a[1].includes('warn')));
 // Detailed diagnostic: the page asks, shows the time left and notices the end that the firmware decided.
 assert.equal(await page.locator('#debug-state').textContent(),'Désactivé');
 debugLength=2500;
 await page.getByRole('button',{name:'Activer pour 15 minutes',exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('#debug-state').textContent.startsWith('Actif'));
 assert.equal(await page.locator('#debug-toggle').textContent(),'Désactiver');
 await page.locator('#debug-toggle').scrollIntoViewIfNeeded();await page.screenshot({path:'target/ui-check/debug-mobile.png'});
 assert(actions.some(a=>Array.isArray(a)&&a[0]==='POST /api/debug'&&a[1]==='{"enabled":true}'));
 await page.waitForFunction(()=>document.querySelector('#debug-result').textContent.includes('terminé'),null,{timeout:9000});
 assert.equal(await page.locator('#debug-state').textContent(),'Désactivé');
 assert.equal(await page.locator('#debug-toggle').textContent(),'Activer pour 15 minutes');
 debugLength=900000;
 await page.getByRole('button',{name:'Activer pour 15 minutes',exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('#debug-state').textContent.includes('14 min'));
 await page.getByRole('button',{name:'Désactiver',exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('#debug-state').textContent==='Désactivé');
 assert(actions.some(a=>Array.isArray(a)&&a[0]==='DELETE /api/debug'));
 state.setup=false;
 // LED brightness: hidden without a LED; a preview is throttled and ends with the last value; "Enregistré" only once kept.
 await page.waitForFunction(()=>document.querySelector('#led-setting').hidden);
 state.led.available=true;await page.locator('#led-setting').waitFor({state:'visible'});
 assert.equal(await page.locator('#led-range').inputValue(),'36');assert.equal(await page.locator('#led-value').textContent(),'36 %');
 const previews=()=>actions.filter(a=>Array.isArray(a)&&a[0]==='/api/led/preview');
 const before=previews().length;
 for(let v=10;v<=90;v+=10){await page.locator('#led-range').evaluate((el,x)=>{el.value=x;el.dispatchEvent(new Event('input',{bubbles:true}));},v);await page.waitForTimeout(40);}
 await page.locator('#led-range').evaluate(el=>el.dispatchEvent(new Event('change',{bubbles:true})));
 await page.waitForFunction(()=>true);await page.waitForTimeout(500);
 const sent=previews().slice(before);
 assert(sent.length>=2&&sent.length<=6,'preview requests: '+sent.length);
 assert.equal(sent.at(-1)[1].percent,'90','the last value is always sent');
 for(let i=1;i<sent.length;i++)assert(sent[i][2]-sent[i-1][2]>=120,'previews are at least ~150 ms apart');
 assert.equal(await page.locator('#led-value').textContent(),'90 %');
 assert((await page.locator('#led-result').textContent()).includes('Pas encore enregistré'));
 // Save: pending, then kept. The page never says "Enregistré" before /api/state shows it.
 await page.getByRole('button',{name:'Enregistrer',exact:true}).last().click();
 await page.waitForFunction(()=>document.querySelector('#led-result').textContent.includes('Enregistrement'));
 assert(actions.some(a=>Array.isArray(a)&&a[0]==='/api/led/save'&&a[1].percent==='90'));
 assert(!(await page.locator('#led-result').textContent()).includes('Enregistré !'));
 await page.waitForFunction(()=>document.querySelector('#led-result').textContent==='Enregistré !',null,{timeout:6000});
 assert.equal(state.led.saved,90);
 // A failed save is told, and can be tried again.
 await page.locator('#led-range').evaluate(el=>{el.value=20;el.dispatchEvent(new Event('input',{bubbles:true}));});
 state.led.failNext=true;await page.getByRole('button',{name:'Enregistrer',exact:true}).last().click();
 await page.waitForFunction(()=>document.querySelector('#led-result').textContent.includes('Écriture impossible'),null,{timeout:6000});
 assert(await page.locator('#led-result').evaluate(el=>el.classList.contains('error')));
 assert.equal(state.led.saved,90,'the saved value is not the failed one');
 assert(await page.getByRole('button',{name:'Enregistrer',exact:true}).last().isEnabled());
 // Cancel: back to the saved value, the firmware is told.
 const cancels=()=>actions.filter(a=>Array.isArray(a)&&a[0]==='/api/led/cancel').length;
 const c0=cancels();await page.getByRole('button',{name:'Annuler',exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('#led-range').value==='90');
 assert.equal(cancels(),c0+1);
 // Closing the panel on a preview cancels it too.
 await page.locator('#led-range').evaluate(el=>{el.value=5;el.dispatchEvent(new Event('input',{bubbles:true}));});
 await page.waitForTimeout(400);
 assert(await page.getByRole('button',{name:'Enregistrer',exact:true}).last().isEnabled(),'Enregistrer is usable as soon as the slider moves');
 await page.screenshot({path:'target/ui-check/led-mobile.png',fullPage:true});
 await page.getByRole('button',{name:'Fermer les réglages',exact:true}).click();
 await page.waitForTimeout(300);assert.equal(cancels(),c0+2);
 await page.getByRole('button',{name:'Ouvrir les réglages'}).click();
 await page.screenshot({path:'target/ui-check/settings-mobile.png',fullPage:true});
 await page.getByRole('button',{name:'Fermer les réglages',exact:true}).click();
 await page.getByRole('button',{name:'Choisir France Inter',exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('#active-name').textContent==='France Inter');
 assert(actions.some(a=>Array.isArray(a)&&a[0].endsWith('/select')&&a[1].id==='2'));
 // Names are text, not HTML. Unavailable logos fall back without hiding the station name.
 state.catalog.stations[0].name='<img src=x onerror=alert(1)>';state.catalog.stations[0].logo='http://streambewi.test/missing.png';
 await page.waitForFunction(()=>document.querySelector('.station-name').textContent.includes('<img'));
 assert.equal(await page.locator('#stations img[src="x"]').count(),0);
 await page.setViewportSize({width:320,height:740});
 assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
 await page.setViewportSize({width:1100,height:850});
 await page.screenshot({path:'target/ui-check/home-desktop.png',fullPage:true});
 assert.deepEqual(errors,[]);
 await browser.close();console.log('UI smoke PASS: home, detailed diagnostic (on, time left, ended by the firmware, off), LED brightness (throttled preview, protected save shown as pending then kept, error, cancel), responsive layout, Wi-Fi (setup boot) and read-only Wi-Fi panel (normal boot), catalogue filter/add, selection, safe text, logo fallback, diagnostic log/policy. APIs mocked.');
})().catch(e=>{console.error(e);process.exit(1)});
