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
| Produit | `2b6b3c56` (portage) puis, sur cette branche, le correctif stop/play `b4f0289` ; bibliothèque `rust-mcu-libs@671ca69` |
| Image nominale | artefact CI du run 38708445660, SHA-256 `087e914bd8b821c85537929cfd83aa1eb3b455c6e166482dd2ed89bf2302a5c2` ; flashée à 0x0 sans effacement global |
| Carte | **ESP32-S3 DevKitC-1** (QFN56 rév. v0.2, 16 Mio de flash, pont UART CH343) — **pas la Waveshare ESP32-S3 Zero attendue** |
| Lecteurs | PC Windows (volume brut, `tools/qual/verify_mp3.py`) pour les essais de bas niveau ; **Metronic 477144** pour la lecture, la pause, le stop/play, la coupure amont et le retrait du câble |
| Alimentation | par le câble UART : indépendante du câble USB natif |
| Sauvegarde | aucune sauvegarde complète de la carte (la lecture de 16 Mio a bloqué la page du banc) ; carte de développement, non exigée par le mainteneur |
| Pilotage | Improv écrit sur UART0 par un client du banc ; BOOT simulé par GPIO0 tenu bas (DTR), pas d'appui physique |

L'image nominale est **muette** (journaux en RAM seulement). Les mesures internes viennent de variantes **instrumentées**, jetables (console UART0, sonde pile/heap du banc, journaux `ZEROFILL`/`REBASE`/MSC, traces du pilote USB), construites par `apply_qual.py` sur les branches `qual/67-instr-v2`, `v4` et `v5` : à ne jamais fusionner. Leurs résultats sont des résultats **instrumentés**.

### Résultats

| # | Essai | Résultat | Preuve |
| --- | --- | --- | --- |
| 1 | Flash de l'image CI, boot à config vierge | **PASS** | bootloader ESP-IDF, tables `config` + `factory`, application à 0x50000 |
| 2 | Config vierge : Improv sur UART0 | **PASS** | état `authorized`, `info` = `streambewi / 0.2.0 / ESP32-S3`. Port USB-JTAG et absence de disque côté hôte : **NON EXÉCUTÉ** |
| 3 | Config non vierge illisible (région entière) | **PASS** | `STREAMBEWI:CONFIG-MOUNT` sur UART0, aucun Improv, aucun formatage |
| 3b | Config corrompue sur 2 pages sur 64 | observation | montée sans erreur par EKV : ce n'est pas le cas « illisible » de l'essai 3 |
| 4 | Provisioning Improv, réponse sur le port demandeur | **PASS** | `provisioning` puis `provisioned`, `credentials and OTG flag saved; restart required for MSC` |
| 5 | Aucune bascule JTAG → OTG pendant ce boot | **PASS** | 0 événement `usb:`/`msc:` entre le provisioning et le redémarrage |
| 6 | Redémarrage : credentials, Wi-Fi, vrai flux HTTP, prébuffer 64 Kio avant MSC | **PASS** | HTTP 200 ; prébuffer 83–92 Kio avant `enabling MSC` ; boot → disque ≈ 10 s |
| 7 | Volume et fichier virtuel | **PASS** | `RADIOUSB`, `RADIO.MP3` 1 073 741 824 o, 32 Kio/cluster, 2 097 413 secteurs |
| 8 | Identité USB/SCSI relue côté hôte (VID/PID, chaînes INQUIRY) | **NON EXÉCUTÉ** | INQUIRY observé côté carte, chaînes non relues |
| 9 | Lecture continue de 10 min, vrai flux, > 131 s | **PASS (PC)** | 619 s, 12,8 Mo, 0 secteur nul au milieu du flux, 0 reconnexion HTTP |
| 10 | **Metronic : lecture continue de 10 min** (firmware sans correctif d'attente, `b4f0289`) | **PASS** | 620 s, 10,14 Mo à 15,98 Kio/s, 0 `ZEROFILL`, 0 rebasage, 0 reset, 0 reconnexion ; audio sans défaut selon le mainteneur |
| 11 | **Metronic : pause puis reprise** | **PASS** | reprise à la même position, 0 `ZEROFILL`, ni reconnexion ni reset (vérifié sur `b4f0289` et sur le firmware final) |
| 12 | **Metronic : stop puis play** (5 s, 30 s, 2 min, 5 s) | **FAIL puis PASS avec `b4f0289`** | avant : le lecteur relisait le début du fichier, recevait des zéros et restait muet ; après : un `REBASE` à chaque reprise, 0 `ZEROFILL`, le son revient (4 cycles, puis 2 cycles sur le firmware final) |
| 13 | Trois déconnexions USB propres (+ 4 rapprochées) | **PASS** | réénumération, montage et lecture, sans reboot (alimentation UART) |
| 14 | BOOT maintenu ≈ 3 s | **PASS** | flag et credentials effacés, `rst:0x10 RTCWDT_RTC_RST`, retour Improv, reprovisioning (deuxième tentative), disque relu |
| 15 | Mémoire (instrumenté) | **PASS** | heap 58 620 o au maximum sur 98 304 ; pile 17 120 o au maximum sur 44 416 réservés |
| 16 | **Coupure amont, Windows, firmware de référence** | **FAIL** | Windows réinitialise le bus 30 s après le début, puis boucle ; seul un reset de la carte rétablit le disque |
| 17 | **Coupure amont, Windows, budget par commande + récupération au reset** | **PASS** | script terminé seul, aucun reset de bus ; 7 commandes de 8 secteurs à ≈ 5,0–5,1 s chacune (55 secteurs nuls) ; reprise à 12:52:20 |
| 18 | **Coupure amont, Metronic, sans soft-déconnexion** (PR #10 seule) | **LIMITE du lecteur** | le Metronic abandonne une lecture après ≈ 1 s, essaie 3 fois, puis coupe son port USB (`suspended`) ; play ne le rallume pas, seul un rebranchement le fait |
| 18b | **Coupure amont, Metronic, avec soft-déconnexion** (PR #11) | **PASS** (2 essais de suite) | à l'épuisement du tampon (≈ 5 s) le disque quitte le bus ; au retour du flux avec un prébuffer frais il revient, le Metronic réénumère et la lecture **repart seule**, 0 reset ; coupure de ≈ 35 s |
| 18c | Coupure courte (3 s), pause, stop/play, sur le firmware avec soft-déconnexion | **PASS** | la coupure est absorbée par le tampon (le disque reste sur le bus) ; stop/play et pause sans régression |
| 18d | **Metronic figé une fois** (aucune lecture ni événement de bus après un stop/play) | **NON EXPLIQUÉ, non reproduit** | un seul cas sur 17 cycles stop/play et plusieurs séquences ; la carte était à l'écoute et c'est le lecteur qui n'envoyait plus rien (plantage du lecteur selon le mainteneur) ; les octets servis au rebasage sont journalisés depuis, sans corrélation trouvée (un début non aligné sur une trame passe) |
| 18e | **Coupure amont sous Windows avec soft-déconnexion** | **Observation (hôte hors cible)** | le disque **reste monté** et sain pendant la coupure (35 s) : Windows balaie tout le fichier (lectures lointaines servies tout de suite, sondage de la fin) sans consommer au bord du direct, donc aucun lecteur n'est affamé ; le tampon plein freine le producteur, qui ne lit plus la connexion et ne voit pas la coupure. Aucune erreur ni reset. Un déclencheur « producteur muet depuis 6 s » essayé puis **retiré** (jamais validé sur le Metronic, sans effet sous Windows) |
| 19 | **Retrait du câble en plein transfert, puis rebranchement** | **FAIL puis PASS** | avant : la carte ne répondait plus (INQUIRY sans suite) jusqu'à un reset ; avec l'adaptateur corrigé : trois essais de suite, séquence de commandes complète à chaque fois |
| 20 | Lecture audio sur la S3 Zero | **NON EXÉCUTÉ** | carte indisponible |

### Stop puis play

**Cause démontrée** : après un stop, le Metronic rouvre le fichier à l'offset 0, qui date du début de la session et est sorti de la fenêtre de 96 Kio ; la politique historique répondait par des zéros et le lecteur abandonnait.

**Correctif intégré à cette branche** (`b4f0289`, `core/src/lib.rs`, avec un test hôte ; CI verte) : si la lecture du **premier secteur** du fichier tombe sur un secteur expiré, la session est positionnée comme à l'énumération (`begin_session`, même retenue de 64 Kio) puis la lecture est refaite. Aucune valeur de fenêtre, de prébuffer, d'avance ou d'attente ne change ; tout autre secteur expiré reste servi comme expiré.

**Effet audible, corrigé** : pendant un stop, la contre-pression retient le producteur à 80 Kio d'avance sur le lecteur. Le rebasage place donc le fichier 64 Kio avant le **dernier octet reçu**, et non avant le direct d'aujourd'hui : après un stop assez long pour que le serveur ferme la connexion, le lecteur rejoue environ **4 à 5 s d'audio déjà téléchargé**, puis la reconnexion le fait sauter au direct (mesuré : 2 min d'arrêt ⇒ environ 5 s d'ancien audio). Il n'y a ni zéros ni coupure. Une explication antérieure de ce bilan (« reprise à environ 4 s du direct ») était fausse pour les arrêts longs. Ne pas rejouer ces secondes demanderait une règle différente (écarter les données retenues), non retenue : le mainteneur a demandé la règle de l'énumération.

### Coupure du réseau amont et blocage persistant

Trois causes distinctes ont été démontrées et traitées séparément (branche produit `fix/67-read-budget`, branche bibliothèque `feat/67-read-command-budget` de `rust-mcu-libs`, aucune des deux fusionnée) :

1. **Attente cumulée par secteur.** Une lecture de 8 secteurs pouvait attendre 8 × 5 s = 40 s, plus que le délai de Windows. `iobewi-usb-msc` annonce maintenant chaque READ(10) à la politique (`ReadPolicy::command_started`, sans effet par défaut) et le produit applique le **même seuil de 5 s à toute la commande** : la première attente de 5 s est inchangée, les secteurs suivants de la commande sont servis immédiatement. Mesuré sous Windows : une attente de 5,0 à 5,1 s par commande.
2. **Commande non abandonnée au reset du bus.** Quand l'hôte réinitialisait le bus pendant une lecture en attente, la classe poursuivait l'ancienne commande dans la nouvelle configuration (les points d'accès avaient été réactivés), poussait des données périmées et ne lisait plus la commande suivante. Le gestionnaire de la classe compte désormais les resets et la boucle de lecture abandonne la commande (`Disabled`) : test de dialogue qui échoue sans le correctif (le contrôle négatif émet les 4 secteurs périmés et un CSW).
3. **Point d'accès IN resté armé (cause racine du blocage persistant).** Si le câble est retiré pendant qu'un envoi est armé sur le point d'accès IN, il le reste après le reset suivant : `embassy-usb` ne désactive pas les points d'accès sur un reset, `embassy-usb-synopsys-otg` 0.4 réécrit `DIEPCTL` sans effacer `EPENA`, et son désarmement ne s'exécute qu'avec la détection VBUS, absente sur l'ESP32-S3. Le premier envoi de la session suivante attend pour toujours « le transfert précédent » : l'hôte lit sa commande, n'obtient jamais la réponse, réinitialise le bus toutes les quelques secondes et abandonne ; seul un reset de la carte rétablit. Reproductible à volonté : lecture à pleine vitesse sous Windows (`verify_mp3.py --start-sector 100000`) puis retrait du câble. `iobewi-esp-usb` enveloppe maintenant le pilote d'esp-hal et désarme les points IN restés actifs après chaque reset de bus (même séquence SNAK + EPDIS que le pilote), sans changer l'API de construction. Contournement d'un comportement amont, à retirer quand le pilote le corrigera ; non testable sur l'hôte.

4. **Interruption du flux plus longue que le tampon (PR #11, empilée sur #10).** Le Metronic abandonne une lecture après ≈ 1 s, trois fois, puis coupe son port ; des zéros le font planter ; aucune politique d'attente ne le retient. Quand un lecteur est affamé (`Pending`) alors que le producteur est muet depuis 2 s, le périphérique **quitte le bus** (soft-disconnect, `DCTL.SDIS`, exposé par `iobewi-esp-usb::set_connected`) et se **représente** quand le flux a livré un prébuffer frais de 64 Kio, comme au démarrage. La session consommateur est **conservée à travers cette reconnexion** : un premier essai avait montré que le Metronic reprend à son ancien décalage dans le fichier, que rebaser sur le direct plaçait 610 Kio devant le flux (attente de 38 s, abandon, port coupé) ; s'il revient au début du fichier, le correctif stop/play le rebase. Fenêtre, prébuffer, avance et seuils d'attente inchangés.

Limites : sous Windows le disque reste monté pendant une coupure (voir l'essai 18e : l'hôte lit le fichier entier sans consommer au bord du direct, le tampon plein freine le producteur), ce qui est sans conséquence pour la cible ; un hôte qui lit au bord du direct et se retrouve affamé se déconnecterait comme le Metronic ; le tampon de 5 s absorbe les coupures plus courtes sans effet. Le MP3 n'a pas de marqueur de flux : le fichier virtuel d'1 Gio (≈ 18 h) est la seule façon de dire « très long » à un hôte de fichiers, d'où les comportements de fichier (accès aléatoire, balayage, relecture du début) que les correctifs adaptent.

### Zéros servis à un lecteur qui relit le début du fichier

La politique historique répond sans attendre par des zéros aux secteurs sortis de la fenêtre. À l'énumération, Windows lit environ 64 Kio : **96 secteurs (48 Kio, ≈ 3 s) de tête sont alors déjà expirés** pour un lecteur qui relit le début (96 `ZEROFILL` = 96 secteurs nuls côté hôte, à chaque session). Le Metronic, qui lit séquentiellement, ne les voit pas, et le correctif stop/play le sert correctement quand il revient au début. Un lecteur qui relirait le début au milieu du fichier ne serait pas couvert.

### Limites et points hors périmètre signalés

- **Wi-Fi** (hors périmètre, déjà suivi dans #72) : la première tentative de connexion échoue à chaque boot (`DisassociatedDueToInactivity`) ; la reprise après 1 s réussit. Le provisioning Improv n'effectue qu'une tentative : après `unable to connect`, la seconde a réussi.
- Carte S3 Zero, identité USB relue côté hôte, port USB-JTAG pour Improv : **NON EXÉCUTÉ**.
- Aucun correctif des politiques historiques (fenêtre 96 Kio, prébuffer 64 Kio, avance 80 Kio, attente 5 s / 5 ms, BOOT 3 s) ; les valeurs sont inchangées.
- Mesures mémoire en variante instrumentée seulement.

### Branches et commits

| Dépôt | Branche | Contenu | État |
| --- | --- | --- | --- |
| `streambewi` | `migration/67-rust-mcu-libs` (PR #9) | portage, bilan, **correctif stop/play** `b4f0289` | CI verte |
| `streambewi` | `fix/67-read-budget` (PR #10, brouillon) | budget par commande + épinglage `rust-mcu-libs@0b68389` | CI verte (2/2) ; à relire |
| `streambewi` | `feat/67-stall-soft-disconnect` (PR #11, brouillon, empilée sur #10) | soft-déconnexion sur flux interrompu + session conservée | CI verte (2/2) ; à relire |
| `rust-mcu-libs` | `feat/67-read-command-budget` (PR #78, brouillon) | hook `command_started` (`5df3c4a`), abandon au reset du bus (`61d51d6`), désarmement IN de l'adaptateur S3 (`0b68389`), `set_connected` (`6b0e492`) | CI verte (6/6) ; à relire |
| `streambewi` | `qual/67-instr-v2`, `v4`, `v5`, `v6` | instrumentation jetable | ne pas fusionner |

### Verdict

**La qualification de #67 n'est pas acquise.** La chaîne StreamBeWI complète fonctionne sur le Metronic (lecture de 10 min, pause, quatre cycles stop/play avec le correctif de cette branche), et le comportement du produit face au retrait du câble et aux resets du bus est corrigé dans les branches séparées. Il reste : (1) la carte **S3 Zero** et la lecture de 10 minutes sur ce couple ; (2) la relecture de l'identité USB côté hôte ; (3) la décision de relire et fusionner les deux branches de correctifs (budget d'attente, récupération au reset du bus, désarmement IN), puis de rejouer sur le Metronic les essais de cette campagne : fait avec le firmware final (lecture de 11 min, pause, stop 5 s et 30 s : 0 `ZEROFILL`, 0 reset, 0 avertissement) ; (4) la relecture des PR #10 et #11 et de `rust-mcu-libs#78` ; la coupure du flux de plus de 8 s, que le Metronic ne supportait pas, est traitée par la soft-déconnexion de #11 (validée sur le Metronic sur deux essais).

### Relecture des PR de correctifs (15 octobre 2026)

Relecture de `streambewi#10`, `streambewi#11` et `rust-mcu-libs#78` après les essais. Le relecteur est l'auteur : une relecture indépendante reste à faire.

| Trouvaille | Gravité | Traitement |
| --- | --- | --- |
| #11 : un producteur **freiné** par le lecteur (fenêtre pleine) était compté comme muet. Un hôte qui lit **devant** le bord du flux (Windows sonde jusqu'à 1 Mio : `Pending` qui n'avancent pas la position) ou qui ne lit pas ferait quitter le bus au disque pour un flux sain, **sans retour possible** (un prébuffer frais ne peut pas s'écrire dans une fenêtre pleine). Le Metronic, qui ne lit jamais devant le flux, n'y était pas exposé. | **défaut** | corrigé (`445e898`) : l'attente de place est `SharedStream::wait_for_room()`, qui compte comme activité ; test hôte avec contrôle négatif (il échoue sans la correction) |
| #78 : attente de confirmation de désarmement bornée à 100 000 itérations par point d'accès dans une section critique (pire cas ≈ 60 ms sans interruptions) | mineur | ramenée à 10 000 (`e9f3061`) ; la borne du dernier point d'accès (6) est justifiée par `StateStorage<7>` d'esp-hal |
| #10 : le budget de 5 s couvre maintenant la commande entière ; une lecture de plus de ≈ 80 Kio **au bord du direct** (qui demande plus de 5 s de flux) aurait sa fin remplie de zéros alors qu'elle était attendue secteur par secteur avant | **risque noté** | non observé (Metronic ≈ 3 secteurs par lecture ; Windows 8 secteurs avec le script) ; à surveiller |
| #11 : le drapeau « session conservée » reste levé si l'hôte ne revient jamais ; une session ultérieure d'un autre hôte reprendrait alors sans rebaser | bénin | les décalages restent cohérents ; le correctif stop/play rebase si le début du fichier est expiré |
| #78 : `MscClass::bus_reset_counter()` ajoute une API publique (diagnostic et test) | note | documentée |

Après ces corrections, les révisions relues sont : `streambewi` `feat/67-stall-soft-disconnect` **`e3840c3`** (CI verte 2/2), `rust-mcu-libs` `feat/67-read-command-budget` **`e9f3061`** (CI verte 6/6) ; la PR #10 reste épinglée sur `rust-mcu-libs@0b68389`, ce qui suffit (elle n'utilise pas `set_connected`).

### Images

| Image | Révisions | SHA-256 | Usage |
| --- | --- | --- | --- |
| **nominale** (muette) `streambewi-esp32s3-e3840c3-factory.bin` | `streambewi@e3840c3`, `rust-mcu-libs@e9f3061` | `721140fa7fc74def7afaeabef6e7d59d021133cada27a2fbfb6659ed42b4d445` | image fusionnée à 0x0 (réinitialise `config` : reprovisioning) |
| **nominale** `streambewi-esp32s3-e3840c3-app.bin` | idem | `d9555efda4bad28a38104adb4aa566bfe7a89994db88d84026b83d3a9d273d2c` | application seule à 0x50000 (conserve `config`) |
| **instrumentée v10** `streambewi-qual-v10-app.bin` | `streambewi@1d12daa` (avec le déclencheur « producteur muet 6 s », retiré ensuite) + instrumentation, `rust-mcu-libs@6b0e492` | `4b5eb28094c2a3a5eb63d084c483f229447b4dbf551f795b41b42c2f722d2a5b` | essais Windows de la coupure ; conservée avec sa capture (brute et SSID masqué) et son manifeste |

La campagne sur le Metronic a été menée avec les variantes instrumentées `v7` à `v9` (`31779da` plus instrumentation, mêmes comportements) ; l'image nominale ci-dessus n'a **pas encore été exécutée** sur matériel.
