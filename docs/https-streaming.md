# Streaming HTTP et HTTPS (#21)

Une station peut être `http://` ou `https://`. Le chemin du flux est le même pour les deux :

```
URL → DNS → TCP → TLS (https) → GET → décodage du corps → fenêtre de lecture → FAT16 → USB MSC
```

Seul le connecteur change (`app/src/stream.rs`, `app/src/fetch.rs`) : `TcpConnector` pour `http`, le
connecteur TLS fourni par la cible pour `https`. La production audio, les reconnexions, le préremplissage,
le détachement du disque au changement de station et la politique de lecture USB sont inchangés.

## Sécurité

- Le connecteur TLS est `iobewi-tls-service::StaticTrustConnector` sur `iobewi-esp-tls::EspTlsDialer`, avec
  les racines publiques de `iobewi-tls-roots` (en flash). Chaîne X.509, nom d'hôte (SNI), dates : vérifiés
  par MbedTLS. Pas de mode non vérifié.
- Pas d'horloge, pas de TLS : tant que SNTP n'a pas réglé l'heure (plancher : 2026-01-01), le connecteur
  refuse (`clock not synchronized yet`) et la station est retentée. Les stations `http` n'attendent pas SNTP.
- Un échec TLS ne se replie jamais sur le clair, et une station `https` n'est jamais servie par le
  connecteur TCP.
- Les journaux nomment l'origine (`schéma://hôte[:port]`) des redirections, jamais le chemin ni la requête.

## Redirections

301, 302, 303, 307 et 308 sont suivies, au plus **cinq**. Chaque redirection est une nouvelle connexion
(donc, en `https`, une nouvelle vérification du certificat de l'hôte désigné). Refusées : `https` vers `http`,
une boucle (même cible déjà visitée), la sixième redirection, un `Location` absent, relatif (hors `/chemin`)
ou illisible. Un `#fragment` n'est jamais envoyé. `http` peut passer à `https`.

## Réponses

200 avec `Content-Length`, sans longueur, ou `Transfer-Encoding: chunked` (décodé par `edge-http` : la
fenêtre ne reçoit que des octets audio). Les octets du corps arrivés avec l'en-tête sont conservés. Un en-tête
de réponse de plus de 2 048 octets, un `Transfer-Encoding` autre qu'un seul `chunked`, ou plus de 48 en-têtes
(`TooManyHeaders`, refus explicite, jamais de troncature) sont refusés (la tentative échoue et est retentie). Les erreurs sont journalisées par nature : DNS, TCP, TLS
(`connect … failed`), requête, en-tête de réponse, statut HTTP.

## Mémoire et réseau (ESP32-S3)

- Le tas principal est de 64 KiB (80 KiB avant l'OTA, `docs/ota.md`) ; la DRAM libérée par le bootloader de 2ᵉ étage (`dram2_seg`, 73 744 octets)
  devient une **seconde région** `esp-alloc` (`#[esp_hal::ram(reclaimed)]`, bornes données par l'éditeur de
  liens), sans PSRAM. L'anneau audio (96 KiB statique) et la pile ne changent pas.
- Cinq sockets réseau en STA : DHCP, DNS, flux, interface locale, SNTP. Le mode AP de configuration garde ses
  trois sockets et ne démarre pas SNTP.
- Une poignée de main TLS occupe l'executor environ 270 ms (RSA, 240 MHz) ; il n'y a ni second executor ni
  second cœur. P-384 est plus long et n'a pas été qualifié dans le produit.

La qualification matérielle (Metronic 477144) est dans la PR.
