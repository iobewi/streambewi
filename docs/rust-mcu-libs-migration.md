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
