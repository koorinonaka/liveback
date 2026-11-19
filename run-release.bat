@echo off
cd /d "%~dp0"
rem The everyday launch: a release build, run from a copy outside target\
rem (task t260913-5937). run.bat stays the debug dev loop -- its build is the
rem fast one, and a release compile on every iteration would cost minutes.
rem
rem Why a copy: target\...\release\liveback.exe is overwritten by any other
rem release build in this checkout (task3590 hit that on 2026-09-10), and the
rem login item registers whatever exe was running when autoStart was switched
rem on (desktop::sync_auto_start writes current_exe()). The copy's path never
rem changes, so a registration made from it survives every rebuild.
rem
rem --features insight + --insight: same as run.bat, so this launch writes
rem %LOCALAPPDATA%\com.liveback.desktop\logs\insight.<epoch>.log with
rem profile=release in its header. A login-item launch passes no arguments,
rem so it runs the same exe without the insight log.
rem
rem Build first, stop the old instance second: the app is down only for the
rem copy, not for the compile. Like run.bat this stops EVERY liveback.exe --
rem do not run it while a recording you want to keep is in flight.
cargo build --release --features insight
if errorlevel 1 (
    pause
    exit /b 1
)
taskkill /f /im liveback.exe >nul 2>&1
if not exist local-release mkdir local-release
rem A killed process can hold its image for a moment, so the copy retries.
set LIVEBACK_COPY_TRIES=0
:copy
copy /y target\x86_64-pc-windows-msvc\release\liveback.exe local-release\liveback.exe >nul 2>&1
if not errorlevel 1 goto copied
set /a LIVEBACK_COPY_TRIES+=1
if %LIVEBACK_COPY_TRIES% geq 10 (
    echo could not replace local-release\liveback.exe
    pause
    exit /b 1
)
ping -n 2 127.0.0.1 >nul
goto copy
:copied
start "" "%~dp0local-release\liveback.exe" --insight
