<p align="center">
    <img src="icons/ladder.ico" width="100px" align="center" />
    <h2 align="center">ladder</h2>
    <p align="center">
        一个极简的、管理 sing-box 和 Xray 核心的 Windows 托盘程序。
    </p>
</p>

## 简介

原方案用 PowerShell 脚本管理 sing-box 和 Xray, 现已归档到 [powershell](https://github.com/senzyo/ladder/tree/powershell) 分支。

main 分支是由 Rust 构建的新方案, 程序运行后常驻系统托盘, 单击程序图标即可打开菜单进行管理。

![运行截图](screenshots/main.png)

### 特性

- 启动 sing-box 或 Xray 的 TUN 模式前, 随机化 TUN 接口名称, 避免与残留 TUN 设备冲突。
- 清理注册表网络设备名称列表中无用的 TUN 设备。
- 为 Xray 的 TUN 模式劫持物理网卡的 DNS 服务器为本地回环地址, 有效防止 DNS 泄露。
  - sing-box 的 TUN 模式支持严格路由, 所以不用额外设置。
  - 停止 Xray 时、退出软件时、打开软件时、开机启动时, 都会自动恢复物理网卡的 DNS 服务器为 DHCP。
  - **此逻辑可能会导致 Windows Defender 误杀本软件。**

## Release 目录结构

```
ladder
├── configs
│   ├── sing-box                        # 可切换的 sing-box 配置目录
│   │   └── with_xray.json              # 搭配 xray 使用的 sing-box 配置
│   ├── sing-box.json                   # 当前使用的 sing-box 配置
│   ├── xray                                            # 可切换的 xray 配置目录
│   │   ├── with_sing-box_[xhttp+reality].json          # 搭配 sing-box 使用的 xray 配置
│   │   └── olny_xray_上下行分离[xhttp+tls+cdn].json    # 仅运行 xray 时使用的 xray 配置
│   └── xray.json                                       # 当前使用的 xray 配置
├── icons
│   ├── green_circle.ico                # 核心程序运行中的应用图标
│   ├── ladder.ico                      # 托盘和通知的应用图标
│   ├── red_circle.ico                  # 核心程序未安装的应用图标
│   └── yellow_circle.ico               # 核心程序未运行的应用图标
├── settings.json                       # 主程序配置
├── sing-box_core                       # sing-box 工作目录
│   ├── cache.db
│   ├── libcronet.dll
│   └── sing-box.exe
├── ladder.exe                          # 主程序
└── xray_core                           # xray 工作目录
    ├── geoip.dat
    ├── geosite.dat
    ├── wintun.dll
    └── xray.exe
```

## 首次使用

1. 从 [Releases](https://github.com/senzyo/ladder/releases/latest) 下载对应架构的压缩包 (amd64 或 arm64) 。
2. 解压后, 编辑 `configs/sing-box/*.json` 或 `configs/xray/*.json` 配置你的节点。
3. 不要直接编辑 `configs/sing-box.json` 或 `configs/xray.json`, 防止切换配置时被覆盖。
4. 双击 `ladder.exe` 运行, UAC 提示时选择允许 (需要管理员权限, 因为涉及上面提到的 [特性](#特性))。
5. 在系统托盘中单击图标打开使用菜单。
6. **默认只启用了 Xray 核心**, 你可以自行切换。
7. 在菜单中点击 `更新核心` 来下载核心程序。
8. 在菜单中点击 `切换配置` 来切换配置, 切换后对应的核心会被运行。

## 配置文件
### sing-box 和 Xray 的配置文件

可以根据自身需求仅运行其中一个核心, 比如只用 Xray 运行 `configs\xray\olny_xray_*.json`。

也可以同时运行 sing-box 和 Xray 两个核心:

`configs` 目录下 `with_*.json` 的示例配置是用于双核心搭配的, 比如 `configs\sing-box\with_xray_ipv4_only.json` 和 `configs\xray\with_sing-box_[xhttp+reality].json` 这一组的分工如下:

```mermaid
flowchart LR
    A(用户软件) --> B(sing-box)
    B -->|直连流量| C(目标网站)
    B -->|需代理流量| D(本地 SOCKS)
    D --> E(Xray)
    E --> F(VPS)
    F --> C
```

> **要记得自定义这些配置文件, 而不是直接使用它们。**

### 本程序配置文件

编辑 `settings.json` 可调整以下设置:

| 配置项                               | 默认值           | 说明                                         |
| ------------------------------------ | ---------------- | -------------------------------------------- |
| `log.level`                          | `debug`          | 日志级别                                     |
| `log.max_size_mb`                    | `5`              | 日志文件大小上限 (MB) , 0 表示不限制         |
| `download.core.gh_proxy`             | 为空时表示不启用 | GitHub CDN 代理地址前缀                      |
| `download.ruleset`                   |                  | Xray 的规则集文件                            |
| `download.ruleset.geoip.last_update` |                  | `geoip.dat` 的上次更新时间, 使用 unix 时间戳 |
| `download.ruleset.interval_days`     | `3`              | 更新间隔 (天)                                |
| `download.retry.max_retries`         | `3`              | 下载重试次数                                 |
| `download.retry.delay_secs`          | `2`              | 重试间隔 (秒)                                |
| `core.mode`                          | `xray`           | 核心模式 (`xray` / `sing-box` / `both`)      |

修改后需重启程序生效。`core.mode` 也可通过菜单中的"切换核心"实时切换。

## 开发
### 依赖

- Rust stable toolchain
- Visual Studio 生成工具 2022
  - MSBuild 工具
  - 使用 C++ 的桌面开发
    - 可选:
      - MSVC v143 - VS 2022 C++ x64/x86 生成工具(最新)
      - Windows 11 SDK
      - 用于 Windows 的 C++ CMake 工具
  - 单个组件
    - MSVC v143 - VS 2022 C++ ARM64/ARM64EC 生成工具(最新)
- LLVM/Clang

### 编译

```powershell
# amd64
cargo build --release

# arm64 (vcvarsall.bat 路径需根据实际安装调整)
cmd /c "`"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat`" x64_arm64 && cargo build --release --target aarch64-pc-windows-msvc"
```

产出路径:

```text
target\release\ladder.exe                           # amd64
target\aarch64-pc-windows-msvc\release\ladder.exe   # arm64
```

### CI/CD

推送到 `main` 分支时自动运行 `cargo clippy` 和 `cargo test`。

推送 `v*` 标签或手动触发 workflow 时, 自动构建双架构并发布 GitHub Release。

`ci.ps1` 只用于本地构建打包。