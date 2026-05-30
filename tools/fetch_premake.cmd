@echo off
setlocal EnableExtensions
rem ===========================================================================
rem  fetch_premake.cmd - vendor premake5.exe (Windows) into tools\.
rem
rem  rive-runtime needs premake 5.0.0-beta7 on Windows: earlier betas (e.g.
rem  beta2, used for the Linux build) mistranslate rive's fatalwarnings({'All'})
rem  into the bogus MSVC flag /weAll for the vs2022 generator. (Linux uses the
rem  ELF tools\premake5 via tools/fetch_premake.sh.)
rem ===========================================================================
set "TAG=v5.0.0-beta7"
set "ZIP=premake-5.0.0-beta7-windows.zip"
set "URL=https://github.com/premake/premake-core/releases/download/%TAG%/%ZIP%"
set "DEST=%~dp0"

echo Downloading %URL% ...
curl -fL -o "%TEMP%\%ZIP%" "%URL%" || ( echo ERROR: download failed & exit /b 1 )

rem Windows 10/11 ships bsdtar (handles .zip); fall back to PowerShell.
tar -xf "%TEMP%\%ZIP%" -C "%DEST%" premake5.exe 2>nul
if not exist "%DEST%premake5.exe" (
    powershell -NoProfile -Command "Expand-Archive -Force '%TEMP%\%ZIP%' '%TEMP%\pm-beta7'; Copy-Item -Force '%TEMP%\pm-beta7\premake5.exe' '%DEST%premake5.exe'" ^
        || ( echo ERROR: extraction failed & exit /b 1 )
)
del "%TEMP%\%ZIP%" 2>nul

echo Installed "%DEST%premake5.exe":
"%DEST%premake5.exe" --version
endlocal
