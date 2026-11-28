# Candidat matériel 2 — correctif R1-P1 (PR #48 à `0e759b7`)

**Conclusion : campagne arrêtée par l'opérateur, qui déclare : « je ne suis pas satisfait du fonctionnement du
stop ». Le candidat n'est pas accepté. La PR #48 n'est pas qualifiée sur le matériel.** Aucune modification
fonctionnelle, aucune fusion, aucune issue fermée.

Les décomptes ci-dessous respectent les critères écrits (aucun échec sur les STOP comptés) mais ne remplacent pas
ce jugement. Le motif précis de l'insatisfaction n'a pas été détaillé. Le dossier du premier candidat
([README.md](README.md)) est conservé sans changement.

## 1. Candidat et livraison

| Élément | Valeur |
|---|---|
| SHA de la PR validé en revue | `0e759b779a87d9577731c18549de255818fc2893` (un commit après `720eb7e`) |
| Portage classique | `qual/47-pr48-classic`, `7bd58be60e21d8cfad2401362710e2dcd4886ee3` |
| Image testée (`usb-debug,msc-trace,board-devkitc-1`) | `qual/47-pr48-classic-trace`, `afc82829297330d1ab46117c2f457a6f925e0d29` |
| Application à `0x50000` | 1 275 120 octets, SHA-256 `eb9f93991449e21f3ee34241fc78857501d871e465e5667ea7c6a12fe8479103` |
| Image de production, **non livrée** | 1 235 664 octets, SHA-256 `e1598e13fb2263507d26e8570be61a125e7d5a0e7b28500e1e5b9b5a73191787` |
| `rust-mcu-libs` | `876d1404d2bdb70dfdc68684867fea70a3e87620` |

Adaptation classique : un import du nouveau test (`streambewi_core::HEAD_SECTORS`). Lignes modifiées du correctif
identiques à la PR. Tests sur PC réussis : production 169 + 11 + 1 + 20 ; instrumentée avec `msc-trace`
177 + 11 + 1 + 20 ; Clippy et format conformes dans ces configurations. Sans `msc-trace`, Clippy signale 4
éléments inutilisés sur la branche instrumentée, déjà présents avant le correctif.

Livraison autorisée : application seule en un bloc ; application précédente `e127ce95…` vérifiée ; relecture
identique ; bootloader, table et configuration inchangés (configuration `d716e9ed…`) ; un seul reset.
Journal : [`evidence-candidat-2/delivery.log`](evidence-candidat-2/delivery.log).

## 2. Résultats

Critères : ceux du premier dossier. Le son est jugé par l'opérateur.

| Essai | Observation technique | Opérateur | Résultat |
|---|---|---|---|
| Q47-06 STOP/PLAY | 12 STOP comptés (§3) | séries 1 à 3 | 10 nominaux, 2 dégradés, 0 échec ; **non accepté** (voir conclusion) |
| Q47-01 lecture 900 s | 16 001 o/s ; avance 55 296–67 584 o ; silence max 277 ms | « 15 minutes d'écoute RAS » | PASS |
| Q47-03 PAUSE 120 s | 121,4 s ; drain 1 923 072 o ; LBA suivante | son au 1er PLAY | PASS nominal (20 et 60 s : résultats du candidat 1 réutilisés, non rejoués) |
| Q47-07 coupure courte | reconnexion forcée UniFi ; tampon épuisé avant le retour HTTP | « coupure de quelques secondes, son revenu seul » | INDÉTERMINÉ : coupure assez courte non obtenue |
| Q47-08 coupure avec famine | Wi-Fi absent ~78 s ; détachement après épuisement ; audio relu ~83 s après la perte, ~5 s après le Wi-Fi | « son revenu seul après environ 20 secondes » | PASS |
| Q47-09 coupure pendant STOP | réseau absent ~219 s dans un STOP de ~296 s ; aucun détachement ; PLAY ~60 s après le retour | son au 1er PLAY | PASS nominal |
| Q47-10 station pendant drain | disque hors bus 2,9 s ; flux HTTPS 200 ; relecture automatique ; 16 123 o/s | « la radio est revenue seule » ; station entendue **non confirmée** | INDÉTERMINÉ (trace conforme) |
| Q47-11 HTTPS | PAUSE : arrêt du lecteur après reprise (§4) ; **STOP prévu non exécuté** | voir §4 | NON TERMINÉ, anomalie |
| Q47-12 endurance | — | — | non exécuté |
| Q47-02, 04, 05 | non rejoués sur ce candidat | — | voir candidat 1 |

## 3. Q47-06

| Série | STOP comptés | Nominaux | Dégradés | Mis à part |
|---|---|---|---|---|
| 1 | 3 | 2 | 1 | 2 cycles à signature de PAUSE (reprise à la LBA suivante) |
| 2 | 4 | 3 | 1 | 1 cycle : erreur PAUSE/STOP signalée par l'opérateur |
| 3 | 5 | 5 | 0 | — |
| **Total** | **12** | **10** | **2** | 3 |

Durées des STOP comptés : environ 60 à 75 s, un de 119 s. La cause des signatures de PAUSE n'est pas établie.
La reprise après la coupure réseau de Q47-09 n'est pas incluse dans ces 12.

- **Le refus au premier PLAY persiste** (2 sur les 12 STOP comptés). Le second PLAY (19,6 s et 10,6 s plus tard) a
  déclenché `REBASE on explicit head retry` et rétabli le son.
- **Les deux refus n'avaient lu que le premier secteur du fichier (LBA 261)** : le candidat 1 aurait aussi permis
  ce nouvel essai.
- **Le cas visé par R1-P1 (refus après lecture des LBA 261 à 263) ne s'est pas reproduit.** Il est couvert par les
  tests sur PC et **n'est pas vérifié sur le matériel**. Des ouvertures réussies ont lu ces LBA.
- Aucun taux fiable ne peut être déduit de cet échantillon, et la disparition du blocage n'est pas démontrée.

## 4. Anomalie en Q47-11 (station 2, HTTPS)

Extrait : [`evidence-candidat-2/q47-11-excerpt.log`](evidence-candidat-2/q47-11-excerpt.log).

1. PAUSE d'environ 53 s, drain engagé.
2. PLAY : reprise à la LBA suivante ; 90 112 octets lus en ~5,4 s (LBA 9657 à 9832), puis **plus aucune lecture**.
   Opérateur : « la lecture a repris 2 3 secondes puis s'est coupée » ; compteur « 001 sans indication de play ».
3. Second PLAY, environ 3 min après le premier : ouverture depuis le début, REBASE, son rétabli ;
   15 992 o/s sur les 10 dernières secondes analysées.

Côté carte, dans les intervalles examinés : aucune erreur HTTPS ou USB, lecture en échec, détachement ni
`ZEROFILL`. Le lecteur a cessé de lire environ 8 Ko après la fin des 81 920 octets retenus pendant la pause,
c'est-à-dire juste après la jonction avec l'audio plus récent. **C'est une coïncidence, pas une cause établie** :
les reprises après PAUSE en HTTP comportaient aussi une telle jonction et ont tenu. Un seul essai : ni le HTTPS,
ni la jonction, ni le lecteur ne peuvent être mis en cause.

Classement **non arbitré** : les critères ne prévoient pas un son qui revient puis s'arrête. Il n'est pas compté
comme nominal. La proposition « dégradé, avec arrêt après reprise » est restée sans réponse.

## 5. Autres observations

- Q47-08 : un `ZEROFILL #1` (`command_elapsed=5003 ms`), seul de la capture, sur la lecture restée en attente au
  détachement. La commande (LBA 4557) ne s'est jamais terminée : abandonnée à la reconnexion (`st=abort`). La trace
  ne permet pas de dire si des octets nuls ont atteint l'hôte. Non classé comme défaut ; à examiner au regard du
  principe « pas de silence généré ».
- Q47-09 : la LED est restée verte pulsante après le retour du flux, puis fixe après la reprise de lecture. Cause
  non vérifiée.

## 6. Écarts au protocole

1. Portage classique et image instrumentée ; image de production non testée.
2. Q47-06 en trois séries de 5 au lieu de 20 cycles, pour limiter les manipulations ; 3 cycles mis à part.
3. Ordre suivi : Q47-06, 01, 03, 07 à 11. Q47-03 limité à 120 s.
4. Q47-07 : un seul essai, sans multiplier les investigations (décision antérieure de l'opérateur).
5. Q47-08 et 09 : durées de blocage effectives plus longues que la consigne d'une minute (78 s et 219 s d'absence).
6. En fin de campagne, la capture montre deux changements de station (vers 1 puis vers 2) non commandés par
   l'exécutant, environ une minute avant la remise sur la station 1 par l'exécutant. Leur auteur n'est pas établi
   par la trace.

## 7. Contrôles sur la capture complète

Aucune ligne `ERROR`, `panic`, `IncompleteBody` ni trace de reset depuis le début de la capture continue (le
démarrage est dans `boot.log`) ; aucune commande avec statut d'échec ; 2 commandes abandonnées (`st=abort`), aux
deux détachements de Q47-07 et Q47-08 ; 1 `ZEROFILL` ; `lost=0` ; intervalle maximal entre lignes `S` de
1 106 ms ; 2 `REBASE on explicit head retry`. Les `WARN` sont : les échecs et pertes Wi-Fi des essais réseau,
2 abandons de connexion HTTP sans données, 1 échec SNTP et le `ZEROFILL`. La capture a débuté après la lecture
d'un arriéré de 62 337 octets non inclus.

## 8. État final et limites

- Station 1 rétablie et vérifiée par l'API ; 2 stations et réglage LED conservés ; services prêts ; capture arrêtée.
- La carte porte toujours l'image instrumentée `eb9f9399…`. L'autorisation de livraison et de reset est consommée.
- Lecture nominale, PAUSE de 120 s en HTTP et récupération réseau (Q47-08, Q47-09) : conformes sur les essais faits.
- **STOP non accepté par l'opérateur.** Non conclus : coupure absorbable, station entendue après changement,
  HTTPS avec STOP, endurance, firmware de production.

## Preuves

| Fichier | Contenu |
|---|---|
| [`evidence-candidat-2/delivery.log`](evidence-candidat-2/delivery.log) | livraison et vérifications |
| [`evidence-candidat-2/boot.log`](evidence-candidat-2/boot.log) | démarrage après l'unique reset |
| [`evidence-candidat-2/uart-full.log.gz`](evidence-candidat-2/uart-full.log.gz) | capture continue, adresses et nom du réseau Wi-Fi masqués |
| [`evidence-candidat-2/q47-11-excerpt.log`](evidence-candidat-2/q47-11-excerpt.log) | anomalie Q47-11 |
