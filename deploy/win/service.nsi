; NexusNet Windows 安装器（NSIS）
;
; 由 build-win.sh 通过 sed 渲染占位符后交给 makensis 编译，占位符为：
;   PKG       规范化包名（小写，'_' -> '-'），服务名 / 目录名
;   BIN       二进制名（Cargo.toml 的 name）
;   VERSION   Cargo.toml 版本
;   BIN_EXE   交叉编译产物 <bin>.exe 的绝对路径
;   NSSM_EXE  nssm.exe 的绝对路径
;   OUTFILE   setup.exe 输出绝对路径
;
; 服务以虚拟账号 NT SERVICE\<包名> 运行，配置与日志位于 %ProgramData%\<包名>。

Unicode true
Name "@BIN@ P2P node"
OutFile "@OUTFILE@"
InstallDir "$PROGRAMFILES64\@PKG@"
InstallDirRegKey HKLM "Software\@PKG@" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
ShowInstDetails show
ShowUnInstDetails show

!include "MUI2.nsh"

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "启动 @PKG@ 服务"
!define MUI_FINISHPAGE_RUN_FUNCTION StartService

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

Var DataDir

Function StartService
  nsExec::ExecToLog 'sc.exe start @PKG@'
FunctionEnd

Function .onInit
  SetRegView 64
  StrCpy $DataDir "$COMMONAPPDATA\@PKG@"
FunctionEnd

Section "Install"
  SetRegView 64

  ; 升级：若已有服务，先用旧 nssm.exe 停删，避免占用而无法覆盖文件
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" stop @PKG@'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" remove @PKG@ confirm'
  Sleep 1000

  SetOutPath "$INSTDIR"
  File "@BIN_EXE@"
  File "/oname=nssm.exe" "@NSSM_EXE@"

  ; 数据目录与虚拟账号写权限
  CreateDirectory "$DataDir"
  CreateDirectory "$DataDir\log"
  nsExec::ExecToLog 'icacls "$DataDir" /grant "NT SERVICE\@PKG@:(OI)(CI)M" /T /C'

  ; 注册服务（NSSM 包装）
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" install @PKG@ "$INSTDIR\@BIN@.exe"'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set @PKG@ DisplayName "@BIN@ P2P node"'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set @PKG@ Description "NexusNet P2P node (OAHD core network layer)"'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set @PKG@ AppDirectory "$INSTDIR"'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set @PKG@ Start SERVICE_AUTO_START'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set @PKG@ AppExit Default Restart'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set @PKG@ AppStopMethodConsole 15000'
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" set @PKG@ AppEnvironmentExtra "NEXUSNET_HOME=$DataDir" "NEXUSNET_CONFIG=$DataDir\config.toml" "NEXUSNET_LOG_PATH=$DataDir"'

  ; 虚拟服务账号（无需密码）
  nsExec::ExecToLog 'sc.exe config @PKG@ obj= "NT SERVICE\@PKG@" start= auto'
  nsExec::ExecToLog 'sc.exe description @PKG@ "NexusNet P2P node (OAHD core network layer)"'

  ; 记录安装目录与卸载信息
  WriteRegStr HKLM "Software\@PKG@" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\@PKG@" "DisplayName" "@BIN@ P2P node"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\@PKG@" "DisplayVersion" "@VERSION@"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\@PKG@" "Publisher" "OAHD"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\@PKG@" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\@PKG@" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\@PKG@" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteUninstaller "$INSTDIR\uninstall.exe"

  nsExec::ExecToLog 'sc.exe start @PKG@'
SectionEnd

Section "Uninstall"
  SetRegView 64

  nsExec::ExecToLog 'sc.exe stop @PKG@'
  Sleep 1000
  nsExec::ExecToLog '"$INSTDIR\nssm.exe" remove @PKG@ confirm'
  Sleep 400

  Delete "$INSTDIR\@BIN@.exe"
  Delete "$INSTDIR\nssm.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\@PKG@"
  DeleteRegKey HKLM "Software\@PKG@"
SectionEnd
