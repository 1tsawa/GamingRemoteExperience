# Tuto : tester GamingRemoteExperience

Deux rôles :
- **Host (serveur)** : le PC qu'on contrôle à distance (votre PC de jeu).
- **Client** : le PC depuis lequel on joue (chez un ami, ou un portable en 4G).

---

## 0. Installer (le plus simple)

Sur chaque PC (host et client), lancez `GRX-Setup-<version>.exe` :
- choisissez **« Installer pour moi seulement »** (pas besoin d'être administrateur) ou **pour tous les utilisateurs** (crée aussi la règle de pare-feu du host) ;
- sur le PC **host**, laissez cochée la case **« Pilote manettes ViGEmBus »** si vous voulez jouer à la manette (Windows demandera l'autorisation administrateur) ;
- si SmartScreen bloque : « Informations complémentaires » → « Exécuter quand même ».

Ensuite, lancez **GamingRemoteExperience** depuis le menu Démarrer. Un lien `grx://…` reçu par messagerie ouvre directement l'onglet Client avec le lien pré-rempli.
Les mises à jour sont proposées dans l'application (bandeau « Version X disponible ») ; elles ne s'installent jamais pendant une session.
Le numéro de version est affiché en haut à droite de la fenêtre (`grx-host.exe --version` en ligne de commande).

## 0 bis. Sans installateur : préparer les fichiers à la main

```powershell
cd C:\Users\Ludow\Documents\vscode\GamingRemoteExperience
cargo build --release -p grx-host -p grx-client -p grx-gui
```

Copiez ces 3 fichiers dans un dossier `GRX` (clé USB, cloud…) :

```
target\release\grx-host.exe
target\release\grx-client.exe
target\release\grx-gui.exe
```

Le PC client n'a pas besoin de Rust. Il faut seulement :
- Windows 10 ou 11 ;
- l'extension **« HEVC Video Extensions »** du Microsoft Store (sinon voir le dépannage) ;
- plus besoin du « Visual C++ Redistributable » : le runtime C est intégré aux exe depuis la 0.2.0.

---

## 1. Côté HOST (votre PC)

### Avec l'interface graphique (le plus simple)

1. Lancez `grx-gui.exe` et ouvrez l'onglet **Host — PC à contrôler**.
2. Cliquez **Démarrer le host**. La box ouvre le port automatiquement (UPnP) et le statut affiche l'adresse publique.
3. Dans **Inviter un nouveau client** :
   - l'adresse publique est pré-remplie ; sinon cliquez **Adresse publique automatique** ;
   - choisissez une durée (1 heure, par exemple) et cliquez **Créer l'invitation**.
4. Envoyez le lien `grx://…` à votre ami **par un canal privé** (messagerie, pas en public).
   Le lien sert **une seule fois**.
5. Notez l'**empreinte du host** affichée en haut, du type `SP7C-BQRZ-…` : votre ami devra vérifier qu'il voit la même.
6. Quand votre ami s'est connecté, il apparaît dans la liste des **clients autorisés**. Vous pouvez :
   - le passer en **lecture seule** : il voit l'écran mais ne contrôle rien ;
   - le **révoquer**.

Laissez la fenêtre ouverte pendant le test. La fermer arrête le host.

### En ligne de commande (équivalent)

```powershell
# Terminal 1 : lancer le host (garder ouvert ; Ctrl+C pour arrêter)
.\grx-host.exe run --bind [::]:7777 --block-win-key

# Terminal 2 : créer l'invitation (adresse publique détectée toute seule)
.\grx-host.exe invite --addr auto --ttl-min 60
```

Autres commandes utiles :

```powershell
.\grx-host.exe info              # empreinte du host
.\grx-host.exe list              # clients autorisés
.\grx-host.exe revoke ABCD       # révoquer un client (début de son empreinte)
.\grx-host.exe set-role ABCD view-only   # spectateur (ou: full)
.\grx-host.exe firewall-help     # commande à lancer si le pare-feu bloque
```

Options utiles de `run` :

| Option | Effet |
|---|---|
| `--codec h264` | Si le client n'a pas l'extension HEVC |
| `--max-bitrate 20` | Limite le débit (Mb/s), si la connexion de l'ami est faible |
| `--fps 30` | Divise la charge par deux |
| `--no-input` | L'ami voit l'écran mais ne peut rien contrôler |
| `--no-audio` | Pas de son |
| `--fill` | Débit d'images constant même sur un écran fixe (utile pour mesurer) |
| `--display 2` | Écran capturé au début de la session (1, 2… de gauche à droite, la liste s'affiche au lancement ; défaut : l'écran principal). L'ami peut en changer depuis son menu |

> **Pare-feu Windows** : au premier lancement, Windows peut demander « Autoriser l'accès ». Cochez **Privé** et validez.
> En cas de doute, `grx-host.exe firewall-help` affiche la commande à lancer en administrateur.

---

## 2. Côté CLIENT (PC de votre ami)

### Avec l'interface graphique

1. Lancez `grx-gui.exe` et ouvrez l'onglet **Client — PC qui contrôle**.
2. Dans **Ajouter un host**, cliquez **Coller depuis le presse-papiers** avec le lien `grx://…` copié.
3. **Vérifiez que l'empreinte affichée est exactement celle du host.** Si elle est différente, arrêtez : ce n'est pas le bon PC.
4. Donnez un nom au host (par exemple `pc-ludo`). Dans **Hosts mémorisés**, vérifiez que les cases « sans envoyer d'entrées » et « sans audio » sont **décochées**, pour pouvoir jouer et entendre le son. Cliquez **Appairer et connecter**.
5. Les fois suivantes, le host est dans **Hosts mémorisés** : cliquez **Connecter**. Plus besoin de lien.

### En ligne de commande

```powershell
# Première fois (lien reçu)
.\grx-client.exe play "grx://…" --save-as pc-ludo --stats

# Fois suivantes
.\grx-client.exe play pc-ludo --stats
```

### Pendant la session

| Touche | Action |
|---|---|
| `Ctrl+Maj+M` | **Menu en overlay** (comme Parsec), voir ci-dessous ; même raccourci pour le fermer (ou `Échap`) |
| `Ctrl+Maj+S` | Statistiques détaillées à l'écran (FPS, débit, RTT, perte, latences, résolution, codec) |
| `Ctrl+Maj+Q` | Déconnexion |
| `Ctrl+Maj+←` / `Ctrl+Maj+→` | Écran précédent / suivant du PC host (s'il en a plusieurs) |
| `F11` | Plein écran / fenêtré |
| `F10` | Stats (FPS, débit, ping, pertes) dans le titre de la fenêtre |
| `Ctrl+Alt+Maj+G` | Capture de la souris en mode relatif (jeux FPS) ; même raccourci pour sortir |
| `Alt+F4` | Quitter |

Ces raccourcis ne sont jamais envoyés au PC host.

**Menu en overlay** : `Ctrl+Maj+M`, ou le petit bouton « Menu GRX » qui apparaît en haut au centre quand la souris s'approche du bord haut de la fenêtre (masquable dans le menu). Tant que le menu est ouvert, la souris et le clavier vont au menu, **pas** au PC host (la capture souris est suspendue puis rétablie à la fermeture). Navigation à la souris (clic, glisser, molette) ou au clavier (`↑`/`↓` choisir la ligne, `←`/`→` régler, `Entrée`/`Espace` activer, `Échap` fermer ; un clic hors du panneau ferme aussi). Réglages :

| Ligne | Effet |
|---|---|
| Plein écran | bascule plein écran / fenêtré (comme `F11`) |
| Statistiques | panneau de statistiques en haut à gauche (comme `Ctrl+Maj+S`) |
| Volume | curseur 0-100 % + bouton Son/Muet |
| Bande passante max | plafond de débit envoyé au host, de 2 Mb/s jusqu'à « Auto » (= maximum du host, `--max-bitrate`) ; appliqué en ~1 s |
| FPS max | 30 / 60 / 120 / 144 selon la fréquence de votre écran ; le host ne dépasse jamais son `--fps` (lancez-le avec `--fps 144` pour jouer à 144) |
| Écran | un bouton par écran du PC host (« 1 », « 2 »… de gauche à droite, l'écran affiché en bleu ; au survol : nom et résolution, `*` = écran principal). Bascule en quelques dizaines de ms sans couper la session ni le son ; aussi `Ctrl+Maj+←/→`. Ligne absente si le host n'a qu'un écran |
| Capture souris | active la capture relative à la fermeture du menu |
| Verrouiller le clavier | `Win`, `Alt+Tab`, `Alt+Échap`, `Ctrl+Échap`, `Alt+Espace` partent au host au lieu d'agir sur votre PC (`Ctrl+Alt+Suppr` et `Win+L` restent à Windows) |
| VSync | synchronisation verticale (désactivée par défaut : latence minimale) |
| Bouton flottant | affiche / masque le bouton « Menu GRX » |
| Position du menu | ↖ ↗ ▣ ↙ ↘ : haut-gauche, haut-droite, centre, bas-gauche, bas-droite ; ↺ : position d'origine (menu et bouton flottant) |
| Position des stats | mêmes positions rapides pour le panneau de statistiques ; ↺ : en haut à gauche |
| Raccourcis | rappel des raccourcis |
| Déconnexion | ferme la session proprement |

**Déplacer le menu, les stats et le bouton** (s'ils vous gênent) :
- **menu** : attrapez sa barre de titre (« GRX · nom-du-host », le curseur devient une croix de déplacement) et glissez-la où vous voulez ;
- **statistiques** : menu ouvert, attrapez le panneau des stats n'importe où et glissez-le (menu fermé, la souris va au PC host, donc on ne peut pas le saisir) ;
- **bouton « Menu GRX »** : glissez-le où vous voulez (un simple clic, sans bouger, ouvre toujours le menu). Il apparaît quand la souris s'en approche, ou s'approche du bord de fenêtre contre lequel il est posé ;
- ou utilisez les lignes « Position du menu » / « Position des stats » (positions rapides, `←`/`→` au clavier, `Entrée` = position d'origine).

Les éléments restent toujours entièrement visibles : leur position est relative à la fenêtre, elle suit donc les redimensionnements et le passage en plein écran.

**Réglages mémorisés** : positions, statistiques affichées, bouton flottant, volume/muet, VSync, capture souris, bande passante, FPS max et écran choisi sont enregistrés dans `overlay.json` (dossier de données du client, `%APPDATA%\GamingRemoteExperience\client`) et rétablis à la session suivante ; la bande passante et les FPS mémorisés sont renvoyés au host dès la connexion, et l'écran choisi est redemandé s'il existe encore (même nom, résolution et position). Fichier supprimé ou abîmé = réglages par défaut. `--vsync` ou un `--volume` explicite l'emportent sur la valeur mémorisée.

Le bas du menu affiche les limites du host et les plafonds qu'il a réellement appliqués. Un client en lecture seule peut aussi régler sa bande passante et ses FPS, et choisir l'écran affiché. Les statistiques indiquent l'écran affiché (« 2/3 VG27A » = 2e écran sur 3). La souris (hors mode capture) vise bien l'écran affiché. Pas encore de choix de la résolution du PC host (évolution future).

**Curseur** : le curseur affiché sur l'image est celui **du PC host** (forme personnalisée du jeu comprise, ex. World of Warcraft) ; votre curseur Windows est masqué au-dessus de la vidéo et réapparaît sur les bandes noires. Il reste affiché en mode capture souris, et disparaît quand le jeu le cache (caméra au clic droit). Pour retrouver l'ancien comportement (votre curseur Windows local, pas celui du host) : `--local-cursor`, par exemple `.\grx-client.exe play pc-ludo --local-cursor`.

---

## 3. Ce qu'il faut tester (et me remonter)

Faites les tests dans l'ordre et notez le résultat (OK / KO + message d'erreur).

**Test A : connexion depuis Internet (le plus important)**
- Le client est sur **un autre réseau** : chez l'ami, ou un PC portable en **partage de connexion 4G/5G**.
- Lancez `grx-client.exe bench "grx://…" --save-as pc-ludo --secs 20`.
- Notez la ligne de résumé : FPS, débit, RTT, perte.

**Test B : jouer**
- `grx-client.exe play pc-ludo --stats` pendant 5 à 10 minutes, sur un jeu ou une vidéo.
- Vérifiez :
  - [ ] l'image est fluide et nette ;
  - [ ] **le son** est audible et synchronisé avec l'image ;
  - [ ] **le clavier et la souris** fonctionnent ;
  - [ ] **le plein écran** (F11) fonctionne ;
  - [ ] **le menu** (`Ctrl+Maj+M` ou bouton en haut au centre) s'ouvre ; baisser « Bande passante max » fait baisser le débit affiché par `Ctrl+Maj+S` ; « Verrouiller le clavier » envoie bien `Alt+Tab` au host ;
  - [ ] **la capture souris** (`Ctrl+Alt+Maj+G`) fonctionne dans un jeu à la 1re personne ;
  - [ ] **le curseur du jeu** (forme personnalisée) s'affiche côté client, suit la souris sans retard gênant et disparaît quand le jeu le cache (clic droit maintenu dans WoW) ;
  - [ ] une **manette** branchée côté client est reconnue par le jeu côté host (voir « Manettes » ci-dessous).
- Notez la latence ressentie : imperceptible, acceptable, ou gênante.

**Test C : sécurité et gestion**
- Réutilisez le même lien d'invitation sur un 2e PC : il doit être **refusé**.
- Sur le host, **révoquez** le client pendant une session : elle doit se couper en environ 1 s.
- Passez le client en **lecture seule** : il doit voir l'écran sans pouvoir rien contrôler.

**Test D : coupures**
- Débranchez le Wi-Fi du client 10 s, puis rebranchez : le client doit se fermer avec « connexion perdue ». Une reconnexion avec `play pc-ludo` doit marcher.
- Fermez le host (Ctrl+C) : le client doit s'arrêter proprement en quelques secondes.

Envoyez-moi pour chaque test : OK / KO, les lignes de stats, et les messages d'erreur éventuels (copier-coller du terminal).

### Manettes (Xbox, PlayStation, génériques)

**Ce qui marche** : Xbox 360 / One / Series (USB, sans fil, Bluetooth) et manettes compatibles XInput ; **DualShock 4** et **DualSense** (PS5) en USB et en Bluetooth, sans pilote ni DS4Windows ; manettes génériques et Switch Pro « au mieux » (si Windows les reconnaît). Jusqu'à 4 manettes. On peut les brancher/débrancher pendant la session.

1. **Côté client, avant de se connecter** : `grx-client.exe gamepads` liste les manettes et affiche leur état pendant 5 s (bougez les sticks, appuyez sur des boutons : les valeurs changent). `--rumble-test` fait vibrer chaque manette une demi-seconde.
2. Lancez `play` comme d'habitude : dans le menu (`Ctrl+Maj+M`), la ligne du bas indique « Manettes : 1 Xbox (XInput) » ou « 1 DualSense (USB) »…
3. **Côté host**, ViGEmBus doit être installé (le host affiche « manettes virtuelles : 0:Xbox360 » à la première action). Testez dans le jeu, ou dans `joy.cpl` (Panneau de configuration > Contrôleurs de jeu) sur le host.
4. **Manette PlayStation** : par défaut le host voit une manette Xbox 360 (marche avec tous les jeux). Pour que le jeu affiche les symboles PlayStation : `play pc-ludo --gamepad-as auto` (DualShock 4 virtuelle). Le host peut imposer : `grx-host run --gamepad-type xbox` ou `ds4`.
5. **Vibrations** : un jeu qui fait vibrer la manette Xbox 360 virtuelle fait vibrer votre manette (Xbox ou PlayStation). Pas de vibration avec la DualShock 4 virtuelle.
6. Quand la fenêtre du client perd le focus ou que le menu est ouvert, les manettes sont remises au neutre côté host. `--no-gamepad` désactive complètement les manettes.

Limites : gyroscope, position sur le pavé tactile, LED, gâchettes adaptatives du DualSense : non transmis. Si DS4Windows / Steam Input émulent déjà une manette Xbox, la manette peut apparaître deux fois (fermez-les ou désactivez l'émulation).

---

## 4. Dépannage

| Problème | Solution |
|---|---|
| Le client affiche « aucune réponse » | Le host n'est pas lancé, l'adresse est fausse, le lien a déjà servi, ou le client a été révoqué (le host ne répond jamais aux inconnus). Créez une nouvelle invitation. |
| Le statut UPnP du host est en échec | Vérifiez que l'UPnP est activé dans l'interface de la box. Sinon, créez une redirection manuelle UDP 7777 vers l'IP locale du PC host. |
| Ça marche en local mais pas depuis Internet | Pare-feu : `grx-host.exe firewall-help`. Vérifiez que le réseau Windows est en **Privé**. |
| Erreur de décodeur HEVC côté client | Installez « HEVC Video Extensions » (Microsoft Store), ou lancez le host avec `--codec h264`. |
| Image saccadée, pertes élevées | Lancez le host avec `--max-bitrate 15 --fps 30` et réessayez. Préférez un câble Ethernet côté host. |
| L'adresse publique a changé | Recréez une invitation, ou utilisez un nom DuckDNS (`--addr monpc.duckdns.org:7777`). |
| Une touche reste « enfoncée » côté host | Fermez le client : le host relâche tout à la déconnexion. |
| La manette n'est pas vue par le jeu | `grx-client gamepads` côté client doit la lister ; côté host, installez ViGEmBus (la console du host affiche « manettes virtuelles … »). Une manette PlayStation en DualShock 4 virtuelle n'est pas reconnue par certains jeux : revenez à `--gamepad-as xbox`. |

## 5. Bonnes pratiques de sécurité

- Le lien d'invitation est un **secret à usage unique** : ne le publiez pas.
- **Comparez toujours l'empreinte** du host lors de la première connexion.
- Révoquez les clients dont vous ne vous servez plus (`grx-host.exe list` puis `revoke`).
- Arrêtez le host quand vous ne l'utilisez pas : l'arrêt referme le port sur la box.
