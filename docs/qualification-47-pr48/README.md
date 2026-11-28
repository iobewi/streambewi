# Qualification matérielle de la PR #48 (issue #47) — dossier partiel

> **Deux candidats matériels distincts.** Ce document décrit le **candidat 1** (PR à `720eb7e`) et reste inchangé.
> Le **candidat 2** (correctif R1-P1, PR à `0e759b7`), campagne arrêtée par l'opérateur, est décrit dans
> [candidat-2.md](candidat-2.md).

**Conclusion : Q47-06 est en échec (blocage durable après STOP). La campagne est suspendue à la demande de
l'opérateur pour soumettre le constat à la revue. La disponibilité du service audio après STOP n'est pas
qualifiée.** Aucune modification fonctionnelle, aucune fusion, aucune issue fermée.

## 1. Candidat et livraison

| Élément | Valeur |
|---|---|
| SHA audité de la PR #48 | `720eb7ea0392acca0b67d38e326e2ed11ac8df1d` (inchangé) |
| Portage classique | `qual/47-pr48-classic`, `b6132ea` |
| Image testée (instrumentée `usb-debug,msc-trace`) | `qual/47-pr48-classic-trace`, `e1b6747` |
| `rust-mcu-libs` | `876d1404d2bdb70dfdc68684867fea70a3e87620` |
| Application à `0x50000` | 1 275 072 octets, SHA-256 `e127ce95699350265398869524a3665ffedffb7ae7ec36d4bea7cbbf7e9d89bb` |
| Application remplacée | SHA-256 `23958292bc6de059b12b6fd7778eda8761a7a74a505f932961362766dfb3ac24` |
| Banc | `board_1` (identité vérifiée avant écriture), Metronic 477144 |

Livraison autorisée par l'opérateur : application seule, en un bloc ; relecture identique ; bootloader et table
inchangés ; configuration identique avant et après (SHA-256 `d716e9ed8e2291ecf97ab5a1570086b71b7c0f83508ecc3ec01c9a06888fd147`) ;
un seul reset. Aucun autre reset ni écriture ensuite.

## 2. Critères fixés par l'opérateur

PASS nominal : son au premier PLAY. PASS dégradé : son au deuxième PLAY, au moins 5 s après le premier.
FAIL : aucun son après deux PLAY, ou intervention nécessaire sur StreamBeWI. Une reprise des READ(10) ne suffit
pas : le son est jugé par l'opérateur.

## 3. Résultats

Les durées sont les silences de READ(10) mesurés dans la trace.

| Essai | Observation technique | Opérateur | Résultat |
|---|---|---|---|
| Q47-01 lecture 15 min (899 s analysées) | 15 999 o/s ; avance 80 896–81 920 o ; silence max 269 ms ; 0 ligne perdue | « 01 OK » | PASS |
| Q47-02 PAUSE | 6,6 s ; drain 124 928 o ; reprise à la LBA suivante | son au 1er PLAY | PASS nominal |
| Q47-03 PAUSE 20 s | 22,1 s ; drain 344 064 o ; LBA suivante | son revenu (1) | PASS nominal |
| Q47-03 PAUSE 60 s | 65,9 s ; drain 1 050 624 o ; LBA suivante | son revenu (1) | PASS nominal |
| Q47-03 PAUSE 120 s | 127,7 s ; drain 2 041 856 o ; LBA suivante | son revenu (1) | PASS nominal |
| Q47-04 STOP 5 s | 7,2 s ; REBASE sur secteur 0 | « RAS » (2) | PASS nominal |
| Q47-05 STOP 60 s | 61,7 s ; REBASE ; drain 970 752 o | « revenu direct » | PASS nominal |
| Q47-05 STOP 120 s | 125,6 s ; REBASE ; drain 2 004 992 o | son revenu (2) | PASS nominal |
| Q47-06 cycles STOP/PLAY | voir section 4 | blocage | **FAIL** |
| Q47-07 à Q47-12 | — | — | non exécutés |

(1) Après les trois pauses, à la question « au premier PLAY à chaque fois ? » : « oui le son est toujours
revenu ». (2) Classé au premier PLAY d'après la trace (une seule séquence de réouverture) et l'absence de second
appui signalé.

Reprises après PAUSE/STOP comptées : **7 nominales, 1 dégradée, 1 échec** (PAUSE : 4 nominales ; STOP : 3
nominales, 1 dégradée, 1 échec). Échantillon réduit : aucun taux ne peut en être déduit.

## 4. Q47-06 : constat

Extrait lisible : [`evidence/q47-06-excerpt.log`](evidence/q47-06-excerpt.log). `t=` est l'horloge de la carte
en millisecondes ; les lignes `msct R` sont agrégées et sortent après les messages `stream:` qu'elles encadrent.

### 4.1 Premier cycle : PASS dégradé

STOP de 62,8 s. Au premier PLAY (`t=11333424`), REBASE puis lectures `259, 1, 259, 261, 261` et arrêt : aucun son.
Second PLAY 50,4 s plus tard (`t=11383944`) : `REBASE on explicit head retry`, lecture et son rétablis.

### 4.2 Cycles ajoutés

Trois cycles d'environ 59 à 61 s ont repris à la LBA suivante, sans REBASE : signature d'une PAUSE. Ils ne sont
pas comptés comme des STOP ; la trace ne dit pas quel bouton a été utilisé.

### 4.3 Dernier cycle : FAIL

STOP de 61,1 s (`t=11870726`). Six ouvertures, toutes sur le même schéma `259, 1, 259, 261`, puis lectures des
LBA **261 à 263**, puis arrêt :

| Ouverture (`t=`) | Écart depuis la lecture précédente | Contexte |
|---|---|---|
| 11870726 | 61,1 s | premier PLAY après STOP, REBASE |
| 11876950 | 6,1 s | PLAY suivant |
| 11885069 | 8,0 s | PLAY suivant ; l'opérateur signale « je suis bloqué » |
| 12043978 | 158,8 s | après la consigne STOP, 10 s, PLAY ; « ça fonctionne pas » |
| 12044560 | 0,4 s | |
| 12059010 | 14,3 s | |

Aucun `REBASE on explicit head retry`. La trace montre des ouvertures, pas les appuis : leur nombre exact n'est
pas connu. L'API indiquait flux, USB et Wi-Fi prêts. Environ 245 s après le premier refus, l'opérateur a agi sur
l'USB côté Metronic (consigne : débrancher ou éteindre 10 s). La trace montre suspension, reset de bus,
énumération, nouvelle session, puis une lecture à 16 109 o/s sur 10 s ; « son revenu ». Aucun reset de la carte.

### 4.4 Analyse

Établi par le code, au SHA audité : `retry_head()` n'autorise le nouvel essai que si
`session.read_end == session.base + SECTOR_SIZE`, c'est-à-dire si **seul le premier secteur du fichier** a été lu
(`app/src/stream_policy.rs:147` sur la PR ; `core/src/lib.rs:149` sur le portage). Le test de `app/src/usb.rs`
attend explicitement l'absence de nouvel essai après lecture de la LBA 262.

| Lectures de l'ouverture refusée | Nouvel essai | Issue observée |
|---|---|---|
| LBA 261 seulement (§4.1, un cas) | déclenché | son rétabli |
| LBA 261 à 263 (§4.3, un épisode) | non déclenché | blocage durable |

Après lecture des LBA 261–263, la garde reste fausse tant que la session et son origine ne changent pas ; la
correspondance étant stable, les ouvertures suivantes reçoivent la même position du flux. C'est une limite du
correctif face à un comportement observé du Metronic.

Non établi : la cause du refus initial par le Metronic (déjà indéterminée lors de la caractérisation #47) ; la
fréquence de chaque variante ; le fait qu'un élargissement de la garde suffirait. Le premier refus n'est pas
supprimé par le correctif, qui n'agit qu'à une ouverture ultérieure. Cette firmware ne capture pas le contenu
servi. Aucun correctif n'est proposé ici : décision réservée à la revue.

## 5. Écarts au protocole

1. Candidat testé : portage de la PR sur la disposition classique, pas `720eb7e` lui-même ; `720eb7e` (CI seule)
   non porté ; un commit d'adaptation de visibilité (`retry_head` rendu `pub`).
2. Image instrumentée ; **l'image de production n'a pas été testée ni livrée**.
3. Après la livraison, le Metronic n'était pas branché (aucune lecture) ; lecture nominale établie ensuite avec
   validation sonore.
4. Première tentative de Q47-02 : aucune pause visible dans la trace, cause inconnue ; essai non compté, refait.
5. Durée de Q47-01 (15 min) proposée par l'exécutant, le mandat n'en fixant pas.
6. Essai STOP 120 s : PAUSE par erreur (124,1 s, reprise réussie), hors décompte ; refait avec STOP.
7. Q47-06 réduit à la demande de l'opérateur (20 cycles puis 1 + 5 prévus), arrêté sur le blocage ; seuls 2 des
   cycles visibles ont une signature STOP.
8. La capture a eu des retards de transmission (environ 40 s observés), sans ligne perdue.
9. Un silence de 2,7 s sans drain (`t=11674217`, LBA contiguës) pendant les cycles ajoutés n'est pas expliqué.

## 6. Contrôles sur la capture complète

Sur toute la capture : aucune ligne `WARN`, `ERROR`, `panic`, `IncompleteBody`, `ZEROFILL` ni trace de reset ;
aucune commande MSC en échec ; 0 ligne de trace perdue (`lost=0`) ; intervalle maximal entre lignes `S` de
1 068 ms ; 5 `REBASE on read of sector 0` et 1 `REBASE on explicit head retry` ; 15 entrées et 15 sorties de
drain (la dernière sortie à la reconnexion USB).

## 7. Disponibilité du service, limites

- Lecture nominale et reprises après PAUSE jusqu'à 127,7 s : conformes sur les essais réalisés.
- STOP : un blocage a exigé une intervention côté consommateur, sans retour possible par les PLAY essayés.
- Non évalués : coupures réseau (Q47-07 à 09), changement de station (Q47-10), HTTPS (Q47-11), endurance après
  récupération (Q47-12). Aucune conclusion sur ces points.
- La station initiale (1) est restée sélectionnée ; aucun changement de configuration.

La carte porte toujours l'image instrumentée. La capture est arrêtée.

## Preuves

| Fichier | Contenu |
|---|---|
| [`evidence/boot.log`](evidence/boot.log) | démarrage après l'unique reset |
| [`evidence/uart-full.log.gz`](evidence/uart-full.log.gz) | capture continue complète, adresses masquées ; débute avant le branchement du Metronic |
| [`evidence/q47-06-excerpt.log`](evidence/q47-06-excerpt.log) | extrait filtré de Q47-06 et du rétablissement |
