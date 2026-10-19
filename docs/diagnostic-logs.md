# Journal d'exploitation : consultation et politique persistée (#15)

## Ce que fait le produit

- Lit un **historique borné** des lignes de journal récentes, sans toucher au flux audio ni au MSC.
- Persiste la **politique de journalisation** (`iobewi-log-config`, YAML restreint) dans un `ConfigSpace` `log` de 1 024 octets, la relit au démarrage et la remplace à chaud, sans redémarrage.
- Affiche les deux dans la section **Diagnostic** des réglages de l'interface Web (la page d'accueil n'est pas modifiée).

Aucun moteur de filtrage n'est ajouté : le filtre reste `iobewi_log::LogPolicy`. Pas de WebSocket, pas de flux permanent, pas de nouvelle infrastructure réseau.

## API HTTP

Serveur existant (port 80, AP ou STA). Toutes les réponses sont en JSON, `Cache-Control: no-store`.

| Route | Accès | Rôle |
| --- | --- | --- |
| `GET /api/logs?n=16` | AP et LAN | `n` lignes récentes, 1 à 32 (16 par défaut), plus anciennes d'abord : `{"lines":[{"l":"I","m":"cible: message"}],"capacity":32,"seen":N,"overwritten":N,"withheld":N}` |
| `GET /api/logs/config` | AP et LAN | Politique active : `{"yaml":"…","source":"default\|persisted","load_error":false,"writable":false,"max_bytes":1024}` |
| `PUT /api/logs/config` | **AP seulement** (`StreamBeWI-Setup`) | Corps `text/plain` : le YAML restreint de `iobewi-log-config`, 1 024 octets au plus. `200 {"ok":true,"generation":N}` ; `400` YAML refusé ; `403` hors AP ; `413` trop long ; `500` écriture impossible. |

```yaml
log:
  default_level: info
  targets:
    streambewi::usb: debug
```

Niveaux : `off`, `error`, `warn`, `info`, `debug`, `trace`. Huit préfixes au plus. Le corps est lu dans un tampon fixe de 1 024 octets : une requête plus longue est rejetée avant toute allocation, y compris depuis le LAN.

## Modèle d'accès

Le serveur n'a pas d'authentification (voir « Interface radio ») : AP protégé par WPA2, usage local. Ce chantier n'ajoute pas de secret.

- **Lecture** : possible sur le LAN, parce que c'est le seul moyen de voir le journal pendant la lecture audio. Elle ne montre que des lignes assainies (ci-dessous).
- **Écriture** de la politique : refusée hors du boot de configuration (AP). La présence physique à la configuration reste requise, comme pour le Wi-Fi.

## Assainissement

Les lignes sont assainies **avant** d'entrer dans l'historique : aucun secret n'est conservé en RAM côté HTTP.

- Les cibles des propriétaires Wi-Fi (`iobewi_esp_wifi`, `iobewi_wifi_manager`, `iobewi_wifi_core`, `improv…`) sont retenues (compteur `withheld`).
- Toute ligne nommant `password`, `passwd`, `token`, `bearer`, `secret`, `authorization` ou `credential` (casse ignorée) est retenue.
- Après `ssid=`, le reste de la ligne est masqué (`<redacted>`).
- Les URL `http(s)://` sont réduites à `schéma://hôte[:port]/chemin` : identifiants, requête et fragment sont retirés (`?…`), le chemin est limité à 48 octets.

Limite : l'assainissement est lexical. Un secret placé dans le chemin d'une URL, ou une ligne qui ne nomme pas le mot-clé, n'est pas détecté. L'adresse IP de la clé reste visible (le client la connaît déjà).

## Fonctionnement et ressources

- `iobewi-log` garde un anneau de 24 lignes, destructif, qui **abandonne les nouvelles entrées une fois plein** ; personne ne le lisait. Une boucle (400 ms, une section critique courte, ni réseau ni flash) le vide dans un historique de 32 lignes × 160 octets (≈ 5,3 Ko de tas), qui écrase les plus anciennes. La lecture HTTP ne consomme rien.
- Réponse bornée : 16 lignes par défaut, 32 au plus. Elle est écrite directement en JSON dans **une seule allocation exacte** (`try_reserve_exact`), pendant un emprunt court de l'historique (aucun `await`), puis envoyée une fois l'emprunt rendu : ni arbre `serde_json::Value`, ni copie des lignes, ni agrandissement. Les caractères de contrôle sont remplacés par une espace à l'entrée de l'historique ; un octet stocké coûte donc au plus 2 octets échappé, et la réponse ne dépasse jamais `RESPONSE_MAX` = 10 912 octets (32 lignes de 160 octets toutes en guillemets). Typiquement 3 à 6 Ko. Si l'allocation échoue, la réponse est un **503 sans corps**, pas un arrêt du firmware. Les octets sont identiques à ceux de l'ancienne construction (test).
- Le serveur sert une connexion à la fois ; une requête de journal retarde l'interface Web, pas la tâche audio ni le MSC.
- Les compteurs `seen`, `overwritten` et `withheld` rendent le débordement visible.
- `LogConfig` est derrière un mutex asynchrone : un seul `replace` à la fois. La validation précède l'écriture, l'écriture précède l'application ; un YAML refusé ou une écriture en échec laisse la politique active inchangée.
- Au démarrage : politique absente = comportement actuel (aucune écriture, aucun formatage) ; politique illisible ou invalide = comportement actuel et `load_error: true`, le contenu stocké n'est pas écrasé.
- Le démarrage lit le `ConfigSpace` lui-même puis applique la politique décodée : `LogConfig::reload` n'expose pas la politique appliquée, et l'interface doit pouvoir la renvoyer. La liste des cibles `qual` (debug USB) appliquée par le firmware de qualification n'est donc pas reflétée par `GET /api/logs/config`.
- Le calcul d'admission du magasin EKV passe de 32 à 40 pages sur 48 (`boot_policy::SPACES`, test `spaces_fit_the_config_partition`). Aucun reformatage.
- Pile libre du firmware : 46 336 octets avant, 45 008 après (minimum exigé : 36 864). Le premier jet la faisait tomber à 39 384 : la tâche `product` (future du serveur) grossissait de 6,7 Ko ; `Diag` et son état sont maintenant sur le tas (`Box`), et la persistance d'un PUT est `Box::pin` (tas, le temps de la requête AP). Il y a donc ≈ 7 Ko de tas en plus (historique, états) et jusqu'à 6 Ko transitoires par lecture : à mesurer sur carte avec la sonde `qual` (tas de 80 Ko, pic mesuré avant ce chantier ≈ 60 Ko).

## Incident de qualification du 19 octobre 2026 (`GET /api/logs?n=32`)

Sur la carte, la première requête `GET /api/logs?n=32` a figé tout le firmware (console, HTTP, USB, son), sans message ; un reset EN l'a rétabli, configuration intacte. La première construction de la réponse copiait les lignes, bâtissait un arbre `serde_json::Value` (une `Map` et des `String` clonées par ligne), puis sérialisait.

- **Mesure sur hôte (allocateur compteur, 32 lignes pleines, réponse de 5 920 o)** : ancienne construction = 404 allocations, 76 148 o demandés, **pic de 67 932 o** ; nouvelle = 1 allocation, 5 920 o, pic 5 920 o. Le pic cible (pointeurs de 32 bits) est plus petit mais du même ordre de grandeur, face à 27 128 o de tas libre à la dernière sonde (minimum libre 18 868 o).
- **Cause : hypothèse la plus probable, non démontrée sur la carte.** Un échec d'allocation dans le gestionnaire de tas conduit à `handle_alloc_error`, donc à une panique, et le gestionnaire de panique du firmware boucle sans rien afficher : ce serait le gel observé. Aucune trace ne permet de l'affirmer.
- **Hypothèses examinées** : le décodage de `?n=` ne panique pas (test hôte, valeurs valides et invalides) ; l'historique n'utilise ni section critique ni verrou tenu à travers un `await` (`NoopRawMutex`, accès synchrones dans une seule tâche) ; les cadres de pile du nouveau chemin sont petits (un tableau de 32 `usize`). Un dépassement de pile ne peut pas être exclu par ces seuls éléments (marge mesurée 11,8 Ko avant la requête).
- **Ressources attendues** : `GET /api/logs` demande au plus 10,9 Ko de tas transitoires (typiquement 6 Ko) au lieu de ≈ 35 à 65 Ko ; `GET /api/logs/config` ≈ 1,1 Ko. La marge de pile n'est pas modifiée. À confirmer sur carte avec la sonde `qual` après nouvelle autorisation.

## Hors périmètre

La limitation de débit des lignes répétitives (dernier point de #15) n'est pas faite. `POST /wifi`, qui était accessible depuis le LAN, est réservé au boot de configuration depuis #23 : la politique « Wi-Fi seulement en AP » que ce modèle d'accès suppose est maintenant vraie.

## Validation

Tests hôte (`cargo test -p streambewi-core -p streambewi`) : réponse `GET /api/logs` (octets identiques à l'ancienne construction pour 0 à 40 lignes, JSON valide, échappement identique à `serde_json`, borne `RESPONSE_MAX`, une seule allocation exacte contre 404 avant), assainissement, historique borné et ordre, découpe UTF-8, politique absente, invalide ou illisible, refus hors AP, YAML invalide ou trop long, écriture en échec, persistance après rechargement, admission des espaces, vidage de l'anneau. `tools/qual/check-ui.cjs` couvre la section Diagnostic (Playwright, API simulées). Non exécuté : tout essai sur carte (persistance après redémarrage réel, requêtes pendant la lecture audio, mesure pile/tas).
