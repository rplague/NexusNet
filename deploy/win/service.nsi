Unicode true
Name "@BIN@ P2P node"
OutFile "@OUTFILE@"
InstallDir "$PROGRAMFILES64\@PKG@"
InstallDirRegKey HKLM "Software\@PKG@" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
ShowInstDetails show
ShowUnInstDetails show

!define PKG "@PKG@"
!define BIN "@BIN@"
!define VERSION "@VERSION@"

!include "common.nsh"
!include "wizard.nsh"

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "打开配置目录"
!define MUI_FINISHPAGE_RUN_FUNCTION OpenDataDir

!insertmacro MUI_PAGE_WELCOME
Page custom ModePageCreate ModePageLeave
Page custom NodePageCreate NodePageLeave
Page custom NetworkPageCreate NetworkPageLeave
Page custom BootstrapPageCreate BootstrapPageLeave
Page custom ServicesPageCreate ServicesPageLeave
Page custom PingPageCreate PingPageLeave
Page custom DispatcherPageCreate DispatcherPageLeave
Page custom RelayPageCreate RelayPageLeave
Page custom SecurityPageCreate SecurityPageLeave
Page custom SummaryPageCreate SummaryPageLeave
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  SetRegView 64
  ReadEnvStr $DataDir "ProgramData"
  StrCpy $DataDir "$DataDir\${PKG}"
  InitPluginsDir
  Call InitDefaults
  IfFileExists "$DataDir\config.toml" 0 config_missing
    StrCpy $ConfigExists "1"
    StrCpy $CfgAction "keep"
    Goto config_ready
  config_missing:
    StrCpy $ConfigExists "0"
    StrCpy $CfgAction "wizard"
  config_ready:
  IfSilent silent_block silent_ready
  silent_block:
    ${GetOptions} $CMDLINE "/SETUPFILE=" $R1
    ${If} $R1 != ""
      StrCpy $CfgAction "wizard"
      StrCpy $SetupFile "$R1"
      StrCpy $DoBackup "1"
    ${ElseIf} $ConfigExists == "1"
      StrCpy $CfgAction "keep"
    ${Else}
      StrCpy $CfgAction "skip"
    ${EndIf}
  silent_ready:
FunctionEnd

Section "Install"
  SetRegView 64

  StrCpy $WasManual "0"
  nsExec::ExecToStack '"$INSTDIR\nssm.exe" get ${PKG} Start'
  Pop $0
  Pop $1
  StrCpy $R0 "$1"
  StrCpy $R1 "SERVICE_AUTO_START"
  Call Contains
  ${If} $R2 != "1"
    StrCpy $R0 "$1"
    StrCpy $R1 "SERVICE_"
    Call Contains
    ${If} $R2 == "1"
      StrCpy $WasManual "1"
    ${EndIf}
  ${EndIf}

  nsExec::ExecToLog '"$INSTDIR\nssm.exe" stop ${PKG}'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" remove ${PKG} confirm'
  Sleep 1000

  SetOutPath "$INSTDIR"
  File "@BIN_EXE@"
  File "/oname=nssm.exe" "@NSSM_EXE@"

  CreateDirectory "$DataDir"

  ${If} $CfgAction == "wizard"
    ${If} $SetupFile == ""
      Call WriteBasicJson
      Call WriteAdvancedJson
      StrCpy $R0 '"$INSTDIR\${BIN}.exe" --setup "$PLUGINSDIR\setup-basic.json" --advanced "$PLUGINSDIR\setup-advanced.json" --config-dir "$DataDir"'
    ${Else}
      StrCpy $R0 '"$INSTDIR\${BIN}.exe" --setup "$SetupFile" --config-dir "$DataDir"'
    ${EndIf}
    ${If} $DoBackup == "1"
      StrCpy $R0 "$R0 --backup"
    ${EndIf}
    nsExec::ExecToLog $R0
    Pop $0
    StrCmp $0 "0" setup_ok
      IfSilent +2 0
        MessageBox MB_ICONSTOP "配置写入失败，安装已中止。请检查安装日志。"
      SetErrorLevel 2
      Abort
    setup_ok:
  ${ElseIf} $CfgAction == "skip"
    nsExec::ExecToLog '"$INSTDIR\${BIN}.exe" --defaults --config-dir "$DataDir"'
    Pop $0
    StrCmp $0 "0" defaults_ok
      IfSilent +2 0
        MessageBox MB_ICONSTOP "默认配置写入失败，安装已中止。"
      SetErrorLevel 2
      Abort
    defaults_ok:
  ${Else}
    nsExec::ExecToStack '"$INSTDIR\${BIN}.exe" --check-config --config-dir "$DataDir"'
    Pop $0
    Pop $1
    StrCmp $0 "0" check_ok
      IfSilent +2 0
        MessageBox MB_ICONSTOP "现有 config.toml 无法在本版本使用：$1$\r$\n请重新运行安装器并选择“重新配置”，或手动修改后重试。"
      SetErrorLevel 2
      Abort
    check_ok:
  ${EndIf}

  nsExec::ExecToLog 'icacls "$DataDir" /grant "NT SERVICE\${PKG}:(OI)(CI)M" /T /C'

  ${If} $CfgAction == "wizard"
    nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="NexusNet (${PKG})"'
    ${If} $Firewall == "1"
      ${If} $Ipv4 == "1"
      ${OrIf} $Ipv6 == "1"
        nsExec::ExecToStack 'netsh advfirewall firewall add rule name="NexusNet (${PKG})" dir=in action=allow protocol=TCP localport=$Port profile=any'
        Pop $0
        Pop $1
        ${If} $0 != "0"
          IfSilent +2 0
            MessageBox MB_ICONEXCLAMATION "Windows 防火墙规则添加失败，请手动放行 TCP $Port。"
        ${EndIf}
      ${EndIf}
    ${EndIf}
  ${EndIf}

  nsExec::ExecToLog '"$INSTDIR\nssm.exe" install ${PKG} "$INSTDIR\${BIN}.exe"'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} DisplayName "${BIN} P2P node"'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} Description "NexusNet P2P node (OAHD core network layer)"'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} AppDirectory "$INSTDIR"'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} AppExit Default Restart'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} AppStopMethodConsole 15000'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} AppEnvironmentExtra "NEXUSNET_CONFIG_PATH=$DataDir" "NEXUSNET_KEYPAIR_PATH=$DataDir" "NEXUSNET_LOG_PATH=$DataDir" "NEXUSNET_LOG_MODE=nssm"'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} AppStdout "$DataDir\service.out.log"'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} AppStderr "$DataDir\service.err.log"'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} AppRotateFiles 1'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} AppRotateOnline 1'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} AppRotateBytes 10485760'

  ${If} $CfgAction == "skip"
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} Start SERVICE_DEMAND_START'
  ${ElseIf} $CfgAction == "keep"
    ${If} $WasManual == "1"
      nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} Start SERVICE_DEMAND_START'
    ${Else}
      nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} Start SERVICE_AUTO_START'
    ${EndIf}
  ${Else}
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set ${PKG} Start SERVICE_AUTO_START'
  ${EndIf}

  nsExec::ExecToLog 'sc.exe config ${PKG} obj= "NT SERVICE\${PKG}"'
  nsExec::ExecToLog 'sc.exe description ${PKG} "NexusNet P2P node (OAHD core network layer)"'

  WriteRegStr HKLM "Software\${PKG}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PKG}" "DisplayName" "${BIN} P2P node"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PKG}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PKG}" "Publisher" "OAHD"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PKG}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PKG}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PKG}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteUninstaller "$INSTDIR\uninstall.exe"

  StrCpy $StartNow "1"
  ${If} $CfgAction == "skip"
    StrCpy $StartNow "0"
  ${ElseIf} $CfgAction == "keep"
    ${If} $WasManual == "1"
      StrCpy $StartNow "0"
    ${EndIf}
  ${EndIf}

  ${If} $StartNow == "1"
    nsExec::ExecToLog 'sc.exe start ${PKG}'
    Sleep 1500
    nsExec::ExecToStack '"$INSTDIR\nssm.exe" status ${PKG}'
    Pop $0
    Pop $1
    StrCpy $R0 "$1"
    StrCpy $R1 "SERVICE_RUNNING"
    Call Contains
    ${If} $R2 != "1"
      IfSilent service_fail_silent
      MessageBox MB_ICONEXCLAMATION "服务未处于运行状态。请查看 $DataDir\service.err.log 和 $DataDir\log。"
      Goto service_fail_done
      service_fail_silent:
      SetErrorLevel 2
      service_fail_done:
    ${EndIf}
  ${Else}
    ${If} $CfgAction == "skip"
      IfSilent skip_message_done
      MessageBox MB_ICONINFORMATION "已跳过配置向导。默认配置已生成，服务未启动。$\r$\n请编辑 $DataDir\config.toml，然后执行 sc.exe start ${PKG}；需要开机自启时执行 sc.exe config ${PKG} start= auto。"
      skip_message_done:
    ${EndIf}
  ${EndIf}
SectionEnd

Section "Uninstall"
  SetRegView 64
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="NexusNet (${PKG})"'
  nsExec::ExecToLog 'sc.exe stop ${PKG}'
  Sleep 1000
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" remove ${PKG} confirm'
  Sleep 400
  Delete "$INSTDIR\${BIN}.exe"
  Delete "$INSTDIR\nssm.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PKG}"
  DeleteRegKey HKLM "Software\${PKG}"
SectionEnd
