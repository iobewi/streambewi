# Qualification matérielle de l'OTA système A (#3) — bilan

Ce document rend compte de la qualification de l'OTA système A sur une carte réelle. Il distingue ce qui a été **mesuré et
observé** de ce qui **n'a pas été exécuté** (section « Limites et essais non exécutés »). Un essai non exécuté n'est
jamais présenté comme réussi.

- **Carte :** ESP32-S3-DevKitC-1 N16R8, MAC `9c:13:9e:ac:4e:8c`, flash de 16 Mio, lecteur Metronic 477144, banc Web Serial.
- **Images de banc :** construites avec un jeton et une identité TLS **jetables, non versionnés**. Elles ne sont pas des
  images de release (voir `docs/ota.md`).
- **Révisions :** première campagne sur `e46fe08` (tas principal 64 Kio) ; seconde campagne, OTA et rollbacks sur
  `799dd49` (tas principal 68 Kio). Le code testé en seconde campagne est celui de la PR, à ses commits de documentation
  près.
- **Méthode :** toute écriture sur la carte a été autorisée séparément ; chaque écriture a été relue et comparée par
  SHA-256 ; les relectures de la flash (`dump`) réinitialisent la carte et n'ont jamais été faites pendant une
  probation. Aucune relecture interrompue par du bruit série n'a servi de preuve : elle a été refaite.

## 1. Première installation de l'image `qual`

L'image `qual` est la nominale avec des sondes de pile et de tas (journal `qual-mem periodic` toutes les 30 s ; valeurs
cumulées depuis le démarrage ; la pile principale cpu0 est peinte au démarrage, les piles des tâches radio sont dans le
tas). Elle sert à mesurer, pas à livrer.

1. **Sauvegarde complète des 16 Mio**, relue deux fois : SHA-256 identiques
   `d61be797638ce8eaf5197f01e409f0864c269731d271e82170cbc8514258457d`. Empreintes par région avant écriture :
   `0x0–0x9000` (second étage amont et table) `d15b37bc…`, ancien `config` `96b59a63…`, ancienne application
   `17a7fe45…`, `0x380000–0x400000` `54532002…` (non vierge : écrasée par la première installation), au-delà de
   `0x400000` vierge.
2. **Écriture de l'image de première installation** à `0x0`, limitée à la plage de l'OTA, sans effacement global,
   relue et comparée.
3. **Vérifications des partitions :** table lue (`otadata` `0xd000`, `ota_0` `0x20000`, `ota_1` `0x1D0000`, `config`
   `0x380000`) ; `otadata` initial `d3137c5c…865d` (emplacement 0 `Valid`) ; second étage `8719f258…7ce7`
   (29 744 o, reproductible : même SHA-256 dans deux répertoires de construction).
4. **Premier démarrage :** `ota: running ota_0 status Stable`, formatage du `config` vierge, Improv, Wi-Fi provisionné
   de nouveau (l'ancienne configuration n'a pas été restaurée, comme décidé), station FIP, LED, flux et disque
   présentés au Metronic.

## 2. Mesures mémoire et correction 64 → 68 Kio

Seuils fixés avant les essais : marge de pile réelle ≥ 8 192 o ; tas libre minimal ≥ 40 960 o en normal et
≥ 16 384 o en maintenance.

| Campagne | Tas principal | Mode | Marge de pile | Tas libre minimal | Verdict |
|---|---|---|---|---|---|
| 1 (`e46fe08`) | 64 Kio | normal | 28 248 o | 67 056 o | conforme |
| 1 (`e46fe08`) | 64 Kio | maintenance | 16 952 o | **15 012 o** | **seuil de tas manqué** : arrêt avant toute activation |
| 2 (`799dd49`) | 68 Kio | normal | 26 528 o | 71 620 o | conforme |
| 2 (`799dd49`) | 68 Kio | maintenance | 12 856 o | **17 400 o** | conforme, marge de **1 016 o** sur le seuil |

La première campagne a donc été arrêtée à la mesure du tas de maintenance. La décision a été de déplacer 4 Kio de la
pile vers le tas (64 → 68 Kio) sans abaisser le seuil. Les prévisions de la correction (environ 19,1 Kio de tas et
12,9 Kio de marge de pile) se sont révélées justes pour la pile (12 856 o) et un peu optimistes pour le tas (17 400 o).
Le pic de tas de la maintenance varie d'un essai à l'autre (117 452 à 125 976 o observés) : la marge est mince.

Les mesures de maintenance couvrent : 2 `health` (poignées de main TLS), mauvais jeton (401), mauvais digest sur image
complète (409, écrit en `ota_1` mais non publié), upload complet puis relecture, validation et publication OTM1. Elles
ont été faites sur l'image `qual` ; **l'image nominale n'a pas de sonde** et sa marge n'est pas mesurée (même code,
mêmes réservations).

Démarrage normal (qual, 68 Kio) : 100 lectures de `/api/state` sans erreur (pire délai 1 151 ms), Diagnostic Live,
quatre changements de station, enregistrement de la luminosité, Metronic en lecture. Aucun panic.

## 3. Essais de maintenance R1 à R6 (première campagne, `e46fe08`)

Question posée : la demande de maintenance, écrite dans la RAM RTC avant un reset RTC (RWDT), survit-elle au reset et au
second étage iobewi, et ne survit-elle à rien d'autre ?

| Essai | Action | Résultat |
|---|---|---|
| R1 | `POST /api/maintenance` depuis le démarrage normal | **PASS** : `rst:0x10 (RTCWDT_RTC_RST)`, second étage, puis `maintenance: the OTA server alone` et `OTA ready` ; la demande a survécu au RWDT et au second étage. Répété en seconde campagne (`799dd49`) avant chaque upload et chaque activation |
| R2 | reset EN du banc pendant la maintenance | **PASS** : démarrage normal (la demande avait été consommée) |
| R3 | `POST /api/maintenance`, puis coupure de courant avant le redémarrage | **PASS** : démarrage normal (la RAM RTC est remise à zéro par la mise sous tension) ; essai manuel fait par le mainteneur |
| R4 | maintenance sans aucune requête OTA | **PASS** : retour au démarrage normal après le délai de repos |
| R5 | maintenance, puis geste BOOT 3 s (récupération) | **NON EXÉCUTÉ** : destructif (efface le Wi-Fi et le drapeau USB), sans autorisation |
| R6 | démarrage qui suit une maintenance et une activation | **PASS** : normal, jamais deux maintenances de suite |

Limites : R1 à R4 et R6 ont été exécutés sur `e46fe08`, avant la correction mémoire (la logique de maintenance est la
même sur `799dd49`, qui a refait R1 à chaque entrée en maintenance). En maintenance, aucun flux ni MSC n'est lancé (aucune ligne de
journal correspondante) ; le mainteneur a confirmé que le disque disparaît puis réapparaît au retour en normal
(seconde campagne).

## 4. OTA nominale `ota_0` → `ota_1`

Image : nominale de `799dd49` (charge utile OTA 1 323 456 o, SHA-256
`00608b2c5c6378091f86b00cad0cc0d6b22744c2e52d5508f7099d2451946c3e`), identifiant `pr40-nominal-2`. Un premier candidat
`pr40-nominal-1`, construit sur l'ancien `HEAD`, a été remplacé par un nouvel upload et **n'a jamais été activé**.

Avant l'activation (lecture seule) : `ota_0` actif et `Stable` (qual `223a5ce3…65ca`), `ota_1` `00608b2c…6c3e`, OTM1
génération 3 `written` (`pr40-nominal-2`), `otadata` `d3137c5c…`.

| Heure | Événement |
|---|---|
| 12:59:38 | maintenance (`RTCWDT_RTC_RST`), `OTA ready` |
| 12:59:49 | `activate pr40-nominal-2` : `200 activating` |
| 12:59:50 | `RTC_SW_SYS_RST`, **`ota: running ota_1 status PendingVerify`** |
| 12:59:51 | **`ota: confirmed`** |
| 13:00:03 | Wi-Fi sans reprovisioning, flux HTTP 200, LED verte fixe, MSC présenté |

Après la confirmation :
- `otadata` `b229dcc9…` : secteur 0 seq 1 `Valid` (`ota_0`), secteur 1 seq 2 `Valid` (`ota_1`).
- `ota_1` inchangé `00608b2c…6c3e` ; `ota_0` relu sur 0x15E490 o, SHA `223a5ce3…65ca`, inchangé.
- OTM1 : génération 3 `written`, 4 et 5 `activating`, 6 `none` (confirmation).
- `/api/state` `running / connected / ready / ready`, `reading: true`, 20/20 requêtes à 200, `/api/logs` lisible.

## 5. Rollback R1 : rejet explicite du self-check

Image `ota-reject-self-check` (`pr40-rollback-reject-1`, SHA-256
`d079f52f767209c7d952b174187f67d3ae1a5ad58358a3a6781cc4c394838c24`, 1 323 456 o), téléversée dans `ota_0`.

- Avant : `ota_1` `00608b2c…6c3e`, `otadata` `b229dcc9…`, OTM1 génération 6, Config-Space `a3ad9717…`.
- Après upload (relecture) : `ota_0` `d079f52f…`, `ota_1` et `otadata` inchangés, OTM1 génération 7 `written`.
- Activation : `ota: running ota_0 status PendingVerify` (13:54:53.4), pas de `ota: confirmed`, puis reset logiciel et
  `ota: running ota_1 status Stable` (13:54:53.9) : retour en environ 0,5 s, exactement trois redémarrages depuis le
  repère (maintenance, activation, retour).
- Après : `otadata` `da0f9395…`, secteur 0 seq 3 **`Invalid`** (état 3), secteur 1 seq 2 `Valid` ; `ota_1` inchangé ;
  OTM1 génération 7 `written`, 8 `activating`, 9 `none` ; Config-Space `fe8d3cb6…`.

## 6. Rollback R2 : expiration du self-check à 15 s

Image `ota-hang-self-check` (`pr40-rollback-hang-1`, SHA-256
`0395734cdd723efbcbf463fbf018cb33be096207c3a710ca7f574149636393f0`, 1 323 456 o), téléversée dans `ota_0`.

- Le moteur a accepté la réécriture d'un slot dont l'entrée était `Invalid`.
- Après upload : `ota_0` `0395734c…`, `ota_1` et `otadata` (`da0f9395…`) inchangés, OTM1 génération 10 `written`.
- Activation : `ota: running ota_0 status PendingVerify` (14:10:14), aucun journal pendant la probation, pas de
  `ota: confirmed`, reset logiciel à **15,3 s** (la course applicative de 15 s ; le watchdog de 30 s n'est que le
  filet), puis `ota: running ota_1 status Stable` (14:10:29.5). Délai total activation → retour : 16,1 s. Trois
  redémarrages, pas de boucle, candidat non relancé.
- Après : `otadata` `98c1c6d7…`, secteur 0 seq 5 **`Aborted`** (état 4), secteur 1 seq 2 `Valid` ; `ota_1` inchangé ;
  OTM1 génération 10 `written`, 11 `activating`, 12 `none` ; Config-Space `c5f05041…`.

Différence R1 / R2 : le rejet explicite marque l'entrée `Invalid` ; un blocage laisse l'entrée en attente, que le second
étage marque `Aborted` au démarrage suivant. Dans les deux cas le candidat n'est pas relancé.

## 7. Intégrité et conservation de Config-Space

Contrôles faits à chaque étape par relecture de la flash et comparaison de SHA-256 : `ota_1` est resté
`00608b2c…6c3e` pendant R1 et R2 (il n'est jamais ciblé par l'OTA, qui n'écrit que le slot inactif) ; `ota_0` a toujours
la charge utile téléversée ; `otadata` ne change qu'aux activations et aux rejets, comme prévu. Le second étage et la
table sont restés identiques après la première installation.

Config-Space (`0x380000`, 0x80000 o) : son SHA-256 change à chaque transaction OTM1 (pages de l'espace `ota` et leurs
compactions), ce qui est attendu. Les enregistrements `wifi` (génération 1), `usb_boot` (génération 1), `stations`
(génération 8) et `settings` (génération 3, luminosité 20 %) sont restés identiques d'une étape à l'autre, et l'API
relit `led.saved` = 20. **Limite de la méthode :** il s'agit d'un balayage d'octets des enregistrements, pas d'un
décodage complet des espaces. Aucun effacement ni formatage de `config` après la première installation.

## 8. Confirmations humaines (Metronic 477144)

Le mainteneur a confirmé, après l'OTA nominale, après R1 et après R2 : le **son FIP est audible** et le **disque USB
disparaît puis réapparaît** (maintenance, puis retour en normal). L'audio n'est jamais déduit de `reading: true`.

## 9. Limites et essais non exécutés

À ne pas lire comme réussis :

- **R5** (geste BOOT en maintenance) : non exécuté.
- **Coupure d'alimentation** pendant l'effacement, la programmation, la publication OTM1, l'activation ou la
  confirmation : non exécutée (sauf R3, qui coupe avant un redémarrage de maintenance et pas pendant une écriture).
- **Image corrompue** autre que le mauvais digest (409) : non exécutée ; image tronquée, identifiant d'activation
  inconnu, transfert interrompu : non exécutés.
- **Panne externe pendant la probation** (Wi-Fi absent, station injoignable avec un candidat sain) : non exécutée. La
  probation ne les teste pas par construction, mais la conséquence n'a pas été observée.
- **Candidat qui démarre mais dont le flux ou l'USB sont cassés** : confirmé par construction (probation minimale) ;
  non essayé.
- **OTA réussie vers `ota_0`** : les deux candidats envoyés dans `ota_0` ont été rejetés volontairement ; aucune
  activation réussie de `ota_1` vers `ota_0` n'a été faite.
- **Reprise après interruption EKV (#70)** : non qualifiée.
- **Marge mémoire de l'image nominale** : estimée, pas mesurée ; celle de maintenance est mince (1 016 o, voir §2).
- **Sécurité** (jeton, TLS, signature) : voir `docs/ota.md`. Rien n'a été testé contre un attaquant.
- **Une seule carte**, un seul lecteur, un seul point d'accès Wi-Fi.

Hors de cette campagne : domaine B, signature des images, runtime supervisé.

## 10. Rejouer la campagne

Les procédures ci-dessous ont servi ; chaque écriture sur une carte exige l'autorisation de son propriétaire.

1. Sauvegarde complète (2 lectures, SHA-256 identiques), SHA-256 publiés des fichiers à écrire.
2. Première installation : `streambewi-esp32s3.bin` à `0x0` (4 Mio, sans autre effacement), relecture et comparaison,
   puis Wi-Fi par Improv.
3. Maintenance : `tools/ota/client.py maintenance http://DEVICE`, puis `health`, `upload IMAGE ID` (sans `--activate`),
   relecture de la flash (slot cible, `ota_1`/slot actif inchangé, `otadata`, OTM1), puis `activate ID` dans une
   nouvelle maintenance (une relecture de la flash réinitialise la carte et sort de la maintenance).
4. Aucun `dump` pendant la probation. Preuve d'un rollback : journal (`ota: running <candidat> status PendingVerify`,
   absence de `ota: confirmed`, puis `ota: running <repli> status Stable`) **et** `otadata` **et** SHA-256 du repli
   inchangé **et** OTM1 `none`. Un simple redémarrage n'est pas une preuve.
5. Pour les rollbacks, les images `ota-reject-self-check` et `ota-hang-self-check`, jamais livrées.
6. Mémoire : image `qual`, journal `qual-mem periodic` ; seuils de la section 2.
