# Qualification matérielle HTTP/HTTPS (#21)

Banc : ESP32-S3 (Metronic 477144 en hôte USB MSC), Wi-Fi 192.168.1.x. Source : `feat/21-https-stream`, socle
`rust-mcu-libs` `f85b0b7`. Trace brute (SSID, MAC masqués, aucun identifiant) : [`hardware-run.log`](hardware-run.log).
Images : `release` (H1–H6, H9–H12, H14) et `qual` avec coupures planifiées (H7, H8, mémoire), même source ;
la config Wi-Fi vient du stockage de la carte, aucune image ne contient d'identifiants.

La qualité d'écoute est **à confirmer par l'utilisateur** (non couverte par les compteurs).

| ID | Essai | Résultat | Preuve (trace) |
|---|---|---|---|
| H1 | Démarrage, config existante | PASS | stations, Wi-Fi conservés ; IP, flux HTTP 200, MSC activé |
| H2 | Lecture HTTP | PASS | 1 min, `lead=81920` constant, 0 défaut |
| H3 | Lecture HTTPS | PASS | `tls up` 544–645 ms, statut 200, chunked |
| H4 | HTTP → HTTPS | PASS | sans redémarrage, disque revenu en 2,0 s |
| H5 | HTTPS → HTTP | PASS | disque revenu en 1,5 s |
| H6 | HTTPS → HTTPS | PASS | nouvelle poignée de main, disque revenu en 2,9 s |
| H7 | Reconnexion brutale | PASS | 4 coupures sans fermeture TLS, reprise seule en 0,1–1,5 s |
| H8 | Reconnexion propre | PASS | 4 coupures avec `close-notify` (18–76 ms), reprise seule |
| H9 | Certificat incorrect | PASS | `wrong.host.badssl.com` : `MbedtlsError(-0x2700)` à chaque tentative, jamais de repli en clair |
| H10 | Horloge non synchronisée | PASS | 1ʳᵉ tentative refusée (`clock not synchronized yet`), 2 s plus tard TLS établi ; une station HTTP n'attend pas SNTP (H1) |
| H11 | Réponse chunked | PASS | `chunked=true` en HTTP et en HTTPS, lecture continue (le décodage est de plus testé octet par octet sur l'hôte) |
| H12 | Redirection vers un autre hôte HTTPS | PASS | `direct.fipradio.fr` → 301 → `icecast.radiofrance.fr` : deuxième poignée de main TLS |
| H13 | Redirection HTTPS → HTTP | **tests hôte seulement** | aucun site HTTPS public ne redirige vers HTTP avec une chaîne couverte par nos racines ; `httpbin.org` (Amazon) est refusé par nos racines, ce qui est voulu |
| H14 | Lecture ≥ 10 min | PASS | 10 min 27 s HTTPS, `lead=81920` à chaque minute, 0 défaut |
| H15 | Retour à la config d'origine | PASS | voir ci-dessous |

## Compteurs MSC (sur l'ensemble des essais)

`ZEROFILL` 0, `Expired` 0, `Pending` 0, `stall` 0, panique 0, `no data for` 0, `body read error` 0.
Pendant les 8 reconnexions planifiées l'avance du flux est restée à 78 336–81 920 octets.
H9 (certificat refusé) détache volontairement le disque tant que la station n'a pas de préremplissage : 14 s.

## Mémoire (image `qual`, HTTPS, 8 reconnexions)

- Tas : 155 664 octets en deux régions : principale 81 920 (utilisée 73 768, libre 8 152), DRAM2 73 744
  (utilisée 37 040, libre 36 704).
- Utilisé : 110 808 octets, **identique** à chaque point de mesure sur 8 reconnexions (pas de dérive) ; pic
  127 032, minimum libre 28 632.
- Pile : réservée 51 136, pic 23 804, marge **27 332** octets (cf. 36 KiB minimum de l'éditeur de liens).
- Anneau audio 96 KiB, PSRAM : inchangés / non utilisés.

## Restauration (H15)

`config` et `factory` réécrits depuis les sauvegardes prises avant le premier flash, relus : SHA-256
`config` = `8660ac5b…51f0` (base prise après provisioning du nouveau Wi-Fi, avant tout essai),
`factory` = `adc6c501…974c` (produit d'origine), table de partitions inchangée `0d6004dd…f272`. Le produit
d'origine redémarre (IP, station HTTP, session MSC avec le Metronic). Le Wi-Fi a été reprovisionné
(nouveau réseau) avant la campagne : la `config` d'origine n'a donc pas été restaurée par-dessus.

## Réserves

- Les coupures de H7/H8 sont planifiées par le firmware (feature `qual`, jamais en production) : elles exercent
  la reconnexion, **pas une vraie coupure Wi-Fi/WAN, non testée**.
- Une station HTTPS publique (Radio France) ; P-384 non exercé de bout en bout (la chaîne `httpbingo.org`, ISRG
  Root YE → X2, n'a pas été essayée).
- Poignée de main TLS ≈ 270 ms d'executor (mesure du socle) ; aucun défaut MSC observé pendant les 12
  poignées de main de cette campagne (lecture continue, avance du flux intacte).
- Pas de capture côté hôte USB ; pas de décodage audio de contrôle ; écoute à confirmer.
