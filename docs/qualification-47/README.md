# Caractérisation de l'activité MSC du Metronic 477144 (#47)

Statut : campagne M0 à M6 exécutée le 27 novembre 2026, arrêtée sur l'échec de reprise de M6. Résultats :
[results.md](results.md).

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

**Effet de l'observation, à mesurer en M0.** L'écriture sur UART0 (115 200 bauds) est bloquante et partage
l'exécuteur avec l'USB : une ligne d'environ 120 octets pourrait l'occuper jusqu'à environ 10 ms (estimation). Les lignes sont agrégées par
fenêtre de 1 s, et chaque ligne `S` donne le coût mesuré (`lines`, `tx_us`, `txmax_us`).
La résolution des silences est celle des temps `msct` (1 ms) ; l'ordre entre une ligne `msct` et une ligne
produit (REBASE, `IncompleteBody`) n'est garanti qu'à environ 1 s.

## Artefact à livrer

Instrumentation portée sur le candidat Wi-Fi #113 installé sur la carte (`2f0d3e5`, application `2a80a339…`).

| | |
|---|---|
| Source | `e2c530fabdfcf94d11a3e5fe0e94fc55f3456a4d` (branche `diag/47-msc-activity-wifi113`) |
| `rust-mcu-libs` | `876d1404d2bdb70dfdc68684867fea70a3e87620`, non modifiée |
| Configuration | `FEATURES=usb-debug,msc-trace,board-devkitc-1 scripts/build-image.sh OUT` (`--no-default-features`) |
| Outils | rustc esp 1.97.0-nightly (8ea53bcd7 2026-07-08), espflash 4.7.0 |
| Application seule | `0x50000`, 1 272 432 o, SHA-256 `0ea65cfabbb813b2e936f50426b6a37233b15bc5ba7fc3c1bb9464511d14f0ec` |
| Image complète (**à ne pas écrire** : son remplissage couvre `config`) | `b20f25a7c3e6beae71bcbd2271cee9bb1a7a09d3811f503c4ebfb93f50d6ad6b` |
| ELF | `95ecfe2046e2c4abbcb93e54f7ed85bc4b14d00d1f46a532284f88c3d365ff18` |

Secteurs écrits : `0x50000` à `0x187000` (fin de la partition : `0x1D0000`). `config` (`0x9000` à `0x49000`) n'est
pas touchée. Les octets `0x0` à `0x9000` de l'image sont identiques à ceux du candidat #113.

Écart avec ce candidat : `app/Cargo.toml`, `app/src/lib.rs`, `app/src/msc_trace.rs`, `app/src/usb.rs`,
`scripts/build-env.sh`, `targets/esp32/Cargo.toml`. `Cargo.lock` est inchangé. Le build de diagnostic diffère
aussi par `usb-debug` : le plafond `release_max_level_info` est retiré et les traces `debug!` sont compilées,
filtrées à l'exécution.

Branches antérieures, non destinées à la carte : `diag/47-msc-activity` (`main`, format OTA, `259c965`) et
`diag/47-msc-activity-classic` (`d99a5e6`, ancienne `rust-mcu-libs`). Elles n'ont pas la mesure du coût UART.

**Équivalence du chemin audio avec `main`.** Entre `8c54266` et `main`, `usb.rs` et `stream.rs` ne diffèrent que
par des chemins d'import et un commentaire ; `stream_policy.rs` est l'ancien `core/src/lib.rs` sans son attribut
`no_std`. `main` épingle `rust-mcu-libs@f9a2a9a` ; les crates USB, FAT16 et tampon sont identiques en `876d140`.
Le reste (OTA, démarrage, Wi-Fi) diffère.

## Montage et collecte

- Metronic sur le port USB natif de la carte, IDLab sur l'UART.
- Station HTTP Icecast sélectionnée (scénario de #47).
- Collecte : `tools/qual/msc_capture.py OUT`, lecture seule, arrêt en créant `OUT/stop`.
- Les gestes de l'opérateur ne sont pas horodatés : le délai entre un appui et la réaction MSC est
  `NOT_MEASURED`. Les durées citées sont les silences des READ(10) sur l'horloge de la carte.

## Recette

Avant M0 : capture ouverte, lecture établie depuis au moins 60 s.

| Test | Gestes | Observation après reprise |
|---|---|---|
| M0 | au moins 60 s de lecture sans geste | porte ci-dessous ; cadence et plus long silence en lecture |
| M1 | PAUSE, 5 s, PLAY | 30 s au moins |
| M2 | PAUSE, 10 s, PLAY | 30 s au moins |
| M3 | PAUSE, 20 s, PLAY | 30 s au moins |
| M4 | PAUSE, 60 s, PLAY | 30 s au moins |
| M5 | STOP, 5 s, PLAY | 30 s au moins |
| M6 | STOP, 60 s, PLAY | 30 s au moins |

Après chaque test, l'opérateur dit si le son est revenu. S'il ne revient pas en 60 s, il le signale avant tout
autre geste.

Arrêt de la campagne : `lost` non nul ou `DROPPED` (trace incomplète : test à refaire), perte du banc, ou lecture
qui ne revient pas. Aucun reset sans nouvel accord.

## Porte M0 : effet des traces sur le MSC

M1 à M6 ne commencent qu'après un M0 réussi et l'accord de l'opérateur. Contrôle :
`tools/qual/msc_m0_check.py OUT DEBUT_MS FIN_MS` (temps de la carte). Seuils fixés avant la mesure :

| Contrôle | Seuil |
|---|---|
| Trace complète | `lost=0`, aucun `DROPPED`, aucune erreur de session, lignes `S` espacées de 1,5 s au plus |
| MSC intact | aucun ZEROFILL, retrait du bus, événement de bus, commande en échec ou abandonnée |
| Débit lu par l'hôte | `consumed` à 16 000 o/s (128 kbit/s) ± 5 % |
| Coût UART | au plus 100 ms par fenêtre de 1 s, 20 ms pour la plus longue écriture |
| Son | normal, jugé par l'opérateur seul |

**Limite.** Sans la trace, l'activité MSC n'est pas observable : il n'y a pas de comparaison A/B à ce niveau.
La référence sans trace est W3 de #113 (`docs/qualification-113/`) : 10 min sans coupure, `consumed` à environ
16 000 o/s, station HTTPS. Un M0 réussi montre que les indicateurs disponibles sont nominaux, pas que la trace
est sans effet sur la cadence des commandes.

## Exploitation

| Information | Source |
|---|---|
| Commandes SCSI, START STOP UNIT | `msct C` (CDB complet), `msct R` |
| LBA, secteurs, cadence | `msct R` : `n`, `lba`, `blk`, `gap`. Position dans `RADIO.MP3` : `(lba − 261) × 512` |
| Suspend, Resume, Reset | `msct B` |
| `live`, `consumed` | `msct S`, chaque seconde |
| Commande en attente de données | `msct S open=…+âge`, `svc` |
| REBASE, `IncompleteBody`, ZEROFILL | lignes produit existantes |
| Délai action → changement MSC | `NOT_MEASURED` |
| Position de reprise | dernier LBA avant le silence comparé au premier après |

Critère de décision, fixé avant les mesures. Un état n'est dit **distinguable** que si une signature le sépare
des autres dans tous ses essais et n'apparaît pas dans M0. PAUSE et STOP sont comparés à durée égale (M1/M5,
M4/M6). Un silence des READ(10) ne suffit pas s'il en existe d'aussi longs en lecture normale. Sept essais, un
par condition, donnent une indication et non une statistique.
