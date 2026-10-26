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
 const actions=[];
 // A device journal for the Live view: numbered lines, a start identifier, a window, a failure switch.
 const dev={boot:1001,total:0,window:32,cap:32,lines:[],fail:false,delay:0,inflight:0,maxInflight:0,reqs:[]};
 const devPush=(l,m)=>{dev.total++;dev.lines.push({seq:dev.total,l,m});while(dev.lines.length>dev.window)dev.lines.shift();};
 devPush('I','streambewi::stream: <b>net</b>: got IP 192.168.1.42/24');devPush('I','streambewi::stream: same');devPush('I','streambewi::stream: same');
 await page.route('http://streambewi.test/**',async route=>{
  const u=new URL(route.request().url()),p=u.pathname;
  if(p==='/api/state')return route.fulfill({json:state});
  if(p==='/api/logs'&&u.searchParams.has('after')){
   dev.reqs.push(u.search);dev.inflight++;dev.maxInflight=Math.max(dev.maxInflight,dev.inflight);
   if(dev.delay)await new Promise(r=>setTimeout(r,dev.delay));
   dev.inflight--;
   if(dev.fail)return route.fulfill({status:503,body:''});
   let after=Number(u.searchParams.get('after'));const boot=u.searchParams.get('boot');
   const reset=(boot!==null&&Number(boot)!==dev.boot)||after>dev.total;if(reset)after=0;
   const oldest=dev.total-dev.lines.length+1,lost=after>0&&after+1<oldest?oldest-after-1:0;
   const lines=dev.lines.filter(x=>x.seq>after).slice(0,dev.cap),next=lines.length?lines[lines.length-1].seq:after;
   return route.fulfill({json:{boot:dev.boot,lines,more:next<dev.total,next,lost,reset,up:10,capacity:32,seen:dev.total,overwritten:0,withheld:0}});
  }
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
 state.setup=false;
 {
 // Live journal: nothing is asked until it is switched on; each line once; pause freezes the display only;
 // clearing keeps the place; losses, restarts and failures are said; one request at a time; it stops with the view.
 const logText=()=>page.locator('#diag-log').textContent(),stateText=()=>page.locator('#live-state').textContent();
 const waitLog=(t,ms=6000)=>page.waitForFunction(x=>document.querySelector('#diag-log').textContent.includes(x),t,{timeout:ms});
 const count=(t,x)=>t.split(x).length-1;
 assert.equal(dev.reqs.length,0,'no request before Live is switched on');
 assert(await page.locator('#live-pause').isDisabled());assert(await page.locator('#live-export').isDisabled());
 await page.locator('#live-toggle').click();
 await waitLog('got IP');
 assert(dev.reqs[0].includes('after=0')&&!dev.reqs[0].includes('boot='));
 assert.equal(await stateText(),'En direct');assert.equal(await page.locator('#live-toggle').textContent(),'Arrêter le suivi');
 assert.equal(count(await logText(),'streambewi::stream: same'),2,'identical lines are both shown');
 devPush('W','x1: first');await waitLog('x1: first');
 assert(dev.reqs.some(r=>r.includes('after=3&boot=1001')),'the next request carries the cursor and the start identifier');
 await page.waitForTimeout(2300);assert.equal(count(await logText(),'x1: first'),1,'no duplicate');
 // Pause: the display stays, the collection goes on.
 await page.locator('#live-pause').click();devPush('E','y1: while paused');
 const before=dev.reqs.length;await page.waitForTimeout(2600);
 assert(dev.reqs.length>before,'still collecting');assert(!(await logText()).includes('y1'));
 assert((await stateText()).includes('1 nouvelle ligne en attente'));
 await page.locator('#live-pause').click();await waitLog('y1: while paused');
 // Clear keeps the place: old lines do not come back.
 await page.locator('#live-clear').click();assert.equal(await logText(),'Aucune ligne.');assert(await page.locator('#live-export').isDisabled());
 devPush('I','z1: after clear');await waitLog('z1: after clear');
 assert(!(await logText()).includes('x1'));
 // A catch-up when the answer says more is coming, without waiting for the next period.
 dev.cap=2;const t0=dev.reqs.length;for(let i=1;i<=5;i++)devPush('I','c'+i);
 await waitLog('c5',5000);assert(dev.reqs.length-t0>=3);dev.cap=32;
 // Stopped, the device goes on: coming back says how many lines the window lost.
 await page.locator('#live-toggle').click();await page.waitForTimeout(300);const stopped=dev.reqs.length;
 const cursor=dev.total;for(let i=1;i<=40;i++)devPush('I','g'+i);
 await page.waitForTimeout(2500);assert.equal(dev.reqs.length,stopped,'no request while stopped');
 await page.locator('#live-toggle').click();await waitLog('g40');
 const lostCount=(dev.total-dev.lines.length+1)-cursor-1;
 assert(lostCount>0&&(await logText()).includes('— '+lostCount+' lignes perdues —'));
 // The device started again: the identifier changes, the numbers start over.
 dev.boot=2002;dev.total=0;dev.lines=[];devPush('I','after restart one');devPush('I','after restart two');
 await waitLog('after restart two');assert((await logText()).includes('— redémarrage détecté —'));
 assert(dev.reqs.some(r=>r.includes('boot=1001')));
 // A failure is said once, and the view recovers.
 dev.fail=true;await page.waitForFunction(()=>document.querySelector('#live-state').textContent.includes('Connexion perdue'),null,{timeout:6000});
 await page.waitForTimeout(4500);assert.equal(count(await logText(),'connexion perdue'),1);
 dev.fail=false;devPush('I','back online');await waitLog('back online');assert.equal(await stateText(),'En direct');
 // Export: a text file made here, with the date, the identifier, the losses and the lines in order.
 const [download]=await Promise.all([page.waitForEvent('download'),page.locator('#live-export').click()]);
 assert(/^streambewi-journal-\d{8}-\d{6}\.txt$/.test(download.suggestedFilename()));
 const exported=fs.readFileSync(await download.path(),'utf8');
 for(const part of ['journal d’exploitation','Exporté le','Session de démarrage : 2002','perdues : '+lostCount,'redémarrages détectés : 1','pas d’heure propre','— redémarrage détecté —','I back online'])assert(exported.includes(part),part);
 assert(exported.indexOf('after restart one')<exported.indexOf('back online'));
 // One request at a time, whatever the latency.
 dev.delay=2500;await page.waitForTimeout(6500);dev.delay=0;assert.equal(dev.maxInflight,1);
 // It stops with the view: panel closed, page hidden; it starts again with the cursor.
 await page.evaluate(()=>document.querySelector('#settings-panel').close());await page.waitForTimeout(300);
 let n=dev.reqs.length;await page.waitForTimeout(2600);assert.equal(dev.reqs.length,n,'no request with the panel closed');
 await page.locator('#open-settings').click();
 await page.waitForTimeout(500);assert(dev.reqs.length>n,'back with the panel');
 await page.evaluate(()=>{Object.defineProperty(document,'hidden',{value:true,configurable:true});document.dispatchEvent(new Event('visibilitychange'));});
 await page.waitForTimeout(300);n=dev.reqs.length;await page.waitForTimeout(2600);assert.equal(dev.reqs.length,n,'no request with the page hidden');
 await page.evaluate(()=>{delete document.hidden;document.dispatchEvent(new Event('visibilitychange'));});
 await page.waitForTimeout(500);assert(dev.reqs.length>n);
 // The browser keeps 2000 lines and says what it let go, before the export.
 dev.window=3000;for(let i=1;i<=2100;i++)devPush('I','bulk'+i);dev.cap=3000;
 await waitLog('bulk2100',6000);
 assert((await stateText()).includes('écartée'));assert((await page.locator('#diag-log').textContent()).split('\n').length<=2000);
 const [big]=await Promise.all([page.waitForEvent('download'),page.locator('#live-export').click()]);
 assert(/écartées par le navigateur : [1-9]/.test(fs.readFileSync(await big.path(),'utf8')));
 await page.locator('#live-toggle').click();assert((await stateText()).startsWith('Suivi arrêté'));
 assert(!(await page.evaluate(()=>/live-/.test(document.querySelector('#diag-log').innerHTML))));
 }
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
 await browser.close();console.log('UI smoke PASS: home, Live journal (cursor, pause, clear, losses, restart, failure, catch-up, stop with the view, export, 2000-line cap), LED brightness (throttled preview, protected save shown as pending then kept, error, cancel), responsive layout, Wi-Fi (setup boot) and read-only Wi-Fi panel (normal boot), catalogue filter/add, selection, safe text, logo fallback, diagnostic log/policy. APIs mocked.');
})().catch(e=>{console.error(e);process.exit(1)});
