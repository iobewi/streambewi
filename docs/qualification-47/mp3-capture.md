# Complément #47 : contenu servi après REBASE (échec M6)

Question : l'échec de reprise après un STOP prolongé vient-il d'un début de fichier virtuel invalide, non reconnu
ou insuffisamment synchronisé ?

## Capture (feature `mp3-capture`, d'après l'enregistreur de #31)

Différence avec #31 : celui-ci gardait les secteurs audio d'indice 8 et plus ; M6 n'a lu que le secteur 0.
`app/src/mp3_capture.rs` enveloppe le disque virtuel : ce sont les octets rendus à la classe MSC, qui les envoie
sans les modifier (lecture du code de `iobewi-usb-msc` ; la capture n'est pas prise sur le bus). À chaque REBASE
elle garde les 16 premiers secteurs du fichier tels que servis la première fois, enregistre chaque lecture avec
son CRC et compare octet à octet chaque relecture. Export différé de 25 s, une ligne toutes les 200 ms au plus ;
son coût UART est compris dans `tx_us`. Environ 9,7 Ko sur le tas.

Analyse hors ligne : `tools/qual/mp3_capture_report.py uart.log [dossier]`. Elle vérifie le CRC de chaque secteur
exporté, la concordance des relectures, la première trame confirmée par la suivante, la chaîne des trames, ses
ruptures et `main_data_begin`. Testée sur des trames synthétiques.

## Artefact

| | |
|---|---|
| Source | `0213d109d7acd3e99c7b1471a894cbf45dc3302b` |
| Base | candidat Wi-Fi #113, `rust-mcu-libs@876d1404d2bdb70dfdc68684867fea70a3e87620`, `Cargo.lock` inchangé |
| Configuration | `FEATURES=usb-debug,msc-trace,mp3-capture,board-devkitc-1 scripts/build-image.sh OUT` |
| Application seule | `0x50000`, 1 277 648 o, SHA-256 `23958292bc6de059b12b6fd7778eda8761a7a74a505f932961362766dfb3ac24` |
| Secteurs écrits | `0x50000` à `0x188000` (partition jusqu'à `0x1D0000`) ; `config` non touchée |
| Pile réservée | 41 992 o (minimum 36 864) ; marge réelle non mesurée |

## Recette

Station HTTP Icecast. Capture UART IDLab en lecture seule. Après chaque PLAY, **90 s sans aucun geste** : l'export
commence à 25 s. Pas de requête à `/api/logs` pendant les essais (tas).

1. Témoin : l'épisode du démarrage (lecture normale acceptée) est exporté et vérifié.
2. A : STOP 5 s, PLAY, 90 s.
3. B : STOP 60 s, PLAY, 90 s.

L'opérateur dit seulement si le son est revenu. **Règles, fixées par l'opérateur avant la livraison :** trois essais
de STOP prolongé au plus. Après un premier refus : un deuxième essai seulement si ce refus montre une anomalie MP3
absente des scénarios acceptés ; sinon la campagne se termine en documentant les limites de l'observation, ce qui ne
réfute pas globalement l'hypothèse. La consommation mémoire et l'export UART sont surveillés au passage. Cette
recette clôt l'investigation de #47.

Arrêt aussi si la trace est incomplète (`lost`, `DROPPED`, secteur exporté incomplet ou CRC faux) : l'essai ne
compte pas.

## Critères, fixés avant les mesures

Un « défaut » est : une relecture différente des premiers octets servis ; des octets nuls ; aucune suite de trames
cohérente ; des paramètres de flux (version, débit d'échantillonnage) incohérents ; une rupture de chaîne. Le
début au milieu d'une trame n'est un défaut que s'il distingue les épisodes refusés des épisodes acceptés.

| Conclusion | Condition |
|---|---|
| **Réfutée** | au moins un refus, dont les octets servis sont vérifiés, sans aucun défaut observable, et dont les propriétés (relectures, paramètres, position de la première trame) se retrouvent dans un épisode accepté |
| **Confirmée** | au moins deux refus, tous avec un même défaut absent de tous les épisodes acceptés |
| **Indéterminée** | tout autre cas : aucun refus, un seul refus avec un défaut propre à lui (indice seulement), données insuffisantes |

**Limite connue d'avance.** En M6 le Metronic n'a lu que le secteur 0 : 512 octets, soit une trame et le début
d'une autre à 128 kbit/s. La continuité des trames d'un refus ne sera alors vérifiable que sur ces octets, et
« réfutée » ne porterait que sur eux : la capture ne montre pas ce que le Metronic n'a pas demandé. #33 a déjà
observé des arrêts avec des trames valides sur les 6 à 8 Kio servis (autre station).

**Exécution :** [mp3-results.md](mp3-results.md). Après les deux premiers essais longs, l'opérateur a remplacé la borne
de trois essais par des essais libres jusqu'au premier refus ; les critères de conclusion sont inchangés.
