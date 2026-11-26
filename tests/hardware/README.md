# Recette matérielle IDLab

Installer le SDK Python IDLab, puis les dépendances de la recette depuis la
racine du projet.

```bash
python -m pip install -r tests/hardware/requirements.txt
```

Connecter la carte dans IDLab et générer son jeton via **Autorisations**.
Configurer l’environnement du terminal qui exécutera pytest.

| Variable | Valeur |
| --- | --- |
| `IDLAB_URL` | Adresse du serveur IDLab joignable depuis ce terminal |
| `IDLAB_BOARD` | Nom de la carte dans IDLab |
| `IDLAB_BOARD_TOKEN` | Jeton de pilotage de cette carte |
| `IDLAB_ALLOW_INSECURE_HTTP` | `true` uniquement pour un accès HTTP distant sur un réseau privé de confiance |

Dans Bash, `read -rsp 'Jeton de carte IDLab : ' IDLAB_BOARD_TOKEN` permet de
saisir le jeton sans l’inscrire dans l’historique. Exécuter ensuite
`export IDLAB_BOARD_TOKEN`. Garder les secrets hors des fichiers du projet.

```bash
pytest tests/hardware/ -v
```

La recette prend le contrôle exclusif, récupère l’état de la carte et lit la
capture UART disponible. Une capture vide est valide. Elle libère le contrôle
en fin de test, y compris si une assertion échoue. Elle fonctionne avec le
seul jeton de carte et n’envoie aucune commande au matériel.

## Recette du firmware classique (campagne A)

Ces tests ciblent le firmware **sans OTA** (commit `8c54266`, layout `config` à `0x9000`,
application `factory` à `0x50000`), construit avec une feature de carte pour la LED
(`FEATURES=board-devkitc-1 scripts/build-image.sh OUT_DIR`). **Ne pas les lancer sur une carte
au layout OTA** : `test_a2_boot.py` attend `Loaded app from partition at offset 0x50000`.

| Fichier | Étape | Niveau | Prérequis |
| --- | --- | --- | --- |
| `test_a2_boot.py` | A2 : 10 `console.reset`, journal de boot | Actif léger (resets) | `IDLAB_*`, carte **sans** identifiants Wi-Fi |
| `test_a5_api.py` | A5 : API et page web | Lecture seule (requêtes invalides refusées) | `BOARD_URL` |
| `test_a5_active.py` | A5 : catalogue de stations | Actif (écrit en flash, coupe le disque USB ~2 s) | `BOARD_URL`, `IDLAB_*`, catalogue d'origine = station 1 seule |
| `test_a4_https.py` | A4 : flux HTTPS, lecture soutenue 150 s | Actif (changement de station) | `BOARD_URL`, `HTTPS_STREAM` |
| `test_a7_led.py` | A7 : LED (feature de carte requise) | Actif (luminosité, restaurée) | `BOARD_URL` |

`BOARD_URL` est l'adresse HTTP de la carte sur le réseau local (par exemple `http://<ip-de-la-carte>`).
`HTTPS_STREAM` est l'URL `https://` d'un flux MP3. Les tests actifs remettent l'état d'origine en
fin de test, même en cas d'échec, et ne suppriment jamais une station qu'ils n'ont pas ajoutée
(sauf `test_a5_active.py`, qui exige un catalogue réduit à la station 1).

Limites : aucun `POST /wifi` n'est envoyé (accepté par erreur, il changerait le Wi-Fi) ; l'audio
et les couleurs de la LED se confirment à l'oreille et à l'œil ; le journal de production ne
contient ni le chemin du flux, ni `tls up in`, ni les reprises d'offset (#31).
