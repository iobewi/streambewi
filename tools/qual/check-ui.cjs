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
 let state={catalog:{version:1,selected:1,stations:[{id:1,name:'FIP',url:'http://example.org/fip',logo:''},{id:2,name:'France Inter',url:'http://example.org/inter',logo:''},{id:3,name:'France Culture',url:'http://example.org/culture',logo:''},{id:4,name:'Radio classique',url:'http://example.org/classique',logo:''}]},online:true,ssid:'Maison',ip:'192.168.1.42',networks:[{ssid:'Maison',strength:-40,secured:true},{ssid:'Livebox-Salon',strength:-60,secured:true}],scanning:false,setup:true,reading:true,ready:true,restarting:false,wifi_message:'',wifi_status:0};
 const actions=[];
 await page.route('http://streambewi.test/**',async route=>{
  const u=new URL(route.request().url()),p=u.pathname;
  if(p==='/api/state')return route.fulfill({json:state});
  if(p==='/api/logs')return route.fulfill({json:{lines:[{l:'I',m:'streambewi::stream: <b>net</b>: got IP 192.168.1.42/24'}],capacity:32,seen:3,overwritten:0,withheld:1}});
  if(p==='/api/logs/config'){
   if(route.request().method()==='PUT'){actions.push(['PUT',route.request().postData()]);return route.fulfill({json:{ok:true,generation:1}});}
   return route.fulfill({json:{yaml:'log:\n  default_level: info\n',source:'default',load_error:false,writable:state.setup,max_bytes:1024}});
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
  const file=path.join(root,p==='/'?'setup.html':p.slice(1));
  if(fs.existsSync(file))return route.fulfill({body:fs.readFileSync(file,'utf8').replace('<!--S-->',''),contentType:file.endsWith('.js')?'application/javascript':file.endsWith('.css')?'text/css':'text/html'});
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
 await browser.close();console.log('UI smoke PASS: home, responsive layout, Wi-Fi (setup boot) and read-only Wi-Fi panel (normal boot), catalogue filter/add, selection, safe text, logo fallback, diagnostic log/policy. APIs mocked.');
})().catch(e=>{console.error(e);process.exit(1)});
