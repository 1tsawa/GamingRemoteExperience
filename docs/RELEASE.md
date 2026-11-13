# Publier une version (installateur + mises à jour signées)

Les mises à jour sont hébergées sur **GitHub Releases**. Chaque release contient :

| Fichier | Rôle |
|---|---|
| `GRX-Setup-<ver>.exe` | installateur Inno Setup |
| `manifest.json` | `{version, published, setup_name, sha256, size, notes, min_version?}` |
| `manifest.json.sig` | signature **Ed25519** du manifeste (128 caractères hexadécimaux) |

grx-gui n'installe une mise à jour que si la signature est valide pour la clé publique intégrée
(`crates/grx-update/keys/release.pub`), que la version est **strictement supérieure** et que la taille
et le SHA-256 du setup téléchargé correspondent au manifeste signé.

## Étapes à faire une seule fois

1. **Clé de signature** : déjà générée par `tools\new-signing-key.ps1` dans
   `%USERPROFILE%\.grx-release\signing.key` (hors du dépôt). **Sauvegardez-la** (clé USB chiffrée,
   gestionnaire de mots de passe). Perdue : plus de mise à jour automatique possible pour les
   installations existantes. Volée : un attaquant pourrait signer une fausse mise à jour.
   Ne la mettez jamais dans le dépôt, un ticket ou un chat. (Sur un autre PC : copiez-la au même endroit.)
2. **Créer le dépôt GitHub** (public, sinon l'API anonyme ne voit pas les releases), par exemple
   `MonCompte/GamingRemoteExperience`.
3. **Renseigner le dépôt** dans `crates/grx-update/src/config.rs` :
   `pub const GITHUB_REPO: &str = "MonCompte/GamingRemoteExperience";`
   Tant qu'il vaut `CHANGE_ME/…`, grx-gui ne fait aucune requête et affiche « Mises à jour pas encore configurées ».
4. **GitHub CLI** : `winget install GitHub.cli` puis `gh auth login`.

## À chaque version

1. Augmenter `version` dans `[workspace.package]` du `Cargo.toml` racine (seule source de vérité :
   exe, `--version`, installateur et manifeste la reprennent).
2. Préparer sans publier : `tools\release.ps1 [-NotesFile notes.txt] [-MinVersion 0.2.0]`
   → tests, build release, `dist\<ver>\GRX-Setup-<ver>.exe`, `manifest.json` signé et vérifié.
3. Tester le setup de `dist\<ver>` sur une machine.
4. Publier : `tools\release.ps1 -Publish` (fait `gh release create v<ver> …`).

`-MinVersion X` : les installations plus anciennes que X ne se mettent pas à jour automatiquement
(message « installation manuelle requise »), utile si un changement incompatible l'exige.

## Construire seulement l'installateur

```powershell
cargo build --release -p grx-gui -p grx-host -p grx-client
tools\build-setup.ps1            # -> dist\<ver>\GRX-Setup-<ver>.exe (Inno Setup 6 requis)
```
Inno Setup : `winget install --id JRSoftware.InnoSetup -e --scope user`.
Le script télécharge l'installateur officiel de **ViGEmBus 1.22.0** (nefarius, BSD-3), vérifie son
SHA-256 épinglé et sa signature Authenticode, puis l'intègre en composant optionnel.

## Tester une mise à jour localement

`tools\fake_github.py --release-dir <dossier> --port-file port.txt` imite l'API GitHub sur 127.0.0.1.
Le client accepte `GRX_UPDATE_URL=http://127.0.0.1:<port>` (http seulement en boucle locale ; la
signature reste exigée). Pour signer avec une clé de TEST, compilez avec
`GRX_UPDATE_PUBKEY_HEX=<clé publique de test>` (variable lue à la **compilation**, jamais à l'exécution),
puis `grx-gui.exe --update-now` (sans fenêtre ; codes : 0 setup lancé, 2 à jour, 3 sessions en cours,
4 installation manuelle requise, 1 erreur).

## Limites

- **Pas de certificat de signature de code** : SmartScreen affichera « Windows a protégé votre
  ordinateur » au premier lancement du setup (« Informations complémentaires » → « Exécuter quand même »)
  tant que le setup n'a pas de réputation. La signature Ed25519 protège les mises à jour automatiques,
  pas le premier téléchargement : publiez le SHA-256 du setup avec la release.
- Le pilote ViGEmBus n'est installé que s'il est absent ; sa désinstallation reste manuelle (il peut
  servir à d'autres logiciels).
