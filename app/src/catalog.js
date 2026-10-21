'use strict';
// Which Radio Browser entries StreamBeWI can play, and why not for the others. Pure: no DOM, no network,
// loaded before ui.js in the page and by tools/qual/check-catalog.cjs.
(root => {
  // What the firmware accepts as a stream URL (stations.rs `stream_url_valid`), checked here so that a result is
  // never offered that the device would refuse to save.
  const MAX_URL = 256;
  function firmwareAccepts(url) {
    if (typeof url !== 'string' || url.length > MAX_URL || /[\s\u0000-\u001f\u007f]/.test(url)) return false;
    const m = /^https?:\/\/([^/?#]*)(.*)$/.exec(url);
    return !!m && m[1] !== '' && !m[1].includes('@') && !url.slice(url.indexOf('//') + 2).includes('#');
  }
  const PLAYLIST = /\.(m3u8?|pls|asx|xspf)$/i;
  const pathOf = url => url.replace(/^https?:\/\/[^/?#]*/, '').split(/[?#]/)[0];

  // 'ok', or the first reason the entry is not playable: 'hls', 'codec', 'playlist' or 'url'.
  function verdict(s) {
    if (!s || typeof s !== 'object') return 'url';
    const url = typeof s.url_resolved === 'string' ? s.url_resolved : '';
    if (Number(s.hls) !== 0) return 'hls';
    if (String(s.codec == null ? '' : s.codec).trim().toUpperCase() !== 'MP3') return 'codec';
    if (!firmwareAccepts(url)) return 'url';
    if (PLAYLIST.test(pathOf(url))) return 'playlist';
    return 'ok';
  }

  // Split an API answer: the playable entries (one per URL, at most `max`) and how many distinct URLs were found
  // in a format that is not played.
  function sort(found, max) {
    const list = Array.isArray(found) ? found : [];
    const seen = new Set(), compatible = [], unsupported = new Set();
    for (const s of list) {
      const key = s && typeof s.url_resolved === 'string' ? s.url_resolved : null;
      if (key !== null && seen.has(key)) continue;
      if (key !== null) seen.add(key);
      if (verdict(s) === 'ok') compatible.push(s); else unsupported.add(key !== null ? key : s);
    }
    return { found: list.length, compatible: compatible.slice(0, max), compatibleCount: compatible.length, unsupported: unsupported.size };
  }

  // The sentence under the search field.
  function summary(r) {
    const other = r.unsupported;
    const plural = n => n > 1 ? 's' : '';
    if (r.found === 0) return 'Aucune station trouvée avec ce nom. Essayez un autre nom ou une adresse manuelle.';
    if (r.compatible.length === 0) {
      return other > 1
        ? `${other} stations trouvées, mais leur format n’est pas pris en charge (HLS, AAC ou liste de lecture). Essayez un autre nom ou une adresse manuelle.`
        : 'Station trouvée, mais son format n’est pas pris en charge (HLS, AAC ou liste de lecture). Essayez un autre nom ou une adresse manuelle.';
    }
    const base = 'Ajoutez vos coups de cœur à vos favoris.';
    return other ? `${base} ${other} autre${plural(other)} station${plural(other)} trouvée${plural(other)} dans un format non pris en charge.` : base;
  }

  const api = { firmwareAccepts, verdict, sort, summary, MAX_URL };
  if (typeof module === 'object' && module.exports) module.exports = api; else root.StreamCatalog = api;
})(typeof self !== 'undefined' ? self : this);
