# Changelog

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
