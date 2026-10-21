// Radio Browser entries: which are offered, which are reported as unsupported (#26). Pure, no dependencies:
//   node tools/qual/check-catalog.cjs
const assert = require('node:assert/strict');
const C = require('../../app/src/catalog.js');
const e = (name, url, codec = 'MP3', hls = 0) => ({ name, url_resolved: url, codec, hls });

// What the directory returned for "Mon Petit France Inter" (de1, 2026-10-21): HLS playlists, codec UNKNOWN. The
// MP3 flux https://icecast.radiofrance.fr/monpetitfranceinter-midfi.mp3 is not listed under that name.
const mpfi = [
  e('Écoutez Mon petit France Inter', 'https://stream.radiofrance.fr/monpetitfranceinter/monpetitfranceinter_hifi.m3u8', 'UNKNOWN', 1),
  e('Mon Petit France Inter', 'https://stream.radiofrance.fr/monpetitfranceinter/monpetitfranceinter_hifi.m3u8', 'UNKNOWN', 1),
  e('Mon Petit France Inter', 'https://stream.radiofrance.fr/monpetitfranceinter/monpetitfranceinter_hifi.m3u8?id=radiofrance', 'UNKNOWN', 1),
];
let r = C.sort(mpfi, 12);
assert.deepEqual([r.found, r.compatible.length, r.unsupported], [3, 0, 2]);
assert.match(C.summary(r), /^2 stations trouvées, mais leur format n’est pas pris en charge/);

// Nothing found, an API that did not answer a list.
assert.match(C.summary(C.sort([], 12)), /^Aucune station trouvée/);
for (const bad of [null, undefined, {}, 'x', 3]) assert.equal(C.sort(bad, 12).found, 0);

// Plausible variants of the codec field are the same codec; others are not played, whatever they claim.
for (const codec of ['MP3', 'mp3', ' Mp3 ']) assert.equal(C.verdict(e('a', 'http://h/s', codec)), 'ok', codec);
for (const codec of ['AAC', 'AAC+', 'MP3+', 'OGG', 'UNKNOWN', '', null, 'MPEG']) {
  assert.equal(C.verdict(e('a', 'http://h/s', codec)), 'codec', String(codec));
}
assert.equal(C.verdict({ name: 'a', url_resolved: 'http://h/s', hls: 0 }), 'codec'); // no codec field
// HLS: the flag decides, in either spelling of the number; a missing flag is not trusted.
assert.equal(C.verdict(e('a', 'http://h/s', 'MP3', 1)), 'hls');
assert.equal(C.verdict(e('a', 'http://h/s', 'MP3', '1')), 'hls');
assert.equal(C.verdict(e('a', 'http://h/s', 'MP3', '0')), 'ok');
assert.equal(C.verdict({ name: 'a', url_resolved: 'http://h/s', codec: 'MP3' }), 'hls');
// Playlists are not direct streams, even when declared MP3.
for (const u of ['http://h/radio.m3u', 'https://h/radio.m3u8', 'http://h/a.pls?x=1', 'http://h/a.ASX', 'http://h/a.xspf']) {
  assert.equal(C.verdict(e('a', u)), 'playlist', u);
}
assert.equal(C.verdict(e('a', 'https://h/live.mp3?file=x.m3u8')), 'ok'); // only the path counts

// HTTP and HTTPS are both offered, kept exactly as given.
for (const u of ['http://h/s', 'https://h/s', 'https://h:8443/live.mp3?token=a&b=c', 'http://192.168.1.2:8000/']) {
  assert.equal(C.verdict(e('a', u)), 'ok', u);
}
// Never offered: what the firmware would refuse to save.
const long = 'https://h/' + 'a'.repeat(C.MAX_URL);
for (const u of [long, 'ftp://h/s', 'HTTP://h/s', 'rtmp://h/s', 'http://', 'http:///x', 'http://u:p@h/s', 'http://h/a b', 'http://h/a\nb', 'http://h/s#frag', '', null, 42]) {
  assert.equal(C.verdict(e('a', u)), 'url', String(u));
  assert.equal(C.firmwareAccepts(u), false, String(u));
}
assert.equal(C.firmwareAccepts('https://h/' + 'a'.repeat(C.MAX_URL - 10)), true);
assert.equal(C.firmwareAccepts('https://h/' + 'a'.repeat(C.MAX_URL - 9)), false); // 256 characters is the limit
assert.equal(C.verdict(null), 'url');

// A mixed answer: duplicates collapse, only playable entries are listed, the rest is counted once per URL.
const mixed = [
  e('A', 'https://h/a.mp3'), e('A bis', 'https://h/a.mp3'), e('B', 'http://h/b.mp3'),
  e('C aac', 'https://h/c.aac', 'AAC'), e('C aac bis', 'https://h/c.aac', 'AAC'), e('D hls', 'https://h/d.m3u8', 'UNKNOWN', 1),
];
r = C.sort(mixed, 12);
assert.deepEqual(r.compatible.map(s => s.name), ['A', 'B']);
assert.deepEqual([r.found, r.unsupported], [6, 2]);
assert.equal(C.summary(r), 'Ajoutez vos coups de cœur à vos favoris. 2 autres stations trouvées dans un format non pris en charge.');
assert.equal(C.summary(C.sort([e('A', 'http://h/a'), e('B', 'http://h/b.aac', 'AAC')], 12)),
  'Ajoutez vos coups de cœur à vos favoris. 1 autre station trouvée dans un format non pris en charge.');
assert.equal(C.summary(C.sort([e('A', 'http://h/a')], 12)), 'Ajoutez vos coups de cœur à vos favoris.');
assert.match(C.summary(C.sort([e('B', 'http://h/b.aac', 'AAC')], 12)), /^Station trouvée, mais son format/);

// Many results: unsupported entries ahead of the playable ones do not hide them, the list is capped after sorting.
const many = [];
for (let i = 0; i < 150; i++) many.push(e('aac' + i, `http://h/aac${i}`, 'AAC'));
for (let i = 0; i < 30; i++) many.push(e('mp3-' + i, `https://h/m${i}.mp3`));
r = C.sort(many, 12);
assert.equal(r.compatible.length, 12);
assert.equal(r.compatibleCount, 30);
assert.equal(r.compatible[0].name, 'mp3-0');
assert.equal(r.unsupported, 150);
console.log('catalog: ok');
