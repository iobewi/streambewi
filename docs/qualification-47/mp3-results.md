# Résultats du complément MP3 (#47), 27 novembre 2026

Firmware : source `0213d109d7acd3e99c7b1471a894cbf45dc3302b`, application
`23958292bc6de059b12b6fd7778eda8761a7a74a505f932961362766dfb3ac24` (`evidence/mp3-delivery.log` : relue,
configuration inchangée). Traces : `evidence/mp3-recipe-uart.log` (le démarrage de la recette) ; analyse de chaque
épisode exporté : `evidence/mp3-episodes-report.txt`.

## Déroulement

Témoin au démarrage, STOP 5 s, puis deux STOP d'environ 60 s, tous acceptés. L'opérateur a ensuite remplacé la
borne de trois essais longs par des essais libres, arrêts de durées variées, jusqu'au premier refus. **Un refus a
été obtenu** après un arrêt de 62,0 s : même suite que M6 (LBA 259, 1, 259, 261, 261, puis plus aucune lecture,
observé 95 s).

16 REBASE dans ce démarrage : 15 suivis de la lecture du fichier, 1 refus. Arrêts de 4,3 à 62,0 s. Les trois
arrêts d'environ 60 s : deux acceptés, un refusé. Certains épisodes n'ont pas été exportés, ou en partie, quand un
essai suivait de près ; leurs commandes restent tracées.

## Octets servis : refus comparé aux acceptés

| | Refus (épisode 17) | Acceptés exportés |
|---|---|---|
| Octets vérifiés (CRC) | 512 (secteur 0 seul demandé) | 512 à 8 192 |
| Relectures différentes | 0 sur 1 | aucune sur l'ensemble |
| Octets nuls | non (au plus 2 de suite) | non |
| Première trame | octet 256 | octet 0, 128 ou 256 |
| En-têtes dans le secteur 0 | un seul, à l'octet 256 | à 256 aussi pour les épisodes 3, 7 et 13, acceptés |
| Format de l'en-tête | MPEG-1 Layer III, 128 kbit/s, 48 kHz, sans CRC | identique |
| `main_data_begin` de la première trame | 462 | 193 à 483 sur les épisodes dont la première trame est à 128 ou 256 |
| Chaîne de trames | **non vérifiable** : l'en-tête suivant (octet 640) n'a pas été demandé | sans rupture sur les octets exportés |

## Conclusion

Aucune anomalie MP3 observable ne distingue les octets du refus de ceux d'épisodes acceptés : même format,
relectures identiques, et la même position de première trame (256) a été acceptée trois fois.

- **Sur les 512 octets servis : l'hypothèse n'est pas soutenue.** Ni le début au milieu d'une trame, ni la position
  256, ne suffisent à expliquer le refus.
- **Hypothèse dans son ensemble : INDÉTERMINÉE.** Un seul refus capturé ; la continuité des trames au-delà de
  l'octet 512 n'est pas vérifiable ; la capture est prise à l'entrée de la classe MSC, pas sur le bus. Comme fixé par
  l'opérateur, l'absence d'anomalie dans ces octets n'est pas une réfutation globale.

Le critère « réfutée » écrit avant la mesure est satisfait pour les seuls octets capturés, à condition de tenir une
chaîne non vérifiable pour une absence d'observation et non pour un défaut.

## Observations pour la conception du correctif

- La suite « REBASE puis `IncompleteBody` » de #47 est apparue après des STOP acceptés : elle n'entraîne pas à elle
  seule le refus. Après le REBASE du refus (comme en M6), aucun `IncompleteBody` n'a été journalisé ; l'hôte n'ayant
  presque rien consommé, cela ne dit rien de l'état de la connexion.
- La cause de l'arrêt des lectures par le Metronic reste inconnue.

## Mémoire et export

Sur ce démarrage (environ 20 min) : tampons alloués (9 216 o), aucune panique, aucun redémarrage, aucun manque de
mémoire signalé, `lost=0`, aucun octet perdu par le banc ni erreur de session, aucun ZEROFILL ni commande en échec.
Coût UART au plus 58 ms par seconde et 14,1 ms par écriture. Le tas libre et la marge de pile n'ont pas été mesurés.
L'effet de l'instrumentation sur le comportement du Metronic ne peut pas être exclu ; le même refus avait été
observé avec `msc-trace` seul (M6).
