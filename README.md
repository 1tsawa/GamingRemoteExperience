# GamingRemoteExperience

Streaming de jeu / bureau à distance à la Parsec, **sans serveur d'authentification** :
chaque machine a sa clé, le host décide seul qui peut se connecter.

Windows uniquement pour l'instant (host et client). Rust, encodage/décodage matériel H.265 (NVIDIA), UDP chiffré AES-256-GCM.

## Installation (setup.exe)

Téléchargez `GRX-Setup-<version>.exe` (page des releases GitHub du projet) et lancez-le :

- **Pour moi seulement** (sans droits administrateur, dans `%LOCALAPPDATA%\Programs\GamingRemoteExperience`) ou **pour tous les utilisateurs** (administrateur, `Program Files`) : le choix est demandé au lancement (`/CURRENTUSER` ou `/ALLUSERS` en ligne de commande).
- Installe `grx-gui.exe`, `grx-host.exe`, `grx-client.exe`, README et TUTO ; raccourci menu Démarrer (bureau en option) ; les liens d'invitation `grx://` ouvrent l'onglet Client avec le lien pré-rempli (rien n'est envoyé avant votre clic sur « Appairer et connecter »).
- Composant optionnel **pilote manettes ViGEmBus** (nécessaire au host pour les manettes ; installé seulement s'il est absent ; demande l'élévation).
- **Pare-feu** : en installation « pour tous », une règle UDP entrante pour `grx-host.exe` est créée (case cochée par défaut) ; en installation « pour moi », Windows demandera l'autorisation au premier lancement du host (ou `grx-host firewall-help`).
- Aucune dépendance : le runtime C est lié statiquement (plus besoin du « Visual C++ Redistributable »).
- **Désinstallation** : Paramètres Windows → Applications. Vos données (`%APPDATA%\GamingRemoteExperience` : identités, clients autorisés, hosts) sont **conservées** sauf si vous répondez « Oui » à « Supprimer aussi vos données ? » (en silencieux : `/DELETEDATA`).
- SmartScreen peut afficher « Windows a protégé votre ordinateur » (setup sans certificat de signature de code) : « Informations complémentaires » → « Exécuter quand même ».

**Mises à jour** : grx-gui vérifie au démarrage (au plus une fois par jour, désactivable dans le menu `v0.x.y` en haut à droite, qui affiche aussi la version et le bouton « Rechercher des mises à jour »). Une nouvelle version s'annonce par un bandeau « Version X disponible — notes — Installer maintenant / Plus tard ». Le manifeste de la release est **signé (Ed25519)**, le setup est vérifié (taille + SHA-256) et une version plus ancienne n'est jamais installée. Si le host tourne ou qu'une session est ouverte, l'installation attend la fin de la session (ou bouton « Arrêter … et installer maintenant »). Publication : [docs/RELEASE.md](docs/RELEASE.md).

## Démarrage rapide (depuis les sources)

```powershell
cargo build --release -p grx-host -p grx-client     # binaires dans target\release
```

**Sur le PC à contrôler (host)**

```powershell
# 1. Lancer le host : il ouvre tout seul le port UDP 7777 sur la box (UPnP), voir plus bas
target\release\grx-host.exe run --bind [::]:7777
#    options : --codec hevc|h264  --fps 60  --max-bitrate 50  --fill  --no-input  --no-audio
#              --no-upnp  --external-port 7777  --upnp-lease 3600

# 2. Dans une autre console : invitation à usage unique avec l'adresse publique détectée
target\release\grx-host.exe invite --addr auto
# -> affiche  grx://<IP publique>:7777/<clé du host>/<jeton>  (+ variante IPv6)
#    ou une adresse fixe : --addr MON_IP_OU_DNS:7777
```

**Sur le PC client**

```powershell
# Première connexion : colle le lien. Le host est mémorisé sous ce nom.
target\release\grx-client.exe play "grx://…" --save-as monpc
# Les fois suivantes, plus besoin de jeton :
target\release\grx-client.exe play monpc
```

Gestion des clients autorisés côté host : `grx-host list` (affiche aussi le rôle), `grx-host revoke <début d'empreinte>` (coupe la session en cours en ~1 s), `grx-host set-role <début d'empreinte> view-only|full` (spectateur : voit et entend, n'injecte rien).
Durcissement de l'injection côté host : `grx-host run --no-keyboard --no-mouse --no-gamepad --block-win-key` (touches Windows filtrées). Manettes virtuelles : `--gamepad-type auto|xbox|ds4`.
Vérifiez que l'empreinte affichée par `invite` est celle que le client voit à la connexion.

## Accès depuis Internet

Seul le **host** doit être joignable ; le client n'a rien à ouvrir. Il n'y a pas de serveur de NAT traversal : le host demande lui-même la redirection à la box.

- **UPnP automatique (par défaut)** : `grx-host run` cherche la box (UPnP IGD, puis PCP/NAT-PMP en repli, ≤ 3 s), redirige le port UDP externe 7777 (ou `--external-port N`) vers ce PC avec un bail d'1 h renouvelé à mi-vie, et **supprime la redirection à l'arrêt** (Ctrl+C). Si le port est déjà redirigé vers une autre machine, il prend 7778…7781 puis un port aléatoire : régénérez l'invitation. Tout se passe dans un thread : un échec (UPnP désactivé sur la box…) est journalisé et le host continue en local. `--no-upnp` désactive tout (redirection manuelle sur la box). Si le host est tué brutalement, la redirection expire seule à la fin du bail.
- **`invite --addr auto`** : prend l'adresse de la redirection du host lancé (`status.json` du dossier de données), sinon lit l'IP externe sur la box et utilise `--port` (7777) en rappelant de lancer le host. Message explicite si la box n'a pas d'IP publique.
- **CGNAT / double NAT** : si l'IP « externe » de la box est privée (10/8, 172.16/12, 192.168/16) ou en 100.64/10, l'UPnP ne suffira pas (le host et `invite` le disent). Solutions : IPv4 publique « full-stack » auprès du FAI, box/routeur amont en mode bridge, IPv6, ou VPN (Tailscale…).
- **IPv6** : avec `--bind [::]:7777` le host écoute en IPv4 **et** IPv6 ; `invite --addr auto` affiche aussi le lien `grx://[2a01:…]:7777/…` (adresse IPv6 globale stable de la machine, lue localement). En IPv6 il n'y a pas de NAT : il suffit d'autoriser le port UDP dans le pare-feu Windows (certaines box filtrent aussi l'IPv6 entrant : option « pare-feu IPv6 » de la box).
- **Pare-feu Windows** : grx-host ne le modifie jamais. Au démarrage il vérifie (lecture seule) qu'une règle autorise `grx-host.exe` en entrée UDP et affiche sinon la commande à lancer en administrateur ; `grx-host firewall-help [--port 7777]` l'imprime (variantes PowerShell et `netsh`). Si vous avez refusé la fenêtre « Autoriser l'accès » de Windows, une règle de **blocage** existe : la commande pour la retirer est aussi affichée.
- **IP publique dynamique** : l'IP de la box change parfois et les liens `grx://IP:port` deviennent caducs. Un nom DuckDNS gratuit (`monpc.duckdns.org`, mis à jour par le client DuckDNS ou la box) règle ça : `grx-host invite --addr monpc.duckdns.org:7777`. Les clients déjà appairés peuvent aussi relancer `play` avec un nouveau lien.
- **État** : pendant l'exécution, `status.json` (dossier de données du host) contient `{pid, bind, started_ms, upnp: {ok, state, method, external_addr, lease_secs, error, warnings}, ipv6, firewall}` ; il est supprimé à l'arrêt. Diagnostic de la box : `cargo run -p grx-portmap --example probe -- info|list|get <port>`.

### Raccourcis du client `play`
`Ctrl+Maj+M` **menu en overlay** (aussi via le bouton qui apparaît en haut au centre) · `Ctrl+Maj+S` stats à l'écran · `Ctrl+Maj+Q` déconnexion · `F11` plein écran · `F10` stats dans le titre · `Ctrl+Alt+Maj+G` (modifiable : `--capture-key`) capture souris relative · `Alt+F4` quitter.
Menu (comme Parsec) : plein écran, statistiques détaillées, volume + muet, **bande passante max** et **FPS max** (envoyés au host, appliqués à chaud), capture souris, **verrouillage du clavier** (Win/Alt+Tab envoyés au host), VSync, bouton flottant, rappel des raccourcis, déconnexion. Menu ouvert : souris et clavier restent au client. Détails dans [TUTO.md](TUTO.md).
Options : `--no-input`, `--no-audio`, `--no-gamepad`, `--gamepad-as xbox|auto|ds4`, `--volume`, `--stats`, `--fullscreen`, `--vsync`, `--width/--height`, `--local-cursor`.

### Manettes
Branchez la manette **côté client** : elle apparaît côté host comme une manette virtuelle (ViGEmBus doit être installé sur le host).
- **Lues par le client** : Xbox 360 / One / Series et toutes les manettes XInput (bouton Guide compris), **DualShock 4** et **DualSense** en USB et Bluetooth (lecture HID directe, Croix = A, Rond = B, Carré = X, Triangle = Y, PS = Guide, clic du pavé tactile), et au mieux les manettes génériques / Switch Pro reconnues par Windows.Gaming.Input. Jusqu'à 4 manettes, branchement/débranchement à chaud, index stables. `grx-client gamepads` liste les manettes détectées et affiche leur état (`--rumble-test` les fait vibrer).
- **Côté host** : Xbox 360 virtuelle par défaut (compatibilité maximale). `play --gamepad-as auto` demande une **DualShock 4 virtuelle** pour les manettes Sony (symboles PlayStation dans les jeux, bouton PS et clic du pavé tactile) ; `grx-host run --gamepad-type xbox|ds4|auto` impose le choix (défaut `auto` = choix du client).
- **Vibrations** : renvoyées du jeu vers la manette physique (Xbox via XInput, DualShock 4 / DualSense via HID) quand la manette virtuelle est une Xbox 360. Pas de vibration en DualShock 4 virtuelle ni sur les manettes génériques ; gyroscope, pavé tactile (position), LED et gâchettes adaptatives non transmis.
- Fenêtre sans focus ou menu ouvert : les manettes envoient l'état neutre. Le menu affiche une ligne « Manettes : … » avec leur type.
Curseur : comme Parsec, le **curseur du host** (forme du jeu, position, visibilité) est dessiné par-dessus la vidéo, y compris en mode capture souris, et le curseur Windows local est masqué au-dessus de l'image. Quand le jeu cache son curseur (caméra au clic droit dans WoW…), il disparaît aussi chez le client. `--local-cursor` revient à l'ancien comportement (curseur Windows local seul).

### Tests sans risque
`grx-host run --no-input --no-audio --fill` + `grx-client bench <cible> --secs 10` mesure FPS, débit, latence, perte sans rien injecter.
`cargo test --workspace --release` lance les tests (certains tests GPU/audio ont besoin du matériel).

## Interface graphique (appairage)

```powershell
cargo build --release -p grx-gui -p grx-host -p grx-client
target\release\grx-gui.exe          # grx-host.exe et grx-client.exe doivent être à côté
# dossiers de test (ne touche pas aux vrais fichiers) :
target\release\grx-gui.exe --data-dir-host C:\tmp\h --data-dir-client C:\tmp\c   # ou GRX_DATA_DIR_HOST / GRX_DATA_DIR_CLIENT
```

- **Onglet Host** : empreinte du host (copiable) ; création d'invitation (adresse publique mémorisée, bouton « Détecter mon IP locale » sans aucune requête Internet, validité 5 min / 1 h / 24 h) avec lien, bouton Copier, **QR code** et compte à rebours ; liste des clients autorisés (rôle Complet / Lecture seule, révocation avec confirmation), rafraîchie toute seule quand `trust.json` change ; démarrage/arrêt de `grx-host run` (bind, codec, fps, `--no-input`, `--no-keyboard/--no-mouse/--no-gamepad`, `--block-win-key`, `--no-audio`) avec ses dernières lignes de sortie. L'arrêt envoie Ctrl+C (arrêt propre), un host lancé depuis un terminal est détecté mais pas arrêté.
- **Accès Internet dans la GUI** : quand le host tourne, la carte Statut lit `status.json` (~1 s) : redirection ouverte (méthode, adresse externe, bail restant), en cours, ou échec avec la cause et un conseil (CGNAT, double NAT, UPnP désactivé → redirection manuelle) ; état du pare-feu Windows et bouton « Voir la commande » qui affiche/copie la sortie de `grx-host firewall-help` (la GUI ne l'exécute **jamais**). Options de lancement : « Ouvrir le port automatiquement (UPnP) » (cochée par défaut, sinon `--no-upnp`) et port externe (`--external-port`). Carte Invitation : l'adresse est pré-remplie avec l'adresse externe de la redirection (sans écraser une saisie manuelle), bouton « Adresse publique automatique » (même logique que `invite --addr auto` : `status.json`, sinon IP externe demandée à la box, réseau local uniquement) et lien IPv6 alternatif (même jeton) quand le host écoute en `[::]`. Fermer la GUI arrête le host par Ctrl+C, ce qui supprime la redirection de la box.
- **Onglet Client** : hosts mémorisés (Renommer / Supprimer / Connecter avec `--no-input`, `--no-audio`, `--fullscreen`), ajout d'un host par lien `grx://` (bouton Coller, validation immédiate, adresse et **empreinte à comparer à l'œil**, nom), « Appairer et connecter » ; empreinte de ce client.
- Le jeton n'existe qu'en mémoire (jamais écrit par la GUI) ; il est passé à `grx-client` par l'entrée standard (`grx-client play - --save-as nom`), pas en ligne de commande. Préférences non secrètes dans `<dossier host>\gui.json`.
- Pas de scan de QR code par caméra (scanner avec un téléphone puis coller le lien).

## Organisation

| Crate | Rôle |
|---|---|
| `grx-core` | identité, invitations, liste de confiance, poignée de main Noise IK, session chiffrée |
| `grx-transport` | transport temps réel sans I/O : canaux vidéo/audio/entrées, FEC Reed-Solomon, retransmission, pacing, contrôle de débit |
| `grx-video` | capture DXGI + encodage matériel H.264/H.265 (Media Foundation, NVIDIA) |
| `grx-render` | décodage matériel + fenêtre (swapchain flip, tearing) |
| `grx-audio` | capture WASAPI loopback, Opus (libopus vendorisé), lecture avec jitter buffer |
| `grx-input` | format des événements, injection SendInput et manettes ViGEm |
| `grx-portmap` | redirection de port automatique : UPnP IGD, PCP/NAT-PMP, détection CGNAT/double NAT, interfaces locales (IPv6) |
| `grx-host` / `grx-client` | exécutables |
| `grx-gui` | interface graphique d'appairage (egui) : invitations + QR, clients autorisés, hosts mémorisés |

Voir [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) pour la conception, le modèle de sécurité et la feuille de route.
