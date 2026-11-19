@echo off
cd /d "%~dp0"
rem A second launch only raises the window the first one already owns and exits
rem (single instance, src/bin/liveback.rs). Left alone, the exe just built
rem would never be the one on screen -- so the old instance goes first.
taskkill /f /im liveback.exe >nul 2>&1
rem Everything before `--` is cargo's, everything after is the app's. `%*` used
rem to sit alone on this line, which meant an app argument could never be
rem passed at all: cargo rejects flags it does not know before the exe runs
rem (task205). It stays on cargo's side so `run.bat --release` still works.
rem
rem `--features insight` + `--insight` is what makes a dev launch write
rem %LOCALAPPDATA%\com.liveback.desktop\logs\insight.<epoch>.log. The installer
rem builds without the feature, so none of that reaches a shipped exe.
cargo run --features insight %* -- --insight
if errorlevel 1 pause
