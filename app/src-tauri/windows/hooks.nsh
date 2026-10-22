; Installer hooks for Tauri's NSIS template. Uninstalling the app from
; Settings > Apps runs signal-agent's own uninstall first, so the logon task,
; Claude entries and running agents leave with the app (PRD goal 11).
;
; NOTE: neither an update nor an upgrade may run it, or the profile would stop
; working. The in-app updater passes /UPDATE, which never runs the old
; uninstaller. A double-clicked newer installer does run it ("Uninstall before
; installing" is the default), so the installer marks its environment and the
; uninstaller it launches inherits the mark and skips the cleanup.

!define MUI_CUSTOMFUNCTION_GUIINIT SignalAgentMarkInstalling

Function SignalAgentMarkInstalling
  System::Call 'kernel32::SetEnvironmentVariable(t "SIGNAL_AGENT_INSTALLING", t "1") i'
FunctionEnd

!macro NSIS_HOOK_PREUNINSTALL
  ReadEnvStr $R9 SIGNAL_AGENT_INSTALLING
  ${If} $UpdateMode <> 1
  ${AndIf} $R9 != "1"
    ; Tauri's "Delete the application data" box is the user's keep-or-delete choice.
    ${If} $DeleteAppDataCheckboxState = 1
      nsExec::ExecToLog '"$INSTDIR\signal-agent.exe" uninstall --delete-data'
    ${Else}
      nsExec::ExecToLog '"$INSTDIR\signal-agent.exe" uninstall'
    ${EndIf}
    ; A failed step is in the log above; removing the app must still go ahead.
    Pop $R9
  ${EndIf}
!macroend

; A running agent locks signal-agent.exe, which an update must replace. End
; the supervisor's task (its job takes the agents with it) and any agent a
; Claude client started, then start the task again once the new files are in.
; On a first install there is nothing to stop or start, and both fail harmlessly.
!macro NSIS_HOOK_PREINSTALL
  nsExec::ExecToLog 'schtasks /End /TN signal-agent'
  Pop $R9
  nsExec::ExecToLog 'taskkill /F /IM signal-agent.exe'
  Pop $R9
!macroend

!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToLog 'schtasks /Run /TN signal-agent'
  Pop $R9
!macroend
