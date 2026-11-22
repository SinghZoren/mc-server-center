; Installer hooks (Tauri NSIS).
;
; Every running Minecraft server is supervised by an msmd.exe "runner" process, so msmd.exe is
; usually in use during an update. Windows lets a running program be renamed but not replaced,
; so move the old one aside; the new app then asks the old service to step down and starts the
; new one, which re-attaches to the running servers (see daemon_link.rs). Leftover copies from
; earlier updates are deleted once nothing uses them.

!macro NSIS_HOOK_PREINSTALL
  Delete "$INSTDIR\msmd-previous-*.exe"
  IfFileExists "$INSTDIR\msmd.exe" 0 msm_no_old_service
    System::Call "kernel32::GetTickCount() i .r9"
    Rename "$INSTDIR\msmd.exe" "$INSTDIR\msmd-previous-$9.exe"
  msm_no_old_service:
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Uninstalling leaves your servers, worlds and backups in %LOCALAPPDATA%\MinecraftServerManager.
  Delete "$INSTDIR\msmd-previous-*.exe"
!macroend
