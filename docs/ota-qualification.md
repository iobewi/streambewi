# Qualification matérielle de l'OTA système A (#3) — plan, non exécuté

Ce plan n'a **pas** été exécuté : aucune carte n'a été flashée, aucun Config-Space modifié. Chaque écriture sur la
carte exige une autorisation spécifique. Les contrôles logiciels (tests hôte, Clippy, builds, validation des images,
mesures) sont dans la description de la PR.

Image : nominale de la carte (`FEATURES=board-devkitc-1`), construite avec un jeton et une identité TLS **de banc**
(aléatoires, non versionnés, jetables). L'image `qual` (instrumentation de la pile et du tas) sert à la mesure de la
marge de pile du chemin de maintenance ; elle n'est pas l'image à valider.

## 0. Préconditions, sans écriture

1. Identité de la carte (ESP32-S3 DevKitC-1 N16R8, MAC `9c:13:9e:ac:4e:8c`, flash 16 Mio).
2. Sauvegarde complète des régions utiles de l'ancienne disposition (second étage + table, `config`, application) et de
   la région `0x380000–0x400000` (elle contient peut-être un ancien magasin EKV d'une campagne de la bibliothèque : non
   vierge, elle serait refusée au montage ; l'image de première installation l'écrase par des `0xFF`).
3. SHA-256 de l'image de première installation, de l'image OTA et du second étage (publiés avant d'écrire).
4. Validation explicite de la procédure par le mainteneur.

## 1. Première installation (sur autorisation)

Écrire `streambewi-esp32s3.bin` à `0x0` (4 Mio, sans autre commande d'effacement), relire, comparer les SHA-256.
Attendu au premier démarrage : `ota: running ota_0 status Stable`, formatage de `config` vierge, Improv répond,
provisionnement du Wi-Fi, LED et flux comme avant, disque présenté au Metronic.

## 2. Non-régression du produit (démarrage normal)

`/api/state` (dont `health`), choix de station sans redémarrage, USB MSC et audio du Metronic, LED à 20 %, Diagnostic
Live, Config-Space relu (stations, Wi-Fi, luminosité).

## 3. Maintenance et OTA nominale

1. `tools/ota/client.py maintenance http://DEVICE` : redémarrage, disque détaché, LED orange pulsante, `OTA ready`.
2. `health`, puis `upload` de l'image OTA (autre identifiant de déploiement), `staged`, `activate`.
3. `ota: running ota_1 status PendingVerify` puis `ota: confirmed` ; démarrage normal, Metronic de nouveau lu.
4. Retour A1 → A0 par le même chemin. Config-Space inchangé à chaque étape.
5. Sortie de maintenance sans OTA : dix minutes sans requête (ou coupure de courant) → démarrage normal.

## 4. Refus et retour arrière

Mauvais jeton (401), mauvais digest (409), image tronquée ou incomplète (409, rien de publié), identifiant d'activation
inconnu (409), transfert interrompu alimentation maintenue (rien de publié, recommence), ancien `staged` remplacé.
Retour arrière : image `ota-reject-self-check` puis `ota-hang-self-check` (rollback vers l'image confirmée, réactivation
refusée) ; une activation suivie d'une image saine confirme.

## 5. Panne externe sans retour arrière injustifié

Candidat sain avec Wi-Fi absent, puis station radio injoignable, pendant la probation : confirmé (le contrôle ne dépend
pas de l'externe).

## 6. Mémoire

Marge de pile du chemin de maintenance (poignée de main TLS, upload, validation) et du démarrage normal, tas minimal
pendant l'upload, avec l'image `qual`. Critère : marge de pile d'au moins 8 192 octets ; sinon arrêt et présentation des
mesures avant toute optimisation.

## Hors de cette campagne

Coupures d'alimentation pendant l'effacement, la programmation, la publication, l'activation ou la confirmation
(autorisation spécifique) ; défaut de reprise EKV (#70) ; domaine B ; signature des images.
