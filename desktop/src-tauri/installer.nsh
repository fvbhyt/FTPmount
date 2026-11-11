; FtpMount 的 NSIS 安装钩子
;
; 目的：把 WinFsp 一起装进去，新电脑一步到位。
; WinFsp 是内核驱动，必须管理员权限，所以 installMode 设成了 perMachine
; （整个安装程序以管理员身份运行，msiexec 才有权限装驱动）。

!include "LogicLib.nsh"

!macro customInstall
  ; 已经装过 WinFsp 就跳过，别覆盖用户现有的版本
  ${If} ${FileExists} "$PROGRAMFILES32\WinFsp\bin\winfsp-x64.dll"
    DetailPrint "已检测到 WinFsp，跳过安装。"
  ${ElseIf} ${FileExists} "$PROGRAMFILES64\WinFsp\bin\winfsp-x64.dll"
    DetailPrint "已检测到 WinFsp，跳过安装。"
  ${Else}
    ; 资源可能落在 $INSTDIR\winfsp 或 $INSTDIR\resources\winfsp，两处都找
    StrCpy $R0 ""
    ${If} ${FileExists} "$INSTDIR\winfsp\winfsp-2.1.25156.msi"
      StrCpy $R0 "$INSTDIR\winfsp\winfsp-2.1.25156.msi"
    ${ElseIf} ${FileExists} "$INSTDIR\resources\winfsp\winfsp-2.1.25156.msi"
      StrCpy $R0 "$INSTDIR\resources\winfsp\winfsp-2.1.25156.msi"
    ${EndIf}

    ${If} $R0 == ""
      MessageBox MB_ICONEXCLAMATION "安装包里没有找到 WinFsp 安装文件（winfsp-2.1.25156.msi）。$\r$\n请到 https://winfsp.dev/rel/ 手动安装后再挂载。"
    ${Else}
      DetailPrint "正在安装 WinFsp（内核驱动）..."
      ExecWait '"$SYSDIR\msiexec.exe" /i "$R0" /qn /norestart' $R1
      ; 0 = 成功，3010 = 成功但需要重启
      ${If} $R1 == 0
        DetailPrint "WinFsp 安装完成。"
      ${ElseIf} $R1 == 3010
        DetailPrint "WinFsp 安装完成（需要重启后生效）。"
      ${Else}
        MessageBox MB_ICONEXCLAMATION "WinFsp 安装失败（返回码 $R1）。$\r$\n应用可以打开，但挂载会失败；请到 https://winfsp.dev/rel/ 手动安装。"
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend
