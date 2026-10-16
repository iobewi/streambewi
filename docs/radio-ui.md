# Interface radio — issue #2

## Usage

L'accueil est le choix des favoris : tuiles pastel, logo de station quand il est disponible,
petite radio locale sinon, station choisie mise en évidence. Wi-Fi et engrenage restent en haut.
Le style reprend la maquette validée ; aucun nouvel asset généré n'est nécessaire.

- Au boot provisioning : interface sur `http://172.23.241.1/`, panneau Wi-Fi ouvert si nécessaire.
- Au boot configuré : même interface sur `http://<adresse-IPv4-de-la-clé>/`. L'adresse est donnée
  par le routeur et apparaît dans les journaux au démarrage. Pas de mDNS ajouté dans ce chantier.
- Un serveur HTTP par boot, sur AP **ou** STA. Après provisioning, le serveur reste sur AP
  jusqu'au reboot ; l'AP n'est pas rendu permanent en usage normal.
- Le panneau Wi-Fi recherche les réseaux sur demande et permet la saisie manuelle. Le
  gestionnaire Wi-Fi reste propriétaire unique, avec Improv en parallèle. Le résultat Web
  est consultable après reconnexion du téléphone, comme dans #1.
- L'engrenage donne accès à l'ajout, la modification, la suppression et la recherche de radios.
  Pas d'OTA ni de configuration avancée des logs dans cette livraison (#15 reste distincte).

## Choix et changement de station

Huit favoris maximum. La première ouverture sans favoris reprend uniquement le flux historique
`Mon petit France Inter` ; les stations de la maquette ne sont pas toutes préinstallées.
Nom, adresse du flux, URL du logo et identifiant stable sont enregistrés dans un ConfigSpace
`stations` de 4096 octets. La sélection est stockée dans le même document JSON version 1.
Les écritures sont sérialisées et l'état RAM n'est publié qu'après commit réussi.
Les credentials Wi-Fi et le flag USB gardent leurs espaces existants. BOOT efface le Wi-Fi,
pas les favoris.

**La sélection d'une autre station entraîne un redémarrage automatique**, 1,2 s après sa
sauvegarde pour permettre l'accusé de réception HTTP. La page affiche « Changement de station… ».
L'application utilise ainsi le démarrage USB existant ; pas de mélange en direct de deux flux.
La modification du flux actif ou sa suppression provoque aussi un redémarrage ; modifier son nom
ou son logo, ou ajouter/modifier un favori inactif, n'en provoque pas. Une nouvelle sélection de
la même station n'écrit rien et ne redémarre pas. Un seul `RADIO.MP3` reste présenté.
Ce raccordement doit encore être qualifié sur Metronic : comportement de la reprise et temps
perçu, y compris si le lecteur coupe l'alimentation USB.

« En lecture » signifie que le firmware a servi des secteurs du fichier dans les six dernières
secondes. Cela tolère les lectures par rafales du Metronic et **ne mesure pas le son audible**.
« À l'arrêt » peut donc apparaître avec quelques secondes de délai. Les états Wi-Fi déconnecté,
préparation et changement de station sont distingués sans exposer les détails USB.

## Catalogue et logos

Le navigateur interroge Radio Browser en HTTPS (miroirs de1 puis nl1 en repli), filtre les MP3
non-HLS dont le flux résolu est en HTTP, et permet d'ajouter le résultat aux favoris.
Le producteur embarqué ne sait toujours pas ouvrir HTTPS audio, AAC/HLS ni des playlists.
Une URL manuelle doit être un flux MP3 HTTP direct ; ce format n'est pas prouvé par le validateur
syntaxique. Le catalogue peut aussi contenir des entrées périmées ou mal décrites.

Catalogue et logos sont téléchargés par le téléphone/PC ; pas par la clé. Sans Internet sur ce
navigateur, recherche indisponible mais favoris, interface et radio de remplacement restent
accessibles. Aucun CDN, police distante ni bibliothèque JS n'est requis pour l'interface locale.
Les logos du catalogue ne constituent pas une garantie de provenance officielle. Une image
absente ou inaccessible est remplacée localement. Le navigateur ne transmet pas de référent
aux serveurs de logos et omet les credentials lors de la recherche externe.

## API produit

| Route | Effet |
| --- | --- |
| GET `/` / `/ui.css` / `/ui.js` | Interface locale |
| GET `/api/state` | Favoris, sélection, états observés et résultats Wi-Fi, sans mot de passe |
| POST `/api/stations/save` | Formulaire id optionnel, name, url, logo optionnel |
| POST `/api/stations/select` | Sélection par id |
| POST `/api/stations/delete` | Suppression ; sélection de remplacement si nécessaire |
| POST `/api/scan` | Demande de scan au propriétaire Wi-Fi |
| POST `/wifi`, GET `/status` | Compatibilité avec le provisioning #1 ; POST répond 202 puis statut partagé |

Les handlers radio empruntent le même objet `Radio` protégé par mutex. Le formulaire Wi-Fi
répond après mise en file ; il ne monopolise plus le serveur pendant l'association radio.
Les textes externes sont insérés avec `textContent`, pas avec `innerHTML`. Les URLs de logos
sont limitées à HTTP/HTTPS ; les flux à HTTP. Pas d'authentification ajoutée : usage local,
AP protégé par WPA2, comme #1. Les endpoints ne doivent pas être publiés sur Internet.

## Compatibilité du stockage

Le manifeste EKV admet désormais `stations`. Sur le layout actuel (64 pages), les trois
espaces réservent 32 pages sur 48 disponibles après réserve globale. Le nouveau firmware
monte les configurations existantes sans réinitialisation ; l'absence de `stations` utilise
le défaut historique sans écriture. Un document existant invalide provoque un diagnostic
`STREAMBEWI:STATIONS-LOAD` : il n'est ni écrasé ni formaté automatiquement.

Après enregistrement de favoris, un ancien firmware sans `stations` dans son manifeste
refusera cette clé inconnue. Un retour arrière nécessite une sauvegarde et une procédure
explicite de configuration compatible ; ne pas promettre un downgrade applicatif transparent.
Une image factory fusionnée réinitialise la configuration comme auparavant.

## Vérifications

- 43 tests hôte : politiques existantes, CRUD/sélection/rechargement, échec d'écriture sans perte,
  refus des documents invalides, dernière station protégée, bornes et formulaires.
- Clippy strict, Rustfmt, syntaxe JS et diff check.
- Smoke navigateur : `node tools/qual/check-ui.cjs` (Playwright + Chromium requis ; variable
  `CHROMIUM_EXECUTABLE` pour un navigateur déjà installé). **API appareil simulée** : accueil,
  mise en page 320/390 px, Wi-Fi, recherche/filtre/ajout, sélection, texte sûr et logo absent.
  Captures de contrôle dans `target/ui-check/`, non versionnées.
- Requête réelle Radio Browser FIP : HTTP 200, CORS `*`, entrées MP3 HTTP et logos reçus.
  Cela ne qualifie pas les flux ni la lecture audio de ces entrées sur carte.
- Liens ESP32-S3 normal et qual : PASS (Rust esp 1.98.1.0, GCC 15.2.0_20250920).
  TaskPool produit : 21 104 octets ; pile réservée par le linker : 28 016 octets normal,
  27 784 octets qual. Ce ne sont pas des mesures dynamiques. Avertissement RWX connu.

## Qualification matérielle restante

Sur S3 Zero + Metronic : conserver la configuration existante avec une écriture applicative
sur layout compatible ; vérifier l'accueil sur STA, l'interface AP après BOOT, Improv et Wi-Fi.
Ajouter deux vrais flux compatibles, changer de station et confirmer le son, recharger la page
et redémarrer pour vérifier la persistance, modifier/supprimer un favori et tester une mauvaise
URL. Observer heap/pile lors d'une écriture et avec huit favoris. Pas de nouvelle campagne
exhaustive Wi-Fi ou USB. #16 reste une réserve indépendante.
