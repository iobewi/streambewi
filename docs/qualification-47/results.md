# Résultats M0 à M6 (27 novembre 2026)

Firmware : source `e2c530fabdfcf94d11a3e5fe0e94fc55f3456a4d`, application `0ea65cfabbb813b2e936f50426b6a37233b15bc5ba7fc3c1bb9464511d14f0ec`
(`evidence/delivery.log`). Station HTTP Icecast, Metronic 477144. Journaux expurgés dans `evidence/`.

M0 : `m0-uart.log`, temps carte 56655 à 243480, porte M0 réussie. M1 à M6 : `m1-m6-uart.log`, à partir du temps
carte 996556 (les lignes antérieures du fichier viennent du tampon du banc). `first-series-superseded-uart.log` est
une première série dont les gestes n'ont pas pu être attribués : elle n'entre pas dans le tableau. Elle contient
une pause d'environ 55 s avec `IncompleteBody` et un silence de 8,3 s sans erreur.

| Essai | Silence MSC | Commande autre que READ(10) | Première lecture après PLAY | REBASE | `IncompleteBody` | Reprise |
|---|---|---|---|---|---|---|
| M0 lecture | 270 ms au maximum | aucune | sans objet | non | non | sans objet |
| M1 PAUSE 5 s | 5,8 s | aucune | à la suite | non | non | oui |
| M2 PAUSE 10 s | 11,9 s | aucune | à la suite | non | non | oui |
| M3 PAUSE 20 s | 21,4 s | aucune | à la suite | non | oui, +1,6 s | oui |
| M4 PAUSE 60 s | 60,3 s | aucune | à la suite | non | oui, +0,7 s | oui |
| M5 STOP 5 s | 6,0 s | aucune | LBA 259 (répertoire) | oui | non | oui |
| M6 STOP 60 s | 61,5 s | aucune | LBA 259 (répertoire) | oui | non observé | **non** |

- Pendant l'arrêt, PAUSE et STOP ne se distinguent pas : les READ(10) cessent, sans commande ni événement USB.
- À PLAY ils se distinguent sur ces essais : PAUSE continue à la LBA suivante ; après STOP l'hôte relit le
  répertoire, la FAT et le début du fichier. M5 parcourt ensuite la FAT du secteur 1 au 129.
- M6 : après LBA 259, 1, 259, 261, 261, plus de lecture. Un second PLAY 20 s plus tard donne la même suite.
- « Reprise : oui » pour M1 à M5 repose sur la reprise des lectures dans la trace ; le son n'a pas été confirmé
  essai par essai dans cette série. M6 : absence de son signalée par l'opérateur.
- Un essai par condition. Aucun seuil de détection n'est fixé.
