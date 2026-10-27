# OTA du système A (#3)

StreamBeWI utilise **directement la chaîne OTA système A de `iobewi/rust-mcu-libs`**, telle que la montre et la
qualifie `examples/firmware/ota-system` : second étage iobewi, partitions `otadata`, `ota_0`, `ota_1`, métadonnées OTM1,
moteur commun de transfert et d'écriture, validation d'image ESP32-S3, activation explicite, redémarrage, probation,
confirmation ou retour arrière. Rien de cela n'est réécrit ici ; ce dépôt ne contient que le raccordement.

L'ancienne disposition (`config` à 0x9000, `factory` à 0x50000) est **abandonnée sans migration** : le produit est en
développement, aucune installation n'est à préserver.

## Disposition flash (`targets/esp32/partitions.csv`)

| Région | Offset | Taille | Type |
|---|---|---|---|
| second étage iobewi | `0x0` | ≤ `0x8000` (29 744 o) | — |
| table | `0x8000` | `0x1000` | — |
| `otadata` | `0xd000` | `0x2000` | data/ota |
| `ota_0` | `0x20000` | `0x1B0000` (1,6875 Mio) | app/ota_0 |
| `ota_1` | `0x1D0000` | `0x1B0000` | app/ota_1 |
| `config` | `0x380000` | `0x80000` (128 pages EKV) | data/0x40 |

Tout tient dans les 4 Mio pour lesquels les images sont conditionnées (en-tête DIO 4 Mo, comme la référence) ; la carte
a 16 Mio et le reste est inutilisé. Alignements : applications sur 64 Kio, effacement sur 4 Kio, `otadata` de deux
secteurs de 4 Kio. `tools/package_image.py` vérifie tout cela avec le générateur de table ESP-IDF épinglé et le
validateur d'image de la bibliothèque (contre les deux slots).

`config` est plus grand qu'avant (128 pages au lieu de 64) : les six espaces (`stations`, `wifi`, `usb_boot`, `log`,
`settings` et `ota`) réservent 60 pages, 16 sont de réserve, il en reste 52 (le test
`the_spaces_fit_the_partition_to_the_page` garde ce compte). L'espace `ota` a le budget de la référence (2048 o ; OTM1
fait au plus 512 o).

## Construction

`scripts/build-image.sh OUT` (la commande de la CI et de `build-release.sh`) :

1. `scripts/build-bootloader.sh` construit le second étage **depuis la révision de `rust-mcu-libs` à laquelle le produit
   est épinglé** (sources lues là où cargo les a récupérées, copiées dans un répertoire temporaire, rien n'est copié
   dans ce dépôt). La bibliothèque n'a pas de `Cargo.lock` : les versions de dépendances sont fixées par
   `targets/esp32/bootloader.Cargo.lock`. Il produit aussi l'`otadata` initial (emplacement 0 confirmé, avec le codec
   de la bibliothèque) et le validateur d'image (`target/tools/validate_image`).
2. Le firmware est compilé comme avant.
3. `tools/package_image.py` produit :
   - `streambewi-esp32s3.bin` : **première installation** (4 Mio : second étage, table, `otadata`, application dans
     `ota_0`, `ota_1` et `config` vierges). Elle **efface la configuration** ; elle ne sert qu'à installer une carte ;
   - `streambewi-esp32s3-ota.bin` : l'application seule, ce que l'OTA téléverse dans le slot inactif ;
   - `streambewi-esp32s3-ota.sha256` : son empreinte `sha256:<hex>`.

Le jeton et l'identité TLS sont **compilés dans l'image** (variables d'environnement `IOBEWI_OTA_TOKEN`,
`IOBEWI_TLS_CERT_PEM`, `IOBEWI_TLS_KEY_PEM`, comme la référence). Sans elles, l'image compile, mais ne propose pas
l'OTA : `POST /api/maintenance` répond 501 et rien ne redémarre en maintenance.

## Mode maintenance

L'OTA ne s'exécute pas pendant la lecture. Le produit a un démarrage de **maintenance** qui ne fait qu'une chose :
serveur OTA sur le réseau Wi-Fi enregistré. Ni flux, ni disque USB, ni page locale.

- **Entrée :** `POST /api/maintenance` (corps `confirm=maintenance`, démarrage normal uniquement). La requête demande le
  redémarrage existant ; la cible écrit une demande à usage unique dans la RAM RTC (conservée par un reset, effacée
  par une coupure de courant), le disque est détaché avant le reset comme pour tout redémarrage, et le démarrage suivant
  lit et efface la demande.
- **Sortie :** une activation redémarre d'elle-même ; sans aucune requête OTA pendant 10 minutes, le produit redémarre
  en normal ; une coupure de courant revient aussi en normal. Le geste BOOT de récupération reste actif.
- **LED :** orange pulsant lentement (état `Maintenance` du registre) ; le bleu clignotant lent du Wi-Fi qui se
  connecte passe avant, les erreurs aussi. La politique existante n'est pas modifiée.
- **Hors maintenance** le produit est inchangé : mêmes chemins audio et USB, mêmes API.

## Protocole (celui de l'exemple `ota-system`)

HTTPS sur le port 443, une connexion à la fois, durée de requête 120 s.

| Requête | Effet |
|---|---|
| `GET /health` | `ok` : le serveur répond (pas une mesure de santé) |
| `POST /ota/upload` | `Authorization: Bearer`, `X-Deployment-ID` (1 à 64 caractères `[A-Za-z0-9-_.]`), `X-Image-SHA256: sha256:<64 hex>`, `Content-Length` obligatoire, pas de `Transfer-Encoding`. Corps : l'image ESP binaire. `200 staged` seulement après écriture, relecture, digest et validation `Full` ; sinon 400/401/409, rien n'est publié |
| `POST /ota/activate` | même identifiant, corps vide : `200 activating` puis redémarrage ; sinon 400/401/409 |

Aucune activation implicite après un transfert. Un transfert interrompu ne publie rien et se recommence. Une réponse
d'activation perdue ne prouve pas que l'activation n'a pas eu lieu : lire le journal avant de réessayer.

`tools/ota/client.py` est le client de qualification : `maintenance`, `health`, `upload [--activate]`, `activate`.

## Probation

La probation est celle de la bibliothèque (`boot::on_boot`, `pending_check`, `confirm_pending`, `reject_pending`) : un
candidat démarre en `PendingVerify`, la confirmation durable précède le désarmement du watchdog (30 s, armé après
l'initialisation HAL et le timer RTOS), un refus, une expiration ou un échec de confirmation redémarrent, et le
démarrage suivant revient à l'image confirmée.

Vérification minimale de StreamBeWI : **la configuration se relit** (`self_check` EKV, lecture seule : tous les
enregistrements vivants et leur CRC). **Rien d'externe n'est demandé** : ni le réseau Wi-Fi, ni la station radio, ni
Internet, ni le lecteur branché sur le port USB. Une panne externe ne provoque donc aucun retour arrière ; en
contrepartie un candidat dont le flux ou l'USB sont cassés est confirmé (limite assumée de cette première version).

Pour les essais de retour arrière sur le banc, deux fonctions de la cible simulent un refus ou un blocage du
contrôle : `ota-reject-self-check` et `ota-hang-self-check`. Jamais dans une image livrée.

## Sécurité : limites à connaître

- **Un SHA-256 n'est pas une signature.** Il prouve l'intégrité de ce qui a été reçu, pas qui l'a envoyé. Le second
  étage ne vérifie que la structure de l'image (`Verify::Structure`). Quiconque détient le jeton peut installer
  n'importe quelle image. Aucune signature n'est conçue dans ce chantier ; une protection définitive est une évolution
  séparée.
- **Le jeton et la clé TLS sont dans le binaire**, donc dans la flash, lisibles par qui tient la carte, et communs à
  toutes les cartes construites avec les mêmes variables. Ne jamais les valider dans le dépôt. Le certificat est celui
  que fournit le constructeur de l'image ; le client doit le connaître (`--ca`).
- La comparaison du jeton n'est pas à temps constant (reprise telle quelle de la référence).
- `POST /api/maintenance` est ouvert à tout le réseau local, comme les autres routes de la page : il peut priver
  la carte de radio pendant dix minutes, il n'installe rien (l'OTA exige le jeton sur TLS).
- Le chiffrement de la flash et Secure Boot ne sont pas utilisés.

## Première installation d'une carte (à exécuter sur autorisation)

Après sauvegarde des régions utiles : écrire `streambewi-esp32s3.bin` à `0x0` (4 Mio : cela remplace le second étage,
la table, `otadata`, l'application, et laisse `config` **vierge**). Provisionner le Wi-Fi (Improv). Le premier
démarrage doit journaliser `ota: running ota_0 status Stable`. Pour la suite : entrer en maintenance, téléverser
`streambewi-esp32s3-ota.bin`, activer, vérifier `ota: confirmed` au démarrage suivant. Procédure matérielle détaillée
et critères : `docs/ota-qualification.md`.

## Mesures (voir la PR)

Taille de l'image, profondeur de pile statique (`tools/stack_depth.py`) et budgets du tas sont comparés à la
référence de `main` dans la description de la PR ; la marge de pile nominale reste estimée, pas mesurée.
