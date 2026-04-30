; Custom NSIS installer hooks for the Mush Stream Tauri app.
;
; Tauri's default NSIS template invokes these macros at well-defined
; lifecycle points (see https://tauri.app/distribute/windows-installer/
; — section "Hooks"). We use NSIS_HOOK_POSTINSTALL to chain ViGEmBus's
; own installer immediately after our app's files are placed, while
; the installer is still running with admin rights (we set
; `bundle.windows.nsis.installMode = perMachine`).
;
; ViGEmBus is required for the host's virtual-Xbox-360-pad path — the
; app starts but client gamepad forwarding silently no-ops if the
; driver is missing. Bundling its installer avoids the "ViGEmBus
; missing" sidebar status on a fresh machine.

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Installing ViGEmBus virtual gamepad driver..."
  ; The bundled installer lands in $INSTDIR thanks to the
  ; `bundle.resources` mapping in tauri.conf.json. We launch it
  ; interactively so the user sees what's happening and can cancel
  ; if they don't want the driver — declining doesn't fail the
  ; main install (the app gracefully reports "ViGEmBus missing" on
  ; the Home page until the user installs it later).
  ExecWait '"$INSTDIR\ViGEmBus_1.18.367_x64_x86.exe"' $0
  ${If} $0 != 0
    DetailPrint "ViGEmBus installer returned exit code $0; install it later from the project page if you want gamepad passthrough."
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Intentionally do NOT uninstall ViGEmBus — it's a system-wide
  ; driver other apps may rely on. Users who want it gone can use
  ; "Add or Remove Programs" -> "ViGEmBus".
!macroend
