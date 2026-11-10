Function ModePageCreate
  ${If} $SkipModePage == "1"
    Abort
  ${EndIf}
  ${If} $CfgAction == "skip"
    Abort
  ${EndIf}
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${If} $ConfigExists == "1"
    ${NSD_CreateLabel} 10 10 280 28 "检测到现有配置。使用向导会备份旧 config.toml，并从默认值开始重新配置；跳过向导会保留现有配置。"
    Pop $0
    ${NSD_CreateRadioButton} 10 45 270 14 "使用配置向导（推荐）"
    Pop $hModeWizard
    ${NSD_CreateRadioButton} 10 65 270 14 "跳过向导（保留现有配置）"
    Pop $hModeSkip
    ${NSD_SetState} $hModeSkip ${BST_CHECKED}
  ${Else}
    ${NSD_CreateLabel} 10 10 280 28 "首次安装。向导会设置节点信息、监听端口、本地服务、鉴权网络和 bootstrap；未列出的配置使用默认值。"
    Pop $0
    ${NSD_CreateRadioButton} 10 45 270 14 "使用配置向导（推荐）"
    Pop $hModeWizard
    ${NSD_CreateRadioButton} 10 65 270 14 "跳过向导（使用默认配置，手动启动）"
    Pop $hModeSkip
    ${NSD_SetState} $hModeWizard ${BST_CHECKED}
  ${EndIf}
  nsDialogs::Show
FunctionEnd

Function ModePageLeave
  ${NSD_GetState} $hModeSkip $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $CfgAction "skip"
    StrCpy $DoBackup "0"
  ${Else}
    StrCpy $CfgAction "wizard"
    ${If} $ConfigExists == "1"
      StrCpy $DoBackup "1"
    ${Else}
      StrCpy $DoBackup "0"
    ${EndIf}
  ${EndIf}
FunctionEnd

Function NodePageCreate
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${NSD_CreateLabel} 10 10 60 14 "节点名称"
  Pop $0
  ${NSD_CreateText} 75 8 210 14 "$NodeName"
  Pop $hName
  ${NSD_CreateLabel} 10 32 60 14 "描述"
  Pop $0
  ${NSD_CreateText} 75 30 210 14 "$NodeDescription"
  Pop $hDescription
  ${NSD_CreateLabel} 10 58 270 28 "网络地址族由程序启动时自动探测；防火墙规则会在安装时按端口自动添加。"
  Pop $0
  nsDialogs::Show
FunctionEnd

Function NodePageLeave
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  ${NSD_GetText} $hName $NodeName
  ${NSD_GetText} $hDescription $NodeDescription
  ${If} $NodeName == ""
    StrCpy $NodeName "NexusNet"
  ${EndIf}
  ${If} $NodeDescription == ""
    StrCpy $NodeDescription "NexusNet P2P node"
  ${EndIf}
FunctionEnd

Function NetworkPageCreate
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${NSD_CreateLabel} 10 10 70 14 "监听端口"
  Pop $0
  ${NSD_CreateText} 120 8 70 14 "$Port"
  Pop $hPort
  StrCpy $BackPressed "0"
  GetFunctionAddress $0 NetworkPageBack
  nsDialogs::OnBack $0
  nsDialogs::Show
FunctionEnd

Function NetworkPageBack
  StrCpy $BackPressed "1"
FunctionEnd

Function NetworkPageLeave
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  ${If} $BackPressed == "1"
    StrCpy $BackPressed "0"
    Return
  ${EndIf}
  ${NSD_GetText} $hPort $Port
  StrCpy $R0 "$Port"
  StrCpy $R1 "1"
  StrCpy $R2 "65535"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "端口必须是 1-65535 的数字。"
    Abort
  ${EndIf}
FunctionEnd

Function RefreshServiceList
  SendMessage $hServicesList ${LB_RESETCONTENT} 0 0
  FileOpen $0 "$ServicesFile" r
  IfErrors RefreshServiceList_done
  RefreshServiceList_loop:
    ClearErrors
    FileRead $0 $1
    IfErrors RefreshServiceList_close
    StrCmp $1 "" RefreshServiceList_close
    ${StrTrimNewLines} $1 $1
    StrCpy $R0 "$1"
    Call ServiceParse
    StrCmp $SvcName "" RefreshServiceList_next
    StrCpy $R3 "$SvcName:$SvcPort "
    ${If} $SvcPublish == "0"
      StrCpy $R3 "$R3[仅本地]"
    ${Else}
      StrCpy $R3 "$R3[已发布]"
    ${EndIf}
    ${If} $SvcFlag == "1"
      StrCpy $R3 "$R3[鉴权]"
    ${EndIf}
    ${NSD_LB_AddString} $hServicesList "$R3"
  RefreshServiceList_next:
    Goto RefreshServiceList_loop
  RefreshServiceList_close:
    FileClose $0
  RefreshServiceList_done:
  SendMessage $hServicesList ${LB_SETCURSEL} 0 0
FunctionEnd

Function ServicePromptDialog
  FileOpen $0 "$IoIni" w
  FileWriteByte $0 255
  FileWriteByte $0 254
  FileWriteUTF16LE $0 "[Settings]$\r$\n"
  FileWriteUTF16LE $0 "NumFields=6$\r$\n"
  FileWriteUTF16LE $0 "Title=$SvcPromptTitle$\r$\n"
  FileWriteUTF16LE $0 "NextButtonText=确定$\r$\n"
  FileWriteUTF16LE $0 "CancelButtonText=取消$\r$\n$\r$\n"
  FileWriteUTF16LE $0 "[Field 1]$\r$\n"
  FileWriteUTF16LE $0 "Type=Label$\r$\n"
  FileWriteUTF16LE $0 "Text=服务名称$\r$\n"
  FileWriteUTF16LE $0 "Left=10$\r$\n"
  FileWriteUTF16LE $0 "Right=-10$\r$\n"
  FileWriteUTF16LE $0 "Top=10$\r$\n"
  FileWriteUTF16LE $0 "Bottom=24$\r$\n$\r$\n"
  FileWriteUTF16LE $0 "[Field 2]$\r$\n"
  FileWriteUTF16LE $0 "Type=Text$\r$\n"
  FileWriteUTF16LE $0 "State=$SvcName$\r$\n"
  FileWriteUTF16LE $0 "Left=10$\r$\n"
  FileWriteUTF16LE $0 "Right=-10$\r$\n"
  FileWriteUTF16LE $0 "Top=26$\r$\n"
  FileWriteUTF16LE $0 "Bottom=40$\r$\n$\r$\n"
  FileWriteUTF16LE $0 "[Field 3]$\r$\n"
  FileWriteUTF16LE $0 "Type=Label$\r$\n"
  FileWriteUTF16LE $0 "Text=TCP 端口$\r$\n"
  FileWriteUTF16LE $0 "Left=10$\r$\n"
  FileWriteUTF16LE $0 "Right=-10$\r$\n"
  FileWriteUTF16LE $0 "Top=50$\r$\n"
  FileWriteUTF16LE $0 "Bottom=64$\r$\n$\r$\n"
  FileWriteUTF16LE $0 "[Field 4]$\r$\n"
  FileWriteUTF16LE $0 "Type=Text$\r$\n"
  FileWriteUTF16LE $0 "State=$SvcPort$\r$\n"
  FileWriteUTF16LE $0 "Left=10$\r$\n"
  FileWriteUTF16LE $0 "Right=90$\r$\n"
  FileWriteUTF16LE $0 "Top=66$\r$\n"
  FileWriteUTF16LE $0 "Bottom=80$\r$\n$\r$\n"
  FileWriteUTF16LE $0 "[Field 5]$\r$\n"
  FileWriteUTF16LE $0 "Type=CheckBox$\r$\n"
  FileWriteUTF16LE $0 "Text=发布到网络$\r$\n"
  FileWriteUTF16LE $0 "State=$SvcPublish$\r$\n"
  FileWriteUTF16LE $0 "Left=10$\r$\n"
  FileWriteUTF16LE $0 "Right=-10$\r$\n"
  FileWriteUTF16LE $0 "Top=90$\r$\n"
  FileWriteUTF16LE $0 "Bottom=104$\r$\n$\r$\n"
  FileWriteUTF16LE $0 "[Field 6]$\r$\n"
  FileWriteUTF16LE $0 "Type=CheckBox$\r$\n"
  FileWriteUTF16LE $0 "Text=需要鉴权$\r$\n"
  FileWriteUTF16LE $0 "State=$SvcFlag$\r$\n"
  FileWriteUTF16LE $0 "Left=10$\r$\n"
  FileWriteUTF16LE $0 "Right=-10$\r$\n"
  FileWriteUTF16LE $0 "Top=110$\r$\n"
  FileWriteUTF16LE $0 "Bottom=124$\r$\n"
  FileClose $0

  InstallOptions::dialog "$IoIni"
  Pop $0
  StrCmp $0 "success" 0 ServicePromptDialog_cancel
  ReadINIStr $1 "$IoIni" "Field 2" "State"
  ReadINIStr $2 "$IoIni" "Field 4" "State"
  ReadINIStr $3 "$IoIni" "Field 5" "State"
  ReadINIStr $4 "$IoIni" "Field 6" "State"
  ${StrTrimNewLines} $1 $1
  ${StrTrimNewLines} $2 $2
  StrCpy $SvcName "$1"
  StrCpy $SvcPort "$2"
  StrCpy $SvcPublish "$3"
  StrCpy $SvcFlag "$4"
  StrCmp $SvcPublish "1" +2
    StrCpy $SvcPublish "0"
  StrCmp $SvcFlag "1" +2
    StrCpy $SvcFlag "0"
  StrCpy $PromptOk "1"
  Return
  ServicePromptDialog_cancel:
    StrCpy $PromptOk "0"
FunctionEnd

Function PromptService
  PromptService_loop:
    Call ServicePromptDialog
    StrCmp $PromptOk "1" 0 PromptService_cancel
    StrCpy $PromptSvcName "$SvcName"
    StrCpy $PromptSvcPort "$SvcPort"
    StrCpy $PromptSvcFlag "$SvcFlag"
    StrCpy $PromptSvcPublish "$SvcPublish"
    StrCpy $R0 "$PromptSvcName"
    Call IsValidServiceName
    ${If} $R1 != "1"
      MessageBox MB_ICONEXCLAMATION "服务名称无效：不能为空，只允许 A-Z a-z 0-9 _ -，最长 64 字符，且不能是 service。"
      Goto PromptService_loop
    ${EndIf}
    StrCpy $R0 "$PromptSvcPort"
    StrCpy $R1 "1"
    StrCpy $R2 "65535"
    Call ValidateInt
    ${If} $R3 != "1"
      MessageBox MB_ICONEXCLAMATION "服务端口必须是 1-65535 的数字。"
      Goto PromptService_loop
    ${EndIf}
    StrCpy $R0 "$PromptSvcName"
    StrCpy $R1 "$SvcOldName"
    Call ServicesHasName
    ${If} $R2 == "1"
      MessageBox MB_ICONEXCLAMATION "服务名称 $PromptSvcName 已存在，请使用其它名称。"
      Goto PromptService_loop
    ${EndIf}
    StrCpy $SvcName "$PromptSvcName"
    StrCpy $SvcPort "$PromptSvcPort"
    StrCpy $SvcFlag "$PromptSvcFlag"
    StrCpy $SvcPublish "$PromptSvcPublish"
    StrCpy $PromptOk "1"
    Return
  PromptService_cancel:
    StrCpy $PromptOk "0"
FunctionEnd

Function ServiceAddClick
  StrCpy $SvcOldName ""
  StrCpy $SvcName ""
  StrCpy $SvcPort "7000"
  StrCpy $SvcFlag "0"
  StrCpy $SvcPublish "1"
  StrCpy $SvcPromptTitle "添加本地服务"
  Call PromptService
  StrCmp $PromptOk "1" 0 ServiceAddClick_done
  Call ServicesAdd
  Call RefreshServiceList
  ServiceAddClick_done:
FunctionEnd

Function ServiceEditClick
  SendMessage $hServicesList ${LB_GETCURSEL} 0 0
  Pop $R0
  StrCmp $R0 "-1" 0 ServiceEditClick_get
    MessageBox MB_ICONINFORMATION "请先选择要编辑的服务。"
    Return
  ServiceEditClick_get:
    Call ServicesGetByIndex
    StrCmp $SvcName "" ServiceEditClick_done
    StrCpy $SvcOldName "$SvcName"
    StrCpy $SvcPromptTitle "编辑本地服务"
    Call PromptService
    StrCmp $PromptOk "1" 0 ServiceEditClick_done
    StrCpy $SvcNewFlag "$SvcFlag"
    StrCpy $SvcNewPublish "$SvcPublish"
    StrCpy $R0 "$SvcOldName"
    Call ServicesReplace
    Call RefreshServiceList
  ServiceEditClick_done:
FunctionEnd

Function ServiceDeleteClick
  SendMessage $hServicesList ${LB_GETCURSEL} 0 0
  Pop $R0
  StrCmp $R0 "-1" 0 ServiceDeleteClick_get
    MessageBox MB_ICONINFORMATION "请先选择要删除的服务。"
    Return
  ServiceDeleteClick_get:
    Call ServicesGetByIndex
    StrCmp $SvcName "" ServiceDeleteClick_done
    MessageBox MB_YESNO|MB_ICONQUESTION "确定删除本地服务 $SvcName？" IDYES +2
      Return
    StrCpy $R0 "$SvcName"
    Call ServicesRemove
    Call RefreshServiceList
  ServiceDeleteClick_done:
FunctionEnd

Function ServicesPageCreate
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${NSD_CreateLabel} 10 10 270 24 "管理本地服务。"
  Pop $0
  ${NSD_CreateListBox} 10 38 270 150 ""
  Pop $hServicesList
  ${NSD_CreateButton} 10 196 80 16 "添加"
  Pop $0
  ${NSD_OnClick} $0 ServiceAddClick
  ${NSD_CreateButton} 100 196 80 16 "编辑"
  Pop $0
  ${NSD_OnClick} $0 ServiceEditClick
  ${NSD_CreateButton} 190 196 80 16 "删除"
  Pop $0
  ${NSD_OnClick} $0 ServiceDeleteClick
  Call RefreshServiceList
  nsDialogs::Show
FunctionEnd

Function ServicesPageLeave
  IfFileExists "$ServicesFile" 0 ServicesPageLeave_done
  Call ServicesCount
  ${If} $SvcCount == 0
    StrCpy $ServicesEnabled "0"
  ${Else}
    StrCpy $ServicesEnabled "1"
  ${EndIf}
  ServicesPageLeave_done:
FunctionEnd

Function AuthPageCreate
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${NSD_CreateCheckBox} 10 10 270 14 "启用鉴权网络"
  Pop $hAuthEnabled
  ${If} $AuthEnabled == "1"
    ${NSD_SetState} $hAuthEnabled ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateLabel} 10 40 80 14 "网络名称"
  Pop $0
  ${NSD_CreateText} 120 38 160 14 "$AuthNetwork"
  Pop $hAuthNetwork
  ${NSD_CreateLabel} 10 64 80 14 "Authority 公钥"
  Pop $0
  ${NSD_CreateText} 120 62 160 14 "$AuthAuthority"
  Pop $hAuthAuthority
  StrCpy $BackPressed "0"
  GetFunctionAddress $0 AuthPageBack
  nsDialogs::OnBack $0
  nsDialogs::Show
FunctionEnd

Function AuthPageBack
  StrCpy $BackPressed "1"
FunctionEnd

Function AuthPageLeave
  ${If} $BackPressed == "1"
    StrCpy $BackPressed "0"
    Return
  ${EndIf}
  ${NSD_GetState} $hAuthEnabled $R0
  ${If} $R0 != ${BST_CHECKED}
    StrCpy $AuthEnabled "0"
    StrCpy $AuthNetwork "none"
    StrCpy $AuthAuthority ""
    Return
  ${EndIf}
  ${NSD_GetText} $hAuthNetwork $AuthNetwork
  ${NSD_GetText} $hAuthAuthority $AuthAuthority
  ${If} $AuthNetwork == ""
    MessageBox MB_ICONEXCLAMATION "启用鉴权网络时网络名称不能为空。"
    Abort
  ${EndIf}
  StrCpy $R0 "$AuthNetwork"
  Call IsValidName
  ${If} $R1 != "1"
    MessageBox MB_ICONEXCLAMATION "鉴权网络名称无效：只允许 A-Z a-z 0-9 _ -，最长 64 字符。"
    Abort
  ${EndIf}
  ${If} $AuthAuthority == ""
    MessageBox MB_ICONEXCLAMATION "启用鉴权网络时 Authority 公钥不能为空。"
    Abort
  ${EndIf}
  StrCpy $AuthEnabled "1"
FunctionEnd

Function BootstrapPageCreate
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${NSD_CreateLabel} 10 10 270 28 "每行一个 bootstrap multiaddr，也可用逗号分隔；必须包含 /ws 和 /p2p/<PeerId>。留空表示不配置。"
  Pop $0
  ${NSD_CreateMLText} 10 42 270 120 "$Bootstrap"
  Pop $hBootstrap
  ${NSD_CreateLabel} 10 168 270 28 "例如 /ip4/167.99.153.118/tcp/5000/ws/p2p/12D3KooW..."
  Pop $0
  StrCpy $BackPressed "0"
  GetFunctionAddress $0 BootstrapPageBack
  nsDialogs::OnBack $0
  nsDialogs::Show
FunctionEnd

Function BootstrapPageBack
  StrCpy $BackPressed "1"
FunctionEnd

Function BootstrapPageLeave
  ${If} $BackPressed == "1"
    StrCpy $BackPressed "0"
    Return
  ${EndIf}
  ${NSD_GetText} $hBootstrap $Bootstrap
  Call ValidateBootstrap
  ${If} $BootResult != "1"
    MessageBox MB_ICONEXCLAMATION "$BootError"
    Abort
  ${EndIf}
  StrCpy $Bootstrap "$BootNormalized"
FunctionEnd

Function SummaryPageCreate
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  Call ServicesSummary
  ${If} $AuthEnabled == "1"
    StrCpy $R1 "$AuthNetwork"
  ${Else}
    StrCpy $R1 "未启用"
  ${EndIf}
  ${If} $Bootstrap == ""
    StrCpy $R2 "未设置"
  ${Else}
    StrCpy $R2 "$Bootstrap"
  ${EndIf}
  StrCpy $R0 "节点名称：$NodeName$\r$\n"
  StrCpy $R0 "$R0节点描述：$NodeDescription$\r$\n"
  StrCpy $R0 "$R0监听端口：$Port$\r$\n"
  StrCpy $R0 "$R0本地服务：$SvcSummary$\r$\n"
  StrCpy $R0 "$R0鉴权网络：$R1$\r$\n"
  StrCpy $R0 "$R0Bootstrap：$R2$\r$\n$\r$\n"
  StrCpy $R0 "$R0未在向导中列出的配置将恢复默认值。"
  ${NSD_CreateMLText} 10 10 270 210 "$R0"
  Pop $hSummary
  nsDialogs::Show
FunctionEnd

Function SummaryPageLeave
FunctionEnd
