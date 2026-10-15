# Migration StreamBeWI vers rust-mcu-libs — #67

## Inventaire avant code — 15 octobre 2026

Produit de référence : `10d4616` (main de StreamBeWI).
Bibliothèque cible : `671ca69b1103cabc1ca24f40c043e8fa73a9d21f`
(main après #75/#76 ; #66 qualifiée S3/Windows).

| Responsabilité actuelle | Source | Cible / décision |
| --- | --- | --- |
| FAT16, rolling, MSC | iobewi@f0360f5 | mêmes packages de rust-mcu-libs, sans réécriture |
| OTG S3 | ancien Board / driver USB | iobewi-esp-usb ; GPIO20/19 imposés par PHY interne (#77) |
| Géométrie RADIO.MP3, sondage lointain | core/src/lib.rs | conserver 1 Gio virtuel, clusters 32 Kio et seuil de sondage 1 Mio |
| Fenêtre / préchargement | core/src/lib.rs | conserver 96 Kio / 64 Kio / avance 80 Kio |
| Producteur HTTP | app/src/stream.rs | conserver reqwless, URL, Icy-MetaData=0, reconnexion, tampon 2048 octets ; aucun cache flash |
| Politique MSC | app/src/usb.rs | conserver identité produit, attente 5 s / sondage 5 ms, exposition après configuration et prébuffer |
| Provisioning / récupération | app/src/provisioning.rs et boot_policy.rs | conserver Improv sur UART0 et USB-JTAG lorsque disponible, réponse au port d’origine, BOOT 3 s |
| Choix USB au boot | ancien Board / BootIoFactory / entry! | composition produit S3 ; flag lu avant construction du transport natif ; aucune bascule à chaud |
| Stockage configuration | ancien Board / ConfigSpace / NVS | ConfigSpace de rust-mcu-libs + Store EKV + EspConfigFlash + propriétaire flash unique |
| Ordre des commits | boot_policy.rs | credentials puis flag OTG ; recovery flag provisioning puis suppression credentials ; conserver les erreurs partielles |
| Log | iobewi-log historique | iobewi-log actuel ; capture RAM, console silencieuse pour ne pas parasiter Improv |
| Démarrage, ressources, série, reset | Board / entry! / entry-build | composition explicite dans targets/esp32 avec esp-hal / esp-rtos / Embassy ; aucun nouveau Board générique |
| OTA | aucune OTA produit dans cette base | hors périmètre : ne pas modifier les compositions OTA de rust-mcu-libs |

Les APIs ConfigBackend/ConfigManager/ConfigSpace et Wi-Fi sont déjà compatibles
avec les politiques produit : la composition matérielle est la partie à remplacer.
Le dépôt reste propriétaire de son Cargo.lock (firmware applicatif).

## Divergence de stockage explicite

NVS et EKV ne partagent pas leur format. Aucune migration automatique, reprise de
NVS ou initialisation après une erreur de montage n’est prévue. Le nouveau firmware
emploie une partition `config` data/0x40 ; il initialise explicitement une partition
entièrement vierge uniquement. Un contenu non vierge illisible provoque un refus.
L’ancien dist et les preuves historiques restent des références, pas des images du
nouveau portage. La préparation du banc doit sauvegarder la carte et choisir un
layout de test adapté, sans effacement global implicite.

## Gates

- Tests hôte des politiques, provisioning, journal et géométrie ; conservation des
  assertions historiques hors adaptation aux points d’entrée série/reset.
- Lien S3 réel en release, configurations normale et usb-debug, outilchain et
  ressources consignés ; dépendances Git épinglées, aucun ancien framework résolu.
- Firmware complet avec vrai producteur HTTP : réception/prébuffer puis MSC,
  interruptions amont, reconnexion USB et mémoire bornée à qualifier sur le banc.
- La validation #66 avec producteur synthétique ne prouve pas le comportement avec
  réseau ni la lecture audio sur le Metronic. #67 reste ouverte sans ces preuves.

## Implémentation et vérification locale

La cible `targets/esp32` compose désormais esp-hal, esp-rtos et les briques
rust-mcu-libs directement. Board, entry!, entry-build, DeviceMetadata et leur
banque de ports ont disparu du graphe résolu. Aucun patch de dépendance, aucun
chemin vers un checkout local. Le manifeste EKV reprend les budgets exacts de
Wi-Fi et du flag USB ; le stockage physique a un propriétaire unique.

Les demi-ports UART0/JTAG réutilisent l'implémentation concrète historique dans
`targets/esp32/src/serial.rs`, sans recopier SerialBank/Board. `take_ports` consomme
un itérateur de couples RX/TX ; le test conserve les assertions d'ordre et de
route de réponse. Le reset est un callback produit qui arme RTC ResetSystem. Le CPU conserve
`CpuClock::max()` de l’ancien entry S3 ; pas de changement implicite de fréquence.
Les diagnostics fatals de configuration restent possibles sur UART0 avant Improv.

Les changements Clippy sont équivalents : condition de sondage regroupée,
`SharedStream::default()` délégué à `new()`, état Improv initialisé dans la boucle
qui le recalculait déjà. Les politiques radio/USB ne changent pas.

Outils effectivement utilisés : Rust stable 1.99.0, esp Rust 1.98.1.0
(`183f762d6`), GCC Xtensa 15.2.0_20250920, esp-hal 1.2.2, esp-rtos 0.4.0,
esp-radio 1.0.0-beta.1, Embassy USB 0.6.0 et Embassy net 0.9.1.

| Contrôle local | Résultat |
| --- | --- |
| Tests core/app, provisioning, boot policy et logs | PASS : 20 tests historiques conservés |
| Clippy hôte `--all-targets -- -D warnings` | PASS |
| Rustfmt global, `git diff --check`, syntaxe des scripts shell | PASS |
| Build/lien release S3 normal | PASS |
| Build/lien release S3 usb-debug | PASS |
| Descripteur ELF produit / version / cible | PASS : streambewi-esp32 / 0.1.0 / ESP32-S3 |
| TaskPool produit (avec métadonnées Embassy) | 11 056 octets, normal et debug |
| Réservation de pile du linker | 44 704 octets normal ; 44 600 debug |
| Consommation maximale réelle de pile et de heap | NON MESURÉE : banc nécessaire |
| Image fusionnée et cohérence table/application | PASS : 1 064 368 octets ; application 736 688 / 1 572 864 octets à 0x50000 |
| Anciennes dépendances IOBEWI / Board / entry résolues | Aucune |
| Dist historique versionné | Inchangé ; le script de flash refuse son ancienne métadonnée runtime |
| Carte, réseau, lecture Metronic | NON EXÉCUTÉ dans cette session |

Le linker émet son avertissement RWX sur un segment LOAD, déjà rencontré avec
ces toolchains embedded ; les deux liens aboutissent. Aucun résultat de compilation
n'est présenté comme une mesure dynamique ou une validation USB/audio.

## Fabrication de l'image : limitation d'espflash

Espflash 4.6.0 panique dans `esp-idf-part` 0.6.0 (`SubType::data`, unwrap) en
traitant le sous-type data/0x40 de la partition EKV. Le sous-type n'est pas changé
pour contourner cet outil.

`tools/package_image.py` réutilise :

- espflash 4.6.0, sans table personnalisée, pour l'image applicative et le bootloader
  S3 amont ; l'extraction du bootloader vérifie les marqueurs et ses bornes ;
- `gen_esp32part.py` officiel Espressif, tag ESP-IDF v6.0, téléchargé et vérifié par
  SHA-256 (`02f6c1c5f012393d47987ee64bc50ba319a2eda48cbcec754d8cfaf780cce529`),
  pour valider et encoder la vraie table ;
- esptool 5.5.0 pour la fusion. La taille factory et les octets table/application
  présents dans le résultat sont contrôlés.

Aucun encodeur ESP/FAT/partition n'est réimplémenté. La première fabrication a
besoin du réseau pour mettre le générateur en cache sous `target/tools` (ignoré).
Le helper pourra être retiré lorsque la version stable d'espflash supportera ce
sous-type. Le script de flash écrit l'image fusionnée par esptool ; `cargo run`
n'est plus proposé comme chemin de flash avec une table qu'espflash ne sait pas lire.

Une image factory fusionnée contient du remplissage vierge dans la région config :
son flash réinitialise cette région et impose de reprovisionner. Ce n'est pas une
mise à jour conservant les données d'un produit installé. Le layout factory de
ce produit ne modifie pas les exemples/contrats OTA du dépôt rust-mcu-libs.

## Qualification restant à faire pour #67

Sur le banc existant, conserver/sauvegarder la carte avant de choisir le layout
ou une écriture applicative seule sur un layout réellement compatible. Consigner
le commit, l'image, la carte et les ports ; aucun effacement global implicite.

1. Premier boot avec config vierge : Improv fonctionne sur les ports disponibles,
   pas de MSC ; valider le refus d'une configuration non vierge illisible sans
   transformation automatique de NVS ni formatage de secours.
2. Provisionner le Wi-Fi par Improv : sauvegarde des credentials puis du flag,
   réponse sur le port demandeur ; aucune bascule native pendant ce boot.
3. Redémarrer : Wi-Fi sauvegardé, vrai flux HTTP, prébuffer de 64 Kio puis MSC.
   Vérifier l'identité produit et RADIO.MP3 virtuel de 1 Gio.
4. Lire dix minutes avec le vrai producteur réseau, dont au moins un passage
   au-delà de l'ancienne limite de 131 s ; vérifier sur le lecteur Metronic 477144
   que l'audio est continu et consigner les interruptions éventuelles.
5. Une interruption/rétablissement amont ciblé : reprise HTTP/Wi-Fi observable,
   pas de blocage durable ; ne pas relancer une qualification exhaustive Wi-Fi.
6. Trois déconnexions/reconnexions USB propres, alimentation indépendante pour
   distinguer rebasage et redémarrage ; reprise de la fenêtre courante.
7. BOOT maintenu 3 s : flag provisioning, clear des credentials puis reset RTC ;
   retour Improv/JTAG, reprovisioning et lecture à nouveau.
8. Mesurer heap/pile pendant lecture et reprise avec les moyens du banc.
   Déclarer les mesures indisponibles comme telles ; les réservations ELF ne
   remplacent pas le high-water mark.

La campagne #66 (producteur synthétique, une carte S3 et Windows) reste une preuve
des briques, pas une preuve de ce nouveau produit. L'acceptation #67 demande
l'ESP32-S3 Zero et le lecteur cible ; un autre banc/hôte ne les remplace pas.
Pas de campagne OTA ni de coupures en plein effacement/écriture dans ce mandat.
La limitation EKV en coupure de courant est déjà suivie par rust-mcu-libs #70.

## Campagne matérielle — 15 octobre 2026

### Matériel, versions, méthode

| Élément | Valeur |
| --- | --- |
| Produit testé | `2b6b3c56` (HEAD de la PR #9) ; bibliothèque `rust-mcu-libs@671ca69` |
| Image nominale | artefact CI `streambewi-esp32s3-ci` (run 38708445660), `streambewi-esp32s3.bin`, SHA-256 `087e914bd8b821c85537929cfd83aa1eb3b455c6e166482dd2ed89bf2302a5c2` |
| Flash de l'image | image fusionnée à 0x0, **sans effacement global**, par le banc (esptool-js, Web Serial) ; le bootloader trouve `config` data/0x40 @0x9000 et `factory` @0x50000 |
| Carte | **ESP32-S3 DevKitC-1** (QFN56 rév. v0.2, flash 16 Mio, pont UART CH343) — **pas la Waveshare ESP32-S3 Zero attendue** |
| Lecteur | **PC Windows** lisant le volume brut avec `tools/qual/verify_mp3.py` (continuité des trames MP3) pour l'essentiel de la campagne ; **Metronic 477144 branché en fin de campagne** (essais de lecture, pause, stop/play seulement) |
| Observation | UART0 (CH343) pour Improv et la capture du banc ; USB natif (GPIO19/20) vers le PC ; alimentation par le câble UART, donc indépendante du câble USB natif |
| Sauvegarde | aucune sauvegarde complète : une lecture de 16 Mio a bloqué la page du banc sans résultat ; carte de développement, sauvegarde non exigée par le mainteneur |
| Pilotage | Improv par un client écrit pour le banc (`tx` UART), BOOT simulé par GPIO0 tenu bas via DTR (pas d'appui physique) |

L'image nominale est **muette** (journaux en RAM seulement, aucun moyen de les lire). Pour mesurer, une **variante instrumentée** a été construite sur la branche `qual/67-instrumented` (jamais à fusionner) : même code et mêmes politiques, avec en plus la console de journaux sur UART0, la sonde pile/heap du banc et les journaux `ZEROFILL` et de la classe MSC. Ses images (application seule écrite à 0x50000, `config` conservée) :

| Image | SHA-256 | Source | Usage |
| --- | --- | --- | --- |
| `instr` | `ab1582a1…489e5a` | `37db38b` | lecture de 10 min |
| `instr2` | `37f3a27a…df5fb9` | `0156457` | pause / stop, premiers cycles USB |
| `instr3` | `5e1d97b3…d3d` | `dbce369` | cycles USB, coupure amont, BOOT |

Les résultats de la variante sont **instrumentés** et séparés de ceux de l'image CI.

### Résultats

| # | Essai | Résultat | Preuve |
| --- | --- | --- | --- |
| 1 | Flash de l'image CI, boot à config vierge | **PASS** | bootloader ESP-IDF, table `config` + `factory`, application chargée à 0x50000 |
| 2 | Config vierge : Improv sur UART0, pas de MSC | **PASS (partiel)** | Improv répond `authorized`, `info` = `streambewi / 0.2.0 / ESP32-S3`. Port USB-JTAG et absence de disque côté Windows : **NON EXÉCUTÉ** (non observé) |
| 3 | Config non vierge illisible | **PASS** | région `config` entièrement remplie d'octets sans structure : `STREAMBEWI:CONFIG-MOUNT` sur UART0, aucun octet Improv, aucun formatage |
| 3b | Config partiellement corrompue (2 pages sur 64) | **observation** | le montage EKV l'accepte, le firmware démarre : ce n'est pas le cas « illisible » testé en 3 |
| 4 | Provisioning Wi-Fi par Improv | **PASS** | états `provisioning` puis `provisioned`, réponse sur UART0 (port demandeur) ; `credentials and OTG flag saved; restart required for MSC` |
| 5 | Pas de bascule JTAG → OTG pendant ce boot | **PASS** | aucun événement `usb:`/`msc:` entre le provisioning et le redémarrage (0 ligne) |
| 6 | Redémarrage : credentials, Wi-Fi, vrai flux HTTP, prébuffer 64 Kio avant MSC | **PASS** | HTTP 200 sur l'URL Radio France ; prébuffer 86–88 Kio avant `enabling MSC`, boot → disque ≈ 10 s |
| 7 | Volume et fichier virtuel | **PASS** | étiquette `RADIOUSB`, `RADIO.MP3` 1 073 741 824 o, 64 secteurs/cluster, 2 097 413 secteurs, données au LBA 261 |
| 8 | Identité USB/SCSI (VID/PID, INQUIRY) | **NON EXÉCUTÉ** | commandes INQUIRY observées côté carte, mais chaînes et VID/PID non relus côté hôte |
| 9 | Lecture ≥ 10 min, vrai flux, > 131 s | **PASS** (lecteur de substitution) | 619 s, 12,8 Mo, 26 164 trames MP3 à 128 kbit/s, **0 secteur nul au milieu du flux**, 0 bloc répété, 0 reconnexion HTTP ; débit du flux 15,72 Kio/s ; avance moyenne 2 979 o ; 15 attentes de 1,05 à 1,36 s (gigue réseau) |
| 10 | Lecture audio sur le Metronic | **PASS (partiel)** | énumération, INQUIRY / TEST UNIT READY / READ CAPACITY, puis lecture séquentielle au rythme du direct (65 Kio toutes les 4 s, aucun `ZEROFILL`) ; audio entendu par le mainteneur. Durée continue de 10 min **non mesurée** sur le Metronic |
| 11 | Pause puis reprise | **PASS** | aucun zéro ; Icecast coupe la connexion bloquée, reconnexion en 2,6 s, l'audio saute du temps écoulé |
| 12 | Stop puis relecture depuis l'offset 0 | **FAIL (Metronic)** | PC : 1 968 secteurs nuls servis. **Metronic** : après stop puis play, il relit le début du fichier (21 secteurs, LBA 261–281) et reçoit des zéros (`ZEROFILL Expired`), puis n'envoie plus aucune lecture : plus de son, jusqu'à un débranchement/rebranchement (mesuré deux fois) |
| 12b | Pause puis reprise sur le Metronic | **PASS** | pause de 10 s : `consumed` figé, reprise à la même position, aucun `ZEROFILL`, ni reconnexion ni reset |
| 13 | Coupure puis rétablissement du réseau amont | **FAIL** | voir ci-dessous : le flux reprend, mais l'USB ne se rétablit pas sans reset de la carte |
| 14 | Trois déconnexions/reconnexions USB propres | **PASS** | trois fois `suspended`, `session end`, `bus reset`, `addressed`, `configured`, nouvelle session, commandes de montage, lecture saine ; aucun reboot (alimentation UART). Quatre rebranchements rapprochés ensuite : tous montés |
| 14b | État bloqué observé une fois (3 rebranchements à 20 s sans lecture, image `instr2` sans journaux MSC) | **NON EXPLIQUÉ** | disque non monté jusqu'à un reset de la carte ; non reproduit avec `instr3` |
| 15 | BOOT maintenu ≈ 3 s | **PASS** | `recovery: provisioning flag and Wi-Fi cleared; restarting`, `rst:0x10 RTCWDT_RTC_RST`, retour Improv ; reprovisioning (deuxième tentative), redémarrage, disque relu |
| 16 | Mémoire | **PASS (instrumenté)** | heap 55 220 o au maximum sur 98 304 ; pile 17 120 o au maximum sur 44 448 réservés (15 392 avant les journaux MSC) ; stables sur > 30 min avec reconnexions |

### Correctif prototypé pour stop puis play (non inclus dans cette PR)

Cause démontrée : après un stop, le Metronic rouvre le fichier à l'offset 0, qui date du début de la session et est sorti de la fenêtre de 96 Kio ; la politique historique répond par des zéros et le lecteur abandonne.

Prototype sur la branche `qual/67-instrumented` (`8a48bb4`, journal `34f4a78`, **jamais à fusionner tel quel**) : si la lecture du **premier secteur** du fichier tombe sur un secteur expiré, la session est rebasée sur le bord du direct, avec la même retenue de 64 Kio qu'à l'énumération, et la lecture est refaite. Aucune valeur de fenêtre, de prébuffer, d'avance ni d'attente ne change ; tout autre secteur expiré reste servi comme `Expired`.

- Test hôte ajouté (`reopening_the_file_rebases_instead_of_serving_zeroes`) : les 20 tests historiques et le nouveau passent.
- Mesure sur le Metronic : `REBASE on re-read of sector 0 base 44234 -> 842954 live=908490` (soit `live − 65 536`), **0 `ZEROFILL`**, données servies 0,18 s après le rebasage, lecture continue ensuite, **le son revient**.
- Conséquence audible : le flux repris est le flux en cours à environ 4 s du direct ; l'audio écoulé pendant le stop est sauté (radio en direct).
- Non vérifié avec ce correctif : pause puis reprise (la logique ne la touche pas, mais l'essai n'a pas été rejoué), coupure du réseau amont, durée de 10 minutes sur le Metronic.
- C'est un changement de comportement de la politique historique : il n'est publié que sur la branche de qualification, en attente de la décision du mainteneur.

### Anomalie : coupure du réseau amont (essai 13)

Journal de la carte (variante `instr3`), coupure du point d'accès à 10:25:10 :

1. Le lien Wi-Fi tombe ; le flux ne reçoit plus rien et reste bloqué jusqu'à `body read error ConnectionReset` (10:25:33), puis des `request error Dns` jusqu'au retour du Wi-Fi (10:25:43) et HTTP 200 (10:25:45).
2. Pendant la coupure, le MSC attend les données : un `ZEROFILL status=Pending after 5001–5003 ms` toutes les 5 s (6 au total). Une lecture de 8 secteurs peut attendre 8 × 5 s = 40 s.
3. **Windows réinitialise le bus à 10:25:40, avant la reprise du flux** : la cause démontrée est la durée d'attente cumulée d'une commande, au-delà de ce que tolère Windows, et non la reprise du flux.
4. Après la reprise, les sessions suivantes (10:26:21, 10:26:41, 10:27:21, 10:27:41, 10:28:22) se terminent avec `reads=0` ou `reads=1` : la carte annonce `msc: connected` mais Windows n'envoie plus aucune commande. Le disque n'est revenu qu'après un reset de la carte.

Non prouvé : pourquoi Windows ne renvoie plus de commandes (le script de lecture est resté bloqué dans un `ReadFile` pendant cet essai, ce qui peut y contribuer). Le délai par secteur est la politique historique de `app/src/usb.rs` (`PENDING_TIMEOUT` de 5 s), conservée par la migration.

### Observation : zéros servis à un lecteur qui relit le début du fichier

La politique historique répond sans attendre par des zéros aux secteurs sortis de la fenêtre de 96 Kio (`Expired`). À chaque session fraîche, Windows lit environ 64 Kio au montage : la fenêtre avance alors de façon que **96 secteurs (48 Kio, ≈ 3 s d'audio) de tête de fichier sont déjà expirés** pour un lecteur qui relit le début (96 `ZEROFILL` côté carte = 96 secteurs nuls côté hôte, à chaque cycle). Un lecteur qui lit séquentiellement depuis l'énumération ne les voit pas ; un lecteur qui revient au début après un stop en voit autant que la session est ancienne (1 968 secteurs après environ 5 min). Un octet nul peut faire échouer le lecteur matériel : **le comportement du Metronic face à ces zéros n'est pas connu**.

### Limites et points hors périmètre signalés

- **Wi-Fi** (hors périmètre) : la première tentative de connexion échoue à chaque boot avec `DisassociatedDueToInactivity` ; la reprise après 1 s réussit. Le provisioning Improv n'effectue qu'une tentative : une fois `unable to connect`, une seconde tentative a réussi. Déjà suivi dans #72.
- Carte S3 Zero, Metronic, identité USB relue côté hôte, port USB-JTAG pour Improv, anomalie 14b : voir « NON EXÉCUTÉ » et « NON EXPLIQUÉ ».
- Aucun correctif appliqué au produit : les politiques (fenêtre 96 Kio, prébuffer 64 Kio, avance 80 Kio, attente 5 s / 5 ms, BOOT 3 s) sont conservées à la demande du mainteneur.
- Les journaux, les captures et les sorties de scripts sont dans la capture du banc (`serial.log`, non versionnée).

### Verdict

**La qualification de #67 n'est pas acquise.** Sur ESP32-S3 DevKitC-1, la chaîne StreamBeWI complète fonctionne (config, provisioning, redémarrage, vrai flux, prébuffer, MSC, lecture continue de 10 min sans secteur nul sous Windows, déconnexions USB, récupération BOOT) et le Metronic lit et reprend après une pause. Mais : (1) l'essai d'acceptation exige la carte S3 Zero, indisponible ; (2) **sur le Metronic, stop puis play ne redonne plus de son** (zéros servis pour le début du fichier) ; un correctif ciblé est prototypé et validé en mesure, en attente de décision ; (3) la coupure du réseau amont mène à un USB non rétabli sans reset (FAIL).

Conditions avant fusion : décision sur le correctif stop/play (l'inclure, le modifier ou l'écarter) et, s'il est retenu, son essai complet sur le Metronic (pause, 10 min, coupure amont) ; décision du mainteneur sur la politique d'attente lors d'une coupure amont (plafonner la durée par commande, ne pas répondre par des zéros, ou accepter la limite) ; essai sur la carte S3 Zero ; relecture de l'identité USB côté hôte.
