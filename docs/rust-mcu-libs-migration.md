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
