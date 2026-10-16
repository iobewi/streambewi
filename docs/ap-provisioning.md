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

## Qualification ciblée sur carte (à exécuter)

- Configuration vierge : AP WPA2, bail DHCP, page accessible, aucun MSC ; Improv officiel en parallèle.
- Web : SSID avec espace/accents et mot de passe avec symboles ; mauvais mot de passe puis réussite.
- Credentials puis flag persistés : redémarrage -> STA, flux, prébuffer, MSC, aucun AP.
- BOOT 3 s : retour AP + Web + Improv au reboot.
- Démarrer un provisioning Web puis Improv : aucune opération Wi-Fi concurrente ni réponse sur le mauvais port.
- Observer heap/pile en mode AP et vérifier la disponibilité du formulaire après un échec STA.

Les tests hôte et liens S3 ne constituent pas un PASS radio/DHCP/HTTP matériel.

## Vérification logicielle de cette implémentation

- 37 tests hôte (politiques existantes, validation en octets, décodage du formulaire avec
  accents/symboles, refus des champs inconnus/dupliqués et des entrées invalides,
  échec du commit du flag puis nouvelle demande Web réussie sans bascule USB).
- Clippy hôte strict (`--all-targets -- -D warnings`), Rustfmt et `git diff --check` : PASS.
- Lien release ESP32-S3 normal : PASS (Rust esp 1.98.1.0, GCC 15.2.0_20250920).
  TaskPool produit : 16 808 octets ; pile réservée par le linker : 33 456 octets.
  Ce ne sont pas des mesures de consommation maximale. Avertissement RWX connu.
- Lien release ESP32-S3 avec `qual` (inclut USB debug) : PASS.
- Tests AP/radio/DHCP/navigateur et Improv simultanés sur matériel : NON EXÉCUTÉS.
