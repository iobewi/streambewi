# Lectures concurrentes du volume : limite connue

Statut : **limite consignée, non corrigée** dans la campagne de qualification #67.
Hors du scénario Metronic (un seul lecteur). Aucun correctif rapide n'est ajouté.

## Symptôme

Quand deux lecteurs utilisent le volume en même temps, l'un peut recevoir des secteurs
remplis de zéros après une série d'attentes de 5 s (une par commande, `ZEROFILL
status=Pending command_elapsed≈5000 ms`).

## Mécanisme

1. Le lecteur A lit séquentiellement, par exemple au secteur 288.
2. Le lecteur B (indexation Windows, ou second accès) relit le secteur 0. Le cœur
   répond par un `begin_session` (`REBASE on re-read of sector 0`) : la base du fichier
   se déplace vers le bord live moins la rétention.
3. Le lecteur A continue à ses anciens décalages. Avec la nouvelle base, le secteur 288
   correspond à `read_end + avance maximale` (ici 2 304 568 = 2 222 648 + 81 920).
4. Le producteur est freiné à `read_end + avance maximale` et une lecture `Pending` ne
   fait pas avancer `read_end` : les données demandées ne peuvent jamais arriver.
   Chaque commande expire à 5 s et est remplie de zéros.

## Reproduction (DevKitC-1, 15 octobre 2026, image finale instrumentée)

- Fenêtre 96 Kio, préchargement 64 Kio, avance maximale 80 Kio.
- Windows garde le volume ouvert pendant que `verify_mp3.py \\.\D:` lit en séquence.
- Observé : REBASE à 16:07:49.706 (base 2 057 728 → 2 157 112, live 2 222 648), puis
  40 secteurs nuls (fichier 320–359) et cinq attentes de 5 s ; `consumed` reste à
  2 222 648 alors que `live` monte à 2 294 653.
- Une seconde lecture, seule, après 90 s d'inactivité : 1 088 secteurs, 0 secteur nul.

## Pourquoi pas de correctif ZeroFill

Répondre aussitôt par des zéros supprimerait l'attente mais laisserait le déplacement de
session perturber l'autre lecture. Le défaut est dans la coexistence de deux lecteurs
avec une session unique, pas dans la durée de l'attente. Une correction demande une
décision de conception (une session par lecteur, ou interdiction du rebase tant qu'un
autre lecteur est actif), à traiter dans un chantier séparé.

## Hors périmètre Metronic

Le Metronic est un lecteur unique : les essais stop/play (5 s, 30 s), pause, coupure
Wi-Fi et reconnexion n'ont pas montré ce comportement.
