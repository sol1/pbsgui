; NSIS hooks: install/start and stop/remove the pbsgui-engine background service
; around app install/uninstall. The per-machine installer runs elevated, so it
; can register the service.
;
; The engine sidecar may be installed with or without the target-triple suffix,
; so we try both names; the one that does not exist simply no-ops.

!macro NSIS_HOOK_PREINSTALL
  ; Upgrade safety: stop and remove any existing service BEFORE files are written,
  ; so the running engine releases its locked exe and there is no stale service.
  ; `sc` is used (not the bundled exe) so this works regardless of the installed
  ; version. The config in %ProgramData%\pbsgui is untouched and so is preserved.
  nsExec::Exec 'sc stop pbsgui-engine'
  Sleep 3000
  nsExec::Exec 'sc delete pbsgui-engine'
  Sleep 1000
!macroend

!macro NSIS_HOOK_POSTINSTALL
  nsExec::Exec '"$INSTDIR\pbsgui-engine.exe" service install'
  nsExec::Exec '"$INSTDIR\pbsgui-engine-x86_64-pc-windows-msvc.exe" service install'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec '"$INSTDIR\pbsgui-engine.exe" service uninstall'
  nsExec::Exec '"$INSTDIR\pbsgui-engine-x86_64-pc-windows-msvc.exe" service uninstall'
!macroend
