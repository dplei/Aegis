; 卸载时删掉 Aegis 自己在「启动」文件夹建的自启快捷方式（注册表 Run 被拦时的兜底，见 win.rs set_startup_lnk）。
; 更新安装时保留，和 Tauri 对注册表 Run 值的处理一致。
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    Delete "$SMSTARTUP\Aegis.lnk"
  ${EndIf}
!macroend
