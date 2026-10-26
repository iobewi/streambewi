'use strict';
(() => {
  const $ = id => document.getElementById(id);
  let state = null, bootPrompted = false, rebooting = false, failures = 0, busy = false;
  const ns = 'http://www.w3.org/2000/svg';
  function icon(name, cls = 'line-icon') {
    const svg = document.createElementNS(ns, 'svg'); svg.setAttribute('class', cls); svg.setAttribute('aria-hidden', 'true');
    const use = document.createElementNS(ns, 'use'); use.setAttribute('href', '#' + name); svg.append(use); return svg;
  }
  function art(station, cls = 'tile-radio') {
    const fallback = icon('radio', cls);
    if (!/^https?:\/\//i.test(station.logo || '')) return fallback;
    const img = document.createElement('img'); img.alt = ''; img.src = station.logo; img.referrerPolicy = 'no-referrer'; img.loading = 'lazy';
    img.addEventListener('error', () => img.replaceWith(fallback), {once:true}); return img;
  }
  function message(id, text, error = false) { const el = $(id); el.textContent = text; el.classList.toggle('error', error); }
  function open(id) { const el = $(id); if (!el.open) el.showModal(); }
  document.querySelectorAll('[data-close]').forEach(b => b.addEventListener('click', () => b.closest('dialog').close()));
  document.querySelectorAll('dialog').forEach(d => d.addEventListener('click', e => { if (e.target === d) { const r=d.getBoundingClientRect(); if(e.clientX<r.left||e.clientX>r.right||e.clientY<r.top||e.clientY>r.bottom)d.close(); } }));
  $('open-wifi').onclick = () => { open('wifi-panel'); if (state && state.setup) scan(); };
  $('open-settings').onclick = () => open('settings-panel');
  $('add-shortcut').onclick = () => { open('settings-panel'); $('query').focus(); };
  $('show-password').onclick = () => { const p=$('password'), show=p.type==='password'; p.type=show?'text':'password'; $('show-password').textContent=show?'Masquer':'Voir'; $('show-password').setAttribute('aria-label',show?'Masquer le mot de passe':'Afficher le mot de passe'); };
  async function request(url, values) {
    const abort = new AbortController(), timer = setTimeout(()=>abort.abort(), 12000);
    try {
      const options = {cache:'no-store', signal:abort.signal};
      if (values !== undefined) { options.method='POST'; options.body=new URLSearchParams(values); }
      const r = await fetch(url, options), text=await r.text();
      let data; try { data=JSON.parse(text); } catch (_) { data={error:text}; }
      if (!r.ok) throw new Error(data.error || 'Réessayez dans un instant.');
      return data;
    } finally { clearTimeout(timer); }
  }
  async function mutate(url, values) {
    if (busy || rebooting) return;
    busy=true;
    try {
      const result=await request(url, values);
      if(result.restart) {
        rebooting=true; message('notice','Changement de station… Votre radio revient dans quelques instants.');
        message('settings-result','Votre choix est pris en compte. La radio change de station.');
        $('settings-panel').close(); $('play-label').textContent='Changement de station…';
      } else { message('settings-result','C’est enregistré !'); await refresh(); }
      return result;
    } catch(e) { message('settings-result',e.message,true); message('notice',e.message,true); }
    finally { busy=false; }
  }
  function renderCatalog(catalog) {
    const chosen=catalog.stations.find(s=>s.id===catalog.selected);
    if(!chosen)return;
    $('active-name').textContent=chosen.name; $('hero-art').replaceChildren(art(chosen));
    $('stations').replaceChildren(); $('manage-list').replaceChildren();
    catalog.stations.forEach(station=>{
      const tile=document.createElement('button'); tile.className='station'+(station.id===catalog.selected?' active':'');
      tile.setAttribute('aria-pressed',String(station.id===catalog.selected)); tile.setAttribute('aria-label','Choisir '+station.name);
      const name=document.createElement('span'); name.className='station-name'; name.textContent=station.name;
      tile.append(art(station), name);
      if(station.id===catalog.selected) { const tick=document.createElement('span'); tick.className='selected-tick'; tick.append(icon('check')); tile.append(tick); }
      tile.onclick=()=>mutate('/api/stations/select',{id:station.id}); $('stations').append(tile);
      const row=document.createElement('div'); row.className='favorite-row';
      const label=document.createElement('span'); label.className='row-name'; label.textContent=station.name;
      const edit=document.createElement('button'); edit.className='small-button'; edit.textContent='Modifier'; edit.setAttribute('aria-label','Modifier '+station.name);
      edit.onclick=()=>{ $('station-id').value=station.id; $('station-name').value=station.name; $('station-url').value=station.url; $('station-logo').value=station.logo; $('manual').open=true; $('station-name').focus(); };
      const remove=document.createElement('button'); remove.className='small-button danger'; remove.textContent='Retirer'; remove.setAttribute('aria-label','Retirer '+station.name); remove.disabled=catalog.stations.length===1;
      remove.onclick=()=>{ if(confirm('Retirer « '+station.name+' » de vos favoris ?')) mutate('/api/stations/delete',{id:station.id}); };
      row.append(art(station),label,edit,remove); $('manage-list').append(row);
    });
  }
  let lastCatalog='', lastNetworks='';
  function render(s) {
    const key=JSON.stringify(s.catalog); if(key!==lastCatalog) { lastCatalog=key;renderCatalog(s.catalog); }
    $('wifi-dot').classList.toggle('online',s.online);
    $('open-wifi').setAttribute('aria-label',s.online?'Wi-Fi connecté. Choisir un réseau':'Choisir le réseau Wi-Fi');
    $('wifi-current').textContent=s.online ? (s.ssid ? 'Connecté à '+s.ssid : 'Votre Wi-Fi est connecté.') : 'Choisissez le Wi-Fi de la maison.';
    $('play-state').classList.toggle('reading',s.reading&&s.online&&!rebooting);
    $('play-label').textContent=rebooting?'Changement de station…':!s.online?'Wi-Fi déconnecté':!s.ready?'La radio se prépare…':s.reading?'En lecture':'À l’arrêt';
    // Credentials are only accepted in the setup boot: in normal operation the panel is read-only.
    ['wifi-form','scan','networks'].forEach(id=>{$(id).hidden=!s.setup;});
    if(!s.setup)$('wifi-current').textContent+=' Pour changer de réseau : réseau StreamBeWI-Setup (bouton BOOT) ou Improv.';
    $('scan').disabled=s.scanning; $('scan').textContent=s.scanning?'Recherche…':'Rechercher les réseaux';
    $('connect').disabled=s.wifi_status===1;
    message('wifi-result',s.wifi_message,s.wifi_status===3||s.wifi_status===4);
    const networkKey=JSON.stringify([s.networks,s.ssid,s.online]);
    if(networkKey!==lastNetworks) {
    lastNetworks=networkKey;
    const list=$('networks'); list.replaceChildren();
    s.networks.forEach(network=>{
      const row=document.createElement('button');row.type='button';row.className='network-row';
      const text=document.createElement('span');text.textContent=network.ssid;
      const detail=document.createElement('small');detail.textContent=network.ssid===s.ssid&&s.online?'Connecté':network.secured?'Protégé':'Ouvert';text.append(detail);
      row.append(icon('wifi'),text); row.onclick=()=>{ $('ssid').value=network.ssid; if(!network.secured)$('password').value=''; $('password').focus(); };list.append(row);
    });
    }
    if(s.setup&&!s.online&&!bootPrompted) { bootPrompted=true;open('wifi-panel');scan(); }
  }
  let refreshBusy=false;
  async function refresh() {
    if(refreshBusy)return;refreshBusy=true;
    try {
      const next=await request('/api/state');
      if(rebooting&&!next.restarting) { rebooting=false;if(next.persisted===false)message('notice','Station active, mais non enregistrée : l’ancienne reviendra au redémarrage.',true);else message('notice','Votre station est choisie. Bonne écoute !'); }
      state=next;failures=0;render(next);renderLed(next.led);
    } catch(_) { failures++;if(failures>2&&!rebooting) { $('play-label').textContent='Connexion à votre radio…'; $('play-state').classList.remove('reading'); } }
    finally {refreshBusy=false;}
  }
  async function scan() { try { await request('/api/scan',{}); await refresh(); } catch(_) { message('wifi-result','Recherche indisponible. Vous pouvez saisir le nom du réseau.',true); } }
  $('scan').onclick=scan;
  $('wifi-form').addEventListener('submit',async e=>{
    e.preventDefault();message('wifi-result','Connexion en cours… Si le Wi-Fi change, rejoignez StreamBeWI-Setup puis rechargez cette page.');$('connect').disabled=true;
    try { await request('/wifi',new FormData(e.target)); await refresh(); }
    catch(_) { message('wifi-result','La connexion a peut-être changé. Rejoignez le réseau de la clé, puis rechargez cette page pour voir le résultat.'); }
  });
  $('clear-editor').onclick=()=>{ $('station-form').reset();$('station-id').value=''; };
  $('station-form').addEventListener('submit',async e=>{
    e.preventDefault();const value={name:$('station-name').value.trim(),url:$('station-url').value.trim(),logo:$('station-logo').value.trim()};
    if($('station-id').value)value.id=$('station-id').value;
    const result=await mutate('/api/stations/save',value);
    if(result&&!result.restart){e.target.reset();$('station-id').value='';}
  });
  let searchSequence=0;
  async function catalogFetch(server,query,signal) {
    const r=await fetch('https://'+server+'/json/stations/search?'+new URLSearchParams({name:query,hidebroken:'true',limit:'200',order:'clickcount',reverse:'true'}),{signal,credentials:'omit',referrerPolicy:'no-referrer'});
    if(!r.ok)throw new Error('catalogue');const list=await r.json();if(!Array.isArray(list))throw new Error('catalogue');return list;
  }
  $('search-form').addEventListener('submit',async e=>{
    e.preventDefault();const seq=++searchSequence,query=$('query').value.trim();if(!query)return;
    message('search-message','On cherche votre radio…');$('search-results').replaceChildren();
    const abort=new AbortController(),timer=setTimeout(()=>abort.abort(),14000);
    try {
      let found;
      for(const server of ['de1.api.radio-browser.info','nl1.api.radio-browser.info']) {
        try{found=await catalogFetch(server,query,abort.signal);break;}catch(e){if(abort.signal.aborted)throw e;}
      }
      if(!found)throw new Error('catalogue');if(seq!==searchSequence)return;
      const sorted=StreamCatalog.sort(found,12),compatible=sorted.compatible;
      message('search-message',StreamCatalog.summary(sorted));
      compatible.forEach(s=>{
        const row=document.createElement('div');row.className='result-row';const name=document.createElement('span');name.className='row-name';name.textContent=s.name;
        const add=document.createElement('button');add.className='small-button';add.textContent='Ajouter';
        add.onclick=async()=>{add.disabled=true;const r=await mutate('/api/stations/save',{name:s.name.slice(0,64),url:s.url_resolved,logo:/^https?:\/\//.test(s.favicon)&&s.favicon.length<=256?s.favicon:''});add.disabled=false;if(r){add.textContent='Ajoutée ✓';add.disabled=true;}};
        row.append(art({logo:s.favicon}),name,add);$('search-results').append(row);
      });
    } catch(_) {if(seq===searchSequence)message('search-message','Le catalogue a besoin d’Internet sur votre téléphone. Vos favoris restent disponibles ; vous pouvez aussi saisir une adresse.');}
    finally {clearTimeout(timer);}
  });
  async function diagLoad() {
    try {
      const [l, c] = await Promise.all(['/api/logs?n=32', '/api/logs/config'].map(u => fetch(u, {cache: 'no-store'}).then(r => r.json())));
      $('diag-log').textContent = l.lines.map(x => x.l + ' ' + x.m).join('\n') || 'Aucune ligne.';
      $('diag-meta').textContent = `${l.lines.length} lignes · ${l.overwritten} écrasées · ${l.withheld} masquées`;
      $('diag-policy').textContent = c.yaml + (c.source === 'default' ? '(politique par défaut)' : '');
      $('diag-form').hidden = !c.writable;
      if (c.writable && !$('diag-yaml').value) $('diag-yaml').value = c.yaml;
      message('diag-result', c.load_error ? 'La politique enregistrée est invalide : la politique par défaut s’applique.' : (c.writable ? '' : 'La politique se modifie seulement sur le réseau StreamBeWI-Setup.'), c.load_error);
    } catch (_) { message('diag-result', 'Diagnostic indisponible.', true); }
  }
  $('diag').addEventListener('toggle', () => { if ($('diag').open) diagLoad(); });
  $('diag-refresh').addEventListener('click', diagLoad);
  $('diag-form').addEventListener('submit', async e => {
    e.preventDefault();
    try {
      const r = await fetch('/api/logs/config', {method: 'PUT', headers: {'Content-Type': 'text/plain'}, body: $('diag-yaml').value});
      const j = await r.json();
      message('diag-result', j.ok ? 'Politique appliquée.' : j.error, !j.ok);
      if (j.ok) diagLoad();
    } catch (_) { message('diag-result', 'Envoi impossible.', true); }
  });
  // LED brightness. Moving the slider only previews (RAM, nothing is written, the firmware drops it after 60 s);
  // "Enregistrer" asks for a protected save, and "Enregistré" is shown only once /api/state says it is kept.
  const led={editing:false,sent:0,inFlight:false,timer:null,waiting:null};
  function ledValue(){return Number($('led-range').value);}
  function ledShow(v){$('led-range').value=v;$('led-value').textContent=v+' %';}
  async function ledSend(){
    led.timer=null;const v=ledValue();led.inFlight=true;led.sent=Date.now();
    try{await request('/api/led/preview',{percent:v});}catch(e){message('led-result',e.message,true);}
    finally{led.inFlight=false;if(ledValue()!==v)ledSoon();}
  }
  function ledSoon(){ // at most one request in flight, 150 ms apart, and the last value is always sent
    if(led.inFlight||led.timer)return;
    led.timer=setTimeout(ledSend,Math.max(0,150-(Date.now()-led.sent)));
  }
  async function ledCancel(){
    led.editing=false;led.waiting=null;if(led.timer){clearTimeout(led.timer);led.timer=null;}
    try{await request('/api/led/cancel',{});}catch(_){}
    message('led-result','');await refresh();
  }
  $('led-range').addEventListener('input',()=>{led.editing=true;led.waiting=null;$('led-value').textContent=ledValue()+' %';message('led-result','Pas encore enregistré.');$('led-save').disabled=false;ledSoon();});
  $('led-range').addEventListener('change',ledSoon);
  $('led-cancel').onclick=ledCancel;
  $('led-save').onclick=async()=>{
    const v=ledValue();$('led-save').disabled=true;message('led-result','Enregistrement… La musique peut s’interrompre un instant.');
    try{await request('/api/led/save',{percent:v});led.waiting=v;await refresh();}
    catch(e){message('led-result',e.message,true);$('led-save').disabled=false;}
  };
  $('settings-panel').addEventListener('close',()=>{if(led.editing)ledCancel();});
  function renderLed(info){
    const box=$('led-setting');box.hidden=!info||!info.available;if(box.hidden)return;
    if(led.waiting!==null){
      if(info.error){message('led-result',info.error+' Vous pouvez réessayer.',true);led.waiting=null;}
      else if(!info.busy&&info.saved===led.waiting){message('led-result','Enregistré !');led.waiting=null;led.editing=false;}
      else message('led-result','Enregistrement… La musique peut s’interrompre un instant.');
    }
    if(!led.editing)ledShow(info.preview===null?info.saved:info.preview);
    $('led-save').disabled=info.busy||!led.editing||(info.preview===null&&ledValue()===info.saved);
  }
  refresh();setInterval(refresh,2000);
})();
