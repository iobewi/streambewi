# Configuration illisible ou formatage interrompu : refus et récupération

Suivi dédié (voir aussi la limitation EKV en coupure de courant, `iobewi/rust-mcu-libs#70`).

## Comportement voulu (inchangé)

Au démarrage, la région `config` (partition `data/0x40`, `0x380000`, `0x80000` dans la disposition OTA ; `0x9000`, `0x40000` avant `docs/ota.md`) est :

- **entièrement vierge** : le firmware l'initialise lui-même (formatage EKV) ;
- **non vierge** : le firmware la **monte** ; si le montage échoue, il **refuse de démarrer** et n'écrit jamais rien de
  secours (pas de formatage automatique, pas de reprise d'une ancienne région NVS).

C'est volontaire : une région non vierge peut contenir des identifiants Wi-Fi ou les données d'une autre version, et
un formatage « de secours » les détruirait sans que personne l'ait demandé.

## Le cas : formatage interrompu

Pendant le **premier démarrage** d'une carte à config vierge, le formatage efface puis écrit plusieurs pages. Une coupure
de courant (ou un débranchement) au milieu laisse une région **ni vierge ni montable** : au démarrage suivant le firmware
la voit non vierge, ne la formate pas, échoue au montage et s'arrête.

**Symptômes**
- aucun Improv, aucun disque, aucune activité réseau ;
- sur l'UART0 (si observable), la ligne `STREAMBEWI:CONFIG-MOUNT` puis plus rien ; l'image nominale ne journalise rien
  d'autre ;
- la carte ne revient pas seule, même après plusieurs redémarrages (le bouton BOOT ne sert pas : la récupération BOOT
  s'exécute après le montage).

Le même arrêt se produit pour toute région non vierge illisible (autre firmware, flash corrompue).

## Récupération (vérifiée le 15 octobre 2026)

Remettre la région `config` **entièrement à blanc**, puis redémarrer : la carte la formate, annonce `authorized` en
Improv, et il faut **reprovisionner** le Wi-Fi.

1. Effacer la région (adapter le port) :
   ```
   python -m esptool --chip esp32s3 --port COMx erase_region 0x380000 0x80000
   ```
   ou, avec le banc, écrire un fichier de `0x80000` octets `0xFF` à `0x380000` (`benchctl.py write <fichier>@0x380000`).
2. Redémarrer la carte (débrancher puis rebrancher, ou RESET).
3. Reprovisionner (Improv Serial : `improv.py wifi` du banc, `improv_serial.py` sous Windows, ou ESP Web Tools).

L'effacement ne touche ni le bootloader, ni la table de partitions, ni l'application.

**Vérification faite** : sur une DevKitC-1, la région `config` a été remplie d'octets sans structure (refus attendu :
`STREAMBEWI:CONFIG-MOUNT`, aucun octet Improv, aucun formatage), puis remise à blanc par écriture de `0x40000` octets
`0xFF` à `0x9000` : la carte a redémarré et Improv a de nouveau répondu `authorized`.
Une région dont **seules 2 pages sur 64** étaient corrompues a, elle, été **montée sans erreur** par EKV (le firmware
démarre) : ce n'est pas le cas décrit ici.

## Pistes (non faites, hors périmètre)

- rendre le formatage EKV atomique ou reprenable (amont, `rust-mcu-libs#70`) ;
- journaliser plus que `STREAMBEWI:CONFIG-MOUNT` pour distinguer « jamais formatée » de « formatage interrompu » ;
- un témoin d'écriture de fin de formatage : à étudier avec le propriétaire du stockage, **sans** formatage automatique
  d'une région non vierge.

## Depuis l'OTA (`docs/ota.md`)

Le watchdog de probation (30 s) est armé dès le démarrage et ne se désarme qu'à la décision de boot qui suit le montage de
`config`. Un montage refusé **n'arrête donc plus définitivement la carte** : elle redémarre toutes les 30 s environ, et
`STREAMBEWI:CONFIG-MOUNT` est réaffiché à chaque tentative. La récupération ci-dessus reste la même (effacer la région
`config`, désormais `0x380000` / `0x80000`).
