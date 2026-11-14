# Changelog

## 0.2.3

- Client : une seule conversion YUV→RGB (VideoProcessorBlt) par image ; curseur/calque/redimensionnement recopient l'image RGB, re-présentations plafonnées à la fréquence de l'écran (≤ 120 Hz).
- Décodeur MF : sortie au format du flux (NV12 en 8 bits 4:2:0) au lieu de P010/AYUV/Y410 (conversion par image dans le MFT, GPU Intel saturé).
- GPU Intel : FFmpeg D3D11VA préféré à Media Foundation (repli MF) ; FFmpeg LGPL 8.1 (avcodec/avutil/swresample) intégré au setup, SHA-256 épinglé.
- Débit max par défaut 30 Mb/s (au lieu de 50) ; `--max-bitrate` inchangé.
- Host : encodeurs détectés par test réel (création + 2 images + validation du flux), cache invalidé au changement de GPU/pilote/version ; `grx-host encoders [--refresh]` ; grx-gui signale (⚠) un choix d'encodeur/couleur non supporté.
- RTT : une 1re mesure aberrante au démarrage ne fausse plus l'affichage pendant ~10 s.
- `play --stats` : ligne `diag` (voie, Blt/s, Present/s, ProcessOutput/s, resets, IDR/s, décodeurs créés, renégociations, débit reçu) et bilan GPU.

## 0.2.2

- Encodeurs AMD (AMF) et Intel (Quick Sync) via leurs MFT, choisis selon le GPU de l'écran capturé ; `run --encoder`.
- Secours logiciel OpenH264 (Cisco, téléchargé à l'installation, SHA-256 vérifié) : encodeur host et décodeur client ; `fetch-openh264`.
- Décodeur au choix (Auto / Matériel / Logiciel) dans le menu et `play --decoder` ; FFmpeg logiciel ; MFT décodeurs asynchrones (Intel).
- Couleur 4:4:4 (HEVC RExt via NVENC direct) : case du menu, `run --chroma`.
- 10 bits / HDR10 : HEVC Main10, capture HDR FP16, affichage HDR ou tone-mapping SDR ; case « Préférer 10 bits ».
- Statistiques : encodeur (vendeur), profil, chroma, profondeur, décodeur.
- grx-gui : onglet Paramètres (choix du codec HEVC/H.264/AV1 ou Auto, options client/host).

## 0.2.1

- Négociation automatique du codec (HEVC, H.264, AV1) selon les décodeurs du client ; repli H.264 sans extension HEVC.
- Décodage HEVC de secours via FFmpeg D3D11VA (DLL LGPL optionnelles) ; message d'erreur de décodeur explicite.
- Mode Contrôle total : host élevé via tâche planifiée, rôle « Contrôle total » par client.
- Avertissement côté client quand une UAC ou une fenêtre administrateur est au premier plan sur le host.
- Bascule d'écran à chaud dans l'overlay (Ctrl+Maj+←/→), souris et curseur suivent l'écran choisi.
- grx-gui : onglet Client en premier.

## 0.2.0

- Première version : streaming H.265/H.264 NVENC sur UDP chiffré (Noise IK, AES-256-GCM), sans serveur d'authentification.
- Appairage par clés publiques et invitations à usage unique ; interface grx-gui avec QR code.
- Ouverture automatique du port (UPnP / PCP / NAT-PMP), IPv6.
- Curseur distant, overlay façon Parsec déplaçable, audio Opus, manettes Xbox / PS4 / PS5.
- Installateur (utilisateur ou machine) et mises à jour signées Ed25519 via GitHub Releases.
