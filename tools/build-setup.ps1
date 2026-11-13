# Construit l'installateur GRX-Setup-<version>.exe avec Inno Setup 6 (ISCC).
#   tools\build-setup.ps1 [-BinDir <dossier des exe>] [-OutDir <sortie>] [-NoVigem]
# - Version lue dans [workspace.package] du Cargo.toml racine (source unique).
# - ViGEmBus : installateur officiel téléchargé depuis les releases GitHub de nefarius/ViGEmBus
#   (licence BSD-3), SHA-256 épinglé ci-dessous et signature Authenticode contrôlée. Toute
#   différence = arrêt (jamais d'installateur de pilote non vérifié dans le setup).
[CmdletBinding()]
param(
    [string]$BinDir,
    [string]$OutDir,
    [switch]$NoVigem
)
$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot

$VigemVersion = "1.22.0"
$VigemName = "ViGEmBus_${VigemVersion}_x64_x86_arm64.exe"
$VigemUrl = "https://github.com/nefarius/ViGEmBus/releases/download/v$VigemVersion/$VigemName"
$VigemSha256 = "89220a7865076b342892f98865f3499fb7c4cfd673159e89d352c360fd014c6a"
$VigemSigner = "Nefarius Software Solutions e.U."

function Get-WorkspaceVersion {
    $toml = Get-Content (Join-Path $root "Cargo.toml") -Raw
    $m = [regex]::Match($toml, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"')
    if (-not $m.Success) { throw "version introuvable dans [workspace.package] du Cargo.toml" }
    return $m.Groups[1].Value
}

function Find-Iscc {
    $candidates = @(
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"),
        (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
        (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe")
    )
    foreach ($c in $candidates) { if ($c -and (Test-Path $c)) { return $c } }
    $cmd = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw "ISCC.exe introuvable. Installez Inno Setup 6 : winget install --id JRSoftware.InnoSetup -e --scope user"
}

function Get-VerifiedVigem {
    $cache = Join-Path $env:LOCALAPPDATA "grx-build-cache"
    New-Item -ItemType Directory -Force -Path $cache | Out-Null
    $file = Join-Path $cache $VigemName
    if (-not (Test-Path $file)) {
        Write-Host "Téléchargement de $VigemUrl"
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -Uri $VigemUrl -OutFile $file -UseBasicParsing -UserAgent "grx-build"
    }
    $hash = (Get-FileHash -Algorithm SHA256 $file).Hash.ToLowerInvariant()
    if ($hash -ne $VigemSha256) {
        Remove-Item $file -Force
        throw "SHA-256 de ViGEmBus inattendu ($hash) : fichier supprimé, construction arrêtée"
    }
    $sig = Get-AuthenticodeSignature $file
    if ($sig.Status -ne "Valid" -or $sig.SignerCertificate.Subject -notlike "*CN=$VigemSigner*") {
        throw "signature Authenticode de ViGEmBus invalide ou inattendue ($($sig.Status))"
    }
    Write-Host "ViGEmBus $VigemVersion vérifié (SHA-256 + signature $VigemSigner)"
    return $file
}

$version = Get-WorkspaceVersion
if (-not $BinDir) {
    $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root "target" }
    $BinDir = Join-Path $target "release"
}
if (-not $OutDir) { $OutDir = Join-Path $root "dist\$version" }
$BinDir = (Resolve-Path $BinDir).Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
foreach ($exe in "grx-gui.exe", "grx-host.exe", "grx-client.exe") {
    if (-not (Test-Path (Join-Path $BinDir $exe))) { throw "$exe absent de $BinDir (compilez d'abord en release)" }
}

$iscc = Find-Iscc
$defs = @("/DAppVersion=$version", "/DBinDir=$BinDir", "/DOutDir=$OutDir")
if (-not $NoVigem) { $defs += "/DVigemExe=$(Get-VerifiedVigem)" }
& $iscc /Q @defs (Join-Path $root "installer\grx.iss")
if ($LASTEXITCODE -ne 0) { throw "ISCC a échoué (code $LASTEXITCODE)" }
$setup = Join-Path $OutDir "GRX-Setup-$version.exe"
Write-Host "Setup : $setup"
