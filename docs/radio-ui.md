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
- Le panneau Wi-Fi recherche les réseaux sur demande et permet la saisie manuelle **dans le boot de configuration (AP)** ; en usage normal sur le LAN il est en lecture seule (le serveur n'a pas d'authentification). Le
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

**La sélection d'une autre station est appliquée sans redémarrage** quand le disque est présenté
(démarrage USB normal). La sélection est d'abord prise en compte en RAM ; le disque quitte alors le
bus (`DCTL.SDIS`), l'ancien flux est abandonné, la sélection est écrite en flash **pendant que le
disque est détaché** (personne n'est sur le bus pour subir les sections critiques de la flash, voir
#19), puis le disque revient une fois l'écriture terminée, 1 500 ms au moins après le détachement et
un prébuffer de 64 Kio de la nouvelle station reçu. Le Wi-Fi, la page Web et l'exécuteur ne sont pas
redémarrés ; la page affiche « Changement de station… » jusqu'au retour du disque. Si l'écriture
échoue, la station reste active en RAM mais `/api/state` renvoie `persisted: false` et la page le dit :
l'ancienne station reviendra au redémarrage. En démarrage de provisioning (pas de disque), la
sélection est écrite tout de suite et la carte redémarre comme avant.
La modification du flux actif ou sa suppression suit le même changement de station, mais son écriture
a lieu **avant** le détachement (risque de #19 conservé) ; modifier son nom ou son logo, ou
ajouter/modifier un favori inactif, ne change pas de station. Une nouvelle sélection de la même
station n'écrit rien et ne change rien. Un seul `RADIO.MP3` reste présenté.
Qualifié sur Metronic : voir « Changement de station sans redémarrage ». Les redémarrages restants
(BOOT, provisioning) passent par le détachement USB de D3.

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
| POST `/wifi`, GET `/status` | Compatibilité avec le provisioning #1 ; POST répond 202 puis statut partagé. **Refusé (403) hors du boot de configuration (AP)** : sur le LAN, le panneau Wi-Fi est en lecture seule ; on change de réseau par `StreamBeWI-Setup` (BOOT) ou Improv. |

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
- Liens ESP32-S3 normal et qual : PASS. TaskPool produit : 21 104 octets ; pile réservée par le
  linker après correctif (`db801fe`) : 44 400 octets normal, 44 168 octets qual (tas 80 Kio, pile
  minimale 36 Kio vérifiée au boot). Ce ne sont pas des mesures dynamiques (voir la qualification
  matérielle). Avertissement RWX connu.

## Qualification matérielle

Qualification faite sur la carte, avec le Metronic, le 16 octobre 2026. Les mesures instrumentées
et les mesures nominales sont distinguées. Rien n'est fusionné ni clôturé ici.

### Carte, révision, images

| Élément | Valeur |
| --- | --- |
| Carte | ESP32-S3-DevKitC-1 (pont CH343), ESP32-S3 QFN56 rév. v0.2, flash 16 Mio, MAC `9c:13:9e:ac:4e:8c` |
| Waveshare ESP32-S3 Zero | **NON EXÉCUTÉ** : le testeur a demandé de faire la campagne sur la DevKitC ; la Zero n'a pas été utilisée pour #2 |
| Layout | `config` 0x9000 (0x40000), `factory` 0x50000 (0x180000) |
| Méthode d'écriture | Application seule à 0x50000, configuration conservée ; aucune image fusionnée à 0x0, aucun effacement global |
| Sauvegarde avant #2 | `config` 0x9000 : sha256 `b9352b7d…0653` ; `factory` 0x50000 : `94a86fda…8e01` (fichiers locaux du banc) |

Images du commit `db801fe` (HEAD de `feat/2-radio-ui`), construites localement (GCC 16.2.0 ; la CI
utilise GCC 15.2.0, les empreintes CI peuvent donc différer et n'ont pas été comparées) :

| Image | sha256 |
| --- | --- |
| nominale fusionnée | `fc0404718a3576f83d9aa5fd4c45648d7553153c3487c084841cf01d7314f22c` |
| nominale, application seule (0x50000) | `5952ee422f08b1e8c15621dbd5169d376b2c3aa307c6e1e44b4b3afc147dd7f1` |
| qual fusionnée | `3e607746ce32c4baead5df22611f66ed3eb157a2ed1d6995cb96edb50ef7deae` |
| qual, application seule (0x50000) | `134817a74ad37b57a203436fed669233f957ef47c5adcdc028ca0efd89873a32` |

**Image nominale finale réellement testée : `db801fe`, application seule `5952ee42…`**, flashée
sur la DevKitC. L'image du commit initial `dc30e8e` (nominale et qual) n'a pas permis d'ouvrir
l'interface : voir le défaut D1. Les mesures mémoire ont été prises sur l'image `qual` de
`db801fe`, **instrumentée** (sonde de tas et de pile) ; elles ne sont pas des mesures nominales.
Les images de diagnostic utilisées pour D2 (chronométrage des opérations flash, fusion des
écritures) ne sont pas dans la branche.

### Résultats

| Étape | Vérification | Résultat |
| --- | --- | --- |
| 1 | Interface en mode client (STA, flux, MSC, son) ouverte sur l'IPv4 de la clé | PASS (traces : IP, flux HTTP 200, MSC, relecture du disque) |
| 1 | AP absent en usage normal | PASS (aucune ligne AP après un boot configuré) |
| 1 | Détail visuel de l'accueil, des tuiles, du Wi-Fi, de l'engrenage sur téléphone | NON CONFIRMÉ : le testeur a utilisé l'interface sans que le détail soit consigné |
| 1 | « En lecture » = lectures récentes, pas le son audible | NON EXÉCUTÉ |
| 2 | Deux vrais flux MP3 HTTP directs, dont FIP | PASS (FIP, `monpetitfranceinter`, `allzic52`, `franceinter` sur `icecast.radiofrance.fr`, HTTP 200) |
| 2 | Logo absent : petite radio + nom | NON CONFIRMÉ |
| 2 | Modifier nom ou logo sans redémarrage | NON EXÉCUTÉ isolément (testé par les tests hôte) |
| 2 | Le rechargement conserve les données | PASS (les 8 favoris survivent à un reset) |
| 3 | Sélection d'une autre station : nouveau flux, disque de retour (sans redémarrage) | PASS côté carte et Metronic, voir « Changement de station sans redémarrage » |
| 3 | Son de la nouvelle station sur le Metronic | PASS ; reprise après redémarrage logiciel : voir D3 |
| 3 | Plusieurs changements entre deux stations | PASS côté carte (quatre sélections successives pilotées depuis le banc) |
| 3 | Redémarrage volontaire conserve la sélection | PASS |
| 3 | Resélection de la station active : pas de redémarrage | NON EXÉCUTÉ sur la carte (couvert par un test hôte) |
| 3 | Reprise de la lecture par le Metronic après un redémarrage logiciel | **PASS après correctif D3** : 10/10 (contrôlé), 10/12 (changement réel de station) |
| 4 | Modifier l'URL de la station active : changement de station | PASS côté carte (qualifié avant le passage sans redémarrage) |
| 4 | Supprimer une station inactive : pas de redémarrage | **FAIL partiel** : pas de redémarrage mais coupure du lecteur (D2, issue #19) |
| 4 | Supprimer la station active : remplacement valide et changement de station | PASS côté carte (qualifié avant le passage sans redémarrage ; FIP devient active) |
| 4 | Dernière station protégée | PASS (bouton grisé ; refus firmware couvert par un test hôte, non observé sur la carte) |
| 5 | Catalogue Radio Browser depuis un téléphone avec Internet | PASS |
| 5 | Résultat incompatible non proposé | PASS (seuls des flux HTTP sont listés ; peu de choix car la plupart sont en HTTPS) |
| 5 | Favoris sans catalogue ni logo | PASS (ajout manuel de `franceinter`) |
| 5 | URL invalide refusée sans perte | PASS (« Vérifiez le nom et l'adresse du flux MP3 direct HTTP (256 caractères maximum). ») |
| 5 | URL valide mais injoignable : interface locale accessible | PASS (`example.invalid` active ; `GET /` 395 ms et `/api/state` 77 ms) |
| 6 | BOOT 3 s : provisioning, même interface sur `172.23.241.1`, pas de MSC | PASS (AP prêt, MSC absent) |
| 6 | Favoris conservés après BOOT | NON CONFIRMÉ explicitement à ce moment (les 8 favoris étaient présents à l'étape 7, après le BOOT et le provisioning) |
| 6 | Scan des réseaux | PASS (configuration Web réussie ; aucun défaut signalé par le testeur) |
| 6 | Provisioning Web réussi, puis STA et MSC après redémarrage | PASS (IP, flux, MSC, relecture, AP absent) |
| 6 | Contrôle court Improv avec l'AP actif | Couvert par #1, non rejoué |
| 6 | Résultat affiché après reconnexion sur changement de canal | Couvert par #1, non rejoué |
| 7 | Tas et pile pendant la navigation, les écritures et la lecture | PASS (mesures ci-dessous, image qual) |
| 7 | Huit favoris conservés, refus propre du 9ᵉ | PASS (« La liste est pleine ou trop volumineuse (8 stations maximum). ») |

### Mesures mémoire (image qual `db801fe`, instrumentée)

| Situation | Tas utilisé | Pic du tas | Libre au pire | Pic de pile | Marge de pile |
| --- | --- | --- | --- | --- | --- |
| Repos avec lecture | 48 772 | 54 384 | 27 536 | 27 580 | 16 588 |
| Navigation et rechargements | 49 320 | 61 692 | 20 228 | 30 748 | 13 420 |
| Écritures de favoris jusqu'à 8 | 49 652 | 74 584 | 7 336 | 32 540 | 11 628 |
| 9ᵉ refusé, scan Wi-Fi, catalogue | 50 200 | 74 584 | 7 336 | 32 540 | 11 628 |
| Après reset, 8 favoris | 49 412 | 61 696 | 20 224 | 27 596 | 16 572 |

Tas de 80 Kio (81 920 octets), pile de 44 168 octets réservés. L'utilisé stable est quasi constant,
donc pas de fuite observée ; les pics sont transitoires. **Les écritures de favoris laissent
7,3 Kio de tas libre au pire** : marge faible, à surveiller si le chemin d'écriture grossit.
Ces valeurs viennent de l'image instrumentée ; les réservations de l'ELF ne sont pas une
consommation maximale.

### Défauts

**D1 — Corrigé (`db801fe`).** Sur l'image initiale `dc30e8e`, toute requête HTTP locale provoquait
un dépassement de pile (panic silencieux). Cause : tas de 96 Kio, pile restante de 26,6 Kio, alors
qu'une requête coûte environ 10 Kio de pile (pic mesuré 31 Kio). Correctif : tas de 80 Kio, pile
minimale de 36 Kio vérifiée au boot. Vérifié sur l'image nominale : `GET /`, `/ui.css`, `/ui.js`,
`/api/state` répondent sans panic.

**D2 — Ouvert, issue #19.** Une écriture de configuration (ajout ou suppression de favori) pendant
la lecture peut couper le Metronic (« no usb »). Mesures par opération flash : 9 à 21 écritures
et un effacement par commit, 4 à 244 octets chacune, mais environ 50 ms par appel (surcoût fixe
indépendant de la taille), exécutées dans une section critique. Aucune opération isolée ne dépasse
51 ms et l'exécuteur n'est pas bloqué (la cession du tour après chaque opération ne corrige pas).
Une fusion expérimentale des écritures contiguës, sur image de diagnostic seulement, ramène les
écritures à 7 à 9 par commit (242 à 392 ms de flash cumulé) mais la coupure reste possible (1
commit sur 3). Aucun correctif n'est dans la branche ; l'arbitrage est renvoyé au mainteneur
(couche flash et USB partagée avec #1). Les suppressions successives aggravent l'exposition.

**D3 — Corrigé pour le redémarrage logiciel contrôlé.** Après un redémarrage logiciel (reset RTC
WDT), le Metronic ne relisait pas le disque (compteur de secondes puis « no usb »). Cause
retenue : le reset coupe le bus sans que l'hôte voie le disque partir. Correctif (commit
`fix(esp32): detach USB before software reset`) : détachement USB-OTG (`DCTL.SDIS`) pendant
1 500 ms avant le reset, uniquement quand le disque est présenté en USB-OTG (jamais au boot de
provisioning, où le port appartient à USB-Serial/JTAG). Mesures sur le Metronic :

- redémarrage logiciel contrôlé : 10 reprises sur 10 avec le détachement, 0 sur 10 sans ;
- changement réel de station : 10 reprises sur 12 ; les 2 échecs suivent le motif de
  battements (flapping) également observé après un reset EN, hors périmètre du correctif ;
- provisioning #1 vérifié : BOOT pendant le prebuffer (AP, pas de panic), saisie Web puis reset
  matériel (STA, MSC, session, pas d'AP) ;
- Windows vérifié : le disque disparaît puis réapparaît seul après un redémarrage logiciel.

Ce n'est pas une fiabilité USB de 100 % en toutes circonstances : des reprises échouent encore
(2 sur 12) et un reset matériel reste le dernier recours. Les coupures dues aux écritures flash
restent traitées dans #19.

### Changement de station sans redémarrage

Qualifié sur ESP32-S3 DevKitC et Metronic 477144 uniquement (Sony non qualifié). Même séquence que
ci-dessus, 1 500 ms de détachement minimum, alternances FIP et allzic, un arrêt prévu après deux
blocages consécutifs.

| Variante | Énumérations USB | Remarque |
|---|---|---|
| Écriture flash avant le détachement | 2 sur 12 | reset de bus de l'hôte pendant l'écriture, puis lecteur bloqué sur « scan » |
| Sans écriture flash (référence) | 5 sur 5, son revenu 5 fois | 0 écriture et 0 effacement mesurés |
| Écriture pendant le détachement, image instrumentée | 5 sur 5, son revenu 5 fois | écriture de 0,75 à 4,6 s, réattachement à 1,8 à 5,4 s |
| Écriture pendant le détachement, image finale | 9 cycles propres sur 10, son revenu aux 5 changements du contrôle | 1 cycle en blocage (voir ci-dessous) |

Mesures communes : aucun reset de bus de l'hôte pendant la sélection, aucune écriture manquée, aucun
`ZEROFILL`, un reboot restaure la dernière station sélectionnée depuis la flash. Le délai de réponse
HTTP est de 70 à 100 ms (jusqu'à 1,9 s une fois, sans cause établie). La durée du changement dépend
de l'écriture flash (16 à 99 écritures, 0,5 à 6,3 s).

**Limite connue, ouverte (proche de #19) :** sur l'image finale, un cycle sur 10 a énuméré puis le
lecteur a repris à N+1 (`lba=1141`, juste après `0..1140`), secteur en avance de 386 Kio sur le bord live
de la nouvelle station : `Pending`, trois resets de bus après environ 1,2 s, puis bus suspendu et
compteur de secondes figé. Le cycle suivant a remis le lecteur en route. Même signature que le LBA 990
observé avant. Le déclencheur est côté carte (secteur non disponible), le blocage côté lecteur ; les
essais ne permettent pas de les séparer. Une correction toucherait la politique de session de
`StreamFile` et n'a pas été faite.

Non qualifié : URL injoignable et retour à une station valide, resélection de la station active sur
la carte, modification ou suppression de la station active sans redémarrage, Windows, séries longues.

### Limites et observations

- Flux HTTP 301 non suivis (`direct.franceinter.fr`) ; HTTPS audio hors périmètre : peu de
  résultats du catalogue sont utilisables.
- Après provisioning réussi, l'AP reste actif jusqu'au redémarrage (voulu par #1). Suggestion à
  arbitrer pour #1 : redémarrage automatique après une connexion réussie, une fois le statut reçu.
- Avertissement `esp_wifi_internal_tx returned error: 12309` observé en mode AP, non analysé.
- Le test d'URL injoignable laisse le Metronic en « no usb » tant que la station est active.
- Les requêtes de diagnostic depuis le banc passent par le navigateur en mode `no-cors` : les
  réponses ne sont pas lisibles, seul le temps de réponse est consigné.
- Retour arrière : après l'enregistrement de favoris, un ancien firmware sans `stations` dans son
  manifeste refuse cette clé ; conserver une sauvegarde de la configuration (voir ci-dessus).
- Zero non qualifiée pour #2. #15 et #16 restent des chantiers distincts.
