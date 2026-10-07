# Install or update viper on Windows, from PowerShell:
#
#   irm https://raw.githubusercontent.com/stephenberry/viper/main/install.ps1 | iex
#
# Environment:
#   VIPER_VERSION      release to install, e.g. v0.1.1 (default: the latest)
#   VIPER_INSTALL_DIR  where to put viper.exe (default: %LOCALAPPDATA%\Programs\viper)

# `iex` runs this in the caller's session, so everything, including preferences, stays inside the
# function to leave that session unchanged.
function Install-Viper {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is far slower with the progress bar.

    $repo = 'stephenberry/viper'
    $target = 'x86_64-pc-windows-msvc'

    if (-not [Environment]::Is64BitOperatingSystem) {
        throw 'viper needs 64-bit Windows.'
    }
    # Older Windows PowerShell defaults can lack TLS 1.2, which GitHub requires.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $version = $env:VIPER_VERSION
    if (-not $version) {
        # The tag of the latest release, read from where /releases/latest redirects. Unlike the
        # GitHub API, this is not rate limited per IP address.
        $request = [Net.WebRequest]::Create("https://github.com/$repo/releases/latest")
        $request.Method = 'HEAD'
        $response = $request.GetResponse()
        try {
            $version = $response.ResponseUri.Segments[-1]
        }
        finally {
            $response.Close()
        }
        if (-not $version.StartsWith('v')) {
            throw "no viper release found at https://github.com/$repo/releases"
        }
    }
    if (-not $version.StartsWith('v')) {
        $version = "v$version"
    }
    $installDir = $env:VIPER_INSTALL_DIR
    if (-not $installDir) {
        $installDir = Join-Path $env:LOCALAPPDATA 'Programs\viper'
    }
    $name = "viper-$version-$target"
    $base = "https://github.com/$repo/releases/download/$version"

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading viper $version for $target"
        $zip = Join-Path $tmp "$name.zip"
        $sums = Join-Path $tmp 'SHA256SUMS'
        Invoke-WebRequest -UseBasicParsing "$base/$name.zip" -OutFile $zip
        Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS" -OutFile $sums

        $entry = Get-Content $sums | Where-Object { ($_ -split '\s+')[1] -eq "$name.zip" } | Select-Object -First 1
        if (-not $entry) {
            throw "SHA256SUMS has no entry for $name.zip"
        }
        $expected = ($entry -split '\s+')[0]
        $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash
        if ($actual -ne $expected) {
            throw "checksum mismatch for $name.zip"
        }

        Expand-Archive -Path $zip -DestinationPath $tmp
        New-Item -ItemType Directory -Force -Path $installDir | Out-Null
        $exe = Join-Path $installDir 'viper.exe'
        # A running viper.exe cannot be overwritten but can be renamed, so move it aside first.
        $old = "$exe.old"
        Remove-Item $old -Force -ErrorAction SilentlyContinue
        if (Test-Path $exe) {
            Move-Item $exe $old -Force
        }
        Copy-Item (Join-Path $tmp "$name\viper.exe") $exe
        Remove-Item $old -Force -ErrorAction SilentlyContinue

        Write-Host "Installed $(& $exe --version) to $exe"
    }
    finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @($userPath -split ';' | Where-Object { $_ })
    if ($entries -notcontains $installDir) {
        [Environment]::SetEnvironmentVariable('Path', (($entries + $installDir) -join ';'), 'User')
        $env:Path = "$env:Path;$installDir"
        Write-Host "Added $installDir to your user PATH. Open a new terminal to use viper there."
    }

    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        Write-Host ''
        Write-Host 'viper runs tools through bash. Install Git for Windows, which provides it: https://git-scm.com/download/win'
    }
}

Install-Viper
