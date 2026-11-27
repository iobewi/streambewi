# Caractérisation de l'activité MSC du Metronic 477144 (#47)

Statut : firmware de diagnostic compilé, **non livré**. Aucune mesure matérielle.

## Ce que les diagnostics existants ne montraient pas

Lecture du code, `rust-mcu-libs@f9a2a9a` (les crates USB, FAT16 et tampon sont identiques en `876d140`).

| Besoin | `usb-debug` seul |
|---|---|
| Commandes SCSI | Invisibles : la politique par défaut garde `iobewi_usb_msc` à `warn`. Seule l'image `qual` la relève, et `qual` embarque aussi le code de coupure de flux `qual-drop`. |
| Toute la durée de la recette | Non : au plus 24 commandes hors READ(10) par connexion. |
| LBA, nombre de secteurs, cadence des READ(10) | Non : un cumul toutes les 256 commandes (`lba=min..max`). |
| START STOP UNIT | Code et longueur seulement, sans le CDB (bits START et LOEJ). |
| Suspend, Resume, Reset | Oui (`BusLog`), sans horodatage. |
| `live`, `consumed` | Une ligne toutes les 60 s. |
| REBASE, `IncompleteBody` | Oui, sans horodatage. |

## Instrumentation ajoutée : feature `msc-trace`

`app/src/msc_trace.rs` enveloppe le pilote USB et lit les CBW et les CSW des endpoints bulk. Aucune modification
de `rust-mcu-libs`. Les octets, leur ordre, la politique de lecture, le tampon et le client HTTP sont inchangés.
Sans la feature, le module n'est pas compilé dans le firmware. Le format des lignes `msct` est décrit en tête du
module. Les temps sont en millisecondes depuis le démarrage, sur l'horloge de la carte.

**Effet de l'observation, non mesuré.** L'écriture sur UART0 (115 200 bauds) est bloquante et partage
l'exécuteur avec l'USB : une ligne d'environ 120 octets l'occupe environ 10 ms. Les lignes sont agrégées par
fenêtre de 1 s. M0 sert de témoin : `lost=0`, aucun `DROPPED`, aucun ZEROFILL, son jugé normal par l'opérateur.
La résolution des silences est celle des temps `msct` (1 ms) ; l'ordre entre une ligne `msct` et une ligne
produit (REBASE, `IncompleteBody`) n'est garanti qu'à environ 1 s.

## Artefacts

Configuration commune : `FEATURES=usb-debug,msc-trace,board-devkitc-1 scripts/build-image.sh OUT`
(`--no-default-features`, donc sans le plafond `release_max_level_info`), rustc esp 1.97.0-nightly,
espflash 4.7.0, `rust-mcu-libs@f9a2a9a4099dd43c3e9eb0b1410fc3a523286f53`.

| Variante | Source | Artefact | SHA-256 |
|---|---|---|---|
| `main` (format OTA) | `259c965` sur `e0c192c` | `streambewi-esp32s3.bin` (4 Mio, première installation) | `1e3634590b372247a0a74daca3a82936352c9ad1294dc74a04c664c93b4d0264` |
| `main` (format OTA) | idem | `streambewi-esp32s3-ota.bin` | `3a84b31f46f34a92fa4f53b0d7c2671228afbfeb071d76d9f36b26de1f5d31d9` |
| classique | `d99a5e6` sur `8c54266` | application seule, `0x50000`, 1 272 432 o | `757c44b5961b75efc26085c3be0fd76e57572e1a39390d6c4a513f283fbbb14d` |
| classique | idem | image complète, 1 600 112 o | `b06db3ed83ccad3ffa7d813859d082c65dfaafc7a43cc7103bef4a9349e25286` |

La carte est au format classique. L'image `main` change la table de partitions et déplace `config` de `0x9000`
vers `0x380000`, vierge dans l'image : le Wi-Fi et les stations seraient à ressaisir. La variante classique
s'écrit sur l'application seule (secteurs `0x50000` à `0x187000`, fin de partition `0x1D0000`) et ne touche pas
`config` (`0x9000` à `0x49000`).

**Équivalence du chemin audio.** Entre `8c54266` et `main`, `usb.rs` et `stream.rs` ne diffèrent que par des
chemins d'import et un commentaire ; `stream_policy.rs` est l'ancien `core/src/lib.rs` sans son attribut
`no_std`. La révision de `rust-mcu-libs` est la même. Le reste du firmware (OTA, démarrage) diffère.

## Montage et collecte

- Metronic sur le port USB natif de la carte, IDLab sur l'UART.
- Station HTTP Icecast sélectionnée (scénario de #47).
- Collecte : `tools/qual/msc_capture.py OUT`, lecture seule, arrêt en créant `OUT/stop`.
- Actions : l'opérateur ouvre `tools/qual/msc-marker.html` et clique en appuyant sur la touche du Metronic.
  Le rapprochement avec la carte passe par l'heure du PC et `chunks.tsv` (heure d'arrivée côté hôte). Précision
  attendue de l'ordre de la seconde, à condition que les deux horloges soient synchronisées : leur écart est
  vérifié avant M0 et noté. Un délai action → MSC inférieur à cette précision est `NOT_MEASURED`.

## Recette

Avant M0 : capture ouverte, lecture établie depuis au moins 60 s.

| Test | Gestes (chacun marqué) | Observation après reprise |
|---|---|---|
| M0 | Début, 60 s de lecture sans geste, Fin | témoin : cadence et plus long silence en lecture |
| M1 | PAUSE, 5 s, PLAY | 30 s au moins |
| M2 | PAUSE, 10 s, PLAY | 30 s au moins |
| M3 | PAUSE, 20 s, PLAY | 30 s au moins |
| M4 | PAUSE, 60 s, PLAY | 30 s au moins |
| M5 | STOP, 5 s, PLAY | 30 s au moins |
| M6 | STOP, 60 s, PLAY | 30 s au moins |

Pour chaque test, l'opérateur note : délai ressenti avant le silence et avant le retour du son, position de
reprise à l'oreille (direct, reprise, redémarrage), anomalie, affichage du Metronic. Il clique « Son revenu »
quand le son revient. Si le son ne revient pas en 60 s, il le signale avant tout autre geste.

Arrêt de la campagne : `lost` non nul ou `DROPPED` (trace incomplète : test à refaire), perte du banc, ou lecture
qui ne revient pas. Aucun reset sans nouvel accord.

## Exploitation

| Information | Source |
|---|---|
| Commandes SCSI, START STOP UNIT | `msct C` (CDB complet), `msct R` |
| LBA, secteurs, cadence | `msct R` : `n`, `lba`, `blk`, `gap`. Position dans `RADIO.MP3` : `(lba − 261) × 512` |
| Suspend, Resume, Reset | `msct B` |
| `live`, `consumed` | `msct S`, chaque seconde |
| Commande en attente de données | `msct S open=…+âge`, `svc` |
| REBASE, `IncompleteBody`, ZEROFILL | lignes produit existantes |
| Délai action → changement MSC | marqueur et `chunks.tsv` |
| Position de reprise | dernier LBA avant le silence comparé au premier après |

Critère de décision, fixé avant les mesures. Un état n'est dit **distinguable** que si une signature le sépare
des autres dans tous ses essais et n'apparaît pas dans M0. PAUSE et STOP sont comparés à durée égale (M1/M5,
M4/M6). Un silence des READ(10) ne suffit pas s'il en existe d'aussi longs en lecture normale. Sept essais, un
par condition, donnent une indication et non une statistique.
