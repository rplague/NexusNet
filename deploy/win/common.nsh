!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"

Var DataDir
Var ConfigExists
Var CfgAction
Var SetupFile
Var DoBackup
Var WasManual
Var StartNow

Var NodeName
Var NodeDescription
Var Role
Var Port
Var Ipv4
Var Ipv6
Var Firewall
Var Bootstrap
Var Services
Var AllowBootstrap
Var AcceptProviders
Var PqTransport
Var PqIdentity
Var PqRequired
Var AuthNetwork
Var AuthAuthority

Var AdvPingDefaults
Var PingEnabled
Var PingInterval
Var PingTimeout
Var PingFailures
Var KademliaEnabled
Var KademliaTtl
Var KademliaReplication
Var KademliaTimeout
Var AdvDispatcherDefaults
Var DispatcherEnabled
Var DispatcherTimeout
Var TypesSync
Var DispatcherInflight
Var RequestMiB
Var ResponseMiB
Var RegistryInflight
Var AdvRelayDefaults
Var RelayRetry
Var RelayFailures
Var CompatMax

Var Dialog
Var hName
Var hDescription
Var hRoleNat
Var hRoleDirect
Var hRoleCustom
Var hAllowBootstrap
Var hAcceptProviders
Var hPort
Var hIpv4
Var hIpv6
Var hFirewall
Var hBootstrap
Var hServices
Var hModeKeep
Var hModeReconfig
Var hModeWizard
Var hModeSkip
Var hAdvPingDefaults
Var hPingEnabled
Var hPingInterval
Var hPingTimeout
Var hPingFailures
Var hKademliaEnabled
Var hKademliaTtl
Var hKademliaReplication
Var hKademliaTimeout
Var hAdvDispatcherDefaults
Var hDispatcherEnabled
Var hDispatcherTimeout
Var hTypesSync
Var hDispatcherInflight
Var hRequestMiB
Var hResponseMiB
Var hRegistryInflight
Var hAdvRelayDefaults
Var hRelayRetry
Var hRelayFailures
Var hCompatMax
Var hPqTransport
Var hPqIdentity
Var hPqRequired
Var hAuthNetwork
Var hAuthAuthority
Var hSummary

Function InitDefaults
  ReadEnvStr $NodeName "COMPUTERNAME"
  ${If} $NodeName == ""
    StrCpy $NodeName "NexusNet"
  ${EndIf}
  StrCpy $NodeDescription "Windows P2P node"
  StrCpy $Role "nat"
  StrCpy $Port "5000"
  StrCpy $Ipv4 "0"
  StrCpy $Ipv6 "0"
  StrCpy $Firewall "0"
  StrCpy $Bootstrap ""
  StrCpy $Services ""
  StrCpy $AllowBootstrap "1"
  StrCpy $AcceptProviders "0"
  StrCpy $PqTransport "0"
  StrCpy $PqIdentity "0"
  StrCpy $PqRequired "0"
  StrCpy $AuthNetwork "none"
  StrCpy $AuthAuthority ""

  StrCpy $AdvPingDefaults "1"
  StrCpy $PingEnabled "1"
  StrCpy $PingInterval "15"
  StrCpy $PingTimeout "10"
  StrCpy $PingFailures "2"
  StrCpy $KademliaEnabled "1"
  StrCpy $KademliaTtl "3600"
  StrCpy $KademliaReplication "20"
  StrCpy $KademliaTimeout "60"

  StrCpy $AdvDispatcherDefaults "1"
  StrCpy $DispatcherEnabled "1"
  StrCpy $DispatcherTimeout "60"
  StrCpy $TypesSync "300"
  StrCpy $DispatcherInflight "4"
  StrCpy $RequestMiB "16"
  StrCpy $ResponseMiB "16"
  StrCpy $RegistryInflight "4"

  StrCpy $AdvRelayDefaults "1"
  StrCpy $RelayRetry "60"
  StrCpy $RelayFailures "3"
  StrCpy $CompatMax "14336"

  StrCpy $ConfigExists "0"
  StrCpy $CfgAction "wizard"
  StrCpy $SetupFile ""
  StrCpy $DoBackup "0"
  StrCpy $WasManual "0"
  StrCpy $StartNow "1"
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

Function HasJsonSpecial
  StrCpy $R0 "$R0"
  StrCpy $R1 '"'
  Call Contains
  ${If} $R2 == "1"
    StrCpy $R3 "1"
    Return
  ${EndIf}
  StrCpy $R1 '\'
  Call Contains
  ${If} $R2 == "1"
    StrCpy $R3 "1"
    Return
  ${EndIf}
  StrCpy $R3 "0"
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

Function WriteStringArray
  StrCpy $R1 "0"
  StrCpy $R2 ""
  StrCpy $R4 "0"
  FileWriteUTF16LE $R9 "["
  WriteStringArray_loop:
    StrCpy $R3 $R0 1 $R1
    StrCmp $R3 "" WriteStringArray_flush
    StrCmp $R3 "$\r" WriteStringArray_next
    StrCmp $R3 "$\n" WriteStringArray_flush
    StrCpy $R2 "$R2$R3"
    Goto WriteStringArray_next
  WriteStringArray_flush:
    StrCmp $R2 "" WriteStringArray_flush_done
    StrCmp $R4 "0" +2
      FileWriteUTF16LE $R9 ","
    FileWriteUTF16LE $R9 '"$R2"'
    IntOp $R4 $R4 + 1
    StrCpy $R2 ""
  WriteStringArray_flush_done:
    StrCmp $R3 "" WriteStringArray_close
  WriteStringArray_next:
    IntOp $R1 $R1 + 1
    Goto WriteStringArray_loop
  WriteStringArray_close:
    FileWriteUTF16LE $R9 "]"
FunctionEnd

Function WriteServiceArray
  StrCpy $R1 "0"
  StrCpy $R2 ""
  StrCpy $R4 "0"
  FileWriteUTF16LE $R9 "["
  WriteServiceArray_loop:
    StrCpy $R3 $R0 1 $R1
    StrCmp $R3 "" WriteServiceArray_flush
    StrCmp $R3 "$\r" WriteServiceArray_next
    StrCmp $R3 "$\n" WriteServiceArray_flush
    StrCpy $R2 "$R2$R3"
    Goto WriteServiceArray_next
  WriteServiceArray_flush:
    StrCmp $R2 "" WriteServiceArray_flush_done
    StrCpy $R5 "0"
    StrCpy $R6 ""
    StrCpy $R7 ""
    StrCpy $R8 ""
    WriteServiceArray_name:
      StrCpy $0 $R2 1 $R5
      StrCmp $0 "" WriteServiceArray_write
      StrCmp $0 ":" WriteServiceArray_colon1
      StrCpy $R6 "$R6$0"
      IntOp $R5 $R5 + 1
      Goto WriteServiceArray_name
    WriteServiceArray_colon1:
      IntOp $R5 $R5 + 1
    WriteServiceArray_port:
      StrCpy $0 $R2 1 $R5
      StrCmp $0 "" WriteServiceArray_write
      StrCmp $0 ":" WriteServiceArray_colon2
      StrCpy $R7 "$R7$0"
      IntOp $R5 $R5 + 1
      Goto WriteServiceArray_port
    WriteServiceArray_colon2:
      IntOp $R5 $R5 + 1
      StrCpy $R8 $R2 "" $R5
    WriteServiceArray_write:
      StrCmp $R7 "" 0 +2
        StrCpy $R7 "0"
      IntOp $R7 $R7 + 0
      StrCmp $R4 "0" +2
        FileWriteUTF16LE $R9 ","
      FileWriteUTF16LE $R9 '{"name":"$R6","port":$R7,"require_auth":'
      ${If} $R8 == "1"
        FileWriteUTF16LE $R9 "true"
      ${ElseIf} $R8 == "true"
        FileWriteUTF16LE $R9 "true"
      ${Else}
        FileWriteUTF16LE $R9 "false"
      ${EndIf}
      FileWriteUTF16LE $R9 "}"
      IntOp $R4 $R4 + 1
      StrCpy $R2 ""
  WriteServiceArray_flush_done:
    StrCmp $R3 "" WriteServiceArray_close
  WriteServiceArray_next:
    IntOp $R1 $R1 + 1
    Goto WriteServiceArray_loop
  WriteServiceArray_close:
    FileWriteUTF16LE $R9 "]"
FunctionEnd

Function WriteBasicJson
  FileOpen $R9 "$PLUGINSDIR\setup-basic.json" w
  IfErrors WriteBasicJson_error
  FileWriteByte $R9 255
  FileWriteByte $R9 254
  FileWriteUTF16LE $R9 '{"node":{"name":"$NodeName","description":"$NodeDescription","allow_bootstrap":'
  ${If} $AllowBootstrap == "1"
    FileWriteUTF16LE $R9 "true"
  ${Else}
    FileWriteUTF16LE $R9 "false"
  ${EndIf}
  FileWriteUTF16LE $R9 ',"accept_bootstrap_providers":'
  ${If} $AcceptProviders == "1"
    FileWriteUTF16LE $R9 "true"
  ${Else}
    FileWriteUTF16LE $R9 "false"
  ${EndIf}
  FileWriteUTF16LE $R9 '},"network":{"ipv4_enabled":'
  ${If} $Ipv4 == "1"
    FileWriteUTF16LE $R9 "true"
  ${Else}
    FileWriteUTF16LE $R9 "false"
  ${EndIf}
  FileWriteUTF16LE $R9 ',"ipv6_enabled":'
  ${If} $Ipv6 == "1"
    FileWriteUTF16LE $R9 "true"
  ${Else}
    FileWriteUTF16LE $R9 "false"
  ${EndIf}
  FileWriteUTF16LE $R9 ',"port":$Port},"services":{"kademlia":{"bootstrap_nodes":'
  StrCpy $R0 "$Bootstrap"
  Call WriteStringArray
  FileWriteUTF16LE $R9 '},"dispatcher":{"local_services":'
  StrCpy $R0 "$Services"
  Call WriteServiceArray
  FileWriteUTF16LE $R9 '}},"crypto":{"pq_transport_enabled":'
  ${If} $PqTransport == "1"
    FileWriteUTF16LE $R9 "true"
  ${Else}
    FileWriteUTF16LE $R9 "false"
  ${EndIf}
  FileWriteUTF16LE $R9 ',"pq_identity_enabled":'
  ${If} $PqIdentity == "1"
    FileWriteUTF16LE $R9 "true"
  ${Else}
    FileWriteUTF16LE $R9 "false"
  ${EndIf}
  FileWriteUTF16LE $R9 ',"pq_required":'
  ${If} $PqRequired == "1"
    FileWriteUTF16LE $R9 "true"
  ${Else}
    FileWriteUTF16LE $R9 "false"
  ${EndIf}
  FileWriteUTF16LE $R9 '},"auth":{"network":"$AuthNetwork","networks":'
  ${If} $AuthNetwork == "none"
    FileWriteUTF16LE $R9 "{}"
  ${Else}
    FileWriteUTF16LE $R9 '{"$AuthNetwork":{"authority":"$AuthAuthority"}}'
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
  FileWriteUTF16LE $R9 '{"services":{"ping":{"enabled":'
  ${If} $PingEnabled == "1"
    FileWriteUTF16LE $R9 "true"
  ${Else}
    FileWriteUTF16LE $R9 "false"
  ${EndIf}
  FileWriteUTF16LE $R9 ',"interval_secs":$PingInterval,"with_timeout":$PingTimeout,"max_failures":$PingFailures},"kademlia":{"enabled":'
  ${If} $KademliaEnabled == "1"
    FileWriteUTF16LE $R9 "true"
  ${Else}
    FileWriteUTF16LE $R9 "false"
  ${EndIf}
  FileWriteUTF16LE $R9 ',"record_ttl_seconds":$KademliaTtl,"replication_factor":$KademliaReplication,"query_timeout_seconds":$KademliaTimeout},"dispatcher":{"enabled":'
  ${If} $DispatcherEnabled == "1"
    FileWriteUTF16LE $R9 "true"
  ${Else}
    FileWriteUTF16LE $R9 "false"
  ${EndIf}
  FileWriteUTF16LE $R9 ',"query_timeout_secs":$DispatcherTimeout,"types_sync_secs":$TypesSync,"max_inflight_dht":$DispatcherInflight,"max_request_bytes":'
  IntOp $0 $RequestMiB * 1048576
  FileWriteUTF16LE $R9 "$0"
  FileWriteUTF16LE $R9 ',"max_response_bytes":'
  IntOp $0 $ResponseMiB * 1048576
  FileWriteUTF16LE $R9 "$0"
  FileWriteUTF16LE $R9 '},"registry":{"max_inflight_dht":$RegistryInflight},"relay":{"retry_interval_secs":$RelayRetry,"max_failures":$RelayFailures}},"compat":{"max_record_bytes":$CompatMax}}'
  FileClose $R9
  Return
  WriteAdvancedJson_error:
    MessageBox MB_ICONSTOP "无法创建高级配置文件。"
    Abort
FunctionEnd

Function OpenDataDir
  ExecShell "open" "$DataDir"
FunctionEnd
