# Qualification fonctionnelle de la PR #27 (recherche Radio Browser, #26)

Banc : ESP32-S3, Metronic 477144 en hôte USB MSC, Wi-Fi 192.168.1.x. Image : release de `8674aa3`
(SHA-256 `4caf0df5…04ac1`, relue dans `factory` après le flash). Trace brute (SSID, MAC masqués) :
[`hardware-run.log`](hardware-run.log). Le défaut CSS de #28 n'a pas été touché.

## Limite principale de la méthode

La page de la carte est sur un réseau local inaccessible depuis le poste de test : **la recherche n'a pas été
cliquée dans un navigateur**. Le vrai code de la page (`setup.html`, `catalog.js`, `ui.js`) a été exécuté dans
jsdom ([`ui_search.cjs`](ui_search.cjs), `npm i jsdom`) contre le **vrai annuaire Radio Browser** et un faux
appareil ; les requêtes d'ajout envoyées à la carte sont **exactement celles que produit le bouton « Ajouter »**
(corps `name=…&url=…&logo=…`). Un essai à la main sur Firefox desktop reste à faire pour fermer ce point.

## Résultats

| # | Essai | Résultat |
|---|---|---|
| 1 | Chercher « Mon Petit France Inter » | « 2 stations trouvées, mais leur format n'est pas pris en charge (HLS, AAC ou liste de lecture). Essayez un autre nom ou une adresse manuelle. » |
| 2 | La station apparaît avec un flux MP3 HTTPS | **Non, et ce n'est pas un défaut du filtre** : l'annuaire ne l'expose qu'en HLS (`hls=1`, codec `UNKNOWN`, `.m3u8`). Son MP3 `https://icecast.radiofrance.fr/monpetitfranceinter-midfi.mp3` n'y est pas ; l'UI le dit au lieu de se taire. Ajout par adresse manuelle : voir ci-dessous. |
| 3 | Ajout aux favoris, persistance, redémarrage | **PASS** : « France Inter La musique d'Inter » (`https://icecast.radiofrance.fr/franceinterlamusiqueinter-midfi.mp3`, trouvée par la recherche « france inter ») ajoutée avec la requête du bouton, id 12 ; la carte a redémarré ensuite (relecture de config) et la station était là. « Adroit Jazz Underground » (`https://icecast.walmradio.com:8443/jazz`, trouvée par « jazz ») ajoutée, id 13. |
| 4 | Sélection, le flux HTTPS démarre | **PASS** : `tls up in 563 ms`, statut 200, chunked, disque revenu en 2,1 s, sans redémarrage. |
| 5 | MSC avec le Metronic | **PASS** : 4 min 30 s de lecture HTTPS ; `consumed` avance au débit du flux, avance du flux `lead=81920` à chaque minute. Sur toute la campagne, du repère `Q27-run` à la restauration (181 lignes de trace) : `Pending` 0, `ZEROFILL` 0, `Expired` 0, `stall` 0, panique 0. Dans les secondes qui suivent le repère l’hôte s’est ré-énuméré trois fois (`session end consumed=0`), puis la session est restée stable. |
| 6 | Recherche infructueuse, station incompatible | **PASS** : « zzzqqqxxkk » → « Aucune station trouvée avec ce nom… » ; « fip » → 12 flux et « 58 autres stations trouvées dans un format non pris en charge » ; MPFI → message « format non pris en charge » (ligne 1). |
| 7 | Stations HTTP existantes | **PASS** : FIP en HTTP (`tcp up in 50 ms`, disque revenu en 1,5 s) ; station HTTP à redirection 301 (`direct.franceinter.fr` → `icecast.radiofrance.fr`) suivie au démarrage. |

## Constats hors du périmètre de la PR (non corrigés ici)

1. **Le MP3 de « Mon Petit France Inter » n'a pas pu être ajouté** : le catalogue était à 8 stations (maximum),
   l'ajout est refusé (`Full`) sans autre trace côté carte. Comportement existant, non modifié.
2. **« Adroit Jazz Underground » (id 13, port 8443) ne se lit pas** : le TLS s'établit (`tls up in 1,34 s`, chaîne
   acceptée, `Host` avec le port) mais `stream: response head refused (malformed)`. La réponse a **26 en-têtes**
   (`icy-*`, `countrysubdivision*`…) et `iobewi-http-client::read_head` n'en analyse que 24 : tout flux à plus
   de 24 en-têtes est refusé. C'est une limite du socle (rust-mcu-libs), connue et documentée (« plus de 24
   en-têtes »), que cette recherche rend visible : une station proposée comme compatible peut quand même être
   refusée à la lecture. La carte retente alors toutes les ~3,5 s, avec une poignée de main de 1,3 s à chaque
   fois. Correction possible, dans une PR du socle : une capacité d'en-têtes plus grande, ou ignorer le surplus.
   Je n'ai pas élargi le périmètre de cette PR.
3. Le catalogue public ne garantit pas la lisibilité : le tri est syntaxique (codec, HLS, URL), il ne prouve pas
   que le flux se lit.

## Restauration

`factory` et `config` réécrits depuis les sauvegardes de cette campagne (prises avant le flash) puis relus :
`factory` `1dc6edfe…74cd` (version de la PR #25 livrée la veille), `config` `d7fc44a0…6166`, table de partitions
inchangée `0d6004dd…f272`. Le produit redémarre : IP, station HTTP à redirection, session MSC avec le Metronic.
Reset automatique du banc remis en marche.
