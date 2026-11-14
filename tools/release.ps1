<#
.SYNOPSIS
    Prépare (et, avec -Publish seulement, publie) une release GamingRemoteExperience.
    toolselease.ps1            -> tests, build, setup, manifest.json signé dans dist\<version>
    toolselease.ps1 -Publish   -> idem puis gh release create v<version> (voir docs/RELEASE.md)
.DESCRIPTION
    Compile les binaires, génère l'installateur, crée et signe le manifeste de mise à jour,
    puis publie sur GitHub (si -Publish est activé). Compatible Windows PowerShell 5.1 et PS 7.
.PARAMETER Publish
    Active la publication automatique sur GitHub.
.PARAMETER NotesFile
    Chemin vers un fichier de notes de version personnalisé.
.PARAMETER MinVersion
    Version minimale requise pour les mises à jour.
.PARAMETER SkipTests
    Ignore l'exécution des tests.
.PARAMETER FfmpegDir
    Dossier des DLL FFmpeg LGPL à intégrer (défaut : third_party\ffmpeg s'il existe ; voir build-setup.ps1).
.PARAMETER NoFfmpeg
    N'intègre pas FFmpeg même si le dossier existe.
.PARAMETER KeyPath
    Chemin vers la clé privée de signature (défaut : %USERPROFILE%\.grx-release\signing.key).
#>
[CmdletBinding()]
param(
    [switch]$Publish,
    [string]$NotesFile,
    [string]$MinVersion,
    [switch]$SkipTests,
    [string]$FfmpegDir,
    [switch]$NoFfmpeg,
    [string]$KeyPath = (Join-Path $env:USERPROFILE '.grx-release\signing.key')
)

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
$bin = Join-Path $target 'release'

function Fail($msg) {
    Write-Host $msg -ForegroundColor Red
    exit 1
}

Push-Location $root
try {
    # Étape 0 : Vérifications préliminaires
    Write-Host "==> Étape 0 : Vérifications préliminaires" -ForegroundColor Cyan
    $toml = Get-Content (Join-Path $root 'Cargo.toml') -Raw
    $verMatch = [regex]::Match($toml, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"')
    if (-not $verMatch.Success) { Fail "Impossible de lire la version depuis [workspace.package] du Cargo.toml" }
    $version = $verMatch.Groups[1].Value

    $configPath = Join-Path $root 'crates\grx-update\src\config.rs'
    $configContent = Get-Content $configPath -Raw
    $repoMatch = [regex]::Match($configContent, 'pub const GITHUB_REPO: &str = "([^"]+)";')
    if (-not $repoMatch.Success) { Fail "Impossible de lire GITHUB_REPO depuis $configPath" }
    $repo = $repoMatch.Groups[1].Value

    if (-not (Test-Path $KeyPath)) {
        Fail "La clé de signature n'existe pas : $KeyPath. Lancez tools\new-signing-key.ps1 pour en générer une."
    }

    $pubPath = Join-Path $root 'crates\grx-update\keys\release.pub'
    $pubContent = (Get-Content $pubPath -Raw).Trim()
    if ($pubContent.Length -ne 64 -or $pubContent -notmatch '^[0-9a-fA-F]{64}$') {
        Fail "Le fichier de clé publique $pubPath doit contenir exactement 64 caractères hexadécimaux."
    }

    if ($Publish -and $repo.StartsWith('CHANGE_ME/')) {
        Fail "Renseignez GITHUB_REPO dans crates/grx-update/src/config.rs (voir docs/RELEASE.md) avant de publier."
    }

    # Étape 1 : Tests
    Write-Host "==> Étape 1 : Tests" -ForegroundColor Cyan
    if (-not $SkipTests) {
        cargo test --workspace --release
        if ($LASTEXITCODE -ne 0) { Fail "Les tests ont échoué." }
    }

    # Étape 2 : Compilation
    Write-Host "==> Étape 2 : Compilation" -ForegroundColor Cyan
    cargo build --release -p grx-gui -p grx-host -p grx-client -p grx-update
    if ($LASTEXITCODE -ne 0) { Fail "La compilation a échoué." }

    # Étape 3 : Construction de l'installateur
    Write-Host "==> Étape 3 : Construction de l'installateur" -ForegroundColor Cyan
    $dist = Join-Path $root "dist\$version"
    if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    $dist = (Resolve-Path $dist).Path

    $setupArgs = @{ BinDir = $bin; OutDir = $dist }
    if ($FfmpegDir) { $setupArgs.FfmpegDir = $FfmpegDir }
    if ($NoFfmpeg) { $setupArgs.NoFfmpeg = $true }
    & (Join-Path $PSScriptRoot 'build-setup.ps1') @setupArgs
    if ($? -eq $false) { Fail "build-setup.ps1 a échoué." }

    $setupPath = Join-Path $dist "GRX-Setup-$version.exe"
    if (-not (Test-Path $setupPath)) { Fail "L'installateur n'a pas été généré : $setupPath" }

    # Étape 4 : Notes de version
    Write-Host "==> Étape 4 : Notes de version" -ForegroundColor Cyan
    $notesPath = Join-Path $dist "notes.txt"
    if ($NotesFile) {
        if (-not (Test-Path $NotesFile)) { Fail "Le fichier de notes spécifié n'existe pas : $NotesFile" }
        $notesPath = (Resolve-Path $NotesFile).Path
    } else {
        # Section « ## <version> » de CHANGELOG.md (jusqu'à la section suivante). Obligatoire avec -Publish.
        $changelog = Join-Path $root 'CHANGELOG.md'
        $section = $null
        if (Test-Path $changelog) {
            $lines = [IO.File]::ReadAllLines($changelog, [Text.Encoding]::UTF8)
            $start = [Array]::FindIndex($lines, [Predicate[string]]{ param($l) $l.Trim() -eq "## $version" })
            if ($start -ge 0) {
                $body = New-Object System.Collections.Generic.List[string]
                for ($i = $start + 1; $i -lt $lines.Length -and -not $lines[$i].StartsWith('## '); $i++) { $body.Add($lines[$i]) }
                $section = ($body -join "`n").Trim()
            }
        }
        if (-not $section) {
            if ($Publish) { Fail "CHANGELOG.md n'a pas de section « ## $version » : ajoutez le changelog avant de publier." }
            $section = "GamingRemoteExperience $version"
        }
        [IO.File]::WriteAllText($notesPath, $section, (New-Object Text.UTF8Encoding $false))
        Write-Host "Notes : section $version de CHANGELOG.md"
    }

    # Étape 5 : Génération du manifeste
    Write-Host "==> Étape 5 : Génération du manifeste" -ForegroundColor Cyan
    $tool = Join-Path $bin 'grx-release-tool.exe'
    $manifestPath = Join-Path $dist 'manifest.json'
    $manifestArgs = @('manifest', '--version', $version, '--setup', $setupPath, '--notes-file', $notesPath, '--out', $manifestPath)
    if ($MinVersion) { $manifestArgs += @('--min-version', $MinVersion) }
    & $tool @manifestArgs
    if ($LASTEXITCODE -ne 0) { Fail "La génération du manifeste a échoué." }

    # Étape 6 : Signature du manifeste
    Write-Host "==> Étape 6 : Signature du manifeste" -ForegroundColor Cyan
    $sigPath = Join-Path $dist 'manifest.json.sig'
    $fullKeyPath = (Resolve-Path $KeyPath).Path
    $fullPubPath = (Resolve-Path $pubPath).Path
    $fullManifestPath = (Resolve-Path $manifestPath).Path
    $fullSigPath = $sigPath

    & $tool sign --key $fullKeyPath --in $fullManifestPath --out $fullSigPath --expect-pub $fullPubPath
    if ($LASTEXITCODE -ne 0) { Fail "La signature du manifeste a échoué." }

    # Étape 7 : Vérification de la signature
    Write-Host "==> Étape 7 : Vérification de la signature" -ForegroundColor Cyan
    & $tool verify --pub $fullPubPath --in $fullManifestPath --sig $fullSigPath
    if ($LASTEXITCODE -ne 0) { Fail "La vérification de la signature a échoué." }

    # Étape 8 : Résumé des artefacts
    Write-Host "==> Étape 8 : Résumé des artefacts" -ForegroundColor Cyan
    Write-Host "Dossier de sortie : $dist"
    $artifacts = @(
        @{ Name = "Installateur"; Path = $setupPath },
        @{ Name = "Manifeste"; Path = $manifestPath },
        @{ Name = "Signature"; Path = $sigPath }
    )
    foreach ($art in $artifacts) {
        $file = Get-Item $art.Path
        $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $file.FullName).Hash.ToLowerInvariant()
        Write-Host "  - $($art.Name) : $($file.Name) | $($file.Length) octets | SHA-256 : $hash"
    }

    # Étape 9 : Publication
    Write-Host "==> Étape 9 : Publication" -ForegroundColor Cyan
    if (-not $Publish) {
        Write-Host "Rien n'a été publié. Pour publier : tools\release.ps1 -Publish" -ForegroundColor Yellow
        exit 0
    }

    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
        Fail @"
GitHub CLI (gh) introuvable. Installez-le : winget install GitHub.cli
connectez-vous : gh auth login
et relancez tools\release.ps1 -Publish
"@
    }

    gh auth status
    if ($LASTEXITCODE -ne 0) { Fail "Vérifiez votre connexion GitHub : lancez gh auth login" }

    gh release view "v$version" --repo $repo 2>$null
    if ($LASTEXITCODE -eq 0) { Fail "La release v$version existe déjà : augmentez la version dans Cargo.toml" }

    $fullSetupPath = (Resolve-Path $setupPath).Path
    $fullManifestPath = (Resolve-Path $manifestPath).Path
    $fullSigPath = (Resolve-Path $sigPath).Path

    gh release create "v$version" $fullSetupPath $fullManifestPath $fullSigPath --repo $repo --title "GamingRemoteExperience $version" --notes-file $notesPath
    if ($LASTEXITCODE -ne 0) { Fail "La publication sur GitHub a échoué." }

    Write-Host "Publié : https://github.com/$repo/releases/tag/v$version" -ForegroundColor Green
}
finally {
    Pop-Location
}
