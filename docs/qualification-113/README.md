# Qualification matérielle Wi-Fi (rust-mcu-libs #113, PR #115) — 27 novembre 2026

Candidat : StreamBeWI `2f0d3e59dcfbf058fe8e9c8661baebe75264e5c2`, `rust-mcu-libs`
`876d1404d2bdb70dfdc68684867fea70a3e87620`, firmware de production, partitionnement classique, feature
`board-devkitc-1`. Application à `0x50000`, 1 232 800 o, SHA-256
`2a80a3398e7f6f3526beb1d9400ab4718eb954a5aae482eaea10cc33c4ed357f`. ESP32-S3 DevKitC-1, 16 Mio, Metronic 477144,
plusieurs points d'accès sous le même SSID (infrastructure non modifiée). Banc IDLab.

Les journaux de ce dossier sont expurgés : adresse IP, adresse MAC et identifiant de plan remplacés.

| Scénario | Verdict | Preuves | Réserves |
|---|---|---|---|
| W0 livraison | PASS | `w0-flash.log`, `w0-boot.uart.log` | Écriture de l'application seule, relue ; empreinte de `config` identique avant et après. |
| W1 démarrage à froid, 5 | PASS | `w1-events.log`, `w1-uart.log` | Cycle 4 : aucun état `down` échantillonné, durée hors tension non mesurée. Voir ci-dessous. |
| W2 reset, 5 | PASS | `w2-run.log`, `w2-uart.log` | Le reset du banc apparaît en `rst:0x1 (POWERON)` : ce n'est pas une coupure d'alimentation. |
| W3 HTTPS, 10 min | PASS | `w3-run.log`, `w3-uart.log` | Son sans coupure confirmé par l'opérateur. Disque du Metronic et LED non confirmés à l'œil. |
| W4 comparaison avec la campagne A | PASS indicatif | ci-dessous | Causalité non démontrée. |

**W1, lecture des journaux.** `w1-uart.log` contient aussi le tampon présent à l'ouverture des sessions, antérieur
aux essais. Les cinq cycles retenus : coupures vers 14:59:36, 15:01:27, 15:02:12, 15:03:15 et 15:03:36. La session
rouverte à 15:00:44 n'est pas comptée (ni état `down` ni nouveau démarrage). Deux séries antérieures sont
INDETERMINATE, le banc ayant perdu la carte ; elles ne sont pas jointes.

**Résultat sur 10 démarrages.** IPv4 10/10, aucun `DisassociatedDueToInactivity`, aucune erreur
`esp_wifi_internal_tx … 12309`, aucun redémarrage inattendu.

**Non mesuré.** Nombre de tentatives d'association, canal, RSSI, BSSID, délais d'association et DHCP : le firmware
de production ne les journalise pas. « Association à la première tentative » est déduit de l'absence de ligne WARN
d'échec.

**W4.** La campagne A (`rust-mcu-libs@f9a2a9a`) montrait un échec d'association au démarrage suivi d'une reprise
(voir aussi `docs/qualification-26/hardware-run.log`) et des erreurs `12309`. Aucun des deux n'a été vu avec le
candidat. Le code de StreamBeWI est le même, mais les conditions radio ne sont pas contrôlées, il n'y a pas
d'essai alterné et les mesures fines manquent : le résultat est cohérent avec le correctif sans en prouver la cause.

**Non testé.** Scan de provisioning puis connexion, perte du point d'accès associé, échec d'authentification,
annulation d'association, récupération après erreur, timeout DHCP, AP+STA, non-régression du provisioning.
