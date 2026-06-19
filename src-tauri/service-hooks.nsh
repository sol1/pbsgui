; NSIS hooks: install/start and stop/remove the pbsgui-engine background service
; around app install/uninstall. The per-machine installer runs elevated, so it
; can register the service.
;
; The engine sidecar may be installed with or without the target-triple suffix,
; so we try both names; the one that does not exist simply no-ops.

!macro NSIS_HOOK_POSTINSTALL
  nsExec::Exec '"$INSTDIR\pbsgui-engine.exe" service install'
  nsExec::Exec '"$INSTDIR\pbsgui-engine-x86_64-pc-windows-msvc.exe" service install'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec '"$INSTDIR\pbsgui-engine.exe" service uninstall'
  nsExec::Exec '"$INSTDIR\pbsgui-engine-x86_64-pc-windows-msvc.exe" service uninstall'
!macroend
