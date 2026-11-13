# Génère la clé de signature Ed25519 des mises à jour (une seule fois).
#   - clé PRIVÉE : %USERPROFILE%\.grx-release\signing.key (hors du dépôt, jamais affichée)
#   - clé PUBLIQUE : crates\grx-update\keys\release.pub (intégrée aux binaires)
# Refuse d'écraser une clé existante : une nouvelle clé rendrait impossible la mise à jour
# automatique des installations existantes.
[CmdletBinding()]
param(
    [string]$KeyPath = (Join-Path $env:USERPROFILE ".grx-release\signing.key")
)
# Pas de "Stop" global : sous PowerShell 5.1, la sortie stderr de cargo serait prise pour une erreur.
$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
$pub = Join-Path $root "crates\grx-update\keys\release.pub"
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root "target" }

if (Test-Path $KeyPath) {
    Write-Host "Une clé existe déjà : $KeyPath (rien n'est modifié)."
    exit 0
}
Push-Location $root
try {
    cargo build --release -p grx-update --bin grx-release-tool
    if ($LASTEXITCODE -ne 0) { throw "compilation de grx-release-tool impossible" }
    & (Join-Path $target "release\grx-release-tool.exe") keygen --key $KeyPath --pub $pub
    if ($LASTEXITCODE -ne 0) { throw "génération de la clé impossible" }
} finally {
    Pop-Location
}
# Restreint l'accès au fichier à l'utilisateur courant.
icacls $KeyPath /inheritance:r /grant:r "$($env:USERNAME):(R,W)" | Out-Null

Write-Host ""
Write-Host "==================================================================" -ForegroundColor Yellow
Write-Host " SAUVEGARDEZ MAINTENANT $KeyPath" -ForegroundColor Yellow
Write-Host " (clé USB chiffrée, gestionnaire de mots de passe...)." -ForegroundColor Yellow
Write-Host " Perdue  : plus aucune mise à jour automatique possible pour les" -ForegroundColor Yellow
Write-Host "           installations existantes (réinstallation manuelle)." -ForegroundColor Yellow
Write-Host " Volée   : un attaquant pourrait signer une fausse mise à jour." -ForegroundColor Yellow
Write-Host " Ne la copiez JAMAIS dans le dépôt ni dans un ticket/chat." -ForegroundColor Yellow
Write-Host "==================================================================" -ForegroundColor Yellow
Write-Host "Recompilez ensuite les binaires : la clé publique y est intégrée."
