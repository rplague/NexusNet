Function ModePageCreate
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${NSD_CreateLabel} 10 10 280 36 "选择配置方式。首次安装可以跳过向导，服务将使用默认配置并保持手动启动。重新配置从默认值开始，旧配置会备份。"
  Pop $0
  ${If} $ConfigExists == "1"
    ${NSD_CreateRadioButton} 10 50 270 14 "保留现有配置（推荐）"
    Pop $hModeKeep
    ${NSD_CreateRadioButton} 10 70 270 14 "重新配置（备份旧 config.toml）"
    Pop $hModeReconfig
    ${If} $CfgAction == "wizard"
      ${NSD_SetState} $hModeReconfig ${BST_CHECKED}
    ${Else}
      ${NSD_SetState} $hModeKeep ${BST_CHECKED}
    ${EndIf}
  ${Else}
    ${NSD_CreateRadioButton} 10 50 270 14 "使用配置向导（推荐）"
    Pop $hModeWizard
    ${NSD_CreateRadioButton} 10 70 270 14 "跳过向导，使用默认配置并手动启动服务"
    Pop $hModeSkip
    ${If} $CfgAction == "skip"
      ${NSD_SetState} $hModeSkip ${BST_CHECKED}
    ${Else}
      ${NSD_SetState} $hModeWizard ${BST_CHECKED}
    ${EndIf}
  ${EndIf}
  nsDialogs::Show
FunctionEnd

Function ModePageLeave
  ${If} $ConfigExists == "1"
    ${NSD_GetState} $hModeKeep $R0
    ${If} $R0 == ${BST_CHECKED}
      StrCpy $CfgAction "keep"
      StrCpy $DoBackup "0"
    ${Else}
      StrCpy $CfgAction "wizard"
      StrCpy $DoBackup "1"
    ${EndIf}
  ${Else}
    ${NSD_GetState} $hModeSkip $R0
    ${If} $R0 == ${BST_CHECKED}
      StrCpy $CfgAction "skip"
      StrCpy $DoBackup "0"
    ${Else}
      StrCpy $CfgAction "wizard"
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
  ${NSD_CreateLabel} 10 30 60 14 "描述"
  Pop $0
  ${NSD_CreateText} 75 28 210 14 "$NodeDescription"
  Pop $hDescription
  ${NSD_CreateLabel} 10 55 270 14 "节点角色："
  Pop $0
  ${NSD_CreateRadioButton} 10 72 270 14 "NAT / 客户端（不监听，使用 relay）"
  Pop $hRoleNat
  ${NSD_CreateRadioButton} 10 89 270 14 "公网 / 直连（监听 IPv4/IPv6，可做 bootstrap）"
  Pop $hRoleDirect
  ${NSD_CreateRadioButton} 10 106 270 14 "自定义"
  Pop $hRoleCustom
  ${If} $Role == "direct"
    ${NSD_SetState} $hRoleDirect ${BST_CHECKED}
  ${ElseIf} $Role == "custom"
    ${NSD_SetState} $hRoleCustom ${BST_CHECKED}
  ${Else}
    ${NSD_SetState} $hRoleNat ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateCheckBox} 10 130 270 14 "允许在双栈公网时作为 bootstrap"
  Pop $hAllowBootstrap
  ${If} $AllowBootstrap == "1"
    ${NSD_SetState} $hAllowBootstrap ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateCheckBox} 10 148 270 14 "接受 Identify 发现的 bootstrap provider"
  Pop $hAcceptProviders
  ${If} $AcceptProviders == "1"
    ${NSD_SetState} $hAcceptProviders ${BST_CHECKED}
  ${EndIf}
  nsDialogs::Show
FunctionEnd

Function NodePageLeave
  ${NSD_GetText} $hName $NodeName
  ${NSD_GetText} $hDescription $NodeDescription
  ${If} $NodeName == ""
    MessageBox MB_ICONEXCLAMATION "节点名称不能为空。"
    Abort
  ${EndIf}
  StrCpy $R0 "$NodeName"
  Call HasJsonSpecial
  ${If} $R3 == "1"
    MessageBox MB_ICONEXCLAMATION "节点名称不能包含双引号或反斜杠。"
    Abort
  ${EndIf}
  StrCpy $R0 "$NodeDescription"
  Call HasJsonSpecial
  ${If} $R3 == "1"
    MessageBox MB_ICONEXCLAMATION "描述不能包含双引号或反斜杠。"
    Abort
  ${EndIf}
  ${NSD_GetState} $hRoleNat $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $Role "nat"
  ${Else}
    ${NSD_GetState} $hRoleDirect $R0
    ${If} $R0 == ${BST_CHECKED}
      StrCpy $Role "direct"
    ${Else}
      StrCpy $Role "custom"
    ${EndIf}
  ${EndIf}
  ${NSD_GetState} $hAllowBootstrap $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $AllowBootstrap "1"
  ${Else}
    StrCpy $AllowBootstrap "0"
  ${EndIf}
  ${NSD_GetState} $hAcceptProviders $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $AcceptProviders "1"
  ${Else}
    StrCpy $AcceptProviders "0"
  ${EndIf}
  ${If} $Role == "nat"
    StrCpy $Ipv4 "0"
    StrCpy $Ipv6 "0"
    StrCpy $Firewall "0"
  ${ElseIf} $Role == "direct"
    StrCpy $Ipv4 "1"
    StrCpy $Ipv6 "1"
    StrCpy $Firewall "1"
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
  ${NSD_CreateLabel} 10 10 60 14 "WebSocket 端口"
  Pop $0
  ${NSD_CreateText} 120 8 70 14 "$Port"
  Pop $hPort
  ${NSD_CreateCheckBox} 10 40 270 14 "监听 IPv4（地址自动检测）"
  Pop $hIpv4
  ${If} $Ipv4 == "1"
    ${NSD_SetState} $hIpv4 ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateCheckBox} 10 58 270 14 "监听 IPv6（地址自动检测）"
  Pop $hIpv6
  ${If} $Ipv6 == "1"
    ${NSD_SetState} $hIpv6 ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateCheckBox} 10 82 270 14 "添加 Windows 防火墙入站规则"
  Pop $hFirewall
  ${If} $Firewall == "1"
    ${NSD_SetState} $hFirewall ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateLabel} 10 110 270 32 "端口是 WebSocket 的 TCP 端口。公网地址由程序自动检测；无公网地址时使用 relay。"
  Pop $0
  nsDialogs::Show
FunctionEnd

Function NetworkPageLeave
  ${NSD_GetText} $hPort $Port
  StrCpy $R0 "$Port"
  StrCpy $R1 "1"
  StrCpy $R2 "65535"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "端口必须是 1-65535 的数字。"
    Abort
  ${EndIf}
  ${NSD_GetState} $hIpv4 $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $Ipv4 "1"
  ${Else}
    StrCpy $Ipv4 "0"
  ${EndIf}
  ${NSD_GetState} $hIpv6 $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $Ipv6 "1"
  ${Else}
    StrCpy $Ipv6 "0"
  ${EndIf}
  ${NSD_GetState} $hFirewall $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $Firewall "1"
  ${Else}
    StrCpy $Firewall "0"
  ${EndIf}
  ${If} $Ipv4 == "0"
    ${AndIf} $Ipv6 == "0"
    StrCpy $Firewall "0"
  ${EndIf}
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
  ${NSD_CreateLabel} 10 10 270 28 "每行一个 bootstrap multiaddr，必须包含 /ws 和 /p2p/<PeerId>。"
  Pop $0
  ${NSD_CreateMLText} 10 42 270 105 "$Bootstrap"
  Pop $hBootstrap
  ${NSD_CreateLabel} 10 155 270 32 "例如 /ip4/167.99.153.118/tcp/5000/ws/p2p/12D3KooW..."
  Pop $0
  nsDialogs::Show
FunctionEnd

Function BootstrapPageLeave
  ${NSD_GetText} $hBootstrap $Bootstrap
  StrCpy $R0 "$Bootstrap"
  Call HasJsonSpecial
  ${If} $R3 == "1"
    MessageBox MB_ICONEXCLAMATION "bootstrap 地址不能包含双引号或反斜杠。"
    Abort
  ${EndIf}
  ${If} $Role == "nat"
    ${AndIf} $Bootstrap == ""
    MessageBox MB_ICONEXCLAMATION "NAT/客户端模式至少需要一个 bootstrap。"
    Abort
  ${EndIf}
  ${If} $Bootstrap != ""
    StrCpy $R0 "$Bootstrap"
    StrCpy $R1 "/ws"
    Call Contains
    ${If} $R2 != "1"
      MessageBox MB_ICONEXCLAMATION "本版本只支持 WebSocket，bootstrap 必须包含 /ws。"
      Abort
    ${EndIf}
    StrCpy $R0 "$Bootstrap"
    StrCpy $R1 "/p2p/"
    Call Contains
    ${If} $R2 != "1"
      MessageBox MB_ICONEXCLAMATION "bootstrap 必须包含 /p2p/<PeerId>。"
      Abort
    ${EndIf}
  ${EndIf}
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
  ${NSD_CreateLabel} 10 10 270 28 "可选。每行一个本地服务：name:port 或 name:port:auth。"
  Pop $0
  ${NSD_CreateMLText} 10 42 270 105 "$Services"
  Pop $hServices
  ${NSD_CreateLabel} 10 155 270 32 "auth 可写 1/0/true/false；服务固定连接 127.0.0.1。"
  Pop $0
  nsDialogs::Show
FunctionEnd

Function ServicesPageLeave
  ${NSD_GetText} $hServices $Services
  StrCpy $R0 "$Services"
  Call HasJsonSpecial
  ${If} $R3 == "1"
    MessageBox MB_ICONEXCLAMATION "本地服务配置不能包含双引号或反斜杠。"
    Abort
  ${EndIf}
  ${If} $Services != ""
    StrCpy $R0 "$Services"
    StrCpy $R1 ":"
    Call Contains
    ${If} $R2 != "1"
      MessageBox MB_ICONEXCLAMATION "本地服务格式应为 name:port[:auth]。"
      Abort
    ${EndIf}
  ${EndIf}
FunctionEnd

Function PingPageCreate
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${NSD_CreateCheckBox} 10 5 270 14 "使用默认 Ping / Kademlia 参数"
  Pop $hAdvPingDefaults
  ${If} $AdvPingDefaults == "1"
    ${NSD_SetState} $hAdvPingDefaults ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateCheckBox} 10 25 270 14 "启用 Ping"
  Pop $hPingEnabled
  ${If} $PingEnabled == "1"
    ${NSD_SetState} $hPingEnabled ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateLabel} 10 45 80 14 "Ping 间隔(秒)"
  Pop $0
  ${NSD_CreateText} 95 43 45 14 "$PingInterval"
  Pop $hPingInterval
  ${NSD_CreateLabel} 155 45 80 14 "超时(秒)"
  Pop $0
  ${NSD_CreateText} 235 43 45 14 "$PingTimeout"
  Pop $hPingTimeout
  ${NSD_CreateLabel} 10 65 80 14 "连续失败数"
  Pop $0
  ${NSD_CreateText} 95 63 45 14 "$PingFailures"
  Pop $hPingFailures
  ${NSD_CreateCheckBox} 10 85 270 14 "启用 Kademlia"
  Pop $hKademliaEnabled
  ${If} $KademliaEnabled == "1"
    ${NSD_SetState} $hKademliaEnabled ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateLabel} 10 105 80 14 "记录 TTL(秒)"
  Pop $0
  ${NSD_CreateText} 95 103 45 14 "$KademliaTtl"
  Pop $hKademliaTtl
  ${NSD_CreateLabel} 155 105 80 14 "复制因子"
  Pop $0
  ${NSD_CreateText} 235 103 45 14 "$KademliaReplication"
  Pop $hKademliaReplication
  ${NSD_CreateLabel} 10 125 80 14 "查询超时(秒)"
  Pop $0
  ${NSD_CreateText} 95 123 45 14 "$KademliaTimeout"
  Pop $hKademliaTimeout
  nsDialogs::Show
FunctionEnd

Function PingPageLeave
  ${NSD_GetState} $hAdvPingDefaults $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $AdvPingDefaults "1"
    StrCpy $PingEnabled "1"
    StrCpy $PingInterval "15"
    StrCpy $PingTimeout "10"
    StrCpy $PingFailures "2"
    StrCpy $KademliaEnabled "1"
    StrCpy $KademliaTtl "3600"
    StrCpy $KademliaReplication "20"
    StrCpy $KademliaTimeout "60"
    Return
  ${EndIf}
  StrCpy $AdvPingDefaults "0"
  ${NSD_GetState} $hPingEnabled $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $PingEnabled "1"
  ${Else}
    StrCpy $PingEnabled "0"
  ${EndIf}
  ${NSD_GetState} $hKademliaEnabled $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $KademliaEnabled "1"
  ${Else}
    StrCpy $KademliaEnabled "0"
  ${EndIf}
  ${NSD_GetText} $hPingInterval $PingInterval
  ${NSD_GetText} $hPingTimeout $PingTimeout
  ${NSD_GetText} $hPingFailures $PingFailures
  ${NSD_GetText} $hKademliaTtl $KademliaTtl
  ${NSD_GetText} $hKademliaReplication $KademliaReplication
  ${NSD_GetText} $hKademliaTimeout $KademliaTimeout
  StrCpy $R0 "$PingInterval"
  StrCpy $R1 "1"
  StrCpy $R2 "86400"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "Ping 间隔必须是 1-86400 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$PingTimeout"
  StrCpy $R1 "1"
  StrCpy $R2 "3600"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "Ping 超时必须是 1-3600 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$PingFailures"
  StrCpy $R1 "0"
  StrCpy $R2 "100"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "Ping 失败数必须是 0-100 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$KademliaTtl"
  StrCpy $R1 "1"
  StrCpy $R2 "604800"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "Kademlia TTL 必须是 1-604800 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$KademliaReplication"
  StrCpy $R1 "1"
  StrCpy $R2 "100"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "Kademlia 复制因子必须是 1-100 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$KademliaTimeout"
  StrCpy $R1 "1"
  StrCpy $R2 "3600"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "Kademlia 查询超时必须是 1-3600 的数字。"
    Abort
  ${EndIf}
FunctionEnd

Function DispatcherPageCreate
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${NSD_CreateCheckBox} 10 5 270 14 "使用默认 Dispatcher / Registry 参数"
  Pop $hAdvDispatcherDefaults
  ${If} $AdvDispatcherDefaults == "1"
    ${NSD_SetState} $hAdvDispatcherDefaults ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateCheckBox} 10 25 270 14 "启用 Dispatcher"
  Pop $hDispatcherEnabled
  ${If} $DispatcherEnabled == "1"
    ${NSD_SetState} $hDispatcherEnabled ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateLabel} 10 45 80 14 "查询超时(秒)"
  Pop $0
  ${NSD_CreateText} 95 43 45 14 "$DispatcherTimeout"
  Pop $hDispatcherTimeout
  ${NSD_CreateLabel} 155 45 80 14 "类型同步(秒)"
  Pop $0
  ${NSD_CreateText} 235 43 45 14 "$TypesSync"
  Pop $hTypesSync
  ${NSD_CreateLabel} 10 65 80 14 "DHT 并发"
  Pop $0
  ${NSD_CreateText} 95 63 45 14 "$DispatcherInflight"
  Pop $hDispatcherInflight
  ${NSD_CreateLabel} 155 65 80 14 "Registry 并发"
  Pop $0
  ${NSD_CreateText} 235 63 45 14 "$RegistryInflight"
  Pop $hRegistryInflight
  ${NSD_CreateLabel} 10 85 80 14 "请求上限(MiB)"
  Pop $0
  ${NSD_CreateText} 95 83 45 14 "$RequestMiB"
  Pop $hRequestMiB
  ${NSD_CreateLabel} 155 85 80 14 "响应上限(MiB)"
  Pop $0
  ${NSD_CreateText} 235 83 45 14 "$ResponseMiB"
  Pop $hResponseMiB
  nsDialogs::Show
FunctionEnd

Function DispatcherPageLeave
  ${NSD_GetState} $hAdvDispatcherDefaults $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $AdvDispatcherDefaults "1"
    StrCpy $DispatcherEnabled "1"
    StrCpy $DispatcherTimeout "60"
    StrCpy $TypesSync "300"
    StrCpy $DispatcherInflight "4"
    StrCpy $RequestMiB "16"
    StrCpy $ResponseMiB "16"
    StrCpy $RegistryInflight "4"
    Return
  ${EndIf}
  StrCpy $AdvDispatcherDefaults "0"
  ${NSD_GetState} $hDispatcherEnabled $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $DispatcherEnabled "1"
  ${Else}
    StrCpy $DispatcherEnabled "0"
  ${EndIf}
  ${NSD_GetText} $hDispatcherTimeout $DispatcherTimeout
  ${NSD_GetText} $hTypesSync $TypesSync
  ${NSD_GetText} $hDispatcherInflight $DispatcherInflight
  ${NSD_GetText} $hRegistryInflight $RegistryInflight
  ${NSD_GetText} $hRequestMiB $RequestMiB
  ${NSD_GetText} $hResponseMiB $ResponseMiB
  StrCpy $R0 "$DispatcherTimeout"
  StrCpy $R1 "1"
  StrCpy $R2 "3600"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "Dispatcher 查询超时必须是 1-3600 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$TypesSync"
  StrCpy $R1 "1"
  StrCpy $R2 "86400"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "类型同步间隔必须是 1-86400 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$DispatcherInflight"
  StrCpy $R1 "1"
  StrCpy $R2 "16"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "DHT 并发必须是 1-16 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$RegistryInflight"
  StrCpy $R1 "1"
  StrCpy $R2 "16"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "Registry 并发必须是 1-16 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$RequestMiB"
  StrCpy $R1 "1"
  StrCpy $R2 "64"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "请求上限必须是 1-64 MiB 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$ResponseMiB"
  StrCpy $R1 "1"
  StrCpy $R2 "64"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "响应上限必须是 1-64 MiB 的数字。"
    Abort
  ${EndIf}
FunctionEnd

Function RelayPageCreate
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${NSD_CreateCheckBox} 10 5 270 14 "使用默认 Relay / Compat 参数"
  Pop $hAdvRelayDefaults
  ${If} $AdvRelayDefaults == "1"
    ${NSD_SetState} $hAdvRelayDefaults ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateLabel} 10 35 100 14 "Relay 重试(秒)"
  Pop $0
  ${NSD_CreateText} 120 33 60 14 "$RelayRetry"
  Pop $hRelayRetry
  ${NSD_CreateLabel} 10 60 100 14 "最大失败次数"
  Pop $0
  ${NSD_CreateText} 120 58 60 14 "$RelayFailures"
  Pop $hRelayFailures
  ${NSD_CreateLabel} 10 85 100 14 "Compat 记录上限"
  Pop $0
  ${NSD_CreateText} 120 83 60 14 "$CompatMax"
  Pop $hCompatMax
  nsDialogs::Show
FunctionEnd

Function RelayPageLeave
  ${NSD_GetState} $hAdvRelayDefaults $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $AdvRelayDefaults "1"
    StrCpy $RelayRetry "60"
    StrCpy $RelayFailures "3"
    StrCpy $CompatMax "14336"
    Return
  ${EndIf}
  StrCpy $AdvRelayDefaults "0"
  ${NSD_GetText} $hRelayRetry $RelayRetry
  ${NSD_GetText} $hRelayFailures $RelayFailures
  ${NSD_GetText} $hCompatMax $CompatMax
  StrCpy $R0 "$RelayRetry"
  StrCpy $R1 "1"
  StrCpy $R2 "86400"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "Relay 重试间隔必须是 1-86400 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$RelayFailures"
  StrCpy $R1 "0"
  StrCpy $R2 "100"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "Relay 最大失败次数必须是 0-100 的数字。"
    Abort
  ${EndIf}
  StrCpy $R0 "$CompatMax"
  StrCpy $R1 "1"
  StrCpy $R2 "1048576"
  Call ValidateInt
  ${If} $R3 != "1"
    MessageBox MB_ICONEXCLAMATION "Compat 记录上限必须是 1-1048576 的数字。"
    Abort
  ${EndIf}
FunctionEnd

Function SecurityPageCreate
  ${If} $CfgAction != "wizard"
    Abort
  ${EndIf}
  nsDialogs::Create 1018
  Pop $Dialog
  ${If} $Dialog == error
    Abort
  ${EndIf}
  ${NSD_CreateCheckBox} 10 10 270 14 "启用 PQ 传输"
  Pop $hPqTransport
  ${If} $PqTransport == "1"
    ${NSD_SetState} $hPqTransport ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateCheckBox} 10 28 270 14 "启用 PQ 身份"
  Pop $hPqIdentity
  ${If} $PqIdentity == "1"
    ${NSD_SetState} $hPqIdentity ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateCheckBox} 10 46 270 14 "要求对端必须支持 PQ"
  Pop $hPqRequired
  ${If} $PqRequired == "1"
    ${NSD_SetState} $hPqRequired ${BST_CHECKED}
  ${EndIf}
  ${NSD_CreateLabel} 10 72 100 14 "鉴权网络"
  Pop $0
  ${NSD_CreateText} 120 70 160 14 "$AuthNetwork"
  Pop $hAuthNetwork
  ${NSD_CreateLabel} 10 96 100 14 "Authority 公钥"
  Pop $0
  ${NSD_CreateText} 120 94 160 14 "$AuthAuthority"
  Pop $hAuthAuthority
  ${NSD_CreateLabel} 10 120 270 32 "鉴权网络填 none 表示关闭；启用时只支持一个网络。Authority 为 base64 Ed25519 公钥。"
  Pop $0
  nsDialogs::Show
FunctionEnd

Function SecurityPageLeave
  ${NSD_GetState} $hPqTransport $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $PqTransport "1"
  ${Else}
    StrCpy $PqTransport "0"
  ${EndIf}
  ${NSD_GetState} $hPqIdentity $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $PqIdentity "1"
  ${Else}
    StrCpy $PqIdentity "0"
  ${EndIf}
  ${NSD_GetState} $hPqRequired $R0
  ${If} $R0 == ${BST_CHECKED}
    StrCpy $PqRequired "1"
  ${Else}
    StrCpy $PqRequired "0"
  ${EndIf}
  ${NSD_GetText} $hAuthNetwork $AuthNetwork
  ${NSD_GetText} $hAuthAuthority $AuthAuthority
  ${If} $AuthNetwork == ""
    StrCpy $AuthNetwork "none"
  ${EndIf}
  ${If} $AuthNetwork == "none"
    StrCpy $AuthAuthority ""
  ${Else}
    ${If} $AuthAuthority == ""
      MessageBox MB_ICONEXCLAMATION "启用鉴权网络时 Authority 不能为空。"
      Abort
    ${EndIf}
    StrCpy $R0 "$AuthNetwork"
    Call HasJsonSpecial
    ${If} $R3 == "1"
      MessageBox MB_ICONEXCLAMATION "鉴权网络名不能包含双引号或反斜杠。"
      Abort
    ${EndIf}
    StrCpy $R0 "$AuthAuthority"
    Call HasJsonSpecial
    ${If} $R3 == "1"
      MessageBox MB_ICONEXCLAMATION "Authority 不能包含双引号或反斜杠。"
      Abort
    ${EndIf}
  ${EndIf}
  ${If} $PqIdentity == "1"
    StrCpy $PqTransport "1"
  ${EndIf}
  ${If} $PqRequired == "1"
    StrCpy $PqTransport "1"
  ${EndIf}
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
  StrCpy $R0 "节点名称: $NodeName$\r$\n"
  StrCpy $R0 "$R0角色: $Role$\r$\n"
  StrCpy $R0 "$R0端口: $Port$\r$\n"
  StrCpy $R0 "$R0IPv4: $Ipv4  IPv6: $Ipv6  防火墙: $Firewall$\r$\n"
  StrCpy $R0 "$R0PQ: $PqTransport / $PqIdentity / $PqRequired$\r$\n"
  StrCpy $R0 "$R0鉴权网络: $AuthNetwork$\r$\n"
  StrCpy $R0 "$R0本地服务: $Services$\r$\n"
  StrCpy $R0 "$R0Bootstrap:$\r$\n$Bootstrap"
  ${NSD_CreateMLText} 10 10 270 165 "$R0"
  Pop $hSummary
  nsDialogs::Show
FunctionEnd

Function SummaryPageLeave
  ${If} $Ipv4 == "0"
    ${AndIf} $Ipv6 == "0"
    ${AndIf} $Bootstrap == ""
    MessageBox MB_YESNO|MB_ICONEXCLAMATION "未启用监听且未配置 bootstrap，节点将无法联网。仍然继续？" IDYES +2
    Abort
  ${EndIf}
FunctionEnd
