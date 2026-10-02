; Windows installer for the Algo Trading desktop app.
; Produces a per-user (no admin / no UAC) NSIS setup that installs the desktop
; shell (with the manual updater), the algo-server binary and the static UI,
; then offers to launch the app.
;
; Build with: makensis -DSTAGE=<staging-dir> installer-windows.nsi

Unicode True
!include "MUI2.nsh"

!ifndef STAGE
  !define STAGE "..\..\dist-windows"
!endif
!ifndef OUTDIR
  !define OUTDIR "."
!endif

!define APP_NAME "Algo Trading"
!define APP_EXE "algo-desktop.exe"
!define PUBLISHER "Sarat Upadhyay"
!define VERSION "0.1.14"
!define REGKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\AlgoTrading"

Name "${APP_NAME}"
OutFile "${OUTDIR}\AlgoTradingSetup.exe"
InstallDir "$LOCALAPPDATA\AlgoTrading"
InstallDirRegKey HKCU "${REGKEY}" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "Launch ${APP_NAME}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "Install"
  SetOutPath "$INSTDIR"
  File "${STAGE}\algo-desktop.exe"
  File "${STAGE}\algo-server.exe"

  SetOutPath "$INSTDIR\static"
  File /r "${STAGE}\static\*.*"

  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_EXE}"
  CreateShortCut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_EXE}"

  WriteRegStr HKCU "${REGKEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${REGKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${REGKEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${REGKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${REGKEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegDWORD HKCU "${REGKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${REGKEY}" "NoRepair" 1
  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\algo-desktop.exe"
  Delete "$INSTDIR\algo-server.exe"
  RMDir /r "$INSTDIR\static"
  Delete "$INSTDIR\uninstall.exe"

  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"
  Delete "$DESKTOP\${APP_NAME}.lnk"

  DeleteRegKey HKCU "${REGKEY}"
  RMDir "$INSTDIR"
SectionEnd
