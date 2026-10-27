!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "StrFunc.nsh"
!include "InstallOptions.nsh"
!include "WinMessages.nsh"

${Using:StrFunc} StrRep
${Using:StrFunc} StrTrimNewLines

Var DataDir
Var ConfigExists
Var CfgAction
Var SetupFile
Var DoBackup
Var WasManual
Var StartNow

Var NodeName
Var NodeDescription
Var Port
Var Bootstrap
Var AuthNetwork
Var AuthAuthority
Var ServicesEnabled
Var AuthEnabled
Var AuthDisplay

Var ServicesFile
Var IoIni
Var SvcName
Var SvcPort
Var SvcFlag
Var SvcOldName
Var SvcCount
Var SvcSummary
Var SvcIndex
Var SvcPromptTitle
Var PromptOk
Var NameCheck
Var BackPressed

Var BootToken
Var BootNormalized
Var BootResult
Var BootError
Var BootIndex
Var BootChar
Var ArrSource
Var ArrIndex
Var ArrToken
Var ArrCount
Var ArrChar

Var Escaped

Var Dialog
Var hModeWizard
Var hModeSkip
Var hName
Var hDescription
Var hPort
Var hServicesEnabled
Var hAuthEnabled
Var hServicesList
Var hAuthList
Var hAuthNetwork
Var hAuthAuthority
Var hBootstrap
Var hSummary

Function InitDefaults
  StrCpy $NodeName "NexusNet"
  StrCpy $NodeDescription "NexusNet P2P node"
  StrCpy $Port "5000"
  StrCpy $Bootstrap ""
  StrCpy $AuthNetwork "none"
  StrCpy $AuthAuthority ""
  StrCpy $ServicesEnabled "0"
  StrCpy $AuthEnabled "0"

  StrCpy $ConfigExists "0"
  StrCpy $CfgAction "wizard"
  StrCpy $SetupFile ""
  StrCpy $DoBackup "0"
  StrCpy $WasManual "0"
  StrCpy $StartNow "1"
  StrCpy $BackPressed "0"

  StrCpy $ServicesFile "$PLUGINSDIR\services.txt"
  StrCpy $IoIni "$PLUGINSDIR\service-prompt.ini"
  Call ServicesInit
FunctionEnd

Function Contains
  StrLen $R2 $R0
  StrLen $R3 $R1
  StrCpy $R4 0
  StrCmp $R3 0 Contains_found
  Contains_loop:
    IntCmp $R4 $R2 Contains_not_found
    IntOp $R5 $R4 + $R3
    IntCmp $R5 $R2 0 0 Contains_not_found
    StrCpy $R6 $R0 $R3 $R4
    StrCmpS $R6 $R1 Contains_found
    IntOp $R4 $R4 + 1
    Goto Contains_loop
  Contains_found:
    StrCpy $R2 "1"
    Return
  Contains_not_found:
    StrCpy $R2 "0"
FunctionEnd

Function ValidateInt
  StrCmp $R0 "" ValidateInt_bad
  StrCpy $R4 "0"
  StrLen $R5 "$R0"
  ValidateInt_loop:
    StrCmp $R4 $R5 ValidateInt_parse
    StrCpy $R6 $R0 1 $R4
    StrCmp $R6 "0" ValidateInt_next
    StrCmp $R6 "1" ValidateInt_next
    StrCmp $R6 "2" ValidateInt_next
    StrCmp $R6 "3" ValidateInt_next
    StrCmp $R6 "4" ValidateInt_next
    StrCmp $R6 "5" ValidateInt_next
    StrCmp $R6 "6" ValidateInt_next
    StrCmp $R6 "7" ValidateInt_next
    StrCmp $R6 "8" ValidateInt_next
    StrCmp $R6 "9" ValidateInt_next
    Goto ValidateInt_bad
  ValidateInt_next:
    IntOp $R4 $R4 + 1
    Goto ValidateInt_loop
  ValidateInt_parse:
    IntOp $R3 $R0 + 0
    IntCmp $R3 $R1 0 ValidateInt_bad 0
    IntCmp $R3 $R2 0 0 ValidateInt_bad
    StrCpy $R3 "1"
    Return
  ValidateInt_bad:
    StrCpy $R3 "0"
FunctionEnd

Function TrimSpaces
  StrCpy $R1 "$R0"
  TrimSpaces_lead:
    StrCpy $0 $R1 1
    StrCmp $0 " " 0 TrimSpaces_trail
    StrCpy $R1 $R1 "" 1
    Goto TrimSpaces_lead
  TrimSpaces_trail:
    StrLen $0 $R1
    StrCmp $0 0 TrimSpaces_done
    IntOp $0 $0 - 1
    StrCpy $2 $R1 1 $0
    StrCmp $2 " " 0 TrimSpaces_done
    StrCpy $R1 $R1 $0
    Goto TrimSpaces_trail
  TrimSpaces_done:
FunctionEnd

Function JsonEscape
  ${StrRep} $R1 "$R0" "\" "\\"
  ${StrRep} $R1 "$R1" '"' '\"'
  ${StrRep} $R1 "$R1" "$\r" " "
  ${StrRep} $R1 "$R1" "$\n" " "
  ${StrRep} $R1 "$R1" "$\t" " "
FunctionEnd

Function IsValidName
  StrCpy $NameCheck "$R0"
  StrCpy $R1 "0"
  StrCmp $NameCheck "" IsValidName_done
  StrLen $0 $NameCheck
  ${If} $0 > 64
    Goto IsValidName_done
  ${EndIf}
  StrCpy $5 "0"
  IsValidName_loop:
    StrCpy $2 $NameCheck 1 $5
    StrCmp $2 "" IsValidName_ok
    StrCpy $R0 "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-"
    StrCpy $R1 "$2"
    Call Contains
    StrCmp $R2 "1" IsValidName_next
    StrCpy $R1 "0"
    Return
  IsValidName_next:
    IntOp $5 $5 + 1
    Goto IsValidName_loop
  IsValidName_ok:
    StrCpy $R1 "1"
  IsValidName_done:
FunctionEnd

Function IsValidServiceName
  Call IsValidName
  StrCmp $R1 "1" 0 IsValidServiceName_done
  StrCmp $NameCheck "service" 0 IsValidServiceName_done
  StrCpy $R1 "0"
  IsValidServiceName_done:
FunctionEnd

Function ServicesInit
  FileOpen $0 "$ServicesFile" w
  FileClose $0
FunctionEnd

Function ServicesCount
  StrCpy $SvcCount "0"
  IfFileExists "$ServicesFile" 0 ServicesCount_done
  FileOpen $0 "$ServicesFile" r
  IfErrors ServicesCount_done
  ServicesCount_loop:
    ClearErrors
    FileRead $0 $1
    IfErrors ServicesCount_close
    StrCmp $1 "" ServicesCount_close
    IntOp $SvcCount $SvcCount + 1
    Goto ServicesCount_loop
  ServicesCount_close:
    FileClose $0
  ServicesCount_done:
FunctionEnd

Function ServiceParse
  StrCpy $SvcName ""
  StrCpy $SvcPort ""
  StrCpy $SvcFlag "0"
  StrCpy $7 "0"
  StrCpy $9 ""
  ServiceParse_name:
    StrCpy $8 $R0 1 $7
    StrCmp $8 "" ServiceParse_done
    StrCmp $8 ":" ServiceParse_name_done
    StrCpy $9 "$9$8"
    IntOp $7 $7 + 1
    Goto ServiceParse_name
  ServiceParse_name_done:
    StrCpy $SvcName "$9"
    IntOp $7 $7 + 1
    StrCpy $9 ""
  ServiceParse_port:
    StrCpy $8 $R0 1 $7
    StrCmp $8 "" ServiceParse_port_done
    StrCmp $8 ":" ServiceParse_port_done
    StrCpy $9 "$9$8"
    IntOp $7 $7 + 1
    Goto ServiceParse_port
  ServiceParse_port_done:
    StrCpy $SvcPort "$9"
    StrCmp $8 ":" 0 ServiceParse_done
    IntOp $7 $7 + 1
    StrCpy $SvcFlag "$R0" "" $7
    StrCmp $SvcFlag "" 0 ServiceParse_done
    StrCpy $SvcFlag "0"
  ServiceParse_done:
FunctionEnd

Function ServicesGetByIndex
  StrCpy $SvcName ""
  StrCpy $SvcPort ""
  StrCpy $SvcFlag "0"
  StrCpy $SvcIndex "0"
  StrCpy $3 "$R0"
  IfFileExists "$ServicesFile" 0 ServicesGetByIndex_done
  FileOpen $0 "$ServicesFile" r
  IfErrors ServicesGetByIndex_done
  ServicesGetByIndex_loop:
    ClearErrors
    FileRead $0 $1
    IfErrors ServicesGetByIndex_close
    StrCmp $1 "" ServicesGetByIndex_close
    ${StrTrimNewLines} $1 $1
    ${If} $SvcIndex == $3
      StrCpy $R0 "$1"
      Call ServiceParse
      Goto ServicesGetByIndex_close
    ${EndIf}
    IntOp $SvcIndex $SvcIndex + 1
    Goto ServicesGetByIndex_loop
  ServicesGetByIndex_close:
    FileClose $0
  ServicesGetByIndex_done:
FunctionEnd

Function ServicesHasName
  StrCpy $R2 "0"
  StrCpy $3 "$R0"
  StrCpy $4 "$R1"
  IfFileExists "$ServicesFile" 0 ServicesHasName_done
  FileOpen $0 "$ServicesFile" r
  IfErrors ServicesHasName_done
  ServicesHasName_loop:
    ClearErrors
    FileRead $0 $1
    IfErrors ServicesHasName_close
    StrCmp $1 "" ServicesHasName_close
    ${StrTrimNewLines} $1 $1
    StrCpy $R0 "$1"
    Call ServiceParse
    StrCmp $SvcName "$3" 0 ServicesHasName_next
    StrCmp "$3" "$4" ServicesHasName_next ServicesHasName_found
  ServicesHasName_next:
    Goto ServicesHasName_loop
  ServicesHasName_found:
    StrCpy $R2 "1"
  ServicesHasName_close:
    FileClose $0
  ServicesHasName_done:
FunctionEnd

Function ServicesAdd
  FileOpen $0 "$ServicesFile" a
  IfErrors ServicesAdd_done
  FileWrite $0 "$SvcName:$SvcPort:$SvcFlag$\r$\n"
  FileClose $0
  ServicesAdd_done:
FunctionEnd

Function ServicesRemove
  StrCpy $R3 "$R0"
  FileOpen $0 "$ServicesFile" r
  IfErrors ServicesRemove_done
  FileOpen $1 "$ServicesFile.tmp" w
  IfErrors ServicesRemove_abort
  ServicesRemove_loop:
    ClearErrors
    FileRead $0 $R4
    IfErrors ServicesRemove_close
    StrCmp $R4 "" ServicesRemove_close
    ${StrTrimNewLines} $R4 $R4
    StrCpy $R0 "$R4"
    Call ServiceParse
    StrCmp $SvcName "$R3" ServicesRemove_next
    FileWrite $1 "$R4$\r$\n"
  ServicesRemove_next:
    Goto ServicesRemove_loop
  ServicesRemove_close:
    FileClose $1
    FileClose $0
    Delete "$ServicesFile"
    Rename "$ServicesFile.tmp" "$ServicesFile"
    Return
  ServicesRemove_abort:
    FileClose $0
    Delete "$ServicesFile.tmp"
  ServicesRemove_done:
FunctionEnd

Function ServicesReplace
  StrCpy $R3 "$R0"
  StrCpy $5 "$SvcName"
  StrCpy $6 "$SvcPort"
  FileOpen $0 "$ServicesFile" r
  IfErrors ServicesReplace_done
  FileOpen $1 "$ServicesFile.tmp" w
  IfErrors ServicesReplace_abort
  ServicesReplace_loop:
    ClearErrors
    FileRead $0 $R4
    IfErrors ServicesReplace_close
    StrCmp $R4 "" ServicesReplace_close
    ${StrTrimNewLines} $R4 $R4
    StrCpy $R0 "$R4"
    Call ServiceParse
    StrCmp $SvcName "$R3" 0 ServicesReplace_keep
    FileWrite $1 "$5:$6:$SvcFlag$\r$\n"
    Goto ServicesReplace_next
  ServicesReplace_keep:
    FileWrite $1 "$R4$\r$\n"
  ServicesReplace_next:
    Goto ServicesReplace_loop
  ServicesReplace_close:
    FileClose $1
    FileClose $0
    Delete "$ServicesFile"
    Rename "$ServicesFile.tmp" "$ServicesFile"
    Return
  ServicesReplace_abort:
    FileClose $0
    Delete "$ServicesFile.tmp"
  ServicesReplace_done:
FunctionEnd

Function ServicesSetFlag
  StrCpy $R3 "$R0"
  StrCpy $5 "$R1"
  FileOpen $0 "$ServicesFile" r
  IfErrors ServicesSetFlag_done
  FileOpen $1 "$ServicesFile.tmp" w
  IfErrors ServicesSetFlag_abort
  ServicesSetFlag_loop:
    ClearErrors
    FileRead $0 $R4
    IfErrors ServicesSetFlag_close
    StrCmp $R4 "" ServicesSetFlag_close
    ${StrTrimNewLines} $R4 $R4
    StrCpy $R0 "$R4"
    Call ServiceParse
    StrCmp $SvcName "$R3" 0 ServicesSetFlag_keep
    FileWrite $1 "$SvcName:$SvcPort:$5$\r$\n"
    Goto ServicesSetFlag_next
  ServicesSetFlag_keep:
    FileWrite $1 "$R4$\r$\n"
  ServicesSetFlag_next:
    Goto ServicesSetFlag_loop
  ServicesSetFlag_close:
    FileClose $1
    FileClose $0
    Delete "$ServicesFile"
    Rename "$ServicesFile.tmp" "$ServicesFile"
    Return
  ServicesSetFlag_abort:
    FileClose $0
    Delete "$ServicesFile.tmp"
  ServicesSetFlag_done:
FunctionEnd

Function ServicesToggleAuth
  Call ServicesGetByIndex
  StrCmp $SvcName "" ServicesToggleAuth_done
  StrCmp $SvcFlag "1" 0 ServicesToggleAuth_on
  StrCpy $SvcFlag "0"
  Goto ServicesToggleAuth_apply
  ServicesToggleAuth_on:
    StrCpy $SvcFlag "1"
  ServicesToggleAuth_apply:
    StrCpy $R0 "$SvcName"
    StrCpy $R1 "$SvcFlag"
    Call ServicesSetFlag
  ServicesToggleAuth_done:
FunctionEnd

Function ServicesClearAuth
  IfFileExists "$ServicesFile" 0 ServicesClearAuth_done
  FileOpen $0 "$ServicesFile" r
  IfErrors ServicesClearAuth_done
  FileOpen $1 "$ServicesFile.tmp" w
  IfErrors ServicesClearAuth_abort
  ServicesClearAuth_loop:
    ClearErrors
    FileRead $0 $R4
    IfErrors ServicesClearAuth_close
    StrCmp $R4 "" ServicesClearAuth_close
    ${StrTrimNewLines} $R4 $R4
    StrCpy $R0 "$R4"
    Call ServiceParse
    StrCmp $SvcName "" ServicesClearAuth_next
    FileWrite $1 "$SvcName:$SvcPort:0$\r$\n"
  ServicesClearAuth_next:
    Goto ServicesClearAuth_loop
  ServicesClearAuth_close:
    FileClose $1
    FileClose $0
    Delete "$ServicesFile"
    Rename "$ServicesFile.tmp" "$ServicesFile"
    Return
  ServicesClearAuth_abort:
    FileClose $0
    Delete "$ServicesFile.tmp"
  ServicesClearAuth_done:
FunctionEnd

Function ServicesSummary
  Call ServicesCount
  ${If} $SvcCount == 0
    StrCpy $SvcSummary "无"
    Return
  ${EndIf}
  StrCpy $SvcSummary ""
  FileOpen $0 "$ServicesFile" r
  IfErrors ServicesSummary_done
  ServicesSummary_loop:
    ClearErrors
    FileRead $0 $1
    IfErrors ServicesSummary_close
    StrCmp $1 "" ServicesSummary_close
    ${StrTrimNewLines} $1 $1
    StrCpy $R0 "$1"
    Call ServiceParse
    StrCmp $SvcName "" ServicesSummary_next
    ${If} $SvcSummary == ""
      StrCpy $SvcSummary "$SvcName:$SvcPort"
    ${Else}
      StrCpy $SvcSummary "$SvcSummary, $SvcName:$SvcPort"
    ${EndIf}
    ${If} $SvcFlag == "1"
      StrCpy $SvcSummary "$SvcSummary[鉴权]"
    ${EndIf}
  ServicesSummary_next:
    Goto ServicesSummary_loop
  ServicesSummary_close:
    FileClose $0
  ServicesSummary_done:
FunctionEnd

Function BootstrapFlushToken
  StrCmp $BootResult "1" 0 BootstrapFlushToken_done
  StrCpy $R0 "$BootToken"
  Call TrimSpaces
  StrCpy $BootToken "$R1"
  StrCmp $BootToken "" BootstrapFlushToken_done
  StrCpy $R0 "$BootToken"
  StrCpy $R1 "/ws"
  Call Contains
  ${If} $R2 != "1"
    StrCpy $BootResult "0"
    StrCpy $BootError "bootstrap 必须包含 /ws：$BootToken"
    Goto BootstrapFlushToken_done
  ${EndIf}
  StrCpy $R0 "$BootToken"
  StrCpy $R1 "/p2p/"
  Call Contains
  ${If} $R2 != "1"
    StrCpy $BootResult "0"
    StrCpy $BootError "bootstrap 必须包含 /p2p/<PeerId>：$BootToken"
    Goto BootstrapFlushToken_done
  ${EndIf}
  ${If} $BootNormalized == ""
    StrCpy $BootNormalized "$BootToken"
  ${Else}
    StrCpy $BootNormalized "$BootNormalized$\r$\n$BootToken"
  ${EndIf}
  BootstrapFlushToken_done:
    StrCpy $BootToken ""
FunctionEnd

Function ValidateBootstrap
  StrCpy $BootResult "1"
  StrCpy $BootError ""
  StrCpy $BootNormalized ""
  StrCpy $BootToken ""
  StrCpy $BootIndex "0"
  ValidateBootstrap_loop:
    StrCpy $BootChar "$Bootstrap" 1 "$BootIndex"
    StrCmp $BootChar "" ValidateBootstrap_end
    StrCmp $BootChar "$\r" ValidateBootstrap_next
    StrCmp $BootChar "$\n" ValidateBootstrap_flush
    StrCmp $BootChar "," ValidateBootstrap_flush
    StrCpy $BootToken "$BootToken$BootChar"
    Goto ValidateBootstrap_next
  ValidateBootstrap_flush:
    Call BootstrapFlushToken
    StrCmp $BootResult "1" 0 ValidateBootstrap_end
  ValidateBootstrap_next:
    IntOp $BootIndex $BootIndex + 1
    Goto ValidateBootstrap_loop
  ValidateBootstrap_end:
    StrCmp $BootResult "1" 0 ValidateBootstrap_done
    Call BootstrapFlushToken
  ValidateBootstrap_done:
FunctionEnd

Function WriteStringArray
  StrCpy $ArrSource "$R0"
  StrCpy $ArrIndex "0"
  StrCpy $ArrToken ""
  StrCpy $ArrCount "0"
  FileWriteUTF16LE $R9 "["
  WriteStringArray_loop:
    StrCpy $ArrChar "$ArrSource" 1 "$ArrIndex"
    StrCmp $ArrChar "" WriteStringArray_flush
    StrCmp $ArrChar "$\r" WriteStringArray_next
    StrCmp $ArrChar "$\n" WriteStringArray_flush
    StrCpy $ArrToken "$ArrToken$ArrChar"
    Goto WriteStringArray_next
  WriteStringArray_flush:
    StrCmp $ArrToken "" WriteStringArray_flush_done
    StrCmp $ArrCount "0" +2
      FileWriteUTF16LE $R9 ","
    StrCpy $R0 "$ArrToken"
    Call JsonEscape
    FileWriteUTF16LE $R9 '"$R1"'
    IntOp $ArrCount $ArrCount + 1
    StrCpy $ArrToken ""
  WriteStringArray_flush_done:
    StrCmp $ArrChar "" WriteStringArray_close
  WriteStringArray_next:
    IntOp $ArrIndex $ArrIndex + 1
    Goto WriteStringArray_loop
  WriteStringArray_close:
    FileWriteUTF16LE $R9 "]"
FunctionEnd

Function WriteServicesJson
  StrCpy $2 "1"
  FileWriteUTF16LE $R9 "["
  IfFileExists "$ServicesFile" 0 WriteServicesJson_done
  FileOpen $0 "$ServicesFile" r
  IfErrors WriteServicesJson_done
  WriteServicesJson_loop:
    ClearErrors
    FileRead $0 $1
    IfErrors WriteServicesJson_close
    StrCmp $1 "" WriteServicesJson_close
    ${StrTrimNewLines} $1 $1
    StrCpy $R0 "$1"
    Call ServiceParse
    StrCmp $SvcName "" WriteServicesJson_next
    StrCmp $2 "0" +2
      FileWriteUTF16LE $R9 ","
    FileWriteUTF16LE $R9 '{"name":"$SvcName","port":$SvcPort,"require_auth":'
    ${If} $SvcFlag == "1"
      FileWriteUTF16LE $R9 "true"
    ${Else}
      FileWriteUTF16LE $R9 "false"
    ${EndIf}
    FileWriteUTF16LE $R9 "}"
    StrCpy $2 "0"
  WriteServicesJson_next:
    Goto WriteServicesJson_loop
  WriteServicesJson_close:
    FileClose $0
  WriteServicesJson_done:
    FileWriteUTF16LE $R9 "]"
FunctionEnd

Function WriteBasicJson
  FileOpen $R9 "$PLUGINSDIR\setup-basic.json" w
  IfErrors WriteBasicJson_error
  FileWriteByte $R9 255
  FileWriteByte $R9 254
  FileWriteUTF16LE $R9 '{"node":{"name":"'
  StrCpy $R0 "$NodeName"
  Call JsonEscape
  FileWriteUTF16LE $R9 "$R1"
  FileWriteUTF16LE $R9 '","description":"'
  StrCpy $R0 "$NodeDescription"
  Call JsonEscape
  FileWriteUTF16LE $R9 "$R1"
  FileWriteUTF16LE $R9 '","allow_bootstrap":true,"accept_bootstrap_providers":false},"network":{"port":'
  FileWriteUTF16LE $R9 "$Port"
  FileWriteUTF16LE $R9 '},"services":{"kademlia":{"bootstrap_nodes":'
  StrCpy $R0 "$Bootstrap"
  Call WriteStringArray
  FileWriteUTF16LE $R9 '},"dispatcher":{"local_services":'
  Call WriteServicesJson
  FileWriteUTF16LE $R9 '}},"crypto":{"pq_transport_enabled":true,"pq_identity_enabled":true,"pq_required":false},"auth":{"network":"'
  StrCpy $R0 "$AuthNetwork"
  Call JsonEscape
  StrCpy $Escaped "$R1"
  FileWriteUTF16LE $R9 "$Escaped"
  FileWriteUTF16LE $R9 '","networks":'
  ${If} $AuthNetwork == "none"
    FileWriteUTF16LE $R9 "{}"
  ${Else}
    FileWriteUTF16LE $R9 '{"'
    FileWriteUTF16LE $R9 "$Escaped"
    FileWriteUTF16LE $R9 '":{"authority":"'
    StrCpy $R0 "$AuthAuthority"
    Call JsonEscape
    FileWriteUTF16LE $R9 "$R1"
    FileWriteUTF16LE $R9 '"}}'
  ${EndIf}
  FileWriteUTF16LE $R9 "}}"
  FileClose $R9
  Return
  WriteBasicJson_error:
    MessageBox MB_ICONSTOP "无法创建临时配置文件。"
    Abort
FunctionEnd

Function WriteAdvancedJson
  FileOpen $R9 "$PLUGINSDIR\setup-advanced.json" w
  IfErrors WriteAdvancedJson_error
  FileWriteByte $R9 255
  FileWriteByte $R9 254
  FileWriteUTF16LE $R9 '{"services":{"ping":{"enabled":true,"interval_secs":15,"with_timeout":10,"max_failures":2},"kademlia":{"enabled":true,"record_ttl_seconds":3600,"replication_factor":20,"query_timeout_seconds":60},"dispatcher":{"enabled":true,"query_timeout_secs":60,"types_sync_secs":300,"max_inflight_dht":4,"max_request_bytes":16777216,"max_response_bytes":16777216},"registry":{"max_inflight_dht":4},"relay":{"retry_interval_secs":60,"max_failures":3}},"compat":{"max_record_bytes":14336}}'
  FileClose $R9
  Return
  WriteAdvancedJson_error:
    MessageBox MB_ICONSTOP "无法创建高级配置文件。"
    Abort
FunctionEnd

Function FinalizeWizard
  Call ServicesCount
  ${If} $ServicesEnabled != "1"
    Call ServicesInit
  ${ElseIf} $SvcCount == 0
    StrCpy $ServicesEnabled "0"
  ${EndIf}
  ${If} $AuthEnabled != "1"
    StrCpy $AuthNetwork "none"
    StrCpy $AuthAuthority ""
    Call ServicesClearAuth
  ${EndIf}
  ${If} $ServicesEnabled != "1"
    StrCpy $AuthEnabled "0"
    StrCpy $AuthNetwork "none"
    StrCpy $AuthAuthority ""
  ${EndIf}
FunctionEnd

Function OpenDataDir
  ExecShell "open" "$DataDir"
FunctionEnd
