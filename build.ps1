param(
    [switch]$SkipTests,
    [switch]$ReleaseOnly
)

$ErrorActionPreference = 'Stop'
$ProjectRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Push-Location $ProjectRoot
try {
    $VsDevCmd = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat'
    if (!(Test-Path $VsDevCmd)) {
        throw "Visual Studio 2022 Build Tools VsDevCmd.bat not found at $VsDevCmd"
    }

    $envOutput = cmd /c "`"$VsDevCmd`" -arch=amd64 -host_arch=amd64 >nul && set"
    if ($LASTEXITCODE -ne 0) {
        throw "VsDevCmd.bat failed with exit code $LASTEXITCODE"
    }
    foreach ($line in $envOutput) {
        if ($line -match '^([^=]+)=(.*)$') {
            [System.Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
        }
    }

    $CargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    $VulkanSdk = Join-Path $env:LOCALAPPDATA 'VulkanSDK\1.4.363.0'
    if (!(Test-Path $VulkanSdk)) {
        $detected = Get-ChildItem (Join-Path $env:LOCALAPPDATA 'VulkanSDK') -Directory -ErrorAction SilentlyContinue |
            Sort-Object {
                $v = $null
                if ([version]::TryParse($_.Name, [ref]$v)) { $v } else { [version]'0.0' }
            } -Descending |
            Select-Object -First 1
        if ($detected) {
            $VulkanSdk = $detected.FullName
        } else {
            throw 'LunarG Vulkan SDK not found in %LOCALAPPDATA%\VulkanSDK'
        }
    }

    $env:CMAKE_GENERATOR = 'Ninja'
    $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
    $env:VULKAN_SDK = $VulkanSdk
    $env:CARGO_TARGET_DIR = 'C:\tl_target'
    $env:PATH = "$CargoBin;$VulkanSdk\Bin;$env:PATH"

    $SherpaVersion = '2.0.2'
    $SherpaArchiveName = "sherpa-onnx-v$SherpaVersion-win-x64-static-MD-MinSizeRel-lib.tar.bz2"
    $SherpaStem = "sherpa-onnx-v$SherpaVersion-win-x64-static-MD-MinSizeRel-lib"
    $SherpaExpectedSha256 = '1420AFF4A273C551D7C13FE40EB49E84280E506EB8860757D964A9A53DEF6636'
    $SherpaCacheDir = 'C:\tl_target\sherpa-onnx-md'
    $SherpaLibDir = Join-Path (Join-Path $SherpaCacheDir $SherpaStem) 'lib'

    if (!(Test-Path (Join-Path $SherpaLibDir 'sherpa-onnx-c-api.lib'))) {
        New-Item -ItemType Directory -Force -Path $SherpaCacheDir | Out-Null
        $ArchivePath = Join-Path $SherpaCacheDir $SherpaArchiveName
        if (!(Test-Path $ArchivePath)) {
            $Url = "https://github.com/k2-fsa/sherpa-onnx/releases/download/v$SherpaVersion/$SherpaArchiveName"
            $PartPath = "$ArchivePath.part"
            Write-Host "Downloading $Url ..."
            curl.exe -fsSL -o $PartPath $Url
            if ($LASTEXITCODE -ne 0) {
                if (Test-Path $PartPath) { Remove-Item -Force $PartPath -ErrorAction SilentlyContinue }
                throw "Failed to download $Url (curl exit code $LASTEXITCODE)"
            }
            $actualHash = (Get-FileHash $PartPath -Algorithm SHA256).Hash.ToUpperInvariant()
            if ($actualHash -ne $SherpaExpectedSha256) {
                Remove-Item -Force $PartPath -ErrorAction SilentlyContinue
                throw "SHA-256 mismatch for $SherpaArchiveName (expected $SherpaExpectedSha256, got $actualHash)"
            }
            Move-Item -Force $PartPath $ArchivePath
        } else {
            $existingHash = (Get-FileHash $ArchivePath -Algorithm SHA256).Hash.ToUpperInvariant()
            if ($existingHash -ne $SherpaExpectedSha256) {
                throw "Existing archive $ArchivePath failed SHA-256 verification (expected $SherpaExpectedSha256, got $existingHash)"
            }
        }
        Write-Host "Extracting $SherpaArchiveName ..."
        tar -xjf $ArchivePath -C $SherpaCacheDir
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to extract $SherpaArchiveName (tar exit code $LASTEXITCODE)"
        }
    }

    if (!(Test-Path (Join-Path $SherpaLibDir 'sherpa-onnx-c-api.lib'))) {
        throw "Expected static /MD sherpa-onnx libs in $SherpaLibDir"
    }
    $env:SHERPA_ONNX_LIB_DIR = $SherpaLibDir

    if (-not $SkipTests -and -not $ReleaseOnly) {
        Write-Host 'Running cargo test...'
        cargo test
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }

    Write-Host 'Building release binary...'
    cargo build --release
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    $BuiltExe = Join-Path $env:CARGO_TARGET_DIR 'release\translate-live.exe'
    $OutExe = Join-Path $ProjectRoot 'Translate-Live.exe'
    Copy-Item -Force $BuiltExe $OutExe
    Write-Host "Built standalone executable: $OutExe"
} finally {
    Pop-Location
}
