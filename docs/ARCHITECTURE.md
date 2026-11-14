# GamingRemoteExperience : architecture

Clone de Parsec (host/client, UDP temps réel, encodage matériel) **sans serveur d'authentification** :
chaque machine possède sa propre identité cryptographique et le host décide seul qui peut se connecter.

## Décisions

| Sujet | Choix |
|---|---|
| Langage | Rust (workspace Cargo) |
| Périmètre v1 | Host Windows + client Windows |
| Authentification | Clés publiques X25519 + liste de confiance côté host (type `authorized_keys`) |
| Premier contact | Lien d'invitation `grx://ip:port/<clé host>/<jeton>`, usage unique, expiration |
| Découverte | IP:port direct (ou VPN type Tailscale) ; le host ouvre son port UDP sur la box par UPnP IGD (PCP/NAT-PMP en repli), voir « Accès depuis Internet » |
| Transport | UDP, Noise IK (`Noise_IK_25519_AESGCM_SHA256`), AES-256-GCM comme Parsec |

## Ce que fait Parsec (observé sur cette machine) et équivalent ici

| Parsec | Ici |
|---|---|
| `parsecd.exe` (client+host) + `pservice.exe` (service) | `grx-host` (service) / `grx-client` |
| Protocole UDP "BUD", AES-256-GCM, STUN | Noise IK sur UDP, AES-256-GCM ; STUN/relais optionnel (jalon 6) |
| Capture écran + encodeur NVENC H.265/H.264 | DXGI Desktop Duplication + NVENC (jalons 2-3) |
| Décodage matériel côté client | D3D11 Video / NVDEC (jalon 3) |
| Pilote écran virtuel `vdd` | Réutiliser un pilote IDD tiers existant (jalon 7, optionnel) |
| `vusb` + ViGEm (manettes) | ViGEmBus (déjà installé ici), crate `vigem-client` (jalon 5) |
| Compte + serveurs d'auth Parsec | **Supprimé** : identité = clé publique ; invitation signée par le host |

## Modèle de sécurité (jalon 1, implémenté)

- **Identité** : paire X25519 générée à la première exécution, stockée dans `%APPDATA%\GamingRemoteExperience\<role>\identity.json`. L'empreinte (`ABCD-EFGH-…`) est un SHA-256 tronqué de la clé publique.
- **Anti-MITM sans autorité** : le client connaît la clé du host *avant* de se connecter (elle est dans le lien d'invitation, puis mémorisée dans `hosts.json`). Noise IK authentifie le host par cette clé : un imposteur ne peut pas répondre.
- **Autorisation** : le host n'accepte que (a) une clé présente dans `trust.json`, ou (b) un jeton d'invitation valide, qui est consommé et ajoute la clé du client. Seul le hash du jeton est stocké.
- **Silence envers les inconnus** : aucune réponse (pas même une erreur) à un client non autorisé → pas de scan de ports applicatif, pas d'amplification UDP.
- **Anti-rejeu** : horodatage ±5 min, strictement croissant par client ET postérieur au démarrage du host (un INIT capturé avant un redémarrage ne peut pas être rejoué) sur la poignée de main ; compteur 64 bits explicite + fenêtre glissante de 1024 paquets sur les données (tolère le réordonnancement UDP). La fenêtre n'avance qu'après authentification du paquet.
- **Révocation** : `grx-host revoke <empreinte>` ; effet sur les nouvelles connexions ET sur la session en cours (revérifiée chaque seconde, voir « Revue de sécurité »).

### Format des paquets
```
INIT  [0x01][Noise msg1 : e, es, s, ss + {v, ts, name, token?}]
RESP  [0x02][Noise msg2 : e, ee, se + {v, sid, name}]
DATA  [0x10][sid u32 LE][nonce u64 LE][AES-256-GCM ciphertext + tag 16]
```

## Limites connues / à traiter

- **Pas de forward secrecy de l'identité du client côté host si la clé statique du host fuit** (propriété de IK : le trafic passé reste protégé grâce aux clés éphémères, mais le premier message l'est moins). Acceptable ; à documenter pour l'utilisateur.
- ~~Les sessions existantes ne sont pas coupées à la révocation~~ : corrigé (revérification chaque seconde, voir « Revue de sécurité »).
- **`invite`/`revoke`/`set-role` en CLI écrivent `trust.json` depuis un autre processus** que `run` : désormais sûr (verrou de fichier + relecture dans le verrou) ; un IPC (named pipe) vers le service reste préférable à terme.
- **Le fichier d'identité n'est pas chiffré** (protégé uniquement par les ACL du profil Windows). Option future : DPAPI.
- **NAT** : sans serveur de rendez-vous, le host demande lui-même la redirection à la box (UPnP/PCP/NAT-PMP, `grx-portmap`). Impossible derrière un CGNAT ou un double NAT : IPv6 ou VPN dans ce cas.

## État d'avancement (mesuré en boucle locale, RTX 4070 Ti, 2560x1440 HEVC)

| Jalon | État | Chiffres clés |
|---|---|---|
| 1. Identité, invitations, Noise IK, transport chiffré | ✅ | 10 tests + test UDP réel |
| 2. Capture DXGI + encodage matériel | ✅ (`grx-video`) | latence capture→flux 2,9 ms moy., 57,9 fps |
| 3. Transport média UDP (FEC, ARQ, pacing, débit adaptatif) + décodage + fenêtre | ✅ (`grx-transport`, `grx-render`) | 5 % de perte : 600/600 images intactes ; décodage 0,2 ms |
| 4. Entrées clavier/souris/manette | ✅ (`grx-input`) | 40 tests ; manette ViGEm vérifiée |
| Manettes de bout en bout (Xbox, DS4, DualSense, génériques) | ✅ | voir « Manettes » ; boucle locale : manette simulée → Xbox 360 / DS4 virtuelle, vibration jeu → client |
| 5. Audio WASAPI + Opus | ✅ (`grx-audio`) | ~61 ms bout en bout (tampon WASAPI inclus) |
| Assemblage host + client (`play`, `bench`) | ✅ | 59 fps, capture→réception ~9 ms, RTT 0,8 ms |
| GUI d'appairage (`grx-gui`, egui/eframe 0.37) | ✅ | QR décodé depuis une capture = lien affiché ; nouveau client visible en ≤ 2 s ; arrêt du host par Ctrl+C en < 0,1 s ; 21 tests |

**Pas encore validé en conditions réelles** : mode capture souris, plein écran, lecture audio réellement entendue, manette physique (aucune branchée sur la machine de développement : validé avec des manettes virtuelles ViGEm lues par XInput/HID et une manette simulée), connexion entre deux machines/Internet, changement de résolution ou verrouillage de session côté host.

## Limites connues

- **Curseur** : ~~non incrusté, curseur local seul~~ **levée** : curseur distant façon Parsec (voir « Curseur distant ») ; restent : pas de prédiction locale (le curseur suit la position du host, donc RTT + capture), sortie tournée (écran en portrait) non gérée, l'inversion XOR est exacte pour les pixels noir/blanc et approchée pour les couleurs intermédiaires.
- **Host = session utilisateur** : pas un service Windows (SendInput n'atteint pas le bureau depuis la session 0). Pas d'injection vers les fenêtres administrateur/UAC.
- **Écran statique** : DXGI ne produit rien sans changement ; `--fill` réencode la dernière image.
- **Une seule session** à la fois. HDR : voir « Phase 2 ». ~~un seul écran (le principal)~~ **levée** : choix de l'écran capturé à chaud depuis l'overlay (voir « Écrans multiples ») ; un seul écran à la fois (pas de bureau étendu complet).
- **Détection de coupure** d'un client tué : 5 s.
- **Latence mesurée** capture→réception : valable en local ou avec horloges synchronisées ; la latence jusqu'au pixel affiché n'est pas mesurée.
- **Décodage HEVC** : Media Foundation demande l'extension « HEVC Video Extensions » ; sans elle, FFmpeg D3D11VA (DLL LGPL facultatives) ou, à défaut, H.264 négocié automatiquement (voir « Codecs et négociation »).
- **Contrôleur de débit** AIMD simple (oscille, réagit à une perte aléatoire >10 % comme à de la congestion).

## Feuille de route restante

1. ~~Revue de sécurité et correctifs~~ : faite, voir « Revue de sécurité » ci-dessous (restes listés)
2. ~~NAT traversal~~ : remplacé par la redirection automatique UPnP/PCP/NAT-PMP (voir « Accès depuis Internet ») ; un relais auto-hébergé ne serait utile que derrière un CGNAT sans IPv6
3. **Service/tray host + IPC** (named pipe) pour `invite`/`revoke` à chaud et coupure des sessions révoquées
4. ~~**UI**~~ : `grx-gui` fait (appairage). Reste : tray/service, lancement au démarrage, scan de QR par caméra
5. ~~Incrustation du **curseur** (forme + position envoyées à part)~~ (fait, voir « Curseur distant »), ~~multi-écrans~~ (fait, voir « Écrans multiples »), presse-papiers
6. Écran virtuel (pilote IDD tiers) pour les hosts sans écran ; chiffrement DPAPI des identités
7. Tests réseau réels (2 machines, Wi-Fi, 4G) et réglage du contrôle de débit

## Codecs et négociation

- **Protocole** (`grx_core::codec_neg`, canal Control fiable) : `[magie 7][version 1][count ≤ 16][(clé u8, longueur u8, valeur)…]`, ≤ 512 o, longueur exacte ; clé connue mal formée → message refusé, clé inconnue ignorée, capacités inconnues (codec/backend) ignorées : extensible sans casser les anciens pairs. `GRX-COD` client → host : 1 = capacités de décodage `[codec][backend][formats][réservé]` × ≤ 32, 2 = codec demandé (`play --codec`), 3 = renégociation, 4 = mode de décodeur (Auto/matériel/logiciel). `GRX-CDC` host → client : 1 = encodeurs, 2 = codec choisi (obligatoire), 3 = raison, 4 = format du flux. Fuzz 200 000 tampons. **Points d'extension (phase 2)** : `BackendId` ouvert (MF, FFmpeg D3D11VA, logiciel, NVENC/AMF/QSV réservés), `FormatSet` (4:2:0/4:4:4 × 8/10 bits, par capacité et pour le flux), `DecoderMode`, octet réservé « profil », clés 5..255 libres ; côté rendu, `DecoderKind` + `DecoderKind::CHAIN` (`grx-render/src/chain.rs`).
- **Choix** (`codec_neg::choose`, testé exhaustivement) : sans annonce (ancien client), `auto` → H.264 ; un codec forcé par le host est gardé (comportement historique). Avec annonce : candidats = encodeurs du host ∩ décodeurs du client ; préférence du client, puis du host, puis HEVC > H.264 ; AV1 jamais choisi automatiquement ; aucun commun → H.264.
- **Host** (`grx-host/src/codec.rs`) : `CodecState` partagé boucle réseau / thread vidéo. Le thread vidéo attend l'annonce au plus 700 ms avant d'ouvrir l'encodeur (`open_encoder` : échec de création → codec marqué en panne, repli jusqu'à H.264) ; en cours de session, un nouveau choix recrée l'encodeur (~30 ms) avec image clé. Encodeurs disponibles : énumération MFT matériels (`grx_video::available_encoders`, en cache ; ici NVIDIA H.264/HEVC/AV1). AV1 : profil Main 8 bits, flux d'OBU « low overhead » (`grx_video::obu` : découpe, sequence header, `KEY_FRAME`).
- **Client** (`grx-client/src/codec.rs`, `play.rs`) : device D3D11 créé avant la connexion et partagé avec le présentateur ; `grx_render::probe_decoders` (thread dédié MTA, vérifie le profil D3D11 du GPU puis crée vraiment le décodeur, chaîne MF → FFmpeg, cache par adaptateur et voies désactivées ; ~80 ms ici) → `GRX-COD`. Le codec du flux est reconnu sur chaque image clé (Annex-B ou OBU) : changement → nouveau décodeur. Création impossible ou 8 erreurs de décodage consécutives → codec retiré, `GRX-COD` de renégociation, attente des images du nouveau codec ; impossible (H.264, ancien host) → message clair (codec, cause, solutions ; pas de conseil réseau) et code de sortie 4, que `grx-gui` affiche comme « décodage vidéo impossible ». Options cachées de test : `--disable-decoder mf-hevc,ffmpeg,av1,…`, `--fail-decoder-after hevc:120`.
- **FFmpeg D3D11VA** (`grx-render/src/ffmpeg.rs`) : `avcodec-62` / `avutil-60` (+ `swresample-6` si présent) chargées par `LoadLibraryExW` en chemin absolu depuis `%GRX_FFMPEG_DIR%`, le dossier de l'exe puis `exe\ffmpeg` (jamais le `PATH`) ; version majeure vérifiée (offsets `AVCodecContext.hw_device_ctx` = 560, `AVHWDeviceContext.hwctx` = 16, `AVFrame.width/height/format` = 104/108/116, mesurés sur les en-têtes FFmpeg 8.1). Contexte D3D11VA créé sur **notre** device (zéro copie), décodeurs natifs `h264`/`hevc`/`av1`, `threads=1`, `low_delay`, `extra_hw_frames=4` ; sortie `AV_PIX_FMT_D3D11` = texture NV12 + index de tranche, gardée par une référence `AVFrame` jusqu'à l'affichage ; une image non matérielle (profil refusé) = erreur → repli. Absent ou autre version : voie désactivée. **Non testé sur cette machine** (le chargement de DLL téléchargées n'a pas été autorisé) : à valider avec des DLL fournies.
- **DLL FFmpeg et licence** : jamais de build GPL (`build-setup.ps1` refuse `--enable-gpl`/`--enable-nonfree` trouvés dans la configuration embarquée ; le `F:\ffmpeg` local est une build statique GPL, inutilisable). **Depuis 0.2.3, intégré par défaut** : sans `-FfmpegDir`, `build-setup.ps1` télécharge la build BtbN `ffmpeg-n8.1.3-35-gf1acb0a266-win64-lgpl-shared-8.1.zip` (URL d'« autobuild » et SHA-256 épinglés ; `third_party\ffmpeg\` sert ensuite de cache, non versionné), n'en extrait que `avcodec-62`, `avutil-60` et `swresample-6` (dépendance réelle d'avcodec ; 95 Mo, ~28 Mo compressés dans le setup) + `LICENSE.txt` (LGPL v3, `--enable-version3`) et écrit `SOURCE.txt` ; `-NoFfmpeg` pour s'en passer. **Ordre des voies** : sur GPU Intel (`VendorId` 0x8086, `grx_render::order_for_vendor`), FFmpeg D3D11VA passe avant Media Foundation (repli MF) ; MF reste premier sur NVIDIA/AMD ; `GRX_FORCE_GPU_VENDOR=0x8086` le simule. Intégration à l'installateur seulement si `third_party\ffmpeg\` (ou `-FfmpegDir`) contient les DLL, `LICENSE.txt` (LGPL, installé dans `licenses\FFmpeg-LGPL.txt`) et `SOURCE.txt` (provenance : URL de la build et des sources correspondantes, obligation LGPL). Build prête à l'emploi : BtbN `ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip` (github.com/BtbN/FFmpeg-Builds, 91 Mo de DLL : setup 6,7 → 34 Mo). Build minimale recommandée (~5 Mo) : `./configure --enable-shared --disable-static --disable-programs --disable-doc --disable-everything --disable-avdevice --disable-avformat --disable-avfilter --disable-swscale --disable-swresample --enable-d3d11va --enable-decoder=h264,hevc,av1 --enable-hwaccel=h264_d3d11va,hevc_d3d11va,av1_d3d11va` (MSYS2/mingw-w64, FFmpeg 8.x, sans `--enable-gpl`).
- **Mesures** (RTX 4070 Ti, boucle locale 2560x1440 60 img/s, appel du décodeur) : HEVC MF 0,18 ms ; H.264 MF 0,27 ms ; AV1 MF 0,16-0,19 ms ; `codec_check` 1080p (appel + fin GPU) : HEVC 0,48 ms, H.264 0,88 ms, AV1 1,11 ms. Renégociation HEVC → H.264 en session : encodeur recréé en 32 ms, aucune coupure.

## Phase 2 des codecs (0.2.2) : parité Parsec

| Fonction Parsec | Ici | État (RTX 4070 Ti) |
|---|---|---|
| Encodeur NVIDIA | MFT NVIDIA (`grx-video/encoder.rs`) | réel |
| Encodeurs AMD / Intel | MFT AMF / Quick Sync (asynchrones), choisis sur l'adaptateur de l'écran capturé (`MFTEnum2` + LUID) ; `run --encoder` | simulé (sélection testée exhaustivement, pas de GPU AMD/Intel ici) |
| Détection des encodeurs par test réel | `grx_video::probe_adapter` : chaque voie (MFT par vendeur × H.264/HEVC/AV1 × 4:2:0 8/10 bits, NVENC direct × 4:4:4 8/10 bits, OpenH264) est créée sur l'adaptateur, encode 2 images 256x256, flux validé (`streaminfo::check_stream` : NAL/OBU, IDR, chroma, profondeur lus dans SPS/sequence header). `grx_video::probe` : résultats installés par LUID ; `encoders_for_adapter` n'annonce que les combinaisons validées, `Encoder::with_device` écarte les voies refusées (message explicite) ; énumération tant que la sonde n'a pas fini. Host (`encprobe.rs`) : sonde de fond au démarrage de `run`, cache `encoder-probe.json` (clé : adaptateur + version du pilote + version GRX + OpenH264 ; échec passager = re-sonde), `grx-host encoders [--refresh] [--json]` ; grx-gui (Paramètres) affiche la liste et ⚠ sur un choix Encodeur/Couleur/Codec non supporté | réel (NVIDIA) ; AMD/Intel : logique simulée |
| Encodeur logiciel | OpenH264 2.6.0 (Cisco), NV12 réduit sur GPU -> I420 -> H.264 Baseline, tranches = threads | réel : 1080p 4,9 ms/image (4 threads) |
| Décodeur logiciel | OpenH264 (H.264) ; FFmpeg 8.1 sans hwaccel (HEVC/AV1/4:4:4/10 bits), images téléversées NV12/P010/AYUV/Y410 | réel : OpenH264 1,2 ms, FFmpeg HEVC 4:4:4 1440p ~10 ms |
| Choix du décodeur | Auto / Matériel / Logiciel (`DecoderPref`, overlay, `play --decoder`, annoncé dans `DecoderMode`) | réel |
| Décodeurs matériels asynchrones | MFT asynchrones pilotés par `grx_video::mft_async` (Intel QSV) | simulé (faux MFT) |
| Color mode 4:4:4 | NVENC direct (`nvenc.rs`, API 12.1 chargée dynamiquement), HEVC RExt 8 bits, entrée BGRA (CSC NVENC = BT.709 exact) ; décodé par le MFT HEVC Microsoft (AYUV) ou FFmpeg | réel : 1440p60, écart YCbCr 0 vs référence |
| 10 bits / HDR | P010 + HEVC Main10 (MFT) ; capture HDR `DuplicateOutput1` FP16 -> PQ BT.2020 (shader) ; client : PQ -> écran HDR (scRGB FP16) ou tone-mapping SDR (shader) | réel en 10 bits avec mire HDR synthétique ; capture/affichage HDR non testés (pas d'écran HDR) |

- **Protocole** : `BackendId::NVENC` = 20 ; `GRX-COD` clé 5 = `FormatPrefs` (bit 0 4:4:4, bit 1 10 bits, bit 2 écran HDR, bits inconnus ignorés) ; `GRX-CDC` clé 5 = encodeur utilisé (backend inconnu ignoré), clé 6 = transfert (0 SDR, 1 PQ ; autre valeur : message refusé). Capacités par voie avec leurs `FormatSet`. Choix : codec d'abord (`choose`), puis format (`choose_format` : 4:4:4 voulu par `--chroma 444` ou le client, 10 bits si préféré ou source+écran HDR ; on lâche le 10 bits, puis le 4:4:4) ; échec de création de l'encodeur d'un format évolué -> 4:2:0 8 bits ; panne du décodeur d'un format évolué -> le client retire ce format seulement.
- **Mesures** : NVENC HEVC 4:4:4 720p 1,5 ms/image, MF HEVC Main10 1,2 ms ; décodage MF HEVC 4:4:4 1,1 ms, Main10 0,9 ms ; bout en bout 1440p60 : HEVC 4:4:4 décodage 0,19 ms, capture->écran ~7 ms ; OpenH264 (1440p -> 1080p) ~9 ms.
- **Limites mesurées** : NVENC D3D11 refuse les textures YUV 4:4:4 préparées (planaire : `UNSUPPORTED_PARAM`, AYUV lu comme du RGB) -> entrée BGRA et 4:4:4 8 bits seulement ; H.264 4:4:4 ne respecte pas le débit sur motif difficile -> annoncé en HEVC seulement ; FFmpeg 8.1 D3D11VA ne décode pas le HEVC 4:4:4 sur ce GPU (repli logiciel) -> non annoncé ; le VideoProcessor n'accepte pas le FP16 en entrée ni ne fait de tone-mapping -> shaders.
- **Licences** : OpenH264 = BSD + licence de brevets MPEG LA de Cisco, à condition que le binaire soit téléchargé séparément depuis Cisco sur le poste de l'utilisateur, désactivable, avec la mention « OpenH264 Video Codec provided by Cisco Systems, Inc. » : tâche facultative du setup (`grx-host fetch-openh264`, SHA-256 de l'archive et de la DLL épinglés, désinstallé avec l'application), texte de licence dans `licenses\OpenH264-BINARY_LICENSE.txt`. NVENC : DLL du pilote, ABI mesurée sur nv-codec-headers (MIT), aucun en-tête embarqué. libx264 (GPL) : jamais.

## Curseur distant

Comme Parsec, le curseur n'est **pas** incrusté dans la vidéo : le host l'envoie à part et le client le dessine par-dessus l'image.

- **Host** (`grx-video/src/cursor.rs`, `grx-host/src/cursor.rs`) : à chaque `AcquireNextFrame`, y compris pour les mises à jour « curseur seul » sans nouvelle image, `DXGI_OUTDUPL_FRAME_INFO.PointerPosition` (si `LastMouseUpdateTime != 0` : visibilité + coin de la forme) et `GetFramePointerShape` sont publiés dans un `CursorTracker` (dernier état gagnant). Les 3 formats sont convertis en un calque RGBA (alpha non prémultiplié) + un calque d'inversion optionnel : MONOCHROME (AND/XOR 1 bpp : noir, blanc, transparent, inversion), COLOR (BGRA + alpha), MASKED_COLOR (masque 0 = couleur opaque, masque FF = XOR de la couleur avec l'écran, noir = transparent). Formes > 256x256 réduites (plus proche voisin, hotspot recalculé). Un jeu qui cache son curseur (`ShowCursor(FALSE)`, caméra de WoW) donne `Visible = false` → caché aussi chez le client. La boucle réseau (`CursorSender::drive`, chaque tour ≤ 250 µs) envoie les changements tout de suite et rafraîchit l'état toutes les 100 ms.
- **Protocole** (`grx-transport/src/cursor.rs`) : forme = corps compressé RLE (pixels de 4 o) découpé en morceaux `GCS1` de 1100 o sur le canal **Control** (fiable, ordonné) ; `shape_id` = FNV-1a du contenu, vérifié à la réception (un id forgé ne peut pas empoisonner le cache) ; host et client tiennent un cache FIFO (32 / 64 entrées, évictions identiques car canal ordonné) : la flèche, la main, le I ne sont envoyés qu'une fois. Position = datagramme **non fiable** `GCP1` de 33 o (nouveau drapeau `F_DATAGRAM` du type `RELIABLE` : pas de seq, pas d'ACK, file bornée) : `seq` (le plus récent gagne), visibilité, `shape_id`, point chaud en fraction ×65536 de l'image capturée (indépendant du scaling encodeur), taille de l'image capturée (échelle de la forme), horodatage host (mesure de latence). Décodage borné (256x256, corps ≤ ~530 Kio), sans panique (tests fuzz : 20 000 messages aléatoires/mutés).
- **Client** (`grx-client/src/cursor.rs`, `grx-render/src/cursor.rs`) : le thread réseau remplit une boîte aux lettres ; un thread `grx-client-cursor` pousse chaque nouvel état au présentateur (`PresenterHandle::set_remote_cursor`), qui **re-présente aussitôt la dernière image** avec le curseur à jour (sans attendre la vidéo ; réveils coalescés, swapchain non bloquante). Rendu D3D11 : 2 quads texturés (shaders compilés par `D3DCompile`), calque couleur en alpha prémultiplié, calque d'inversion en `INV_DEST_COLOR / INV_SRC_COLOR` (XOR exact pour 0/255), échelle = facteur de la vidéo dans la fenêtre (letterbox), découpe au rectangle vidéo, filtrage point à l'échelle 1 et bilinéaire sinon. Dessiné entre la vidéo et l'overlay. Le curseur Windows local est masqué au-dessus de l'image dès qu'un curseur distant est reçu (visible sur les bandes noires) ; en mode capture le curseur distant reste affiché. `--local-cursor` : ancien comportement.
- **Mesuré** (boucle locale 127.0.0.1, 1920x1080 → fenêtre 960x540) : mise à jour DXGI du host → dessin client **~1 ms en moyenne** (p99 par seconde ≤ 5,3 ms) pendant les mouvements, contre ~10 ms capture→réception pour la vidéo : le curseur précède l'image. `SetCursorPos` → pixels visibles dans la fenêtre client (sondage `CopyFromScreen`, composition DWM comprise) : 14-22 ms. Formes vérifiées à l'écran : flèche, main, I (captures dans le rapport de test).
- **Limites** : pas de prédiction locale en mode absolu ; sortie tournée non gérée ; inversion approchée hors noir/blanc ; une forme volumineuse (rare) part en rafale de morceaux non pacés sur le canal fiable.

## Manettes

- **Client** (`grx-client/src/gamepads.rs`, fil `grx-client-pad` à ~250 Hz) : trois sources. **XInput** (`xinput1_4.dll` chargée dynamiquement, `XInputGetStateEx` ordinal 100 pour le bouton Guide, emplacement vide interrogé ≤ 1 fois/s) pour Xbox 360/One/Series ; **HID Sony** (crate `hidapi` en implémentation Windows native, pur Rust, sans cmake ni C) pour DualShock 4 (054C:05C4/09CC/0BA0) et DualSense (054C:0CE6/0DF2), énumération toutes les 2 s, lecture non bloquante, décodeur pur `grx-input/src/sony.rs` (USB 0x01, Bluetooth complet 0x11/0x31, Bluetooth simplifié ; la lecture du rapport de calibration fait passer le Bluetooth en rapports complets) ; **gilrs** (Windows.Gaming.Input) pour le reste (Switch Pro, génériques avec correspondance SDL), en ignorant les manettes XInput (`is_ff_supported`) et Sony pour éviter les doublons. Normalisation (`padlogic.rs`) : zone morte radiale 2500 avec remise à l'échelle, gâchettes ≤ 8 → 0. Index : plus petit libre à la connexion, stable, libéré au débranchement (`SlotAllocator`). Envoi (`SendScheduler`) sur changement, 250 Hz max, rafraîchi toutes les 100 ms, par le même fil d'envoi que le clavier (canal Input fiable, ordonné). Focus perdu / menu ouvert : état neutre. Fin de session : neutre + `GamepadDisconnect`, moteurs coupés. Boucle locale (host sur 127.0.0.1) : seules les manettes présentes la première seconde sont lues (sinon le client relirait les manettes virtuelles du host). `--sim-gamepad xbox|ds4|dualsense` (caché) : manette scriptée pour les tests.
- **Protocole** (`grx-input/src/event.rs`, rétrocompatible) : tags existants inchangés ; bit 0x0800 de `buttons` = clic du pavé tactile (`xbutton::TOUCHPAD`, accepté désormais) ; `0x07 GamepadConnect [index][kind 0..=4][virt 0..=1]` (type physique + manette virtuelle souhaitée) ; `0x08 GamepadDisconnect [index]`. Décodage strict, borné, sans panique, fuzz (200 000 tampons). Un host ancien ignore les deux nouveaux tags (et un état portant le bit pavé tactile). Vibrations host → client : message Control `GRX-RMB [index][gros][petit]` (10 o exacts, `rumble.rs`, fuzz).
- **Host** (`grx-input/src/gamepad.rs`) : une manette virtuelle ViGEm par index, Xbox 360 (défaut) ou DualShock 4 (`vigem-client` feature `unstable_ds4`), choisie par `GamepadPolicy` (`--gamepad-type auto|xbox|ds4`, `auto` = demande du client `play --gamepad-as`). Changer de type débranche puis rebranche. Correspondance DS4 (`ds4map.rs`) : A→Croix, B→Rond, X→Carré, Y→Triangle, LB/RB→L1/R1, gâchettes analogiques + boutons L2/R2 au-delà de 30, Back→Share, Start→Options, Guide→PS, pavé tactile→clic. ViGEmBus renvoie `ERROR_NO_MORE_ITEMS` (259) quand aucune lecture n'est en attente juste après le branchement : le rapport est mis en cache, traité comme un succès (comme le client ViGEm officiel). Vibrations : notifications XUSB (`unstable_xtarget_notification`) d'une Xbox 360 virtuelle, dédoublonnées, canal borné (64) → boucle réseau (≤ 16/tour) → client → `XInputSetState` ou rapport de sortie DS4 (0x05 USB, 0x11 BT + CRC32) / DualSense (0x02 USB, 0x31 BT + CRC32). `GamepadDisconnect` n'est jamais limité en débit (remise au neutre).
- **Vérifié** : manettes virtuelles ViGEm Xbox 360 + DS4 lues par `grx-client gamepads` (XInput et HID natif : boutons, gâchettes, sticks, Guide/PS/pavé tactile) ; vibration XInput → notification ViGEm ; boucle locale 127.0.0.1:7841 : manette simulée → DS4 virtuelle (`--gamepad-as auto`) ou Xbox 360 virtuelle, état relu côté host par XInput, vibration « jeu » (200/100) → client, manette virtuelle retirée à la déconnexion.
- **Limites / évolutions** : pas de vibration en DualShock 4 virtuelle (le crate `vigem-client` 0.1 n'expose pas ses notifications) ni sur la voie gilrs ; gyroscope/accéléromètre, position sur le pavé tactile, LED, gâchettes adaptatives et haut-parleur du DualSense non transmis ; DualSense Edge : palettes arrière ignorées ; rapports de vibration Sony écrits selon les formats documentés (Linux hid-playstation) mais non vérifiés sur une vraie manette ; manettes génériques non testées sur matériel ; un host plus ancien ne débranche les manettes virtuelles qu'en fin de session.

## Menu en overlay (client)

Équivalent de l'overlay de Parsec dans `grx-client play`.

- **Rendu** (`grx-render/src/overlay.rs`) : Direct2D + DirectWrite directement sur le back buffer DXGI, après la vidéo (et le curseur distant), juste avant `Present`. Choisi plutôt qu'egui : aucune dépendance, même device D3D11 (`BGRA_SUPPORT`, fabrique D2D `MULTI_THREADED` car le décodeur partage le device), polices système (Segoe UI, Consolas). L'intégration décrit une `OverlayScene` (rectangles arrondis, contours, textes en DIP + facteur DPI) et la pousse par `PresenterHandle::set_overlay` (ignorée si identique ; redessine la dernière image si aucune n'est en attente, pour un écran statique). **Coût nul menu fermé** : scène `None` → aucun objet Direct2D créé, aucun dessin ; seuls le petit bouton flottant (souris près du bord haut) ou les stats coûtent un `BeginDraw/EndDraw`. Cible D2D libérée avant `ResizeBuffers`. `PresenterHandle::set_vsync` bascule la VSync à chaud.
- **Logique** (`grx-client/src/overlay.rs`, testée sans fenêtre) : `Overlay` (état, géométrie, hit-test, clavier, curseurs, segments) consomme des entrées abstraites et produit des `Action`s + la scène ; `OverlayInput` adapte winit et décide ce qui est **consommé** : raccourcis `Ctrl+Maj+M/S/Q` (appui, répétitions et relâchement), et tout clavier/souris quand le menu est ouvert. Un événement consommé n'atteint jamais l'`InputHandler`, donc jamais le host ; à l'ouverture, `release_all` relâche côté host ce qui était tenu (Ctrl/Maj du raccourci compris) et la capture souris est suspendue (rétablie à la fermeture). Le collage avec la fenêtre, l'audio (volume atomique lu par le fil audio) et le réseau est dans `play.rs` (`OverlayGlue`) ; les stats sont publiées une fois par seconde par le fil vidéo.
- **Déplacement et positions** (`grx-client/src/overlay_layout.rs`, `overlay.rs`) : menu (poignée = barre de titre, curseur `Move` au survol), panneau de stats (glissé depuis n'importe où, menu ouvert seulement : menu fermé la souris va au host) et bouton flottant (glissé librement ; appui+relâchement sous 4 DIP = clic, l'ouverture du menu se fait donc au relâchement). Chaque position est une `Frac` (fx, fy ∈ [0,1]) = fraction de l'espace **libre** de la fenêtre (`marge + f × (vue − taille − 2·marge)`, marge 8 DIP) : un élément est entièrement visible par construction, garde son coin d'ancrage entre tailles de fenêtre, et rien n'est à recalculer au redimensionnement / plein écran / changement de DPI (élément plus grand que la fenêtre : collé en haut à gauche). Glisser = `frac_of(curseur − prise)`. Lignes « Position du menu / des stats » : 5 préréglages + réinitialiser. Le bouton flottant apparaît près de lui ou dans la bande du bord de fenêtre contre lequel il est posé (bande haute par défaut, comportement d'origine).
- **Préférences** (`grx-client/src/overlay_prefs.rs`) : `overlay.json` dans le dossier de données du client (positions, stats affichées, bouton flottant, volume/muet, VSync, capture souris, plafonds débit/FPS ; aucun secret). Lecture tolérante (absent, > 64 Kio, JSON invalide → défauts ; champs manquants → défauts ; valeurs bornées : volume [0,1], débit [1,1000] Mb/s, FPS [10,240], positions [0,1]). Écriture `write_atomic` quand les préférences changent, jamais pendant un glisser (au relâchement) ; une erreur d'écriture est seulement signalée. Au démarrage : appliquées avant la création de la fenêtre (VSync, volume), capture souris rétablie, plafonds renvoyés au host (`GRX-SET`) dès que la fenêtre est prête ; `--vsync` et un `--volume` ≠ 0,8 l'emportent.
- **Réglages de session** (`grx_core::stream_settings`) : messages du canal Control fiable, `[magie 7 o][version][count][(clé u8, valeur u32 LE)…]`, longueur exacte, ≤ 8 entrées, clés inconnues ignorées. `GRX-SET` (client → host : plafond de débit borné à [1, 1000] Mb/s, FPS max borné à [10, 240], 0 = aucun) ; `GRX-HST` (host → client, au début de session et après chaque réglage : plafonds appliqués, `--max-bitrate`, `--fps`). Décodage sans panique (fuzz déterministe de 200 000 tampons).
- **Host** (`grx-host/src/control.rs`) : `BitrateController::set_cap` / `Endpoint::set_max_bitrate_bps` plafonnent le budget total (média + FEC, donc aussi le pacing) à `min(plafond, --max-bitrate)`, immédiatement ; `pipeline::Shared::max_fps` (atomique) est lu par le thread vidéo : cadence `min(--fps, plafond)` et débit encodeur multiplié par `--fps / cadence` (l'encodeur reste configuré pour `--fps`, le débit réel reste donc celui visé). Autorisé aux clients en lecture seule (ne touche que leur flux). Un message invalide est ignoré sans effet.
- **Verrouillage du clavier** (`grx-client/src/kbdlock.rs`) : crochet `WH_KEYBOARD_LL` installé seulement quand l'option est activée, sur le thread de la fenêtre ; n'agit que si la fenêtre du client est au premier plan et le menu fermé ; ignore les frappes injectées ; prend `Win`, `Alt+Tab`, `Alt+Échap`, `Ctrl+Échap`, `Alt+Espace` et les envoie au host par le chemin d'entrée normal ; une touche prise à l'appui l'est aussi au relâchement ; tout est relâché à la perte de focus et à l'ouverture du menu.
- **Vérifié** (boucle locale, `run --no-input --no-audio --fill --no-upnp`, `play --no-input --open-overlay --overlay-stats` (options cachées), messages postés à la seule fenêtre du client) : menu et stats lisibles par-dessus la vidéo ; `Ctrl+Maj+M` puis `↓`×3 et `←`×8 → plafond 5 Mb/s : débit vidéo reçu 13-14 Mb/s → 3,8-6,1 Mb/s (moy. ~4,8) dès la seconde suivante, host « cible enc 4,8 Mb/s » ; FPS max 30 : 60 → 30 img/s reçues en 1 s, débit conservé ; `F11` et la ligne « Plein écran » → fenêtre 1920x1080 en (0,0) puis retour ; `Ctrl+Maj+Q` → déconnexion propre (code 0, host : « déconnexion du client »).
- **Vérifié (déplacement, boucle locale port 7851, messages souris postés à la seule fenêtre du client)** : titre du menu glissé de (540,31) à (900,400) → panneau collé en bas (borné), stats glissées vers le bas puis « Haut-droite » d'un clic, bouton flottant glissé du haut au centre vers le bas à gauche (resté fermé, réapparaît près du bord gauche) ; `overlay.json` écrit au relâchement ; client relancé : menu, stats (affichées sans `--overlay-stats`) et bouton à leurs places ; « FPS max 30 » choisi, client relancé : « réglages mémorisés -> FPS max 30 », 28-30 img/s dès la 1re seconde côté client et « cadence 30 img/s » côté host.
- **Limites** : le bouton flottant (survol) n'a pas pu être testé de bout en bout sans bouger le vrai curseur (les messages souris postés sont annulés par `WM_MOUSELEAVE`) : il l'est par les tests unitaires ; le verrouillage du clavier n'est testé que par sa règle de décision et l'installation/retrait du crochet (aucune frappe réelle simulée) ; pas de FPS au-delà du `--fps` du host ; les choix de FPS dépendent de la fréquence de l'écran qui affiche la fenêtre au lancement.
- **Évolutions futures** (non supportées par le host aujourd'hui) : résolution distante. (Le choix de l'écran distant est fait : voir « Écrans multiples ».)

## Écrans multiples

Le client choisit l'écran du host qu'il voit (ligne « Écran » du menu, `Ctrl+Maj+←/→`), à chaud.

- **Énumération** (`grx-video/src/capture.rs`, `displayconfig.rs`) : `list_outputs` (DXGI : adaptateur, sortie, rectangle dans le bureau virtuel, nom GDI `\\.\DISPLAYn`) + `monitor_names` (API DisplayConfig : nom convivial du moniteur, technologie de sortie). `capturable` garde les sorties rattachées au bureau et de surface non nulle (sorties inactives/déconnectées exclues), une seule fois par nom GDI, triées de gauche à droite puis de haut en bas (c'est la numérotation « 1, 2… » montrée à l'utilisateur). Écran **virtuel** marqué (sortie indirecte IddCx, ou nom typique : « VDD by MTT », Parsec, Idd…). `grx-host run` se déclare *per-monitor DPI aware v2* : sans ça DXGI et le curseur renvoient des coordonnées virtualisées (ex. 1920x1080 au lieu de la taille réelle avec une mise à l'échelle Windows).
- **Protocole** (`grx_core::displays`, canal Control fiable, décodage strict sans panique, fuzz 200 000 tampons) : `GRX-DSP` host → client `[magie 7][version 1][actif u8|0xFF][n ≤ 16]` + n × `[left i32][top i32][w u16][h u16][drapeaux : principal, virtuel][len ≤ 48][nom UTF-8]`, longueur exacte, dimensions 1..16384, coordonnées bornées ±2²⁴ ; envoyé dès que la liste est connue puis **réannoncé à chaque changement** (liste ou écran actif). `GRX-MON` client → host `[magie][version][index < 16]`, 9 o exactement. Messages ignorés par les versions précédentes (rétrocompatible dans les deux sens : un ancien host n'envoie pas de liste, la ligne reste masquée).
- **Host** (`grx-host/src/display.rs`, `pipeline.rs`) : `Displays` (liste, écran actif, génération atomique, demande en attente). Un thread `grx-displays` relit la configuration toutes les 2 s (hors des boucles critiques). La boucle réseau compare la génération à chaque tour (≤ 250 µs) et envoie `GRX-DSP` ; un `GRX-MON` (permis aussi en lecture seule : ne touche que le flux de ce client) dépose l'index. Le thread vidéo, seul propriétaire du capturer et de l'encodeur, prend la demande avant chaque capture : même adaptateur → `ScreenCapturer::switch_output` (même device D3D11 : seule la duplication DXGI est recréée, ~1-3 ms) et encodeur conservé si la taille est la même (image clé forcée), sinon recréé à la taille native (le décodeur du client suit le nouveau SPS) ; autre adaptateur → capturer + encodeur recréés sur cet adaptateur ; échec → retour à l'écran précédent (ou au principal). L'audio, le réseau et la session ne sont pas touchés. L'écran capturé disparaît (débranché) → repli automatique sur le principal après 5 erreurs de capture. `--display N` : écran de départ (sinon le choix historique : adaptateur NVIDIA, écran principal).
- **Souris absolue** (`grx-input/src/absmap.rs`, `sendinput.rs`) : le client envoie (x, y) ∈ 0..65535 dans l'image ; le host vise le pixel `rect.left + x·(w−1)/65535` de l'écran capturé puis le convertit en coordonnées `MOUSEEVENTF_VIRTUALDESK` du bureau virtuel (union des écrans actifs, pixels physiques) : `n = ((p − vleft)·65536 + 32768) / vlargeur`, que Windows ramène exactement à `p` (centre du pixel). La cible (`AbsTarget`) est mise à jour par le thread vidéo à chaque bascule et lue par le thread d'injection ; inconnue → comportement d'origine (écran principal). Testé pour un second écran à droite, à gauche, au-dessus, en diagonale négative et plus petit/décalé : coins et centre tombent sur le bon pixel, tout point de l'image reste dans l'écran visé.
- **Curseur distant** : la position DXGI est relative à la sortie dupliquée ; DXGI ne la redonne qu'au prochain mouvement, donc après une bascule le host la recalcule depuis `GetCursorInfo` (visible seulement si le curseur est sur l'écran capturé, sinon masqué) ; ensuite DXGI rend `Visible = false` quand le curseur quitte l'écran capturé.
- **Client** (`overlay.rs`, `play.rs`, `net.rs`) : liste stockée par le fil réseau, appliquée à l'overlay par le fil vidéo dès réception ; boutons « 1 », « 2 »… (actif en surbrillance, nom + résolution au survol, `*` = principal), choix optimiste puis confirmé par le host ; ligne masquée avec ≤ 1 écran. `Ctrl+Maj+←/→` (consommés, jamais transmis) : écran précédent/suivant en boucle. Choix mémorisé dans `overlay.json` (`display` = « nom LxH @x,y », car deux moniteurs du même modèle ont le même nom) et redemandé une fois par connexion s'il existe encore. Stats : ligne « Écran » (« 2/3 VG27A »).
- **Vérifié** (boucle locale port 7861, host `run --no-input --no-audio --fill --no-upnp`, client `play --no-input --open-overlay`, raccourcis et clic postés à la seule fenêtre du client ; machine avec 3 écrans 1920x1080 : 2 moniteurs VG27A + l'écran virtuel « VDD by MTT » principal) : ligne « Écran » 1/2/3, bascules 3→2→1→3→2 par `Ctrl+Maj+←` et 2→1 par clic, image décodée identique à la capture directe de l'écran visé (captures comparées), stats « 2/3 VG27A » ; host : duplication recréée en 0,6-2,7 ms, première image (clé) encodée 3-20 ms après le début de la bascule ; client : **image clé du nouvel écran reçue 17-28 ms après la demande** ; client relancé → écran mémorisé redemandé et obtenu ; `--display 2` respecté, `--display 7` refusé avec la liste.
- **Limites** : un seul écran à la fois (pas de vue « tous les écrans ») ; bascule entre écrans de tailles différentes et entre adaptateurs différents non testées en réel (pas le matériel ici : chemin couvert par la logique, encodeur recréé) ; sorties tournées (portrait) non gérées ; la liste n'est relue que toutes les 2 s ; la souris absolue sur un écran à mise à l'échelle différente n'est testée que par les tests unitaires (aucune injection réelle).

## Accès depuis Internet (UPnP au lieu du NAT traversal)

**Décision** : seul le host doit être joignable. Plutôt qu'un serveur de rendez-vous/STUN/relais (infrastructure à héberger, contraire à l'esprit « sans serveur »), le host ouvre lui-même son port UDP sur la box, comme le font Parsec/Sunshine/Steam en complément de leurs serveurs.

- **`grx-portmap`** : `PortMapper::open(local, external_port_hint, lease, description) -> Mapping { external, local, protocol, lease, method: Upnp|NatPmp|Pcp, gateway, warnings }`, `Mapping::renew()`, `close()` (et `Drop`), `PortMapper::external_ip()`, `UpnpGateway::{discover, list, specific, remove}`, `netif::{default_route_v4, global_ipv6}`.
  - Ordre : UPnP IGD (`igd-next` synchrone, SSDP lié à l'IP de l'interface qui mène à la passerelle par défaut, 3 s max), puis PCP (RFC 6887, MAP) et NAT-PMP (RFC 6886) écrits à la main (quelques octets UDP vers la passerelle:5351, retransmission 250 ms → 1 s).
  - IP locale : jamais 0.0.0.0 ; table de routage Windows (`GetAdaptersAddresses`, passerelle de plus faible métrique) puis `connect` UDP (aucun paquet émis).
  - Port déjà redirigé vers une autre machine (`GetSpecificPortMappingEntry`, erreur 718) : hint+1..hint+4 puis 6 ports aléatoires (20000-60999) ; une ancienne entrée de CETTE machine (arrêt brutal) est remplacée. Box qui n'accepte que les baux permanents (725) : bail 0, supprimé à la fermeture et ré-affirmé toutes les 20 min. Le bail réellement appliqué est relu sur la box.
  - IP externe non publique (`plan::classify_ipv4`) : 10/8, 172.16/12, 192.168/16 → double NAT ; 100.64/10 → CGNAT ; 0.0.0.0 → WAN coupé : erreur explicite « l'UPnP ne suffira pas, votre box n'a pas d'IP publique », aucune redirection créée. IGD trouvé ≠ passerelle par défaut : avertissement (double NAT probable).
  - Logique pure testée : classification, choix des ports, `renew_delay` (mi-vie, min 5 s), `retry_delay` (15 s → 5 min, borné par la moitié du bail restant), encodage/décodage PCP/NAT-PMP.
- **Host** (`grx-host run`) : redirection dans un thread (`portmap_task`), jamais dans la boucle réseau ; bail 1 h renouvelé à mi-vie, recréé s'il est perdu (redémarrage de la box, port repris), nouvel essai périodique après un échec ; suppression à l'arrêt propre (attente bornée à 8 s, sinon expiration naturelle du bail). `status.json` (écriture atomique, supprimé à l'arrêt uniquement par son propriétaire `pid`) pour `invite --addr auto` et l'interface graphique. `--bind [::]:port` ouvre une socket double pile (`IPV6_V6ONLY=0`, vrai par défaut sous Windows).
- **Pare-feu Windows** : lecture seule via l'objet COM `HNetCfg.FwPolicy2` (PowerShell, indépendant de la langue de Windows, contrairement à `netsh … show rule`) : règle d'entrée UDP active pour `grx-host.exe` (ou pour le port) couvrant le profil réseau courant, règle de blocage prioritaire ; sinon avertissement + `grx-host firewall-help`.
- **Vérifié sur une vraie box** (UPnP IGD + PCP + NAT-PMP, IPv4 publique) : découverte ~0,6 s, redirection créée/visible/renouvelée/supprimée (UPnP et PCP), bail de 120 s relu sur la box puis renouvelé à ~60 s, `run` → `status.json` + redirection, Ctrl+C → les deux disparaissent ; hairpinning (client local → IP publique) fonctionnel sur cette box ; client local → IPv6 globale fonctionnel en double pile.

**Limites** :
- **CGNAT** (opérateurs 4G/5G, certaines offres fibre) et **double NAT** (box derrière un autre routeur) : la redirection de la box ne sert à rien ; détecté et signalé, solution côté FAI (IPv4 full-stack), mode bridge, IPv6 ou VPN.
- UPnP désactivé sur la box, ou box qui refuse (606) : redirection manuelle (`--no-upnp`).
- IPv6 : pas de pinhole IGD v2 (`WANIPv6FirewallControl`) : si la box filtre l'IPv6 entrant, il faut l'autoriser dans son interface.
- Arrêt brutal (processus tué, fermeture de la console) : la redirection reste jusqu'à la fin du bail (≤ 1 h) ; elle est remplacée au prochain démarrage.
- L'IP publique peut changer : nom DuckDNS conseillé ; l'adresse vue par le host est mise à jour au renouvellement.
- Exposer un port UDP : le host reste silencieux envers les inconnus (voir « Modèle de sécurité ») ; l'UPnP permet aussi à n'importe quel logiciel du LAN d'ouvrir des ports, c'est un choix de l'utilisateur sur sa box.

## Revue de sécurité

Une revue du code (lecture) a trouvé 8 problèmes (A1-A8) et quelques remarques ; tous ont reçu un correctif avec test automatisé. Les tests citent le numéro (`a1_…`, `a2_…`) dans `crates/grx-host/src/server.rs`.

| # | Gravité | Problème | Statut | Correctif | Test |
|---|---|---|---|---|---|
| A1 | critique | Un datagramme > 2048 o non authentifié provoquait `WSAEMSGSIZE` (10040) et arrêtait le host (même schéma côté client) | corrigé | Tampon de 64 Kio ; seules les erreurs `recv` réellement fatales (50 d'affilée non transitoires) arrêtent la boucle, `util::is_transient_recv_error` (MSGSIZE, CONNRESET...) est ignoré avec journal limité ; client : idem + paquets parasites ignorés pendant la poignée de main | `a1_oversized_datagrams_do_not_stop_the_host`, `net::tests::client_survives_oversized_and_junk_datagrams`, `util::tests::msgsize_and_reset_are_transient` |
| A2 | haute | Chaque paquet `0x01` coûtait lecture disque + JSON + DH avant authentification | corrigé | (1) taille d'INIT bornée `[137 o, 1 Ko]` ; (2) seaux à jetons AVANT tout travail coûteux : par IP (5/s, rafale 10) puis global (20/s, rafale 40) ; (3) `trust.json` relu seulement après déchiffrement réussi, via `TrustStore::refresh` (date+longueur, pas de relecture si inchangé) ; (4) journal d'erreurs limité à 1 ligne/s avec compteur de messages supprimés | `a2_init_flood_is_bounded_and_legit_client_still_served`, `a2_global_limit_caps_spoofed_sources`, `a2_log_limiter_…`, `a2_token_bucket_refills`, `session::tests::init_size_bounds_match_real_packets` |
| A3 | moyenne-haute | Rejeu d'un INIT capturé après redémarrage (`last_ts` en mémoire) : kick de session et flux média vers une adresse usurpée | corrigé | (1) `HostAuth::set_boot_ms` : tout INIT horodaté avant le démarrage du host est refusé ; (2) une poignée de main réussie crée une session « en attente » : aucun pipeline, aucun Endpoint, rien d'émis hormis la réponse de poignée de main, jusqu'au PREMIER paquet DATA authentifié venant de la même adresse ; la session précédente (même clé) n'est remplacée qu'à cet instant (un rejeu ne peut donc plus la « kicker ») | `session::tests::init_older_than_host_boot_is_refused`, `a3_nothing_is_sent_or_started_before_first_authenticated_data`, `a3_replayed_init_never_kicks_active_session`, `a3_reconnect_of_same_key_replaces_only_once_validated` |
| A4 | moyenne | Migration d'adresse (`a.peer = from`) sans validation de chemin | corrigé | La fenêtre anti-rejeu n'impose PAS un nonce strictement croissant (elle tolère le réordonnancement : un paquet capturé puis retenu reste valide), donc choix de la validation 1 RTT : un paquet authentique d'une autre adresse ne migre rien ; le host y envoie au plus 1 challenge/s (`PATH_CHALLENGE`, 9 o, nouveau type de paquet transport) et ne migre que sur le `PATH_RESPONSE` (écho du jeton aléatoire) reçu de cette adresse | `a4_address_migration_requires_path_validation`, `a4_replayed_packet_from_spoofed_address_gets_only_a_tiny_challenge`, `endpoint::tests::path_challenge_is_echoed_as_response` |
| A5 | moyenne | Toute clé autorisée/jeton remplaçait la session active ; la révocation ne la coupait pas | corrigé | (1) un INIT d'une AUTRE clé est refusé (sans brûler jeton ni horodatage : `HostAuth::accept_if`) tant que la session active a reçu un paquet authentifié depuis < 3 s ; la même clé reprend la main ; (2) `check_trust` chaque seconde (`TrustStore::refresh` bon marché) : clé révoquée, rôle modifié ou liste illisible 5 fois d'affilée → session coupée (les threads média s'arrêtent, `release_all` des entrées) | `a5_other_key_cannot_take_over_a_live_session_but_same_key_can`, `a5_revocation_cuts_the_active_session`, `session::tests::refused_init_burns_nothing` |
| A6 | basse-moyenne | Lecture-modification-écriture de `trust.json` non verrouillée (CLI vs `run`), temporaire à nom fixe, fail-open sur erreur de lecture | corrigé | Verrou de fichier exclusif (`<fichier>.lock`, `File::try_lock` = `LockFileEx`) autour de relecture→modification→écriture, verrou partagé à la lecture ; temporaire unique `nom.<pid>.<n>.tmp` supprimé en cas d'échec (résidu `.tmp` historique nettoyé) ; erreur de lecture/JSON = INIT REFUSÉS (fail-closed) et journal | `trust::tests::concurrent_invite_consume_revoke_never_resurrects`, `unreadable_or_corrupt_file_is_an_error_not_an_empty_list`, `session::tests::unreadable_trust_file_refuses_init`, `a6_unreadable_trust_file_…`, `util::tests::write_atomic_leaves_no_temp_file` |
| A7 | basse | Le host analysait/réassemblait des paquets média entrants (`total` jusqu'à 40 000 entrées) ; `other_q` (PONG) non bornée | corrigé | `Config.accept_media` (faux sur le host : vidéo/audio entrants ignorés sans analyse) ; `other_q` bornée à 256 (le plus ancien est jeté) ; le client garde le comportement symétrique | `endpoint::tests::host_side_endpoint_ignores_incoming_media`, `small_packet_queue_is_bounded`, `a7_host_ignores_incoming_media_packets` |
| A8 | basse | Pas de moyen de restreindre ce qu'un client peut injecter | corrigé | `grx-host run --no-keyboard --no-mouse --no-gamepad --block-win-key` (filtre E0 5B/5C dans l'injecteur) ; rôle `view_only` par client dans `trust.json` (défaut faux), affiché par `list`, `set-role <préfixe> view-only\|full` ; un spectateur n'a AUCUN thread d'injection ; changer le rôle d'un client connecté coupe sa session (il se reconnecte avec le nouveau rôle) | `injector::tests::win_key_is_filtered_only_when_requested`, `a8_view_only_client_never_gets_an_injection_path`, `a8_session_pipeline_opts_…`, `tests::run_flags_parse`, `tests::set_role_parses_…`, `trust::tests::view_only_role_roundtrip_and_default` |

Remarques B, toutes traitées : durée d'invitation plafonnée à 24 h (`--ttl-min` ≤ 1440, arithmétique saturante, 64 invitations en attente au plus) ; `identity.json` : la clé publique est recalculée depuis la clé privée (Curve25519) et un fichier incohérent est refusé ; zeroize des tampons contenant des secrets (fichier d'identité lu/écrit, clé privée générée, payload d'INIT déchiffré qui contient le jeton, `Token` au `Drop`) ; préfixe d'empreinte de `revoke`/`set-role` de 4 caractères minimum (un préfixe vide révoquait tout le monde).

Vérification en boucle locale (127.0.0.1, `--no-input`, GPU réel) : datagrammes de 3000/9000/60000 o, 5000 faux INIT en 1 s, puis `grx-client bench --secs 5` avec invitation : host toujours vivant, 55-59 img/s en régime établi, 0 perte, 0 image invalide ; le journal d'erreurs du host reste à 1 ligne.

### Ce qui reste (non corrigé / limites connues)

- **Inondation par adresses usurpées** : le seau global (20 INIT/s) protège le CPU, mais un attaquant qui usurpe beaucoup d'adresses peut retarder les connexions légitimes pendant l'attaque (pas de cookie sans état type QUIC « retry »). Un pare-feu/VPN reste conseillé.
- **Un INIT frais capturé puis retenu** (jamais livré) reste valable ±5 min s'il est postérieur au démarrage du host : il ne peut plus kicker ni faire émettre de média (session « en attente » détruite après 5 s), mais le jeton d'invitation qu'il porte peut être consommé par l'attaquant s'il l'intercepte (inhérent à un jeton transporté dans le premier message).
- **Horloges** : un client dont l'horloge retarde de d sera refusé pendant d après un redémarrage du host (INIT antérieur au démarrage) ; la tolérance de ±5 min existe toujours pour l'avance.
- **Révocation / changement de rôle** : la session est coupée sans préavis ; pas d'IPC avec le service (la CLI écrit le fichier, le host le relit chaque seconde).
- **Liste de confiance et identité** non chiffrées (ACL du profil ; DPAPI à faire).
- Une clé autorisée peut toujours reprendre la main de SA propre session (voulu : reconnexion) ; pas de borne sur le nombre de clients autorisés ; `view_only` n'empêche pas de recevoir l'audio/vidéo (voulu).

## Contrôle total (host élevé, fenêtres administrateur et UAC)

Problème : sous Windows, un processus d'intégrité « moyenne » (le host lancé normalement) ne peut
pas injecter d'entrées (`SendInput`) vers une fenêtre d'intégrité supérieure (UIPI). Quand une
fenêtre administrateur passe au premier plan côté host (ex. Gestionnaire des tâches), la souris du
client semble « figée ». Pendant une invite UAC ou l'écran de verrouillage, Windows bascule sur un
**bureau sécurisé** (`Winlogon`) : un processus non-SYSTEM n'y capture rien (DXGI renvoie
`E_ACCESSDENIED`) et n'y injecte rien.

Décision de l'utilisateur : « Host administrateur, sans UAC » — pas de service SYSTEM, pas de jeton
dupliqué, pas de pilotage du bureau Winlogon/UAC. On ne cherche donc **pas** à automatiser l'UAC :
on la laisse se valider sur place, et on rend le host élevé pour le reste.

### Ce qui est possible / impossible

| Situation host | Ce que voit/fait le client |
|---|---|
| Bureau normal | Historique : `ViewOnly` voit, `Full`/`Total` injectent clavier/souris/manette. |
| Fenêtre administrateur au premier plan, **host non élevé** | Image OK, manette OK (ViGEm = pilote, hors UIPI) ; **clavier/souris bloqués pour tous** (même `Total`), bandeau « activez Contrôle total sur le host ». |
| Fenêtre administrateur au premier plan, **host élevé** | `Total` pilote clavier/souris ; `Full` non (bloqué + bandeau « rôle Contrôle total requis ») ; manette OK. |
| Fenêtre d'intégrité **strictement supérieure au host** (ex. SYSTEM devant un host « seulement » élevé) | Clavier/souris bloqués pour tous (UIPI), bandeau « clavier/souris inopérants ». |
| **Bureau sécurisé** (invite UAC, Ctrl+Alt+Suppr, verrouillage) | **Jamais d'image ni d'entrée, pour personne, quel que soit le rôle.** L'UAC se valide physiquement sur le PC host. |

La politique pure est dans `grx_core::secure` (`access`/`notice`, 100 % testée sans Windows) ;
l'observation (lecture seule) du bureau et des intégrités dans `grx-host/src/secure.rs`
(`OpenInputDesktop`+`GetUserObjectInformation UOI_NAME`, `GetForegroundWindow`→`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`→`TokenIntegrityLevel`).
Un thread `grx-secure` publie la situation toutes les 200 ms ; la boucle réseau ne fait que lire un
atomique. À chaque changement, le host envoie au client un message Control `GRX-SEC`
(`grx_core::secure_notice`, 11 octets, décodage strict + fuzz) ; l'overlay affiche un bandeau
discret qui disparaît au retour à la normale. Au passage dans un état où le clavier/souris est
bloqué, le host demande au thread d'entrée un `release_all()` (pas de touche « coincée »). La
capture DXGI reprend seule après l'UAC (gestion `DXGI_ERROR_ACCESS_LOST`/`E_ACCESSDENIED` existante).

### Rôle par client

`trust.rs` : `enum Role { ViewOnly, Full, Total }` (rétrocompat `view_only` : le champ booléen reste
écrit pour qu'une version ancienne lise un rôle sûr — un `total` y apparaît comme `full` ; le plus
restrictif gagne). `grx-host set-role <fp> total`, et dans grx-gui une liste déroulante
Lecture seule / Complet / Contrôle total avec **confirmation explicite** pour Contrôle total (ce
client pourra piloter les fenêtres administrateur). Changer le rôle coupe la session (reconnexion).

### Host élevé via tâche planifiée (pas de service, une seule UAC)

`grx-host elevation enable|disable|status`, et un interrupteur dans l'onglet Host de grx-gui.
`enable` vérifie d'abord le dossier (voir modèle de menace) puis, via **ShellExecute `runas`**
(une seule UAC), relance une sous-commande interne élevée qui crée une **tâche planifiée**
« GamingRemoteExperience Host (élevé) » pour l'utilisateur courant : principal `InteractiveToken`
(sa session, sans mot de passe stocké), `RunLevel=HighestAvailable`, exécution à la demande
(+ déclencheur à l'ouverture de session, optionnel), instance unique, priorité normale. L'action
est figée : `grx-host run --from-task`. Les **options de lancement ne sont pas dans la ligne de
commande de la tâche** (elle est créée sous UAC) : grx-gui les écrit dans `host-options.json`
(`grx_core::elevation::TaskOptions`), filtrées par une **liste blanche stricte**
(`validate_run_args` : pas de `--data-dir`, pas de chemin, pas d'option de test cachée, pas de
`--opt=valeur`, valeurs bornées) ; le host élevé relit ce fichier et revalide avant de l'appliquer.

Le bouton « Démarrer » de grx-gui lance alors le host élevé par `schtasks /Run` (aucune UAC à
chaque fois). « Arrêter » : grx-gui (non élevé) ne peut pas envoyer Ctrl+C à un processus élevé —
le host élevé crée donc un **événement nommé** `Local\GamingRemoteExperience-Host-Stop-<pid>` dont
l'ACL n'autorise que l'utilisateur courant (+ SYSTEM) ; grx-gui le déclenche par
`grx-host elevation stop --pid <pid>`, le host s'arrête proprement (referme la redirection box,
`release_all`). `disable` supprime la tâche (une UAC). L'installateur/`grx-update` arrêtent et
relancent le host élevé lors d'une mise à jour (la mise à jour machine passe déjà par UAC).

### Modèle de menace

- **Élévation silencieuse par remplacement d'exe** : la tâche lance un exe élevé sans ré-auth. Si
  cet exe (ou son dossier) était modifiable par un utilisateur standard, n'importe qui pourrait le
  remplacer et obtenir une élévation silencieuse. `enable` **refuse** donc d'activer si
  `grx-host.exe` n'est pas dans un dossier protégé : vérification à la fois du chemin (sous
  `Program Files`, installation « pour tous les utilisateurs ») **et** d'un test d'écriture réel
  (`can_write`) quand on n'est pas élevé. Chemin absolu obligatoire.
- **Fichier d'options détourné** : au pire, l'utilisateur ne change que **ses propres** paramètres
  de capture (liste blanche ; jamais de chemin ni de `--data-dir`). Le fichier est dans le dossier
  de données de l'utilisateur ; il ne peut pas faire exécuter autre chose à l'exe élevé.
- **Événement d'arrêt détourné** : ACL restreinte à l'utilisateur courant — un autre utilisateur de
  la machine ne peut ni le déclencher ni le créer d'avance.
- **UAC / bureau sécurisé** : jamais capturés ni pilotés à distance (impossible pour un non-SYSTEM,
  et voulu). Une invite UAC se valide physiquement. Un client `Total` peut piloter des fenêtres
  **administrateur** (après élévation déjà accordée), ce qui est un vrai pouvoir : d'où la
  confirmation explicite et un rôle distinct de `Full`.
- **Limites** : le host élevé tourne en permanence avec les droits admin de l'utilisateur ; un
  client `Total` compromis a autant de pouvoir qu'un administrateur local interactif (hors UAC).
  Observation uniquement en lecture seule, aucun pilotage du Planificateur/Winlogon.

Tests : `grx_core::secure` (décision rôle×situation, messages + fuzz `GRX-SEC`),
`grx_core::elevation` (XML de la tâche, liste blanche, dossier protégé, SID, décodage schtasks),
`grx-host::secure` (intégrité lue sur des processus présents, en lecture seule ; simulation),
`grx-host::elevation` (preflight refuse un dossier inscriptible, aller-retour `host-options.json`),
overlay client (bandeau visible/masqué). Boucle locale réelle avec le mode de test caché
`run --simulate-secure cycle` (aucune UAC, aucune tâche créée) : pendant l'état « bureau sécurisé »
simulé, le host n'envoie plus d'image (0 img/s côté client) et envoie `GRX-SEC`.

## Jalon GUI (`crates/grx-gui`)

- **Découpage** : `logic.rs` (pur, testé : validation adresse/lien/nom, compte à rebours, dates, état de la liste de clients, arguments `grx-host run` / `grx-client play`), `qr.rs` (matrice `qrcode` → texture egui, aller-retour vérifié par décodage `rqrr`), `backend.rs` (fils d'arrière-plan), `app.rs` (UI egui uniquement), `settings.rs` (`gui.json`, sans secret).
- **Jamais d'E/S sur le thread UI** : un fil « données » possède `TrustStore` (toutes les mutations passent par son verrou de fichier) et `ProfileStore`, sonde les fichiers toutes les 700 ms (`TrustStore::refresh`, date+taille de `hosts.json`) et pousse les changements ; un fil « host » gère `grx-host run` (console cachée `CREATE_NO_WINDOW`, arrêt par `AttachConsole`+`GenerateConsoleCtrlEvent(CTRL_C)`, kill après 6 s seulement en secours) et détecte un host lancé ailleurs (`sysinfo`) ; un fil par session `grx-client`. Presse-papiers (`arboard`) en arrière-plan.
- **`grx_core::profiles`** : `hosts.json` (même format) déplacé de `grx-client` vers `grx-core`, désormais modifié sous verrou de fichier (GUI et client peuvent écrire en même temps) ; un fichier illisible est une erreur au lieu d'être écrasé.
- **Secrets** : le lien d'invitation (jeton) n'existe qu'en mémoire (zeroize à l'expiration/effacement, effacé dès qu'un nouveau client apparaît), est transmis à `grx-client` par l'entrée standard (`play -`), jamais en argument ni sur disque ; les noms annoncés par les clients sont nettoyés avant affichage.
- **Accès Internet** (`hoststatus.rs`, testé) : lecture de `status.json` du host vivant (pid vérifié par `OpenProcess` ; fichier absent/corrompu/pid mort = arrêté), traduction en lignes d'état (redirection, bail restant estimé d'après la date du fichier, conseils CGNAT/double NAT/manuel ; pare-feu) ; « Voir la commande » lance `grx-host firewall-help` (lecture seule) et n'exécute jamais la commande ; « Adresse publique automatique » = `status.json` puis `grx_portmap::PortMapper::external_ip()` dans un fil dédié ; pré-remplissage seulement si le champ est vide ou déjà automatique (`should_autofill`). Vérifié avec la vraie box : redirection UDP 7777 ouverte par le host lancé depuis la GUI, affichée, puis supprimée à la fermeture de la GUI (`probe list`).
- **Limites** : pas de scan de QR par caméra ; le lien copié reste dans le presse-papiers Windows ; le host est détecté par nom de processus (un `grx-host run` sur un autre dossier de données apparaît aussi) ; pas d'IPC avec le host (la GUI écrit `trust.json`, le host le relit chaque seconde) ; si la GUI est fermée, le host et les sessions qu'elle a lancés sont arrêtés (après confirmation).

## Installateur et mises à jour signées (0.2.0)

- **Version unique** : `[workspace.package] version` (Cargo.toml racine) → `CARGO_PKG_VERSION` de chaque exe (`--version`, VERSIONINFO + icône `installer/grx.ico` via `winresource` dans les `build.rs` ; icône générée par `tools/make_icon.py`), `AppVersion` de l'installateur (`tools/build-setup.ps1` la lit et la passe à ISCC) et `version` du manifeste.
- **Runtime C statique** : `.cargo/config.toml` (`+crt-static` pour `x86_64-pc-windows-msvc` ; libopus compilé en `/MT` par `cc`) : plus de dépendance à `VCRUNTIME140.dll`.
- **Installateur** (`installer/grx.iss`, Inno Setup 6) : `PrivilegesRequired=lowest` + `PrivilegesRequiredOverridesAllowed=dialog commandline` (pour moi / pour tous), `{autopf}`, `HKA` ; français + anglais ; protocole `grx://` → `grx-gui.exe --open-link "%1"` (tout ce qui suit `--open-link` est le lien, jamais une option ; préfixe `grx://` exigé, ≤ 1 Ko, sans caractère de contrôle ; jamais journalisé ; pas de vérification de mise à jour dans ce mode) ; avant installation/désinstallation, `grx-gui --stop-grx-processes` arrête proprement (Ctrl+C) les `grx-host`/`grx-client` **de ce dossier uniquement**, puis Restart Manager (`CloseApplications=force`) ; ViGEmBus 1.22.0 (SHA-256 épinglé + Authenticode vérifiés au build) exécuté seulement si coché et absent, élevé (`runas`) ; règle de pare-feu `netsh` seulement en mode admin ; données `%APPDATA%\GamingRemoteExperience` conservées par défaut.
- **`grx-update`** : `GET {base}/repos/{OWNER/REPO}/releases/latest` (constante `GITHUB_REPO` dans `config.rs` ; `CHANGE_ME/…` = désactivé, aucune requête ; base surchargeable par `GRX_UPDATE_URL`, http seulement vers la boucle locale), User-Agent `grx-updater/<ver>`, réponses bornées (API 1 Mio, manifeste 64 Kio, signature 1 Kio). Ordre : signature Ed25519 (`verify_strict`, message = `"GRX-UPDATE-MANIFEST-V1\0"` ‖ octets du manifeste) **avant** toute lecture → validation (semver sans build, `setup_name = GRX-Setup-<ver>.exe`, sha256, taille ≤ 512 Mio) → tag `v<ver>` cohérent → version strictement supérieure (sinon « à jour » : un ancien manifeste signé rejoué ne fait jamais revenir en arrière) → `min_version` → téléchargement dans un dossier temporaire neuf, taille + SHA-256 (fichier supprimé si faux, revérifié juste avant le lancement) → setup `/SILENT /SUPPRESSMSGBOXES /NORESTART /CURRENTUSER|/ALLUSERS /RELAUNCH` selon le dossier d'installation (`unins000.exe` présent ; Program Files = machine) ; une copie non installée lance le setup interactif.
- **Clés** : publique dans `crates/grx-update/keys/release.pub` (`include_str!`) ; privée dans `%USERPROFILE%\.grx-release\signing.key` (`tools/new-signing-key.ps1`, ACL utilisateur, jamais affichée). Une build de test peut remplacer la clé publique à la **compilation** (`GRX_UPDATE_PUBKEY_HEX`), jamais à l'exécution. Publication : `tools/release.ps1` (voir docs/RELEASE.md).
- **Pas de coupure de partie** : tant qu'un `grx-host` ou `grx-client` du dossier d'installation tourne, l'installation attend (« après la session », sondage toutes les 3 s), sauf choix explicite « Arrêter … et installer maintenant ».
- **Vérifié** : setup 0.2.0 installé en silencieux (mode utilisateur, dossier de test) : fichiers, raccourcis, `--version` des 3 exe sans VCRUNTIME, association `grx://` (onglet Client pré-rempli), désinstallation silencieuse (dossier, raccourcis, clé `grx`, entrée « Applications » supprimés ; données conservées). Mise à jour de bout en bout avec `tools/fake_github.py` : 0.2.0 → 0.2.1 installée ; 0.2.2 signée par une autre clé ou manifeste modifié → refusée (setup jamais téléchargé) ; host en marche → installation reportée. 21 tests unitaires `grx-update` (signature, manifeste modifié, hash/taille faux, downgrade, version égale, tag incohérent, réponses GitHub inattendues, politique d'URL, fuzz des analyseurs).
- **Limites** : pas de certificat de signature de code (SmartScreen) ; un Ctrl+C envoyé à un host lancé depuis un terminal atteint aussi les autres processus de ce terminal ; le mode admin (UAC) n'a été vérifié qu'à la compilation du script ; l'icône de la barre de titre reste celle d'egui (l'exe et la barre des tâches ont la bonne icône).
