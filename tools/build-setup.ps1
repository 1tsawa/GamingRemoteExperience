# Construit l'installateur GRX-Setup-<version>.exe avec Inno Setup 6 (ISCC).
#   tools\build-setup.ps1 [-BinDir <dossier des exe>] [-OutDir <sortie>] [-NoVigem] [-FfmpegDir <dossier>] [-NoFfmpeg]
# - Version lue dans [workspace.package] du Cargo.toml racine (source unique).
# - ViGEmBus : installateur officiel téléchargé depuis les releases GitHub de nefarius/ViGEmBus
#   (licence BSD-3), SHA-256 épinglé ci-dessous et signature Authenticode contrôlée. Toute
#   différence = arrêt (jamais d'installateur de pilote non vérifié dans le setup).
# - FFmpeg (intégré par défaut depuis 0.2.3) : décodeur D3D11VA (préféré sur GPU Intel, secours
#   HEVC/AV1 ailleurs). Sans -FfmpegDir, la build LGPL partagée épinglée ci-dessous (BtbN
#   FFmpeg-Builds, FFmpeg 8.1 = avcodec 62 / avutil 60) est téléchargée, vérifiée par SHA-256 et
#   seules avcodec-62.dll, avutil-60.dll et swresample-6.dll (dépendance réelle d'avcodec) en sont
#   extraites dans third_party\ffmpeg, avec LICENSE.txt (LGPL) et SOURCE.txt (provenance). Une
#   build GPL est REFUSÉE (« --enable-gpl » / « --enable-nonfree » dans la configuration
#   embarquée). -NoFfmpeg : setup sans FFmpeg. Voir docs/ARCHITECTURE.md.
# - OpenH264 (Cisco, BSD) : PAS intégré (la licence de brevets de Cisco exige un téléchargement
#   séparé sur le poste de l'utilisateur) ; le setup installe seulement le texte de licence
#   (third_party\openh264\BINARY_LICENSE.txt) et propose une tâche qui lance « grx-host
#   fetch-openh264 » (SHA-256 épinglés dans grx-update/src/openh264.rs). Vérifié ici : pin cohérent.
[CmdletBinding()]
param(
    [string]$BinDir,
    [string]$OutDir,
    [switch]$NoVigem,
    [string]$FfmpegDir,
    [switch]$NoFfmpeg
)
$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot

$VigemVersion = "1.22.0"
$VigemName = "ViGEmBus_${VigemVersion}_x64_x86_arm64.exe"
$VigemUrl = "https://github.com/nefarius/ViGEmBus/releases/download/v$VigemVersion/$VigemName"
$VigemSha256 = "89220a7865076b342892f98865f3499fb7c4cfd673159e89d352c360fd014c6a"
$VigemSigner = "Nefarius Software Solutions e.U."

# FFmpeg LGPL partagé (BtbN). Les « autobuild » sont purgées au bout de quelques semaines : une
# fois extraite, third_party\ffmpeg sert de cache ; pour une nouvelle machine, mettre à jour
# l'URL et le SHA-256 (même version majeure 8.x, sinon adapter grx-render/src/ffmpeg.rs).
$FfmpegBuild = "ffmpeg-n8.1.3-35-gf1acb0a266-win64-lgpl-shared-8.1"
$FfmpegUrl = "https://github.com/FFmpeg-Builds/FFmpeg-Builds/releases/download/autobuild-2026-11-14-12-37/$FfmpegBuild.zip"
$FfmpegSha256 = "97927ac5391d5e3a11dc3bce679d54f7fde04cf54380a09970c32eea49ba20a2"
$FfmpegSourceUrl = "https://github.com/FFmpeg/FFmpeg/tree/f1acb0a266 (FFmpeg n8.1.3-35-gf1acb0a266) ; scripts de build : https://github.com/FFmpeg-Builds/FFmpeg-Builds (ex-BtbN)"
$FfmpegDlls = @("avcodec-62.dll", "avutil-60.dll", "swresample-6.dll")

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

function Get-VerifiedFfmpeg([string]$dir) {
    foreach ($f in "avcodec-62.dll", "avutil-60.dll", "LICENSE.txt", "SOURCE.txt") {
        if (-not (Test-Path (Join-Path $dir $f))) { throw "FFmpeg : $f absent de $dir (voir docs/ARCHITECTURE.md, section FFmpeg)" }
    }
    $lic = Get-Content (Join-Path $dir "LICENSE.txt") -Raw
    if ($lic -notmatch "LESSER GENERAL PUBLIC LICENSE") { throw "FFmpeg : LICENSE.txt n'est pas la LGPL" }
    foreach ($dll in "avcodec-62.dll", "avutil-60.dll", "swresample-6.dll") {
        $p = Join-Path $dir $dll
        if (-not (Test-Path $p)) { continue }
        # La configuration de compilation est embarquée en clair dans chaque bibliothèque.
        $text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($p))
        if ($text.Contains("--enable-gpl") -or $text.Contains("--enable-nonfree")) {
            throw "FFmpeg : $dll est une build GPL/nonfree, refusée (il faut une build LGPL)"
        }
        $h = (Get-FileHash -Algorithm SHA256 $p).Hash.ToLowerInvariant()
        Write-Host "FFmpeg LGPL : $dll ($([math]::Round((Get-Item $p).Length / 1MB, 1)) Mo) SHA-256 $h"
    }
    return (Resolve-Path $dir).Path
}

# Télécharge (SHA-256 épinglé) la build LGPL et n'en extrait que les DLL nécessaires.
function Get-PinnedFfmpeg([string]$dest) {
    $cache = Join-Path $env:LOCALAPPDATA "grx-build-cache"
    New-Item -ItemType Directory -Force -Path $cache | Out-Null
    $zip = Join-Path $cache "$FfmpegBuild.zip"
    if (-not (Test-Path $zip)) {
        Write-Host "Téléchargement de $FfmpegUrl"
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -Uri $FfmpegUrl -OutFile $zip -UseBasicParsing -UserAgent "grx-build"
    }
    $hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
    if ($hash -ne $FfmpegSha256) {
        Remove-Item $zip -Force
        throw "SHA-256 de la build FFmpeg inattendu ($hash) : fichier supprimé, construction arrêtée"
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    $z = [IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $want = @($FfmpegDlls | ForEach-Object { "$FfmpegBuild/bin/$_" }) + "$FfmpegBuild/LICENSE.txt"
        foreach ($e in $z.Entries) {
            if ($want -contains $e.FullName) {
                [IO.Compression.ZipFileExtensions]::ExtractToFile($e, (Join-Path $dest $e.Name), $true)
            }
        }
    } finally { $z.Dispose() }
    foreach ($f in $FfmpegDlls + "LICENSE.txt") {
        if (-not (Test-Path (Join-Path $dest $f))) { throw "FFmpeg : $f absent de l'archive $FfmpegBuild" }
    }
    $src = @(
        "FFmpeg (LGPL v3 : build « lgpl-shared », --enable-version3 sans --enable-gpl ni --enable-nonfree) intégré à GamingRemoteExperience.",
        "Build binaire : $FfmpegUrl",
        "SHA-256 de l'archive : $FfmpegSha256",
        "Sources : $FfmpegSourceUrl",
        "DLL inchangées, chargées dynamiquement ; remplaçables par toute build compatible (avcodec 62 / avutil 60)."
    )
    Set-Content -Path (Join-Path $dest "SOURCE.txt") -Value $src -Encoding UTF8
    Write-Host "FFmpeg LGPL $FfmpegBuild extrait dans $dest (SHA-256 de l'archive vérifié)"
}

$ohLicense = Join-Path $root "third_party\openh264\BINARY_LICENSE.txt"
if (-not (Test-Path $ohLicense) -or -not ((Get-Content -Raw $ohLicense) -match "OpenH264 Video Codec provided by Cisco Systems")) {
    throw "third_party\openh264\BINARY_LICENSE.txt manquant ou inattendu (texte de licence Cisco requis par le setup)"
}
$ohPin = Select-String -Path (Join-Path $root "crates\grx-update\src\openh264.rs") -Pattern 'DLL_SHA256: &str = "([0-9a-f]{64})"'
if (-not $ohPin) { throw "SHA-256 OpenH264 introuvable dans grx-update" }
Write-Host "OpenH264 : téléchargé par le setup depuis Cisco (tâche facultative), DLL SHA-256 $($ohPin.Matches[0].Groups[1].Value)"
$iscc = Find-Iscc
$defs = @("/DAppVersion=$version", "/DBinDir=$BinDir", "/DOutDir=$OutDir")
if (-not $NoVigem) { $defs += "/DVigemExe=$(Get-VerifiedVigem)" }
if (-not $FfmpegDir) {
    $FfmpegDir = Join-Path $root "third_party\ffmpeg"
    if (-not $NoFfmpeg -and -not (Test-Path (Join-Path $FfmpegDir "SOURCE.txt"))) { Get-PinnedFfmpeg $FfmpegDir }
}
if (-not $NoFfmpeg -and (Test-Path (Join-Path $FfmpegDir "avcodec-62.dll"))) {
    $defs += "/DFfmpegDir=$(Get-VerifiedFfmpeg $FfmpegDir)"
    if (-not (Test-Path (Join-Path $FfmpegDir "swresample-6.dll"))) { $defs += "/DFfmpegNoSwr=1" }
} else {
    Write-Host "FFmpeg non intégré (décodeur de secours désactivé ; HEVC sans extension -> H.264 négocié)"
}
& $iscc /Q @defs (Join-Path $root "installer\grx.iss")
if ($LASTEXITCODE -ne 0) { throw "ISCC a échoué (code $LASTEXITCODE)" }
$setup = Join-Path $OutDir "GRX-Setup-$version.exe"
Write-Host "Setup : $setup"
