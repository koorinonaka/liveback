; Liveback installer (task131).
;
; Replaces what tauri-bundler used to generate. Deliberately small: one exe, one
; Start Menu shortcut, one AUMID registration, one uninstaller. Per-user install.
; The directory, uninstall key and AUMID all moved with the task177 rename, so a
; machine that still carries the old install has to run its own uninstaller
; first -- this one cannot see, let alone replace, an entry under the old name.
;
; A running Liveback holds its own exe, so an update has to deal with that
; before it writes anything. It probes with a write open, warns and asks, kills
; only on consent, then polls until the handle is really gone -- and aborts
; without touching the install directory if it never is. What it must never do
; is reach `File` on a locked exe: NSIS falls back to Abort/Retry/Ignore there,
; and Ignore leaves an install that reports success with the old exe still in
; place. See the Install section for the measurements behind each step
; (task3001).
;
; The AUMID goes in the registry rather than onto the shortcut. Tauri's template
; set System.AppUserModel.ID on the .lnk, which needs the nsis_tauri_utils
; plugin; `HKCU\Software\Classes\AppUserModelId\<id>` with a DisplayName reaches
; the same place with no plugin at all -- verified by firing a toast under a
; throwaway id registered only that way. The icon on that toast comes from the
; same key's IconUri, which takes an image file and nothing else: pointed at the
; exe it is silently ignored and the toast shows an empty icon slot, because the
; notification platform never extracts an icon resource out of a binary
; (measured side by side under two throwaway ids, png vs exe).

; This file must keep its UTF-8 BOM: makensis otherwise reads it as ACP and
; the Japanese ProgID label below aborts the build with "Bad text encoding".
Unicode true
!include "MUI2.nsh"

!define PRODUCT "Liveback"
; CI は `makensis /DVERSION=<ver>` で上書きする (task t260920-cfc0)。
; 既定値は Cargo.toml の version と一致させること -- 出力名はここから決まる。
!ifndef VERSION
  !define VERSION "0.9.0"
!endif
!define PUBLISHER "liveback"
; Must match `APP_USER_MODEL_ID` in src/bin/liveback/desktop.rs and
; `BUNDLE_IDENTIFIER` in src/settings.rs -- toasts are dropped silently if the
; id the app sends under is not registered here.
!define AUMID "com.liveback.desktop"
!define EXENAME "liveback.exe"
; The toast icon -- see the AUMID note above for why it is a PNG and not the exe.
; Same picture build.rs embeds in the exe, and coloured enough to read on both
; the light and the dark toast surface.
!define ICONNAME "liveback-256.png"
; The ProgID behind the .lvb association (task590). Registered here rather than
; by the app at startup: a dev machine runs the debug exe constantly, and
; runtime registration would keep repointing the extension at target\debug.
!define PROGID "Liveback.Session"
; The thumbnail handler's class id and the shell's `IThumbnailProvider` IID
; (task710). The first must match `CLSID_LVB_THUMBNAIL` in
; crates/lvb-thumb/src/lib.rs; the second is the well-known key name Explorer
; looks under.
!define THUMBCLSID "{0D9EC9D9-F746-4C28-8662-3B546BB83CB9}"
!define THUMBIID "{e357fccd-a995-4576-b01f-234630154e96}"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Liveback"

Name "${PRODUCT}"
OutFile "out\${PRODUCT}_${VERSION}_x64-setup.exe"
; Per-user: the app writes to %LOCALAPPDATA% and %APPDATA% anyway, and this
; keeps the installer free of UAC prompts.
InstallDir "$LOCALAPPDATA\Liveback"
InstallDirRegKey HKCU "${UNINSTKEY}" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!define MUI_ICON "..\assets\app-icon\liveback-dark.ico"
!define MUI_UNICON "..\assets\app-icon\liveback-dark.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXENAME}"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
; 更新の自動確認 (task t260920-2be2)。初回起動でダイアログを出すのをやめ、ここで一度だけ
; 聞く -- インストーラは「これから何が起きるか」を読む場面であって、ゲームの直前に割り込む
; 起動時のダイアログとは立場が違う。
;
; `MUI_FINISHPAGE_SHOWREADME` を空文字で使うのは、完了ページに任意のチェックボックスを
; 1つ置くための定番。既定はチェック済み (`_NOTCHECKED` を定義しない)。チェックが外れた
; ときは関数が呼ばれないので、既定値 0 は Install セクションが先に書いてある。
!define MUI_FINISHPAGE_SHOWREADME ""
!define MUI_FINISHPAGE_SHOWREADME_TEXT "自動で最新版を確認する"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION EnableUpdateCheck
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "Japanese"

Section "Install"
  ; The window's close button hides to the tray rather than quitting, so a
  ; graceful WM_CLOSE would leave the exe locked -- hence `taskkill`. What the
  ; kill needs is a way to tell when it has actually landed (task3001).
  ;
  ; This used to be an unconditional `taskkill /F` followed by `Sleep 500`, and
  ; that fixed wait is what broke: `taskkill /F` only reports that the signal
  ; went out, and a Liveback process carries the ring buffer in its working set
  ; -- 12.7 GB on 2026-09-05 -- so tearing down that address space and dropping
  ; the file handles does not reliably finish inside 500 ms. When `File` then
  ; hit ERROR_SHARING_VIOLATION, NSIS put up its default Abort/Retry/Ignore box,
  ; and Ignore let the rest of the section run: on 2026-09-05 15:33 the install
  ; "completed" with lvb_thumb.dll and uninstall.exe new (15:28 / 15:33) while
  ; liveback.exe stayed at the 08-22 build. Registry keys and the shortcut were
  ; written as if it had worked.
  ;
  ; So: probe the real condition, poll instead of guessing, and never reach
  ; `File` unless the exe is known to be replaceable.
  ;
  ; The probe is a write open (`FileOpen ... a` = OPEN_ALWAYS, read+write),
  ; because that is exactly the access `File` needs and exactly what a mapped
  ; image denies. `Rename` would NOT work as a probe: Windows deliberately
  ; allows renaming a running exe (that is how self-updaters work), so a rename
  ; probe succeeds while the app is up, skips the warning and the kill, and
  ; leaves a `.old` that cannot be deleted. The rename below is only a rollback
  ; slot, taken after the probe has already proved nobody holds the file.
  IfFileExists "$INSTDIR\${EXENAME}" 0 fresh_install
    ClearErrors
    FileOpen $R0 "$INSTDIR\${EXENAME}" a
    IfErrors 0 unlocked
      MessageBox MB_YESNO|MB_ICONEXCLAMATION "Liveback が起動中です。$\r$\n録画中の場合、そのセッションは中断されます。$\r$\n終了してインストールを続行しますか?" IDYES kill_it
        ; Nothing has been touched yet -- the install directory is exactly as
        ; the user left it.
        Abort
      kill_it:
      nsExec::Exec 'taskkill /IM "${EXENAME}" /F'
      Pop $0
      StrCpy $R1 0
      poll:
        Sleep 500
        ClearErrors
        FileOpen $R0 "$INSTDIR\${EXENAME}" a
        IfErrors 0 unlocked
        IntOp $R1 $R1 + 1
        IntCmp $R1 20 give_up poll give_up
      give_up:
      MessageBox MB_OK|MB_ICONSTOP "Liveback を終了できませんでした。$\r$\n手動で終了してから再実行してください。"
      Abort
    unlocked:
    FileClose $R0
    ; Rollback slot, not a lock test. A stale `.old` from an earlier aborted run
    ; would make `Rename` fail (MoveFile refuses an existing destination), so
    ; clear it first. The image is provably unmapped here, so both calls stick.
    Delete "$INSTDIR\${EXENAME}.old"
    Rename "$INSTDIR\${EXENAME}" "$INSTDIR\${EXENAME}.old"
  fresh_install:

  SetOutPath "$INSTDIR"
  File "..\target\x86_64-pc-windows-msvc\release\${EXENAME}"
  File "..\target\x86_64-pc-windows-msvc\release\lvb_thumb.dll"
  File "..\assets\app-icon\${ICONNAME}"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  ; The rollback slot did its job -- the new exe is in place.
  Delete "$INSTDIR\${EXENAME}.old"

  CreateShortcut "$SMPROGRAMS\${PRODUCT}.lnk" "$INSTDIR\${EXENAME}"

  ; What makes OS toasts appear at all.
  ; 更新の自動確認の既定値 (task t260920-2be2)。完了ページのチェックが外れたままだと
  ; `EnableUpdateCheck` は呼ばれないので、オフをここで先に書いておき、チェックされた
  ; ときだけ 1 で上書きされる。アプリは初回起動で1回だけこれを読む。
  WriteRegDWORD HKCU "Software\Liveback" "UpdateCheck" 0

  WriteRegStr HKCU "Software\Classes\AppUserModelId\${AUMID}" "DisplayName" "${PRODUCT}"
  WriteRegStr HKCU "Software\Classes\AppUserModelId\${AUMID}" "IconUri" "$INSTDIR\${ICONNAME}"

  ; Double-clicking a recording opens it (task590). `%1` is quoted so a path
  ; with spaces arrives as one argument -- which is the only argument the app
  ; reads.
  WriteRegStr HKCU "Software\Classes\.lvb" "" "${PROGID}"
  WriteRegStr HKCU "Software\Classes\.lvb\OpenWithProgids" "${PROGID}" ""
  WriteRegStr HKCU "Software\Classes\${PROGID}" "" "Liveback セッション"
  ; A negative index is a resource ID, not a position, so this keeps pointing at
  ; the `.lvb` icon group (build.rs embeds it as ID 2) however the linker orders
  ; the resources. Icon group 1 stays the app icon (task890).
  WriteRegStr HKCU "Software\Classes\${PROGID}\DefaultIcon" "" "$INSTDIR\${EXENAME},-2"
  WriteRegStr HKCU "Software\Classes\${PROGID}\shell\open\command" "" '"$INSTDIR\${EXENAME}" "%1"'
  ; A recording shows the frame it opens on rather than the app icon (task710).
  ; No self-registration: the DLL exports no DllRegisterServer, these two keys
  ; are the whole registration, and they are per-user like everything else here.
  ; `SetRegView 64` is load-bearing: makensis builds a 32-bit installer, and
  ; WOW64 redirects a 32-bit process's writes under Software\Classes\CLSID into
  ; Software\Classes\Wow6432Node\CLSID -- where 64-bit Explorer never looks.
  ; Without this the handler installs and simply never runs. The ProgID's
  ; ShellEx key below is not a redirected path, so it stays in the default view.
  SetRegView 64
  WriteRegStr HKCU "Software\Classes\CLSID\${THUMBCLSID}" "" "Liveback thumbnail handler"
  WriteRegStr HKCU "Software\Classes\CLSID\${THUMBCLSID}\InprocServer32" "" "$INSTDIR\lvb_thumb.dll"
  WriteRegStr HKCU "Software\Classes\CLSID\${THUMBCLSID}\InprocServer32" "ThreadingModel" "Apartment"
  SetRegView 32
  WriteRegStr HKCU "Software\Classes\${PROGID}\ShellEx\${THUMBIID}" "" "${THUMBCLSID}"
  ; Without this Explorer can keep the old (or absent) association until the
  ; next logon. SHCNE_ASSOCCHANGED.
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, i 0, i 0)'

  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${PRODUCT}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\${EXENAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
SectionEnd

; 完了ページのチェックが付いたままなら呼ばれる。
Function EnableUpdateCheck
  WriteRegDWORD HKCU "Software\Liveback" "UpdateCheck" 1
FunctionEnd

Section "Uninstall"
  nsExec::Exec 'taskkill /IM "${EXENAME}" /F'
  Pop $0

  ; Same reason as the install side (task3001): `taskkill /F` is asynchronous
  ; and a big process takes longer than any fixed sleep to let go of its exe.
  ; Here the probe is the delete itself -- there is nothing to preserve -- and
  ; giving up has to abort, because an uninstall that reports success while
  ; liveback.exe is still sitting in the directory is the same half-done lie
  ; the install side used to tell.
  StrCpy $R1 0
  un_poll:
    Delete "$INSTDIR\${EXENAME}"
    IfFileExists "$INSTDIR\${EXENAME}" 0 un_gone
    IntOp $R1 $R1 + 1
    IntCmp $R1 20 un_give_up 0 un_give_up
    Sleep 500
    Goto un_poll
  un_give_up:
  MessageBox MB_OK|MB_ICONSTOP "Liveback を終了できませんでした。$\r$\n手動で終了してからアンインストールし直してください。"
  Abort
  un_gone:
  Delete "$INSTDIR\${EXENAME}.old"
  Delete "$INSTDIR\${ICONNAME}"
  ; The shell's thumbnail host (dllhost.exe) may still hold this one, and it
  ; cannot be killed as bluntly as the app: /REBOOTOK leaves it for the next
  ; boot rather than failing the uninstall.
  Delete /REBOOTOK "$INSTDIR\lvb_thumb.dll"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${PRODUCT}.lnk"

  ; 更新の自動確認の記録 (task t260920-2be2)。残しても害はないが、アンインストール後に
  ; 残る設定はこれ1つだけなので消しておく。
  DeleteRegKey HKCU "Software\Liveback"

  DeleteRegKey HKCU "Software\Classes\AppUserModelId\${AUMID}"
  DeleteRegKey HKCU "Software\Classes\${PROGID}"
  DeleteRegKey HKCU "Software\Classes\.lvb"
  ; Same view the installer wrote it in -- see the note next to SetRegView above.
  SetRegView 64
  DeleteRegKey HKCU "Software\Classes\CLSID\${THUMBCLSID}"
  SetRegView 32
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, i 0, i 0)'
  DeleteRegKey HKCU "${UNINSTKEY}"
  ; The login item, if the user ever turned it on.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCT}"

  ; Recordings (%LOCALAPPDATA%\Liveback\buffer) and settings
  ; (%APPDATA%\com.liveback.desktop) are left alone on purpose: uninstalling
  ; the app is not a request to throw away the user's captures.
SectionEnd
