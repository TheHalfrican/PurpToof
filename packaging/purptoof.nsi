; NSIS installer for PurpToof.
;
; Per-user by default, into %LOCALAPPDATA%\Programs. That is deliberate:
;
;   - No UAC prompt, so installing does not require an administrator.
;   - The app is per-user by nature. It advertises THIS PC as a speaker and
;     routes audio to the logged-in user's default output, and its autostart
;     entry is a per-user Run key. A machine-wide install would put the binary
;     somewhere shared while everything it does stays personal.
;   - It writes its config and logs to %APPDATA%\PurpToof either way, so a
;     Program Files install would gain nothing and only add an elevation step.
;
; Built with:  makensis packaging\purptoof.nsi

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"

!define APP        "PurpToof"
!define PUBLISHER  "TheHalfrican"
; Overridable from the command line, so scripts/package.ps1 can pass the
; version out of Cargo.toml rather than this file carrying a second copy that
; drifts.
!ifndef VERSION
  !define VERSION "0.1.0"
!endif
!define EXE        "purptoof.exe"
!define REGKEY     "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP}"

Name "${APP} ${VERSION}"
OutFile "..\dist\PurpToof-${VERSION}-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\${APP}"
InstallDirRegKey HKCU "Software\${APP}" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!define MUI_ICON   "..\assets\purptoof.ico"
!define MUI_UNICON "..\assets\purptoof.ico"
!define MUI_ABORTWARNING

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Start ${APP}"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "Install"
  ; Stop a running copy first. Without this the install silently fails to
  ; replace a locked exe, and the user ends up running the old build while the
  ; installer reports success.
  nsExec::Exec 'taskkill /IM ${EXE} /F'
  Pop $0

  SetOutPath "$INSTDIR"
  File "..\target\release\${EXE}"
  File "..\assets\purptoof.ico"
  File "..\LICENSE"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\${APP}" "InstallDir" "$INSTDIR"

  CreateDirectory "$SMPROGRAMS\${APP}"
  CreateShortcut "$SMPROGRAMS\${APP}\${APP}.lnk" "$INSTDIR\${EXE}" "" "$INSTDIR\purptoof.ico"

  ; Add/Remove Programs. EstimatedSize is in KB and is computed rather than
  ; hardcoded, so it stays honest as the binary changes.
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  WriteRegStr   HKCU "${REGKEY}" "DisplayName"     "${APP}"
  WriteRegStr   HKCU "${REGKEY}" "DisplayVersion"  "${VERSION}"
  WriteRegStr   HKCU "${REGKEY}" "Publisher"       "${PUBLISHER}"
  WriteRegStr   HKCU "${REGKEY}" "DisplayIcon"     "$INSTDIR\purptoof.ico"
  WriteRegStr   HKCU "${REGKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr   HKCU "${REGKEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegDWORD HKCU "${REGKEY}" "EstimatedSize"   "$0"
  WriteRegDWORD HKCU "${REGKEY}" "NoModify"        1
  WriteRegDWORD HKCU "${REGKEY}" "NoRepair"        1
SectionEnd

Section "Uninstall"
  nsExec::Exec 'taskkill /IM ${EXE} /F'
  Pop $0

  ; The app registers its own autostart entry. Leaving it behind would point
  ; at a deleted exe and fail silently at every login.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${APP}"

  Delete "$INSTDIR\${EXE}"
  Delete "$INSTDIR\purptoof.ico"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\${APP}\${APP}.lnk"
  RMDir "$SMPROGRAMS\${APP}"

  DeleteRegKey HKCU "${REGKEY}"
  DeleteRegKey HKCU "Software\${APP}"

  ; Settings and logs in %APPDATA%\PurpToof are deliberately left alone. A
  ; reinstall should not lose someone's configuration, and the logs are the
  ; record of what the app did - which is exactly what you want to keep after
  ; uninstalling something that was misbehaving.
SectionEnd
