; Installer hooks for Rustify.
;
; Rustify's player (rustifyd.exe) is a separate process that deliberately
; outlives the window — that is the whole point of the design, since it keeps
; music playing while the window is closed. The consequence is that it is
; almost always running during an update, and Windows will not let the
; installer overwrite a running executable.
;
; Tauri's generated installer stops the main application, but knows nothing
; about the daemon, so without this every update failed to replace
; rustifyd.exe and left the app unable to reach a player.

!macro NSIS_HOOK_PREINSTALL
  DetailPrint "Stopping the Rustify player..."
  ; /F because the daemon has no window to close politely; it holds no
  ; unsaved state, so terminating it is safe. Failure is fine: it usually
  ; just means it was not running.
  nsExec::Exec 'taskkill /F /IM rustifyd.exe'
  Pop $0
  Sleep 800
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Stopping the Rustify player..."
  nsExec::Exec 'taskkill /F /IM rustifyd.exe'
  Pop $0
  Sleep 800
!macroend
