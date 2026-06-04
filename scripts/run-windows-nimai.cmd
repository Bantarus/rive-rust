@echo off
setlocal EnableExtensions EnableDelayedExpansion

rem ===========================================================================
rem  run-windows-nimai.cmd - native Windows launcher for the Nimai face live
rem  viewer (the `nimai_face` floor-tier example in bevy-rive). The face's eyes
rem  and head follow the mouse cursor (pointer Listeners / a head joystick).
rem
rem  Why native Windows: WSL2 forces rive/wgpu onto Mesa Dozen (a non-conformant
rem  Vulkan->D3D12 layer) or llvmpipe, and the WSLg compositor is flaky. On the
rem  4090 box there is a real NVIDIA Vulkan ICD and a real desktop -- so the
rem  window actually paints AND there is a real OS cursor to track, which is the
rem  whole point of this example.
rem
rem  This sets up the VS x64 toolchain (clang-cl, MSBuild, fxc) + GNU make + Git
rem  Bash `sh` that rive-renderer-sys's build.rs needs (premake + the shader make
rem  step), then `cargo run --release` the example. Mirrors win.cmd /
rem  run-windows-voxelien.cmd / ../voxelith/scripts/run-windows-rive.cmd.
rem
rem  Prerequisite (from WSL2): push the tree to E: first so the example and the
rem  .riv assets are present:
rem    scripts/sync_to_windows.sh
rem
rem  Usage (Windows shell, repo at E:\DEV\rive-rust):
rem    scripts\run-windows-nimai.cmd                       (published/signed face)
rem    scripts\run-windows-nimai.cmd nimai_published.riv   (explicit file)
rem
rem  From WSL2 (opens a window on the Windows desktop):
rem    scripts/sync_to_windows.sh && cmd.exe /c "scripts\run-windows-nimai.cmd"
rem
rem  NOTE: the first run does a full native C++ rebuild of rive WITH scripting
rem  (premake clones luau + libhydrogen, clang-cl compiles them); subsequent runs
rem  are incremental. crt-static (/MT, to match rive's static CRT) comes from
rem  .cargo\config.toml, so RUSTFLAGS is intentionally NOT set here.
rem ===========================================================================

rem -- Which .riv to play: first arg, else the signed/published face. ----------
if "%~1"=="" (
    set "RIVE_RIV=nimai_published.riv"
) else (
    set "RIVE_RIV=%~1"
)

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" (
    echo [run-windows-nimai] ERROR: vswhere.exe not found at "%VSWHERE%".
    echo                      Install Visual Studio 2022 with "Desktop development with C++"
    echo                      and the "C++ Clang tools for Windows" component.
    exit /b 9009
)

rem -- Newest VS install carrying the x64 C++ toolset. ------------------------
set "VSINSTALL="
for /f "usebackq tokens=* delims=" %%i in (`
    "%VSWHERE%" -latest -products * ^
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 ^
        -property installationPath
`) do set "VSINSTALL=%%i"
if not defined VSINSTALL (
    echo [run-windows-nimai] ERROR: no Visual Studio install with the x64 C++ toolset found.
    echo                      In the VS Installer add "Desktop development with C++".
    exit /b 9009
)

set "VCVARS=%VSINSTALL%\VC\Auxiliary\Build\vcvars64.bat"
if not exist "%VCVARS%" (
    echo [run-windows-nimai] ERROR: vcvars64.bat not found at "%VCVARS%".
    exit /b 9009
)
call "%VCVARS%" >nul
if errorlevel 1 (
    echo [run-windows-nimai] ERROR: vcvars64.bat failed ^(exit %errorlevel%^).
    exit /b %errorlevel%
)

rem -- clang-cl is rive's toolset but is NOT added by vcvars64; prepend it. ----
if exist "%VSINSTALL%\VC\Tools\Llvm\x64\bin\clang-cl.exe" (
    set "PATH=%VSINSTALL%\VC\Tools\Llvm\x64\bin;%PATH%"
) else (
    echo [run-windows-nimai] WARNING: clang-cl.exe not found under the VS install. Install the
    echo                      "C++ Clang tools for Windows" component ^(rive needs clang-cl^).
)

rem -- GNU make (rive's shader step) + Git Bash sh (make's recipe shell).
rem    APPEND Git\usr\bin so MSVC link.exe is NOT shadowed by Git's coreutils link.
if exist "%ProgramData%\chocolatey\bin\make.exe" set "PATH=%PATH%;%ProgramData%\chocolatey\bin"
if exist "%ProgramFiles%\Git\usr\bin\sh.exe"     set "PATH=%PATH%;%ProgramFiles%\Git\usr\bin"

rem -- Vulkan SDK Bin (glslangValidator + spirv-opt) if present; harmless if unset
rem    (a synced tree carries prebuilt SPIR-V headers under out/).
if defined VULKAN_SDK set "PATH=%VULKAN_SDK%\Bin;%PATH%"

rem -- Floor tier: rive renders via its own native Vulkan; Bevy displays the
rem    sprite via wgpu's default Windows backend (D3D12), which is the most stable
rem    here. We deliberately do NOT force WGPU_BACKEND (that's only needed for the
rem    zero_copy tier).

rem -- Repo root = parent of this scripts\ dir (works from any invocation cwd).
pushd "%~dp0.." || (
    echo [run-windows-nimai] ERROR: could not cd to repo root from "%~dp0".
    exit /b 1
)
echo [run-windows-nimai] VS:    "%VSINSTALL%"
echo [run-windows-nimai] repo:  "%CD%"
echo [run-windows-nimai] RIVE_RIV=!RIVE_RIV!
echo [run-windows-nimai] cargo run --release -p bevy-rive --example nimai_face --features floor

cargo run --release -p bevy-rive --example nimai_face --features floor
set "CARGO_ERR=%ERRORLEVEL%"

popd
endlocal & exit /b %CARGO_ERR%
