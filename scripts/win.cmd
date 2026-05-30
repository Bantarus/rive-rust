@echo off
setlocal EnableExtensions EnableDelayedExpansion

rem ===========================================================================
rem  win.cmd - native Windows build relay for rive-rust.
rem
rem  Sets up the x64 Visual Studio dev environment (clang-cl, MSBuild, fxc) plus
rem  GNU make, Git Bash `sh`, python and premake on PATH, then forwards ALL args
rem  to cargo. This is how rive-runtime expects to be built on Windows (its
rem  shader step shells out to make), with rive's default clang-cl toolset.
rem
rem  Usage (Windows shell, repo at E:\DEV\rive-rust):
rem    scripts\win.cmd run --release --example offscreen_png -- assets\coffee_loader.riv out.png
rem    scripts\win.cmd build --release
rem
rem  From WSL2:
rem    cmd.exe /c "scripts\win.cmd run --release --example offscreen_png -- assets\coffee_loader.riv out.png"
rem ===========================================================================

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" (
    echo [win.cmd] ERROR: vswhere.exe not found at "%VSWHERE%".
    echo           Install Visual Studio 2022 with "Desktop development with C++"
    echo           and the "C++ Clang tools for Windows" component.
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
    echo [win.cmd] ERROR: no Visual Studio install with the x64 C++ toolset found.
    echo           In the VS Installer add "Desktop development with C++".
    exit /b 9009
)

set "VCVARS=%VSINSTALL%\VC\Auxiliary\Build\vcvars64.bat"
if not exist "%VCVARS%" (
    echo [win.cmd] ERROR: vcvars64.bat not found at "%VCVARS%".
    exit /b 9009
)
call "%VCVARS%" >nul
if errorlevel 1 (
    echo [win.cmd] ERROR: vcvars64.bat failed ^(exit %errorlevel%^).
    exit /b %errorlevel%
)

rem -- clang-cl is rive's toolset but is NOT added by vcvars64; prepend it. ----
if exist "%VSINSTALL%\VC\Tools\Llvm\x64\bin\clang-cl.exe" (
    set "PATH=%VSINSTALL%\VC\Tools\Llvm\x64\bin;%PATH%"
) else (
    echo [win.cmd] WARNING: clang-cl.exe not found under the VS install. Install the
    echo           "C++ Clang tools for Windows" component ^(rive needs clang-cl^).
)

rem -- GNU make (rive's shader step) + Git Bash sh (make's recipe shell).
rem    APPEND Git\usr\bin so MSVC link.exe is NOT shadowed by Git's coreutils link.
if exist "%ProgramData%\chocolatey\bin\make.exe" set "PATH=%PATH%;%ProgramData%\chocolatey\bin"
if exist "%ProgramFiles%\Git\usr\bin\sh.exe"     set "PATH=%PATH%;%ProgramFiles%\Git\usr\bin"

rem -- Vulkan SDK (hermetic / CI path): if installed it sets VULKAN_SDK; expose its
rem    Bin so rive's shader step can run glslangValidator + spirv-opt to generate
rem    SPIR-V from a clean checkout. Harmless if unset (a tree with prebuilt SPIR-V
rem    headers doesn't need it). See BUILD.md §1b.
if defined VULKAN_SDK set "PATH=%VULKAN_SDK%\Bin;%PATH%"

rem -- Forward-looking; no effect under M0/M1.0 (the shim self-manages Vulkan,
rem    no wgpu in the graph yet). Set so M1a/M1b inherit it. --------------------
set "WGPU_BACKEND=vulkan"

rem -- Repo root = parent of this scripts\ dir (works from any invocation cwd).
pushd "%~dp0.." || (
    echo [win.cmd] ERROR: could not cd to repo root from "%~dp0".
    exit /b 1
)
echo [win.cmd] VS:   "%VSINSTALL%"
echo [win.cmd] repo: "%CD%"
echo [win.cmd] cargo %*

cargo %*
set "CARGO_ERR=%ERRORLEVEL%"

popd
endlocal & exit /b %CARGO_ERR%
