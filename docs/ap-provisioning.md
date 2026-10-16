# Provisioning AP + Web — issue #1

Le boot de provisioning expose deux moyens parallèles de configurer le même Wi-Fi :
Improv Serial (UART0 et USB-JTAG disponibles) et un formulaire Web sur le point d'accès.
La méthode officielle ESP Web Tools a été validée par le mainteneur avant ce chantier ;
sa non-régression avec l'AP actif reste à vérifier sur carte.

## Utilisation

1. Au premier boot ou après récupération BOOT (3 s), rejoindre **StreamBeWI-Setup**.
2. Mot de passe WPA2 de développement : **streambewi**. Pour le changer à la fabrication,
   exporter `STREAMBEWI_AP_PASSWORD` (8 à 63 octets) avant `scripts/build-image.sh`.
   Cette valeur est compilée dans le firmware ; elle n'est pas le mot de passe du réseau domestique.
3. Accepter de rester connecté à ce réseau sans Internet et ouvrir **http://172.23.241.1/**.
   Le DHCP est fourni par `iobewi-esp-wifi` ; il n'y a pas de DNS captif ni de redirection automatique.
4. Saisir le nom et le mot de passe du réseau **2,4 GHz**, puis valider.
   Le serveur accepte une connexion HTTP à la fois. La configuration peut changer le canal radio
   et interrompre le client AP pendant l'association STA ; rejoindre l'AP si nécessaire.
5. Après confirmation, débrancher/rebrancher StreamBeWI. Les credentials sont validés par
   association/DHCP, persistés, puis le flag USB est enregistré. Le prochain boot démarre
   en STA sans AP et présente le MSC après prébuffer, selon la politique existante.

L'AP reste présent pendant tout le boot de provisioning, même après succès Web ou Improv.
Aucune bascule JTAG/OTG à chaud et aucun AP de secours sur perte du réseau en boot MSC.
Un mauvais mot de passe ou un échec de persistance laisse la possibilité de réessayer.
Si l'AP ne démarre pas, Improv reste disponible avec un diagnostic UART.

## Composition

- ESP32 : même `WifiManager<STA_SOCKETS, AP_SOCKETS>` et `with_access_point` que
  `rust-mcu-libs/examples/wifi-modes`, trois sockets AP (socket DNS interne d'embassy-net + DHCP + HTTP), trois sockets STA.
- HTTP : picoserve 0.20.1 (formulaire URL-encoded, limites du buffer HTTP 1 Kio ; RX/TX 1 Kio chacun).
- Application : une file Web bornée à une requête, réponse identifiée pour ignorer un résultat ancien.
  Un seul propriétaire du gestionnaire traite Web et Improv ; `BootPolicy::provision` est commun.
- L'interface reste limitée au provisioning. Choix des radios et politique de logs : #2 et #15.

## Qualification ciblée sur carte

Campagne du 16 octobre 2026 sur la **DevKitC-1** (ESP32-S3 QFN56 rév. v0.2, flash 16 Mio, pont UART CH343),
via le banc `bench/` (le conteneur n'accède pas à la carte). Réseau de test 2,4 GHz du banc, canal 11 d'après
les campagnes précédentes (non journalisé ce jour). SSID et mots de passe masqués.

| Scénario | Résultat |
| --- | --- |
| Configuration vierge : AP WPA2 annoncé, aucun credential, aucun MSC | PASS |
| Association d'un client à l'AP, bail DHCP, page sur `http://172.23.241.1/` | PASS (observé par le mainteneur) |
| Web, mauvais mot de passe : erreur affichée, nouvel essai possible, pas de succès annoncé | PASS |
| Web, bons credentials : connexion, DHCP, sauvegarde, résultat visible après reconnexion | PASS (avec la page de statut, voir ci-dessous) |
| Web, SSID avec espaces/accents et mot de passe avec symboles | NON EXÉCUTÉ (couvert par les tests hôte) |
| Changement de canal pendant l'association STA | Client AP éjecté (AP canal 6, réseau canal 11) ; page redevenue joignable après réassociation |
| Improv, AP actif (client du banc) : `state`, `info`, `scan`, mauvais mot de passe | PASS (réponses sur le port demandeur ; `unable to connect` puis `authorized`) |
| Improv officiel ESP Web Tools, AP actif (DevKitC-1) | PASS, cas de succès seulement : credentials puis flag OTG enregistrés, succès affiché par l'outil ; mauvais mot de passe non essayé avec l'outil (couvert par le client du banc) |
| Web puis Improv rapprochés : requête Web mise en file pendant un Improv, traitée ensuite, une erreur par demandeur | PASS ciblé (pas d'exhaustivité Wi-Fi) |
| Redémarrage : credentials conservés, STA, flux, prébuffer, MSC, aucun AP ; son confirmé | PASS |
| BOOT 3 s : flag et Wi-Fi effacés, redémarrage, AP + Web + Improv | PASS |
| Waveshare ESP32-S3 Zero | NON EXÉCUTÉ (carte non déboguable) |

### Défauts trouvés et corrigés

1. **`AP_SOCKETS = 2` : panic au démarrage du serveur HTTP** (`314cf5a`). `embassy-net` réserve un socket DNS
   interne dans chaque pile ; DHCP + écouteur HTTP font donc trois sockets. Avec deux, smoltcp panique
   (`adding a socket to a full SocketSet`) juste après « AP ready » : l'exécuteur se fige, Improv ne répond
   plus et la page n'est pas servie. Trace obtenue avec un panic handler de diagnostic local (non versionné).
   Le commentaire de `rust-mcu-libs` (« 2 = DHCP + un service ») sous-compte ce socket : à corriger séparément.
2. **Résultat Web perdu quand le canal change** (`20a2418`, `ea2a636`). L'association STA déplace l'AP sur le canal
   du réseau cible et coupe le client avant la réponse HTTP. La page prévient, le propriétaire du Wi-Fi mémorise
   le dernier résultat Web, `GET /status` le sert et `/` l'inclut côté serveur (sans JavaScript, `Cache-Control:
   no-store`) : après réassociation, un rechargement suffit.

### Mesures `qual` (instrumentées, distinctes de l'image nominale)

- AP en attente de provisioning : tas utilisé 52 à 55 Kio sur 96 Kio, maximum 59 580 octets ; pile maximale 15 904 octets.
- Pendant une tentative STA (échec puis succès) : maximum de tas 63 908 octets (libre minimal 34 396), pile
  maximale 19 608 octets sur 32 544 réservés (marge 12 936). Les piles des tâches radio ne sont pas couvertes.

### Images (construites localement depuis le commit, `scripts/build-image.sh`, GCC 16.2.0)

| Image | Commit | SHA256 de l'image fusionnée |
| --- | --- | --- |
| Normale | `ea2a636` | `65b13afbbaf14e0fc8a2d092411ffee418e0542b72baae6c565ba2e93890ca6f` |
| `qual` | `ea2a636` | `2476ea5d7da9150bd99ec66cef8fa7eba6d1479f4894f9eaebba05576844bdc7` |

Les essais Web ont tourné sur l'image `qual` de `ea2a636` ; l'image normale de `ea2a636` n'a pas été flashée
(l'image normale de `314cf5a` l'a été pour Improv). L'image USB debug n'a pas été reconstruite ni testée. Les
empreintes CI de la PR n'ont pas pu être comparées (artefact non téléchargeable sans authentification).

### Limites

- AP encore visible après la configuration jusqu'au redémarrage : voulu par #1 (pas de bascule à chaud).
- Canal du réseau cible non relevé ce jour ; client AP : un appareil, pas de campagne multi-clients.
- Avertissement `esp_wifi_internal_tx returned error: 12309` observé pendant et après l'association STA ; non analysé.

## Vérification logicielle de cette implémentation

- 37 tests hôte, inchangés après les correctifs (politiques existantes, validation en octets, décodage du formulaire avec
  accents/symboles, refus des champs inconnus/dupliqués et des entrées invalides,
  échec du commit du flag puis nouvelle demande Web réussie sans bascule USB).
- Clippy hôte strict (`--all-targets -- -D warnings`), Rustfmt et `git diff --check` : PASS.
- Lien release ESP32-S3 normal : PASS (Rust esp 1.98.1.0, GCC 15.2.0_20250920).
  TaskPool produit : 16 808 octets ; pile réservée par le linker : 33 456 octets.
  Ce ne sont pas des mesures de consommation maximale. Avertissement RWX connu.
- Lien release ESP32-S3 avec `qual` (inclut USB debug) : PASS.
- Tests AP/radio/DHCP/navigateur et Improv simultanés sur matériel : voir la section de qualification ci-dessus.
