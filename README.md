<p align="center">
    <img src="https://sing-box.sagernet.org/assets/icon.svg" width="100px" align="center" />&nbsp;&nbsp;
    <img src="https://xtls.github.io/logo-light.svg" width="100px" align="center" />
    <h2 align="center">sing-box-with-xray</h2>
    <p align="center">
        运行裸核的 PowerShell 简单脚本方案, 使用 sing-box 的 TUN 搭配 Xray<br />
    </p>
</p>

## 功能

sing-box 负责大部分工作。直连流量从 sing-box 直接出站, 代理流量从 socks 出站到 Xray, Xray 只负责和 VPS 通信。

- `Restart.ps1` 重启 sing-box 和 xray 进程, 随机化 sing-tun 接口名称避免网络不通。
- `Stop.ps1` 停止 sing-box 和 xray 进程。
- `Update.ps1` 更新 sing-box 和 xray 可执行程序。

## 注意事项

- 仅适用于 Windows。
- 需要 jq 来处理 json 文件, [下载](https://github.com/jqlang/jq/releases/latest) 后将其添加到系统环境变量以便脚本调用。
- 所有文件存放于 `$env:USERPROFILE\Apps\sing-box-with-xray`, 如有需要请自行更改。
- 自行更改 `xray.json` 中的参数, 更多模板参考 `templates` 目录。

## 快捷方式

为 `.ps1` 脚本创建快捷方式, 右击快捷方式, `属性`→`快捷方式`→`目标`, 在脚本文件路径前添加 `powershell.exe -ExecutionPolicy Bypass -File`, 注意 `-File` 和文件之间还有一个空格, 比如:

```powershell
powershell.exe -ExecutionPolicy Bypass -File "C:\Users\admin\Apps\sing-box-with-xray\Stop.ps1"
```

点击 `应用`。

此外, 在 `属性`→`快捷方式`→`高级` 中勾选 `用管理员身份运行`。

如果要为快捷方式更换图标, 图标在 `icon` 目录。

以后使用时点击快捷方式即可。

### 如果脚本无法运行

参考微软官方文档 [Set-ExecutionPolicy](https://learn.microsoft.com/zh-cn/powershell/module/microsoft.powershell.security/set-executionpolicy), 以管理员身份运行 PowerShell, 执行以下命令:

```powershell
Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope LocalMachine
```

也许还需在 `属性`→`常规` 最下方的 `安全` 勾选解除锁定: “此文件来自其他计算机，可能被阻止以帮助保护该计算机”。