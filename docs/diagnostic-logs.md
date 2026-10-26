# Journal d'exploitation : consultation et politique persistée (#15)

## Ce que fait le produit

- Lit un **historique borné** des lignes de journal récentes, sans toucher au flux audio ni au MSC.
- Persiste la **politique de journalisation** (`iobewi-log-config`, YAML restreint) dans un `ConfigSpace` `log` de 1 024 octets, la relit au démarrage et la remplace à chaud, sans redémarrage.
- Affiche les deux dans la section **Diagnostic** des réglages de l'interface Web (la page d'accueil n'est pas modifiée).
- Offre un **diagnostic détaillé temporaire** (15 minutes, en RAM seulement) pour qu'une personne non technicienne puisse obtenir plus de détails sans YAML ni passage en mode AP (voir plus bas).
- Espace les lignes répétitives (relances du flux, secteurs `ZEROFILL`) pour qu'un incident ne chasse pas sa cause de l'historique de 32 lignes.

Aucun moteur de filtrage n'est ajouté : le filtre reste `iobewi_log::LogPolicy`. Pas de WebSocket, pas de flux permanent, pas de nouvelle infrastructure réseau.

## API HTTP

Serveur existant (port 80, AP ou STA). Toutes les réponses sont en JSON, `Cache-Control: no-store`.

| Route | Accès | Rôle |
| --- | --- | --- |
| `GET /api/logs?n=16` | AP et LAN | `n` lignes récentes, 1 à 32 (16 par défaut), plus anciennes d'abord : `{"lines":[{"l":"I","m":"cible: message"}],"capacity":32,"seen":N,"overwritten":N,"withheld":N}` |
| `GET /api/logs/config` | AP et LAN | Politique active : `{"yaml":"…","source":"default\|persisted","load_error":false,"writable":false,"max_bytes":1024}` |
| `GET /api/debug` | AP et LAN | État du diagnostic détaillé : `{"enabled":bool,"remaining_seconds":N,"duration_seconds":900,"persistent":false,"targets":[…],"partial":bool}` |
| `POST /api/debug` | AP et LAN | Corps JSON `{"enabled":true}` : active pour 15 minutes ; `{"enabled":false}` : désactive. Même réponse que `GET`. `400` pour tout autre corps, `413` s'il dépasse 64 octets |
| `DELETE /api/debug` | AP et LAN | Désactive (idempotent), même réponse que `GET` |
| `PUT /api/logs/config` | **AP seulement** (`StreamBeWI-Setup`) | Corps `text/plain` : le YAML restreint de `iobewi-log-config`, 1 024 octets au plus. `200 {"ok":true,"generation":N}` ; `400` YAML refusé ; `403` hors AP ; `413` trop long ; `500` écriture impossible. |

```yaml
log:
  default_level: info
  targets:
    streambewi::usb: debug
```

Niveaux : `off`, `error`, `warn`, `info`, `debug`, `trace`. Huit préfixes au plus. Le corps est lu dans un tampon fixe de 1 024 octets : une requête plus longue est rejetée avant toute allocation, y compris depuis le LAN.

## Diagnostic détaillé temporaire

Un mode **strictement volatil** : il ne modifie jamais le YAML persisté (espace `log`), n'écrit ni la flash ni Config-Space, ne détache pas le disque USB, ne redémarre rien et ne change ni la station ni la connexion radio. Il n'existe que dans la RAM : un redémarrage l'efface.

- **Profil fixe.** Pendant la session, les cibles `streambewi::stream`, `streambewi::usb`, `streambewi::status` et `streambewi::provisioning` passent au niveau `debug` (jamais `trace`). Aucune cible Wi-Fi ou tierce, aucun choix libre dans la requête. Les lignes `debug!` du produit sont rares (début d'une tentative de connexion, fenêtre de sauvegarde de la luminosité, début d'une récupération de calage USB, tour du gestionnaire Wi-Fi…) et n'ont ni URL, ni SSID, ni secret ; la console UART0 est écrite de façon **bloquante**, donc ce niveau doit rester épars.
- **Durée fixe de 15 minutes** à partir de la première activation, **sans prolongation** : un `POST` pendant une session renvoie l'état et ne bouge pas l'échéance. Après la désactivation ou l'expiration, un nouveau `POST` ouvre une nouvelle session. Le micrologiciel tient l'échéance : la boucle de vidage de l'historique (toutes les 400 ms, déjà là) y regarde l'heure ; aucune tâche ni minuteur n'est ajouté. Fermer la page ne désactive rien. La résolution de l'expiration est de 400 ms.
- **Politique de base.** `Diag` garde la politique qui s'applique hors diagnostic : la politique persistée, sinon celle que la cible a appliquée au démarrage (`logging::remember_applied`, par exemple les traces USB d'une image `qual`), sinon la politique par défaut d'`iobewi-log`. L'activation applique une **copie** de cette base avec les quatre cibles relevées ; la désactivation ou l'expiration réapplique **exactement** la base, réglages personnalisés compris (`iobewi-log` n'expose pas la politique courante, d'où cette copie).
- **Instantané du flux.** Pendant une réception active, la boucle de réception émet au plus une ligne `debug` toutes les 15 secondes, cible `streambewi::stream` : `stream: snapshot live=… consumed=… lead=… failures_in_a_row=…`. Ce sont les compteurs déjà lus par le battement `info` de 60 s et le compteur de défaillances ; le niveau est testé (`log::log_enabled!`) avant toute lecture, donc hors diagnostic détaillé il n'y a ni formatage ni ligne. Ni tâche, ni allocation, ni tampon, ni écriture flash, ni effet sur le MSC ou sur les temporisations ; la cadence est comptée en réception (aucune ligne pendant une relance ou un arrêt du flux). C'est la trace qui prouve, en lecture stable, que le diagnostic détaillé produit bien du `debug`.
- **Limite de 8 règles.** Une `LogPolicy` contient au plus 8 règles. Les règles de la base sont conservées ; les cibles du profil qui ne tiennent pas ne sont **pas** relevées, et `GET /api/debug` le dit (`targets` ne liste que celles réellement activées, `partial` vaut `true`). Une règle de la base pour une cible du profil est relevée, pas dupliquée.
- **Politique remplacée pendant une session** (seulement possible en mode AP) : la nouvelle politique devient la base, le profil est réappliqué par-dessus, et la fin de session rétablit la nouvelle base. Un remplacement refusé ne change rien.

**Sécurité.** Cette API est accessible depuis le LAN **sans authentification applicative**, comme le reste du serveur : *tout appareil du réseau local peut activer ce diagnostic temporaire*. C'est voulu (le mode AP, seul à pouvoir écrire une politique, oblige à effacer le Wi-Fi). Les effets sont bornés : le profil est fixe, rien n'est persistant, aucun autre réglage n'est accessible, ni le flux audio ni l'USB ne sont commandés, la lecture de l'historique reste assainie comme avant, et le seul coût est un journal plus détaillé et plus de lignes sur la console pendant 15 minutes.

## Limitation des messages répétitifs

- **Relances du flux.** Quand une station ne répond pas, chaque tentative produisait plusieurs lignes (« connecting attempt », « connect … failed », « HTTP status=404 »…). Les trois premiers échecs de suite sont journalisés, puis un sur dix, chacun avec son numéro (`(failure 11)`). Le premier échec et le retour à la normale sont **toujours** écrits (`stream: back after N failed attempts` dès la première réponse 200 après trois échecs ou plus). Le compteur se remet à zéro à un changement de station. Ni l'adresse de la station ni son chemin ne sont journalisés (voir « Console UART0 »).
- **`ZEROFILL`.** Les trois premiers secteurs nuls d'une série sont journalisés (`usb: ZEROFILL #n …`), puis un tous les 128, puis un total à la fin de la série (`usb: ZEROFILL series ended: N sectors`, au deuxième ordre READ(10) sans secteur nul, ou à la fin de la session). Seule la journalisation change dans `usb.rs` : les décisions de remplissage, les secteurs, les tampons, les temporisations et les transactions sont inchangés (test).
- **Inchangé.** La ligne `stream: live=… consumed=… lead=…` (toutes les 60 s) sert aux campagnes de qualification ; elle n'évince pas le reste (une ligne par minute pour 32 lignes d'historique).
- **Non corrigé ici.** L'anneau de `iobewi-log` (24 lignes, vidé toutes les 400 ms) abandonne les nouvelles lignes quand il est plein et ne compte pas ces pertes : le compteur `overwritten` ne voit que l'historique. La limitation ci-dessus réduit le risque (la seule rafale connue était `ZEROFILL`) ; un compteur de pertes demanderait de modifier la bibliothèque.

## Traces ajoutées

| Où | Niveau | Ligne |
| --- | --- | --- |
| `PUT /api/logs/config` | Info / Warn | `diag: log policy replaced (generation N)` ; `diag: log policy change refused (Invalid|TooLarge|NotAllowed|Storage)` |
| Activation et fin du diagnostic détaillé | Info | `diag: detailed diagnostic on for 900 s`, `diag: detailed diagnostic off (expired|disabled)` |
| Favori : enregistrement, sélection, suppression | Warn / Debug | échec de stockage ; refus d'une saisie |
| Sauvegarde de la luminosité | Debug / Info | fenêtre de détachement : `the disk leaves the bus`, durée avant l'écriture, `write done|FAILED after N ms`, `the disk is back after N ms` ; Info si une bascule de station reprend le disque |
| Flux | Info | `stream: back after N failed attempts` |

## Console UART0

L'assainissement ne protège que la sortie HTTP : **tout ce qui passe le filtre de niveau est écrit tel quel sur l'UART0** (sans mot de passe : aucun n'est jamais journalisé). Contenu actuel : l'adresse IP, le schéma et l'hôte de la station (plus jamais l'adresse complète, qui peut porter un jeton : `stream: connecting attempt=N to http://hôte`), et, **hors de la maîtrise du produit**, les noms de réseau Wi-Fi écrits par `iobewi-wifi-manager` et `iobewi-esp-wifi` (bibliothèques). Le produit n'écrit plus le SSID dans sa propre ligne d'approvisionnement (`improv: provisioning requested`). L'accès à l'UART0 suppose un accès physique à la carte.

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
- Le démarrage lit le `ConfigSpace` lui-même puis applique la politique décodée : `LogConfig::reload` n'expose pas la politique appliquée, et l'interface doit pouvoir la renvoyer. Sans politique persistée, la base est celle que la cible a appliquée (`logging::remember_applied`) : `GET /api/logs/config` reflète donc aussi la liste des cibles d'une image `qual`.
- Le calcul d'admission du magasin EKV passe de 32 à 40 pages sur 48 (`boot_policy::SPACES`, test `spaces_fit_the_config_partition`). Aucun reformatage.
- Pile libre du firmware : 46 336 octets avant, 45 008 après (minimum exigé : 36 864). Le premier jet la faisait tomber à 39 384 : la tâche `product` (future du serveur) grossissait de 6,7 Ko ; `Diag` et son état sont maintenant sur le tas (`Box`), et la persistance d'un PUT est `Box::pin` (tas, le temps de la requête AP). Il y a donc ≈ 7 Ko de tas en plus (historique, états) et jusqu'à 6 Ko transitoires par lecture : à mesurer sur carte avec la sonde `qual` (tas de 80 Ko, pic mesuré avant ce chantier ≈ 60 Ko).

## Incident de qualification du 19 octobre 2026 (`GET /api/logs?n=32`)

Sur la carte, la première requête `GET /api/logs?n=32` a figé tout le firmware (console, HTTP, USB, son), sans message ; un reset EN l'a rétabli, configuration intacte. La première construction de la réponse copiait les lignes, bâtissait un arbre `serde_json::Value` (une `Map` et des `String` clonées par ligne), puis sérialisait.

- **Mesure sur hôte (allocateur compteur, 32 lignes pleines, réponse de 5 920 o)** : ancienne construction = 404 allocations, 76 148 o demandés, **pic de 67 932 o** ; nouvelle = 1 allocation, 5 920 o, pic 5 920 o. Le pic cible (pointeurs de 32 bits) est plus petit mais du même ordre de grandeur, face à 27 128 o de tas libre à la dernière sonde (minimum libre 18 868 o).
- **Cause : hypothèse la plus probable, non démontrée sur la carte.** Un échec d'allocation dans le gestionnaire de tas conduit à `handle_alloc_error`, donc à une panique, et le gestionnaire de panique du firmware boucle sans rien afficher : ce serait le gel observé. Aucune trace ne permet de l'affirmer.
- **Hypothèses examinées** : le décodage de `?n=` ne panique pas (test hôte, valeurs valides et invalides) ; l'historique n'utilise ni section critique ni verrou tenu à travers un `await` (`NoopRawMutex`, accès synchrones dans une seule tâche) ; les cadres de pile du nouveau chemin sont petits (un tableau de 32 `usize`). Un dépassement de pile ne peut pas être exclu par ces seuls éléments (marge mesurée 11,8 Ko avant la requête).
- **Ressources attendues** : `GET /api/logs` demande au plus 10,9 Ko de tas transitoires (typiquement 6 Ko) au lieu de ≈ 35 à 65 Ko ; `GET /api/logs/config` ≈ 1,1 Ko. La marge de pile n'est pas modifiée. À confirmer sur carte avec la sonde `qual` après nouvelle autorisation.

## Profondeur de pile des routes (qualification du 19 octobre 2026)

La qualification sur carte (H5, 10 mutations de favoris) a mesuré une marge de pile de 7 188 o (seuil 8 Kio), contre 15 988 o sur `main`, sans aucune requête `/api/logs` : la hausse vient de la forme du routeur, pas de la consultation des journaux. picoserve imbrique chaque `route` autour des précédentes, et le `poll` d'un groupe de routes imbriquées porte un cadre de plusieurs Ko : deux routes de plus sous les routes de stations ajoutaient un niveau de 6 288 o à tout chemin de mutation.

- **Correctif** : les trois routes de stations sont dans un sous-routeur (`Router::nest("/api/stations", …)`, déclaré en dernier) ; picoserve appelle le sous-routeur seul une fois le préfixe reconnu (frontière de segment et reste non vide exigés). Les routes de diagnostic restent à plat. Un simple réordonnancement ne suffisait pas : il déplaçait le coût vers `GET /api/logs` (+5,9 Ko) ou vers `PUT /api/logs/config`.
- **Contrat inchangé** : les tests `routing_tests` font tourner le vrai serveur picoserve sur un socket en mémoire, avec les routes de production et, en référence, l'ancienne liste plate, et comparent statut, en-têtes et corps de 33 requêtes (mutations valides et invalides, actions inconnues, `/api/stations`, `/api/stationsXYZ`, méthodes erronées, diagnostic, `/wifi`). Statuts attendus : 404 pour les chemins inconnus, 405 pour les méthodes erronées, 403 pour `PUT /api/logs/config` et `POST /wifi` hors configuration.
- **Profondeurs statiques** (somme des cadres Xtensa depuis la tâche produit, graphe d'appels du binaire ; identiques pour les images nominale, `qual` et `usb-debug`) :

| Chemin | `main` | PR avant | PR après |
|---|---|---|---|
| mutation de station → `commit` | 21 840 | 29 888 | 17 520 |
| `PUT /api/logs/config` → `replace_yaml` | n/a | 23 840 | 23 872 |
| `GET /api/logs` (extraction `Query`) | n/a | 25 968 | 26 000 |
| `GET /api/logs/config` → `encode` | n/a | 27 104 | 27 136 |
| `POST /wifi` → handler | 24 816 | 32 864 | 27 024 |
| maximum jusqu'à l'écriture flash | 26 960 | 35 008 | 28 928 |

  Ces profondeurs sont des sommes de cadres, pas des consommations mesurées : elles ignorent les appels indirects, les interruptions et la pile du pilote Wi-Fi. Sur la PR avant correctif, la somme statique (chaîne de mutation) et le filigrane mesuré concordent à 16 o près, une fois retranché le cadre de `main` (−576 o).
- **Marges attendues** (pile réservée 44 776 o en `qual`, 45 008 en nominal) : mutations de favoris ≈ 19 Ko, `PUT /api/logs/config` valide (boot AP) ≈ 12,7 Ko, `GET /api/logs` inchangé (12 164 o mesurés en H2). Pire marge estimée ≈ 12 Ko. À confirmer par le filigrane de la sonde `qual` après les 10 mutations.
- **Règle** : toute nouvelle route qui aboutit à un commit flash doit être mesurée de la même façon avant fusion ; ne pas en ajouter à plat au routeur principal sans contrôler la profondeur du chemin des mutations.

## Profondeur de pile après le diagnostic détaillé

La profondeur statique est mesurée par `tools/stack_depth.py` (somme des cadres `entry` de la chaîne d'appels directs la plus profonde depuis la tâche produit ; `objdump` ne sait pas décoder le code Xtensa produit par LLVM, le script décode lui-même les instructions). Comme pour le tableau ci-dessus, c'est une **borne basse** : elle ignore les appels indirects et les interruptions. Image `board-devkitc-1`, avant (PR #35) et après ce chantier :

| Chemin | Avant | Après |
| --- | --- | --- |
| `POST /api/led/*` | 28 288 | 31 360 |
| `/api/debug` | n/a | 31 296 |
| mutation de station (`setup::mutation`) | 28 464 | 25 600 |
| `POST /wifi` (mode AP, mène à la flash) | 25 792 | 30 048 |
| plus profond chemin jusqu'à la flash (`FlashStorage::read`) | 33 872 | 32 176 |
| plus profond chemin connu | 33 936 | 34 640 |
| pile réservée par l'éditeur de liens (minimum exigé : 36 864) | 49 792 | 48 976 |

Chaque `route` ajoutée au routeur ajoute à peu près un cadre de 3 Ko aux chemins qu'elle enveloppe (constat de #22, retrouvé ici). `/api/debug` est donc déclarée la première (la plus intérieure), avant le sous-routeur `/api/led` : les routes de mutation et de politique ne gagnent rien. Alternatives mesurées : `/api/debug` déclarée après `/api/led` (équivalent : 34 624) ; juste avant le sous-routeur des stations (plus profond : 35 584) ; `/api/led` et `/api/debug` dans un seul sous-routeur `/api` (plus profond : 37 200, chemin jusqu'à la flash 34 720). Si une route est ajoutée, mesurer à nouveau.

Les profondeurs sont des sommes de cadres : la marge réelle se mesure sur carte avec la sonde de l'image `qual` (filigrane de pile). Sur le chemin le plus profond connu, la marge statique est de 14 336 octets sur la réservation ; #22 avait mesuré 2,7 Ko de plus que la somme statique, d'où une marge attendue d'environ 11,6 Ko pour un seuil de 8 192 : **à confirmer sur carte**.

## Hors périmètre

`POST /wifi`, qui était accessible depuis le LAN, est réservé au boot de configuration depuis #23 : la politique « Wi-Fi seulement en AP » que ce modèle d'accès suppose est maintenant vraie. Le transport distant `iobewi-log-stream` n'est **pas** raccordé : il ajouterait une connexion TLS WebSocket permanente, un jeton à provisionner, une horloge fiable et un collecteur, et les cinq sockets du produit sont déjà tous utilisés (DHCP, DNS, flux, interface, SNTP). La consultation HTTP locale suffit ; à reconsidérer seulement si une collecte distante devient un besoin.

## Validation

**Qualification sur carte de #15 (PR #22, commit `db21162`, ESP32-S3 DevKitC-1 et Metronic 477144), PASS H1 à H10 :** H1 espace de configuration, catalogue et Wi-Fi préservés, politique par défaut chargée ; H2 284/284 puis 280/280 réponses 200 sur `GET /api/logs?n=32` à 1 Hz pendant 5 minutes, sans perte USB ni coupure audio (confirmation de l'utilisateur) ; H3 marge de pile minimale 13 372 octets, tas libre minimal 11 928 octets (seuil 8 192) ; H4 `PUT` refusé en STA (403) ; H5 dix mutations de favoris avec et sans consultation des journaux ; H6 aucun secret parmi 8 960 lignes inspectées ; H7 corps trop long refusé (413) ; H8 `LogConfig::replace()` en mode AP (YAML validé, persisté, appliqué à chaud) ; H9 politique rechargée au redémarrage (`source=persisted`), YAML invalides rejetés ; H10 image nominale fonctionnelle, continuité sonore confirmée. Réserves de l'époque : test Playwright de l'interface non exécuté (fait depuis, hors CI), pile du `PUT` valide en AP non mesurée sur image nominale, assainissement lexical, cause du gel initial (probablement un échec d'allocation) non démontrée. Cette qualification est antérieure aux PR #34 (LED) et #35 (luminosité), au diagnostic détaillé et à la limitation des répétitions : ce qui suit n'a pas été rejoué sur carte tant que la PR de ce chantier n'a pas été qualifiée.

**Tests hôte** (`cargo test -p streambewi-core -p streambewi`) : réponse `GET /api/logs` (octets identiques à l'ancienne construction pour 0 à 40 lignes, JSON valide, échappement identique à `serde_json`, borne `RESPONSE_MAX`, une seule allocation exacte contre 404 avant), assainissement, historique borné et ordre, découpe UTF-8, politique absente, invalide ou illisible, refus hors AP, YAML invalide ou trop long, écriture en échec, persistance après rechargement, admission des espaces, vidage de l'anneau. Diagnostic détaillé : profil limité aux quatre cibles (jamais `trace`, jamais une cible Wi-Fi), politique pleine donc profil partiel, restauration exacte de la base (cibles de `qual` comprises), aucune écriture du stockage, durée de 15 minutes sans prolongation, réactivation après expiration, désactivation idempotente, politique remplacée pendant une session, remplacement refusé, allocations (activation : pic 3 344 o, 4 appels ; réponse : pic 1 251 o), routes `/api/debug` sur le routeur de production (états, refus, méthodes, aucun effet persistant). Cadence de l'instantané (au plus une ligne par 15 s) et filtrage du niveau `debug` hors profil. Limitation : décision « premières puis une sur dix », compteur de défaillances sur des connecteurs simulés, série `ZEROFILL` (premiers secteurs, un tous les 128, total, fin de session) et décisions de remplissage inchangées. `tools/qual/check-ui.cjs` couvre la section Diagnostic (Playwright, API simulées, hors CI).

**Non exécuté sur carte :** diagnostic détaillé, limitation des répétitions, nouvelles traces, profondeur de pile réelle.
