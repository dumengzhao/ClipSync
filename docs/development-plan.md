# Tauri + Rust 跨平台剪贴板同步程序开发方案

# 一、项目概述

## 1.1 项目目标

开发一款支持 Windows / macOS / Linux 桌面三端的剪贴板同步工具，实现跨设备的文本、图片、文件实时同步。核心亮点是采用**文件延迟渲染（Delayed Rendering）**技术——复制文件时仅同步元数据，用户实际粘贴时才按需传输文件内容，提供接近原生的复制粘贴体验。

网络架构采用**纯 P2P 直连**：每台设备既是发送端也是接收端，局域网通过 mDNS 自动发现，跨网由用户手动输入对端地址直连。无中继服务器，无云依赖，数据不经过任何第三方。

## 1.2 核心特性

- **跨平台统一体验**：Windows、macOS、Linux 三端功能一致，复制即同步
- **文件延迟渲染**：大文件复制零等待，粘贴时按需传输，节省带宽
- **纯 P2P 直连**：终端即收即发，局域网 mDNS 自动发现，跨网手动配置地址直连
- **端到端加密**：AES-256-GCM 加密传输，设备配对基于 X25519 + PAKE 协议
- **极低资源占用**：基于 Tauri + Rust，包体积 5-15MB，内存占用 10-30MB

## 1.3 目标用户

多设备办公人群、开发者、设计师等需要频繁在多台电脑间传递内容的用户，尤其适合对文件同步速度和隐私有较高要求的场景。

## 1.4 性能指标

| 指标 | 目标值 |
|---|---|
| 冷启动时间 | < 1s |
| 空闲内存占用 | < 50MB |
| 空闲 CPU 占用 | < 1% |
| 局域网文本同步延迟 | < 100ms |
| 大文件延迟渲染首字节 | < 500ms |
| 安装包体积 | < 15MB |

# 二、技术选型

## 2.1 整体技术栈

| 层级 | 技术选型 |
|---|---|
| 前端 UI | Tauri + React/Vue + TypeScript |
| 核心逻辑 | Rust（异步运行时 Tokio） |
| 剪贴板操作 | 原生系统 API（分平台实现） |
| 网络通信 | WebSocket（tokio-tungstenite）+ TCP |
| 设备发现 | mDNS（mdns crate）+ 手动地址 |
| 加密传输 | AES-256-GCM（ring / aes-gcm）+ X25519 |
| 密钥存储 | 各平台系统密钥链 |
| 序列化 | Serde + Bincode / JSON |
| 系统托盘 | Tauri 内置 tray 模块 |

## 2.2 为什么选择 Tauri + Rust

**性能与体积优势。** 相比 Electron，Tauri 应用包体积可控制在 15MB 以内，内存占用降低 80% 以上，对于常驻后台的剪贴板工具至关重要。Rust 的零成本抽象和内存安全特性，确保了程序长期运行的稳定性。

**系统级集成能力。** Rust 可直接调用各平台原生系统 API，无需通过 Node.js 中间层。剪贴板延迟渲染需要深度对接 Win32 COM、AppKit、X11/Wayland 等底层接口，Rust 的 FFI 能力和丰富的 crate 生态使其成为最佳选择。

**跨平台一致性。** Tauri 官方支持 Windows、macOS、Linux 三端打包，Rust 核心代码可通过 `cfg` 条件编译实现平台差异化逻辑，最大化代码复用。

**安全性。** Rust 的内存安全模型 + Tauri 的沙箱机制，降低了剪贴板这类敏感数据处理的安全风险。

## 2.3 关键依赖 Crate

| 功能 | 推荐 Crate |
|---|---|
| 异步运行时 | tokio |
| WebSocket | tokio-tungstenite |
| mDNS 发现 | mdns |
| 加密对称 | aes-gcm |
| 加密非对称 | x25519-dalek |
| PAKE 配对 | spake2 |
| 密钥派生 | hkdf |
| 序列化 | serde + bincode |
| 哈希（快速） | blake3 / xxhash-rust |
| Windows API | windows（官方） |
| macOS Keychain | security-framework |
| Windows DPAPI | windows |
| Linux Secret | dbus-secret-service |
| macOS/AppKit | objc2 / icrate |
| Linux X11 | x11rb |
| Linux Wayland | smithay-client-toolkit |
| 错误处理 | anyhow / thiserror |
| 日志 | tracing + tracing-appender |
| 崩溃上报 | crash-handler + minidump |

# 三、系统架构设计

## 3.1 整体架构

系统采用**模块化分层架构**，核心逻辑全部由 Rust 实现，前端仅负责 UI 展示与用户交互。各模块通过统一的 Trait 接口解耦，便于平台差异化实现和单元测试。网络层为纯 P2P，每台设备同时扮演服务器与客户端角色。

```text
┌─────────────────────────────────────────┐
│         Tauri 前端 UI（React/Vue）        │
│     设备列表 / 设置面板 / 历史记录         │
├─────────────────────────────────────────┤
│            Tauri Command 层              │
│   前端 ↔ Rust 核心的命令桥接与事件转发     │
├─────────────────────────────────────────┤
│              Rust 核心层                 │
│  ┌─────────┬──────────┬──────────────┐  │
│  │ 剪贴板  │  传输层  │  设备管理    │  │
│  │  模块   │  模块    │  模块        │  │
│  └─────────┴──────────┴──────────────┘  │
│  ┌─────────┬──────────┬──────────────┐  │
│  │ 加密模块 │ 缓存模块 │ 配置管理     │  │
│  └─────────┴──────────┴──────────────┘  │
├─────────────────────────────────────────┤
│          平台原生 API 层                 │
│   Windows │  macOS  │  Linux           │
└─────────────────────────────────────────┘
```

## 3.2 核心模块职责

| 模块 | 职责 |
|---|---|
| 剪贴板模块 | 监听剪贴板变化、读取内容、写入内容、延迟渲染注册 |
| 传输模块 | WebSocket 信令通信、文件分片传输、P2P 直连管理 |
| 设备发现模块 | mDNS 局域网设备广播与发现、手动地址连接、设备配对管理 |
| 加密模块 | 端到端加密、设备密钥管理、签名验证、密钥链存储 |
| 缓存模块 | 已传输文件本地缓存、LRU 淘汰策略、临时文件管理 |
| 配置模块 | 用户配置持久化、同步规则、黑白名单、自启动管理 |
| 同步引擎 | 协调各模块、防回环判断、同步状态管理、冲突处理 |
| 可观测性模块 | 日志轮转、性能指标、崩溃上报 |

## 3.3 数据流概览

一次完整的文件剪贴板同步流程如下：

1. 用户在 A 端复制文件，剪贴板模块监听到变化，提取文件元数据
2. 同步引擎判断为新内容（非回环），生成同步唯一 ID 与逻辑时钟
3. 传输模块通过 WebSocket 将元数据 + 同步 ID 发送给已配对的 B 端
4. B 端收到后，调用剪贴板模块注册延迟渲染，写入自定义标记防回环
5. 用户在 B 端粘贴文件，系统触发延迟回调
6. B 端传输模块向 A 端请求文件分片数据（A 端同时是接收端也是发送端）
7. A 端读取本地文件，分片加密传输给 B 端
8. B 端接收分片并实时返回给系统，同时写入本地缓存

# 四、剪贴板核心模块

## 4.1 统一抽象接口

剪贴板模块通过统一的 Trait 接口抽象，各平台分别实现，上层逻辑无需关心平台差异。

```rust
use std::path::PathBuf;
use anyhow::Result;
use async_trait::async_trait;

/// 强类型设备 ID，避免与 String 混用
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// 强类型同步 ID
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SyncId(pub String);

/// 文件元数据（跨端统一传输结构）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub file_name: String,
    pub file_size: u64,
    pub is_dir: bool,
    pub relative_path: String,           // 目录内相对路径
    pub modified_at: u64,                // 修改时间戳
    pub mime_type: String,               // MIME 类型，跨平台识别
    pub hash: Option<String>,            // BLAKE3 哈希，可选（大文件懒计算）
}

/// 剪贴板内容类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipboardContent {
    Text(String),
    Image {
        data: Vec<u8>,                   // PNG 格式
        max_size: u32,                   // 大小上限，超过拒绝同步
    },
    Files(Vec<FileMeta>),
    Html { html: String, text: String },
}

/// 跨平台剪贴板统一接口
#[async_trait]
pub trait ClipboardProvider: Send + Sync {
    /// 读取当前剪贴板内容
    async fn read(&self) -> Result<ClipboardContent>;

    /// 写入剪贴板（基础模式）
    async fn write(&self, content: ClipboardContent) -> Result<()>;

    /// 延迟渲染模式写入文件
    async fn write_delayed_files<F>(
        &self,
        files: Vec<FileMeta>,
        sync_id: SyncId,
        fetch_cb: F,
    ) -> Result<()>
    where
        F: Fn(usize, u64, u32) -> Result<Vec<u8>> + Send + Sync + 'static;

    /// 监听剪贴板变化
    async fn watch(&self, cb: Box<dyn Fn() + Send>) -> Result<WatchHandle>;

    /// 检查剪贴板是否包含同步标记（防回环）
    async fn has_sync_mark(&self, sync_id: &SyncId) -> Result<bool>;
}
```

**设计要点：**

- `DeviceId` / `SyncId` 使用 newtype 模式，避免在函数签名中误传普通字符串
- `FileMeta` 增加 `mime_type` 字段，跨平台文件类型识别不依赖扩展名
- `FileMeta.hash` 改为 `Option`，大文件可懒计算或跳过
- `Image` 增加大小上限字段，防止 OOM
- `fetch_cb` 改为 `Send + Sync`，支持跨线程调用（系统回调线程与传输线程不同）

## 4.2 Windows 平台实现

**核心 API：** Win32 `user32.dll` 剪贴板系列函数 + Shell 剪贴板格式。

**监听方式：** `AddClipboardFormatListener` 注册窗口监听，接收 `WM_CLIPBOARDUPDATE` 消息。需要创建一个隐藏的消息窗口来接收系统消息。

**文本格式：** `CF_UNICODETEXT`，宽字符字符串。

**图片格式：** `CF_DIB`（设备无关位图），转换为 PNG 后统一传输。

**文件格式：** `CF_HDROP`（本地文件路径列表），对应 `DROPFILES` 结构体；延迟渲染使用 `CFSTR_FILEDESCRIPTOR` + `CFSTR_FILECONTENTS` 组合。

**COM 线程模型注意：** Windows 剪贴板涉及 COM 调用，必须明确线程模型。建议将剪贴板操作所在线程初始化为 STA（Single-Threaded Apartment），通过隐藏窗口的消息泵驱动。`IStream` 实现内部不要跨 apartment 传递对象。

**重试机制：** Windows 剪贴板是系统独占资源，`OpenClipboard` 可能因其他程序占用而失败。必须实现重试机制（建议间隔 10ms，最多重试 10 次，指数退避），失败时跳过本次同步。

## 4.3 macOS 平台实现

**核心 API：** AppKit 框架的 `NSPasteboard`。

**监听方式：** 订阅 `NSPasteboardDidChangeNotification` 通知，或轮询 `changeCount` 属性（兼容性更好，建议默认轮询，通知作为优化）。

**文本格式：** `NSPasteboardTypeString`。

**图片格式：** `NSPasteboardTypePNG`。

**文件格式：** `NSPasteboardTypeFileURL`（标准 `public.file-url` 类型）；延迟渲染通过 `NSPasteboardItem` 的 `setDataProvider` 注册数据提供者。

**沙盒提示：** Tauri 默认开启 App Sandbox 时，访问任意路径文件会受限。如果不上架 App Store，建议关闭沙盒以获得完整文件系统访问权限；上架则需处理安全书签（Security-Scoped Bookmarks）。

## 4.4 Linux 平台实现

Linux 是三端中最复杂的，存在 X11 和 Wayland 两套显示协议，需要分别适配。

**X11 环境：** 基于 X Selection 机制，分为 `PRIMARY`（选中即复制）和 `CLIPBOARD`（Ctrl+C）两个独立缓冲区。文件格式为 `text/uri-list`（file:// 协议路径列表），GNOME 特有 `x-special/gnome-copied-files`。监听使用 XFixes 扩展的 `SelectionNotify` 事件。

**Wayland 环境：** 无统一原生 API，依赖各合成器实现。
- 数据选择：`wl_data_source` / `wl_data_device`（标准协议）
- 剪贴板区分：`zwp_primary_selection_v1`（primary selection 协议，对应 X11 的 PRIMARY）
- 推荐库：`smithay-client-toolkit`（SCTK），纯 Rust 实现

**wl-clipboard 不推荐：** 调用外部命令行工具作为剪贴板后端是糟糕方案——大多数发行版默认未安装，且依赖外部二进制不可控，延迟渲染无法支持。应直接使用 SCTK 适配。

**兼容性建议：** Linux 端建议优先适配 X11，Wayland 逐步兼容。Wayland 延迟渲染受合成器支持度限制，不可用时降级为完整传输模式。初期 MVP 阶段 Linux 可降级为基础方案，延迟渲染功能后续迭代补充。

# 五、文件延迟渲染方案

## 5.1 方案概述

文件延迟渲染（Delayed Rendering）是本项目的核心技术亮点。传统的剪贴板同步方案需要先完整传输文件，再写入本地路径到剪贴板，大文件会造成长时间等待。延迟渲染的思路是：**复制时仅同步文件元数据，用户真正粘贴时才按需传输文件内容**，与系统原生复制粘贴体验一致。

| 对比维度 | 传统完整传输 | 延迟渲染方案 |
|---|---|---|
| 复制后可用时间 | 需等待完整传输 | 立即可用 |
| 带宽消耗 | 无论是否粘贴都传输 | 仅粘贴时传输 |
| 用户体验 | 大文件复制有明显等待 | 与本地复制一致 |
| 实现复杂度 | 低 | 高（需对接系统底层） |

## 5.2 Windows 平台实现

Windows 提供了专门的 Shell 剪贴板格式用于虚拟文件粘贴，是三端中实现最成熟的。

**核心原理：** 不写入传统的 `CF_HDROP`（本地文件路径），而是写入两个 Shell 格式：

- `CFSTR_FILEDESCRIPTOR`：文件描述符，立刻写入剪贴板，包含所有文件的元数据（文件名、大小、修改时间、是否为文件夹）
- `CFSTR_FILECONTENTS`：文件内容，用户粘贴时系统才会回调请求，此时再触发实际传输

**关键实现：** 需要手动实现 COM 接口 `IStream` 与 `IDataObject`，在 `Read` 方法中按需拉取文件分片并返回给系统。多文件场景下，系统会按索引逐个请求文件内容，需要维护索引与文件的映射关系。

> 注：以下为伪代码，仅示意接口和算法思路，实际实现需处理 COM 引用计数、 apartment 模型、错误路径等细节。

```rust
use windows::Win32::System::Com::{IStream, STATSTG, STGC};
use windows::core::implement;

/// 延迟文件流 - 实现 IStream 接口
#[implement(IStream)]
struct DelayedFileStream {
    file_index: usize,
    file_size: u64,
    position: u64,
    /// 通过 channel 向传输线程请求分片，避免阻塞 COM 线程
    fetch_handle: Arc<FetchHandle>,
}

impl IStream_Impl for DelayedFileStream {
    fn Read(&self, pv: *mut u8, cb: u32, pcbread: *mut u32) -> windows::core::Result<()> {
        // 通过 tokio Handle 在异步运行时上发起请求，并阻塞等待结果
        // 注意：必须设置超时（建议 30s），避免系统 Read 调用无限挂起
        let request = FetchRequest {
            file_index: self.file_index,
            offset: self.position,
            size: cb,
        };

        let data = self.fetch_handle
            .block_on_fetch(request, Duration::from_secs(30))
            .map_err(|e| windows::core::Error::from(e))?;

        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), pv, data.len());
            *pcbread = data.len() as u32;
        }
        self.position += data.len() as u64;
        Ok(())
    }

    fn Seek(&self, dlibmove: i64, dworigin: u32, _plibnew: *mut u64) -> windows::core::Result<()> {
        self.position = compute_new_position(self.position, self.file_size, dlibmove, dworigin)?;
        Ok(())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: u32) -> windows::core::Result<()> {
        unsafe {
            (*pstatstg).cbSize = self.file_size;
            (*pstatstg).type_ = STGTY_STREAM;
        }
        Ok(())
    }

    // 其他方法（Write、Clone、Commit、Revert 等）返回 E_NOTIMPL
}
```

**关键设计点：**

1. **COM 线程模型：** `DelayedFileStream` 实例必须存活于 STA 线程，通过隐藏窗口的消息泵处理 COM 调用
2. **超时保护：** `Read` 必须设置超时，对端离线或网络异常时及时返回错误，避免系统资源管理器挂起
3. **`IDataObject` 实现：** 实际写入剪贴板需要实现 `IDataObject` 接口，处理 `CFSTR_FILEDESCRIPTOR` 与 `CFSTR_FILECONTENTS` 两种 format 的 `GetData`/`QueryGetData` 调用
4. **引用计数：** 使用 `windows::core::implement` 宏自动生成 `IUnknown` 实现，确保 COM 对象生命周期正确

**文件夹支持：** `CFSTR_FILEDESCRIPTOR` 通过 `FILEDESCRIPTOR` 的 `dwFlags` 标记目录属性，原生支持文件夹结构。

## 5.3 macOS 平台实现

macOS 的 `NSPasteboard` 原生支持「数据提供者」模式，是三端中实现最优雅的。

**核心原理：** 通过 `NSPasteboardItem` 注册一个数据提供者（Data Provider），不直接写入文件数据；当用户粘贴时，系统回调提供者方法，此时再传输并返回数据。

**关键实现：** 实现 `NSPasteboardItemDataProvider` 协议，在 `pasteboard(_:item:provideDataForType:)` 回调中处理数据请求。声明支持 `public.file-url`、`public.data` 等类型，Finder 粘贴时会自动请求对应格式。

**Soundness 注意：** macOS 的数据提供者对象生命周期可能跨越多个 runloop tick，必须确保对象不会被提前释放。`self as *const _ as *mut Object` 这种写法是未定义行为——`self` 的生命周期可能短于粘贴板持有数据提供者的时间，程序退出或对象释放后再粘贴会崩溃。

> 注：以下为伪代码，仅示意协议结构，实际实现需要使用 `objc2` / `icrate` 的强类型绑定，并显式管理引用计数。

```rust
use objc2::rc::Retained;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardItemDataProvider};
use objc2_foundation::{NSData, NSString};

/// 数据提供者必须持有强引用，并通过 NSPasteboardItem 的 setDataProvider:forTypes: 注册
/// 注册后，pasteboard 会 retain 提供者，直到 pasteboard 内容被替换
#[derive(Debug)]
struct PasteboardDataProvider {
    files: Vec<FileMeta>,
    sync_id: SyncId,
    /// 通过 channel 与传输线程通信
    fetch_handle: Arc<FetchHandle>,
}

unsafe impl NSPasteboardItemDataProvider for PasteboardDataProvider {
    unsafe fn pasteboard_item_provide_data_for_type(
        &self,
        _item: &NSPasteboardItem,
        type_: &NSString,
    ) -> Retained<NSData> {
        let type_str = type_.to_string();
        // 解析 type，确定请求的是哪个文件的哪种数据
        let (file_index, offset, size) = self.parse_request(&type_str);

        let request = FetchRequest { file_index, offset, size };
        // 必须在主线程同步返回，但传输是异步的
        // 两种方案：
        // 1. 小文件：block_on 阻塞等待（< 100ms 可接受）
        // 2. 大文件：先返回空 NSData 占位，异步填充后调用
        //    noteChangesForTypes: 通知系统更新（更复杂但体验更好）
        let data = self.fetch_handle
            .block_on_fetch(request, Duration::from_secs(30))
            .unwrap_or_default();

        NSData::with_bytes(&data)
    }
}

impl PasteboardDataProvider {
    fn register_to_pasteboard(&self) -> Result<()> {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();

            let item = NSPasteboardItem::new();
            // types 数组需要按声明顺序匹配
            let types = vec![
                NSString::from_str("public.file-url"),
                NSString::from_str("public.data"),
            ];

            // 注册数据提供者，pasteboard 会 retain self
            // 必须将 self 包装为 Retained<Self>，确保引用计数正确
            let provider = Retained::new(self.clone());
            item.setDataProvider_forTypes(&provider, &types);

            pb.writeObjects(&[&item]);
        }
        Ok(())
    }
}
```

**关键设计点：**

1. **引用计数：** 使用 `objc2::rc::Retained` 显式管理 ObjC 对象生命周期，禁止裸指针 cast
2. **线程模型：** 回调默认在主线程触发，传输操作必须放到后台线程，通过 channel 通信
3. **超时保护：** 同 Windows，`provide_data_for_type` 必须在有限时间内返回
4. **空数据占位策略：** 大文件场景下，可先返回空 `NSData` 占位，后台异步填充后调用 `noteChangesForTypes:` 通知系统更新——但这需要 Finder 等粘贴应用支持延迟更新，兼容性需测试

## 5.4 Linux 平台实现

Linux 的剪贴板机制天生就是延迟设计的：你只需要声明「我拥有剪贴板、支持哪些 MIME 类型」，数据只有在其他程序请求时才发送。

### 5.4.1 X11 环境

**核心原理：** X Selection 机制，获取 `CLIPBOARD` 选择所有权，声明支持 `text/uri-list`、`application/octet-stream` 等 target。当文件管理器请求数据时，X 服务器发送 `SelectionRequest` 事件，此时再读取/传输文件并返回。

**推荐 crate：** `x11rb`（纯 Rust 实现）或 `xcb`。

**实现要点：**

- 必须维护一个 X11 事件循环线程，处理 `SelectionRequest` 与 `SelectionClear` 事件
- 多 target 支持时，需正确响应 `TARGETS` 请求，列出所有可用格式
- 数据传输通过 `XChangeProperty` 写入请求方窗口属性，大数据需 `INCR` 机制增量传输

### 5.4.2 Wayland 环境

**核心原理：** `wl_data_source` 机制，创建数据源并声明支持的 MIME 类型，提交给合成器。有客户端请求数据时，合成器回调 `send` 事件并提供文件描述符（fd），向 fd 写入数据即可。

**推荐 crate：** `smithay-client-toolkit`（SCTK）。

**primary selection：** Wayland 区分 primary selection（选中即复制）与 clipboard selection（Ctrl+C），通过 `zwp_primary_selection_v1` 协议管理。需根据用户配置决定是否同步 PRIMARY 缓冲区。

**兼容性坑：** GNOME/Nautilus、KDE/Dolphin 对文件剪贴板的 MIME 支持不一致，需要测试各桌面环境。建议初期 Linux 降级为基础完整传输方案，延迟渲染后续迭代。

**合成器支持矩阵：** Wayland 延迟渲染高度依赖合成器实现，需建立支持矩阵：

| 合成器 | clipboard | primary_selection | 文件 MIME |
|---|---|---|---|
| Mutter (GNOME) | 支持 | 部分支持 | 完整 |
| KWin (KDE) | 支持 | 支持 | 完整 |
| wlroots (Sway 等) | 支持 | 支持 | 完整 |
| Weston | 支持 | 不支持 | 部分 |

## 5.5 完整同步流程

1. **复制触发：** 用户在 A 端复制文件/文件夹
2. **元数据提取：** A 端解析剪贴板，提取所有文件的元数据，生成同步唯一 ID 与逻辑时钟
3. **元数据同步：** A 端通过 WebSocket 将「元数据 + 设备ID + 同步ID + 逻辑时钟」加密发送给 B 端
4. **延迟注册：** B 端收到后，调用 `write_delayed_files` 注册延迟剪贴板，同时写入自定义格式标记防回环
5. **粘贴触发：** 用户在 B 端资源管理器执行粘贴
6. **系统回调：** 操作系统回调 B 端的延迟数据提供者，请求指定偏移量的文件数据
7. **按需拉取：** B 端通过 `fetch_cb` 向 A 端发起分片请求（默认 2MB 一片）
8. **流式返回：** B 端将收到的分片实时写入系统数据流，边下边传
9. **本地缓存：** 传输完成后存入本地缓存目录，下次粘贴直接复用
10. **自动清理：** 定时清理超过 24 小时的缓存文件

# 六、网络同步架构

## 6.1 纯 P2P 直连架构

采用**纯 P2P 直连**架构，每台设备同时是服务器与客户端，监听本地端口并主动连接对端。无中继服务器，无云依赖，数据点对点加密传输。

```text
┌─────────────┐                    ┌─────────────┐
│   设备 A    │  ← WebSocket/TCP →  │   设备 B    │
│  (server +  │   双向直连          │  (server +  │
│   client)   │                    │   client)   │
└─────────────┘                    └─────────────┘
       ↑                                  ↑
       │ mDNS 自动发现 / 手动地址          │
       └────────── 局域网或跨网 ──────────┘
```

**核心特性：**

- **终端即收即发：** 每台设备同时监听本地端口（接受入站连接）并主动发起出站连接
- **无中继：** 数据点对点传输，不经过任何第三方
- **无 NAT 穿透：** 用户自行配置对端可达地址（局域网 IP 或公网 IP/域名），程序不主动尝试 STUN/TURN/UPnP
- **多对多：** 一台设备可同时与多台已配对设备建立连接，同步广播

## 6.2 设备发现与连接

设备发现支持两种方式，互补使用：

### 6.2.1 mDNS 自动发现（局域网）

基于 mDNS（Bonjour）协议，同网段设备自动广播与发现，无需手动输入 IP。使用 `mdns` crate 实现，服务类型 `_clipsync._tcp.local`。

发现流程：

1. 启动时注册 mDNS 服务，广播本机设备信息（设备名、端口、公钥指纹）
2. 监听 `_clipsync._tcp.local` 服务，发现同网段其他设备
3. 在 UI 显示已发现设备列表，用户点击配对
4. 配对完成后，自动建立 WebSocket 连接

### 6.2.2 手动地址连接（跨网或 mDNS 不可用）

用户在 UI 手动输入对端地址（IP:Port 或域名:Port），程序直接发起连接。适用于：

- 跨网段、异地网络
- mDNS 被禁用或防火墙拦截的局域网
- 用户已通过端口映射、VPN、ZeroTier 等方式打通网络的环境

UI 提供地址簿功能，保存常用对端地址与备注名。

### 6.2.3 连接管理

- **同时监听与主动连接：** 每台设备默认监听本地端口（接受入站），同时维护已配对设备的出站连接
- **重连机制：** 连接断开后指数退避重连（1s、2s、4s、8s...，上限 60s）
- **双连接避免：** A 主动连接 B 时，若 B 也已主动连接 A，保留一个连接（按 device_id 字典序选择关闭哪个）
- **心跳保活：** 每 30s 发送心跳，3 次未响应则断开重连

## 6.3 传输通道设计

每个 P2P 连接复用**单一 WebSocket 通道**，信令与文件分片都走同一连接的二进制帧。

**为何单通道：**

- **配置简单**：跨网穿透只需映射一个端口，用户体验最好
- **连接管理统一**：加密、重连、心跳只写一套代码
- **WebSocket 全双工**：信令和数据可并发，互不阻塞
- **剪贴板场景流量小**：文本/图片/按需文件分片（默认 2MB/片），WebSocket 帧开销可忽略
- **多文件并发**：通过 `sync_id + file_index + offset` 在同一连接上多路复用

**端口设计：** 默认端口 24681（可在配置中修改）。跨网穿透只需映射此单端口。所有通信均为 TCP（无 UDP 依赖，对内网穿透工具友好）。

**消息复用：** WebSocket 二进制帧首字节区分消息类型，信令 JSON 与文件 Bincode 共用同一连接：

```text
WebSocket binary frame:
  [1 byte: msg_type] [payload...]
    msg_type = 0x01 -> 信令 JSON
    msg_type = 0x02 -> 文件分片请求 Bincode
    msg_type = 0x03 -> 文件分片响应 Bincode
    msg_type = 0x04 -> 心跳
```

## 6.4 通信协议设计

**序列化格式：** 信令消息使用 JSON（便于调试），文件数据使用 Bincode（高性能二进制格式）。

**核心消息类型：**

| 消息类型 | 说明 |
|---|---|
| `ClipboardMeta` | 剪贴板内容元数据同步（文本/图片/文件列表） |
| `FileChunkRequest` | 请求指定文件的指定分片 |
| `FileChunkResponse` | 返回文件分片数据 |
| `FileComplete` | 文件传输完成确认 |
| `DeviceHello` | 连接建立后的设备握手 |
| `PairRequest` | 设备配对请求（PAKE 协议） |
| `PairAck` | 配对应答 |
| `UnpairRequest` | 解除配对，撤销密钥 |
| `Heartbeat` | 心跳保活 |
| `Presence` | 在线状态广播 |

**消息封装：** 所有消息外层包裹 `Envelope`，包含 `version`、`type`、`payload`、`hmac` 字段，便于版本协商与完整性校验。

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u8,
    pub msg_type: MessageType,
    pub payload: Vec<u8>,    // 序列化的具体消息
    pub hmac: [u8; 32],      // HMAC-SHA256，防篡改
}
```

## 6.5 文件传输策略

文件分片通过 6.3 节描述的同一 WebSocket 连接以二进制帧传输，无需独立通道。

**分片大小：** 默认 2MB/片，可根据网络状况动态调整（基于 RTT 与丢包率）。

**并发控制：** 单文件最多 3 个分片同时在途，单连接最多 9 个分片同时在途，通过 `sync_id + file_index + offset` 多路复用，避免内存堆积与网络拥塞。

**断点续传：** 传输中断后，从已确认的最后一个分片继续；支持基于 BLAKE3 哈希的秒传（哈希匹配则跳过传输）。

**流量控制：** 可配置最大传输速率，避免占用全部带宽。

**超时与重试：** 单分片请求超时 30s，重试 2 次；连续 3 次失败则中断本次传输，通知对端。

# 七、安全与配对协议

## 7.1 威胁模型

**关注的威胁：**

- 网络监听（被动窃听）：通过端到端加密防御
- 中间人攻击（主动篡改）：通过设备配对时的 PAKE 协议与公钥指纹人工核对防御
- 设备失窃后的密钥泄露：通过系统密钥链存储与设备撤销机制防御
- 重放攻击：通过 `sync_id` + 逻辑时钟 + 序列号防御
- 伪造消息：通过 HMAC-SHA256 签名防御

**不覆盖的威胁：**

- 设备已被恶意软件感染（攻击者直接读内存）：超出范围，建议配合杀毒软件
- 物理键盘记录器：超出范围

## 7.2 设备配对协议

配对采用 **SPAKE2**（Password-Authenticated Key Exchange）协议，避免明文比对配对码被中间人截获。配对码由发起方生成，6 位数字，10 分钟内有效。

### 7.2.1 配对流程

```text
设备 A (发起方)                    设备 B (应答方)
─────────────────                  ─────────────────
1. 生成配对码 P (6位数字)
2. 生成 X25519 临时密钥对
3. 通过 SPAKE2 计算:
   - 共享密钥 K
   - 公钥确认令牌 T_A
                                   4. 接收配对码 P
                                   5. 生成 X25519 临时密钥对
                                   6. 通过 SPAKE2 计算:
                                      - 共享密钥 K
                                      - 公钥确认令牌 T_B
7. 交换长期公钥 (用 K 加密)
8. 交换确认令牌 T_A, T_B
9. 验证 T_B -> 确认 B 持有 P
                                   10. 验证 T_A -> 确认 A 持有 P
11. 派生会话密钥:
    - 加密密钥: HKDF(K, "enc")
    - HMAC密钥: HKDF(K, "mac")
12. 保存对端长期公钥与设备信息
                                   13. 保存对端长期公钥与设备信息
14. 显示公钥指纹供用户人工核对
                                   15. 显示公钥指纹供用户人工核对
```

### 7.2.2 公钥指纹人工核对

配对完成后，两端均显示对端公钥的指纹（SHA-256 前 16 字节的十六进制表示，分 4 组显示，如 `A1B2-C3D4-E5F6-7890`）。建议用户通过其他可信渠道（电话、面对面）核对指纹，防御中间人。

UI 提供两个选项：

- **「已核对」**：用户已通过其他渠道核对，标记设备为「已信任」
- **「跳过」**：未核对，标记为「未完全信任」，仍可同步但 UI 提示风险

## 7.3 密钥管理

### 7.3.1 密钥类型

| 密钥 | 用途 | 生命周期 |
|---|---|---|
| 长期身份密钥对 | 设备身份，X25519 | 永久，存储于密钥链 |
| 配对会话密钥 | 单次配对派生，用于交换长期公钥 | 配对过程中临时 |
| 通信加密密钥 | AES-256-GCM 加密传输内容 | 会话级，连接断开后失效 |
| 通信 HMAC 密钥 | HMAC-SHA256 签名消息 | 会话级 |
| 文件加密密钥 | 单次同步的文件加密（可选） | 单次同步 |

### 7.3.2 密钥派生

配对完成后，每次建立连接时通过 ECDH 重新派生会话密钥：

```rust
fn derive_session_keys(
    my_static: &StaticSecret,
    peer_static: &PublicKey,
    rng: &mut impl CryptoRng,
) -> SessionKeys {
    let ephemeral_secret = StaticSecret::random_from_rng(rng);
    let ephemeral_public = PublicKey::from(&ephemeral_secret);

    // 双向 ECDH，防静态密钥泄露
    let shared1 = ephemeral_secret.diffie_hellman(peer_static);
    let shared2 = my_static.diffie_hellman(peer_static);

    let mut ikm = Vec::with_capacity(64);
    ikm.extend_from_slice(shared1.as_bytes());
    ikm.extend_from_slice(shared2.as_bytes());

    let mut okm = [0u8; 64];
    hkdf::Hkdf::<Sha256>::new(None, &ikm).expand(b"clipsync-session-v1", &mut okm).unwrap();

    SessionKeys {
        enc: okm[..32].try_into().unwrap(),
        mac: okm[32..].try_into().unwrap(),
        ephemeral_public,
    }
}
```

### 7.3.3 密钥存储

长期身份密钥对存储于各平台系统密钥链，**不落明文盘**：

| 平台 | 存储 | Crate |
|---|---|---|
| macOS | Keychain | `security-framework` |
| Windows | DPAPI / Credential Manager | `windows` |
| Linux | Secret Service / libsecret | `dbus-secret-service` |

密钥链条目命名：`com.clipsync.device.<device_id>.identity`。

## 7.4 端到端加密

**对称加密：** AES-256-GCM，96 位 nonce，128 位 tag。

**Nonce 生成：** 每个会话维护单调递增计数器，nonce = `session_id(8 bytes) || counter(4 bytes)`，避免 nonce 重用。

**消息封装：** 所有传输消息（信令与文件分片）均加密并 HMAC 签名：

```rust
pub struct EncryptedMessage {
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,    // 包含 GCM tag
    pub hmac: [u8; 32],         // HMAC-SHA256 over (nonce || ciphertext)
}
```

**加密流程：**

1. 序列化原始消息为 Bincode
2. 生成 nonce（计数器递增）
3. AES-256-GCM 加密
4. HMAC-SHA256 签名 (nonce || ciphertext)
5. 发送 `EncryptedMessage`

**解密流程：**

1. 校验 HMAC（先验签再解密，防 oracle 攻击）
2. AES-256-GCM 解密
3. 反序列化原始消息

## 7.5 设备撤销

设备失窃、丢失或退出时，需要撤销已配对关系：

### 7.5.1 主动解配对

用户在 UI 选择「解除配对」，本机：

1. 删除对端公钥与设备信息
2. 发送 `UnpairRequest` 通知对端（best effort，对端可能离线）
3. 重新派生所有会话密钥，断开现有连接

对端收到 `UnpairRequest` 后：

1. 删除本机公钥与设备信息
2. 断开连接，不再重连

### 7.5.2 被动撤销（密钥重置）

用户可在设置中「重置身份密钥」，本机：

1. 生成新的长期身份密钥对，替换密钥链中的旧密钥
2. 删除所有已配对设备信息
3. 重启所有连接

已配对设备因无法验证新密钥，自动断开并提示「对端身份变更，需重新配对」。

### 7.5.3 设备列表管理

UI 提供完整的已配对设备列表，显示：

- 设备名、平台、最后在线时间
- 公钥指纹与信任状态
- 「解除配对」按钮
- 「重新核对指纹」按钮

# 八、防回环机制

## 8.1 问题描述

同步程序最基础的问题：A 同步到 B 后，B 检测到剪贴板变化又回传给 A，A 收到后又同步给 B，造成**无限循环**。必须有可靠的防回环机制。

## 8.2 核心方案：自定义格式标记

**原理：** 写入剪贴板时，同时写入一个**自定义 MIME 格式标记**，内容包含「设备 ID + 同步唯一 ID」。每次读取剪贴板时，先检查是否存在该自定义标记：存在则直接忽略，不触发同步。

| 平台 | 自定义格式实现方式 |
|---|---|
| Windows | 通过 `RegisterClipboardFormat` 注册自定义格式，如 `application/x-clipsync-mark` |
| macOS | 使用自定义 UTI 类型，如 `com.clipsync.sync-mark` |
| Linux | 使用自定义 MIME 类型，如 `application/x-clipsync-mark` |

## 8.3 标记数据结构

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncMark {
    /// 写入此标记的设备 ID
    pub device_id: DeviceId,
    /// 本次同步的唯一 ID
    pub sync_id: SyncId,
    /// 同步时间戳（wall clock）
    pub timestamp: u64,
    /// 逻辑时钟（Lamport）
    pub lamport: u64,
    /// 内容哈希（BLAKE3）
    pub content_hash: String,
}

impl SyncMark {
    pub fn new(device_id: &DeviceId, content_hash: &str, lamport: u64) -> Self {
        Self {
            device_id: device_id.clone(),
            sync_id: SyncId(Uuid::new_v4().to_string()),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            lamport,
            content_hash: content_hash.to_string(),
        }
    }
}
```

## 8.4 防回环判断流程

1. 剪贴板监听到变化
2. **延迟读取：** 等待 100ms 再读取，避免剪贴板正在被写入时读取到不完整数据
3. 读取剪贴板内容，检查是否包含同步标记
4. 如果包含标记 → 是其他设备/本设备同步过来的，忽略，不触发同步
5. 如果不包含标记 → 是用户新复制的内容，触发同步
6. 同步到对端时，对端写入剪贴板同时写入标记

## 8.5 边界 case 与兜底

### 8.5.1 第三方剪贴板管理器剥标记

**问题：** 部分密码管理器、浏览器、剪贴板增强工具会清空剪贴板后重写，可能丢失自定义 MIME 标记，导致防回环失效。

**兜底方案：近期同步内容窗口**

- 维护「最近 5 分钟内已同步内容的哈希列表」
- 监听到变化时，若内容哈希在列表中，即使无标记也判定为回环
- 窗口大小可配置，默认 5 分钟，平衡误判与漏判

### 8.5.2 内容哈希去重的副作用

**问题：** 用户从 A 复制文本"hello"同步到 B，5 分钟后又复制同样的"hello"，按哈希去重会被忽略，但用户预期是「再次同步」。

**解决方案：**

- **短窗口（500ms）：** 仅在 500ms 内的相同内容视为回环，去重
- **长窗口（5 分钟）：** 仅作为防回环兜底，不主动抑制用户复制
- **手动触发：** UI 提供「强制同步当前剪贴板」按钮，绕过去重逻辑

### 8.5.3 大文件哈希计算

**问题：** 大文件计算 BLAKE3 哈希可能阻塞复制流程。

**解决方案：**

- **延迟计算：** 复制时仅计算文件大小与元数据，哈希字段置空
- **后台计算：** 复制完成后，后台线程异步计算哈希，更新 `FileMeta`
- **粘贴时计算：** 接收端在文件完整传输后计算哈希，校验完整性
- **算法选择：** BLAKE3，性能优于 SHA-256，并行友好

### 8.5.4 短时高频复制

**问题：** 用户快速连续复制（如复制多段文本），每次变化都触发同步可能造成抖动。

**解决方案：** 防抖动窗口 200ms，窗口内最后一次变化才触发同步。

# 九、冲突与一致性

## 9.1 一致性语义

系统提供**最终一致性**，不保证严格顺序：

- 多设备并发复制时，最终所有设备的剪贴板内容一致
- 临时性不一致是允许的（如 A 复制后 100ms 内 B 复制，B 端可能短暂显示 A 的内容再切换）
- 不提供「撤销」或「历史回滚」语义

## 9.2 逻辑时钟

采用 **Lamport 逻辑时钟**避免依赖系统时钟：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LamportClock {
    counter: u64,
}

impl LamportClock {
    pub fn new() -> Self {
        Self { counter: 0 }
    }

    /// 本地事件（如复制）触发 tick
    pub fn tick(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    /// 收到对端消息时更新
    pub fn observe(&mut self, other: u64) -> u64 {
        self.counter = self.counter.max(other) + 1;
        self.counter
    }
}
```

**用途：**

- 每次同步消息携带 `lamport` 字段
- 接收端通过 `observe` 更新本地时钟
- 多设备并发时，按 `lamport` 排序解决冲突

## 9.3 多设备冲突处理

**场景：** A、B、C 三台设备，A 和 B 几乎同时复制不同内容。

**处理规则：**

1. 每次复制生成 `(lamport, device_id)` 二元组
2. 收到多个并发同步时，按 `(lamport, device_id)` 字典序比较，最大者胜出
3. 败者的内容被覆盖，但保留在历史记录中
4. UI 在冲突时显示「内容已被设备 X 覆盖」提示

**时钟漂移容忍：** Lamport 时钟不依赖 wall clock，系统时间不准不影响正确性。wall clock 仅用于 UI 显示与缓存清理。

## 9.4 多跳传播

**问题：** A → B → C 传播时，sync_id 应保持一致还是逐跳变化？

**方案：** 保持一致。A 生成的 `sync_id` 在 B、C 间传播时不变，便于追踪与去重。每跳的 `lamport` 递增，但 `sync_id` 不变。

**去重：** 设备维护「最近处理的 sync_id 集合」（24 小时窗口），收到已处理的 sync_id 直接忽略，防止环路。

# 十、项目代码结构

## 10.1 目录结构

项目采用标准的 Tauri + Rust 项目结构，核心逻辑放在 `src-tauri/src` 下，按功能模块划分。前端代码放在 `src` 目录。

```text
clipboard-sync/
├── src/                          # 前端代码（React/Vue）
│   ├── components/               # UI 组件
│   ├── pages/                    # 页面
│   ├── stores/                   # 状态管理
│   ├── api/                      # Tauri 命令封装
│   └── App.tsx
├── src-tauri/                    # Rust 后端
│   ├── src/
│   │   ├── main.rs               # 入口
│   │   ├── tauri_cmd.rs          # Tauri 命令定义
│   │   ├── error.rs              # 集中错误类型（thiserror）
│   │   ├── clipboard/            # 剪贴板模块
│   │   │   ├── mod.rs            # 统一 Trait 定义
│   │   │   ├── types.rs          # ClipboardContent, FileMeta 等
│   │   │   ├── windows.rs        # Windows 实现
│   │   │   ├── macos.rs          # macOS 实现
│   │   │   └── linux.rs          # Linux 实现（X11 + Wayland）
│   │   ├── transfer/             # 传输模块
│   │   │   ├── mod.rs
│   │   │   ├── websocket.rs      # WebSocket 单通道（信令 + 文件分片）
│   │   │   └── file_stream.rs    # 文件流式传输
│   │   ├── discovery/            # 设备发现模块
│   │   │   ├── mod.rs
│   │   │   ├── mdns.rs           # mDNS 自动发现
│   │   │   └── manual.rs         # 手动地址连接
│   │   ├── crypto/               # 加密模块
│   │   │   ├── mod.rs
│   │   │   ├── aead.rs           # AES-256-GCM
│   │   │   ├── kdf.rs            # HKDF 密钥派生
│   │   │   ├── pake.rs           # SPAKE2 配对
│   │   │   └── keystore.rs       # 平台密钥链封装
│   │   ├── cache/                # 缓存模块
│   │   │   ├── mod.rs
│   │   │   └── file_cache.rs     # 文件缓存管理（LRU）
│   │   ├── config/               # 配置模块
│   │   │   ├── mod.rs
│   │   │   ├── settings.rs       # 用户设置
│   │   │   └── migration.rs      # 配置 schema 迁移
│   │   ├── sync/                 # 同步引擎
│   │   │   ├── mod.rs
│   │   │   ├── engine.rs         # 同步引擎
│   │   │   ├── anti_loop.rs      # 防回环
│   │   │   └── conflict.rs       # 冲突解决（Lamport 时钟）
│   │   ├── device/               # 设备管理
│   │   │   ├── mod.rs
│   │   │   ├── pairing.rs        # 设备配对
│   │   │   └── registry.rs       # 已配对设备注册表
│   │   ├── update/               # 自动更新
│   │   │   └── mod.rs
│   │   └── obs/                  # 可观测性
│   │       ├── logging.rs        # 日志轮转
│   │       ├── metrics.rs        # 性能指标
│   │       └── crash.rs          # 崩溃上报
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── build.rs
│   └── icons/
├── .github/                      # GitHub Actions 配置
│   └── workflows/
│       ├── ci.yml                # PR 检查（lint + test + build 验证）
│       ├── nightly.yml           # 夜间构建，产物上传
│       ├── release.yml           # tag 触发的正式发布
│       └── security.yml          # 依赖审计与安全扫描
├── scripts/                      # 本地辅助脚本
│   ├── local-ci.sh               # 本地 CI 等价脚本
│   ├── setup-macos-keychain.sh   # macOS 本地密钥链配置
│   └── generate-update-key.sh    # 生成更新签名密钥
├── .editorconfig                 # 编辑器一致性配置
├── .gitignore
├── .npmrc                        # npm 配置
├── clippy.toml                   # clippy 严格配置
├── rust-toolchain.toml           # 锁定 Rust 版本
├── Cargo.toml                    # 工作区根
├── package.json
└── README.md
```

## 10.2 模块依赖关系

```text
tauri_cmd.rs
    ↓
sync/engine.rs (同步引擎 - 核心协调)
    ↓       ↓       ↓       ↓
clipboard  transfer  discovery  device
    ↓       ↓       ↓         ↓
  平台     WebSocket  mDNS    配对
  原生     TCP 直连   手动    管理
    ↓       ↓
  cache   crypto ←→ keystore
    ↓
  config
    ↓
  obs (横切关注点，被所有模块依赖)
```

## 10.3 Tauri 配置要点

**tauri.conf.json 关键配置：**

- `app.windows`：主窗口配置，设置初始隐藏，通过托盘图标控制显示
- `app.trayIcon`：启用系统托盘（Tauri v2 字段名），配置托盘图标和菜单
- `security.csp`：内容安全策略，限制前端权限
- `bundle.macOS`：macOS 打包配置，设置权限声明
- `bundle.windows`：Windows 打包配置，设置安装器选项
- `plugins.updater`：启用 Tauri 自带的自动更新插件

**macOS 权限：** 剪贴板访问和网络客户端权限需要在 `Info.plist` 中声明。如果需要开机自启，使用 `launchagent` payload（通过 Tauri 的 autostart 插件）。

## 10.4 核心数据结构

```rust
/// 设备信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device_id: DeviceId,
    pub device_name: String,
    pub platform: Platform,
    pub listen_addr: Option<String>,    // 监听地址
    pub listen_port: u16,
    pub public_key: PublicKey,          // 强类型，X25519 公钥
    pub fingerprint: String,            // SHA-256 前 16 字节十六进制
    pub is_online: bool,
    pub last_seen: u64,
    pub trust_level: TrustLevel,        // Unverified / Verified
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustLevel {
    Unverified,
    Verified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Platform {
    Windows,
    MacOS,
    Linux,
}

/// 同步消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncMessage {
    /// 剪贴板内容同步
    ClipboardUpdate {
        sync_id: SyncId,
        source_device: DeviceId,
        content: ClipboardContent,
        timestamp: u64,
        lamport: u64,
    },
    /// 文件分片请求
    FileChunkRequest {
        sync_id: SyncId,
        file_index: usize,
        offset: u64,
        size: u32,
    },
    /// 文件分片响应
    FileChunkResponse {
        sync_id: SyncId,
        file_index: usize,
        offset: u64,
        data: Vec<u8>,
    },
    /// 文件传输完成确认
    FileComplete {
        sync_id: SyncId,
        file_index: usize,
        hash: Option<String>,
    },
    /// 心跳
    Heartbeat {
        lamport: u64,
    },
}

/// 用户配置（敏感字段不存于此，存于密钥链）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub device_name: String,
    pub auto_start: bool,
    pub sync_text: bool,
    pub sync_image: bool,
    pub sync_file: bool,
    pub max_file_size_mb: u64,
    pub max_image_size_mb: u32,
    pub listen_port: u16,
    pub enable_mdns: bool,
    pub manual_addresses: Vec<ManualAddress>,
    pub sync_primary_selection: bool,   // Linux only
    pub cache_ttl_hours: u32,
    pub theme: Theme,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualAddress {
    pub label: String,
    pub addr: String,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Theme {
    System,
    Light,
    Dark,
}
```

**注意：** `AppConfig` 不再包含 `encryption_key` 字段——密钥由加密模块独立管理，存储于系统密钥链。

# 十一、工程化与发布

## 11.1 测试策略

### 11.1.1 单元测试

- 加密、密钥派生、序列化等纯逻辑模块：覆盖率目标 > 90%
- 平台剪贴板抽象层：通过 mock trait 测试上层逻辑
- 防回环、冲突解决：构造边界 case 测试

### 11.1.2 集成测试

- 启动两个进程实例，模拟 P2P 同步全流程
- 文件延迟渲染：在测试环境触发实际粘贴（平台特定，CI 上需 headless 模拟）
- 网络异常：通过 `tc` 或 `proxy` 注入延迟与丢包

### 11.1.3 平台 CI 矩阵

| 平台 | CI Runner | 测试范围 |
|---|---|---|
| Windows | windows-latest | 单元 + 集成 + 打包 |
| macOS | macos-latest | 单元 + 集成 + 打包 |
| Linux | ubuntu-latest | 单元 + 集成 + 打包（X11 + Wayland） |

Linux Wayland 测试使用 `weston-headless` 启动虚拟合成器。

### 11.1.4 剪贴板 mock

为支持 CI 上的集成测试，提供 `MockClipboardProvider` 实现 `ClipboardProvider` trait，通过内存模拟剪贴板读写，不依赖真实系统 API。

## 11.2 CI/CD（基于 GitHub Actions）

使用 GitHub Actions 实现多平台持续集成与发布。共 4 个工作流文件，职责分离：

| 工作流 | 触发条件 | 作用 |
|---|---|---|
| `ci.yml` | push / PR | lint、test、build 验证（不签名不打包） |
| `nightly.yml` | schedule（每日 02:30 UTC）+ 手动 | 构建三端安装包，上传到 nightly release |
| `release.yml` | tag `v*` | 构建签名版本，发布正式 release，更新 `latest.json` |
| `security.yml` | schedule + PR | 依赖审计（cargo-deny）、漏洞扫描 |

### 11.2.1 构建矩阵

| 平台 | Runner | Target Triple | 产物 |
|---|---|---|---|
| Linux x64 | `ubuntu-22.04` | `x86_64-unknown-linux-gnu` | `.AppImage` / `.deb` / `.rpm` |
| macOS ARM64 | `macos-14` | `aarch64-apple-darwin` | `.dmg` |
| macOS x64 | `macos-13` | `x86_64-apple-darwin` | `.dmg` |
| Windows x64 | `windows-latest` | `x86_64-pc-windows-msvc` | `.msi` / `-setup.exe` |

> 注：`macos-14` 是 Apple Silicon runner，`macos-13` 是 Intel runner。两端分别构建避免交叉编译 COM/ObjC 桥接的复杂性。

### 11.2.2 ci.yml（PR 检查）

```yaml
name: CI

on:
  pull_request:
  push:
    branches: [main]

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1
  RUSTFLAGS: "-D warnings"

jobs:
  lint-test:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-22.04, macos-14, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt

      - uses: swatinem/rust-cache@v2

      - uses: actions/setup-node@v4
        with:
          node-version: '20'
          cache: 'npm'

      - name: Install Linux system deps
        if: runner.os == 'Linux'
        run: |
          sudo apt-get update
          sudo apt-get install -y \
            libgtk-3-dev \
            libwebkit2gtk-4.1-dev \
            libayatana-appindicator3-dev \
            librsvg2-dev \
            patchelf

      - run: npm ci
      - run: cargo fmt --all -- --check
      - run: cargo clippy --all-targets --all-features -- -D warnings
      - run: cargo test --all --all-features
      - run: npm run lint
      - run: npm run build

      - name: Verify Tauri config
        run: cargo build --manifest-path src-tauri/Cargo.toml --no-default-features
```

**关键点：**

- `RUSTFLAGS: "-D warnings"` 把所有 warning 升级为 error，避免警告积累
- `concurrency` 配置取消同 PR 旧 run，省额度
- `swatinem/rust-cache@v2` 缓存 Cargo 编译产物，二次构建快 5-10 倍
- `libwebkit2gtk-4.1-dev` 是 Tauri 在 Linux 的硬依赖
- 最后一步「Verify Tauri config」用 `--no-default-features` 验证 feature flag 配置正确

### 11.2.3 nightly.yml（夜间构建）

```yaml
name: Nightly

on:
  schedule:
    - cron: '30 2 * * *'   # 02:30 UTC
  workflow_dispatch:        # 支持手动触发

permissions:
  contents: write           # 需要写 release 资产

jobs:
  build:
    strategy:
      fail-fast: false
      matrix:
        include:
          - os: ubuntu-22.04
            target: x86_64-unknown-linux-gnu
            label: linux-x64
          - os: macos-14
            target: aarch64-apple-darwin
            label: macos-arm64
          - os: macos-13
            target: x86_64-apple-darwin
            label: macos-x64
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            label: windows-x64
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - uses: swatinem/rust-cache@v2
        with:
          key: ${{ matrix.target }}

      - uses: actions/setup-node@v4
        with:
          node-version: '20'
          cache: 'npm'

      - name: Install Linux system deps
        if: runner.os == 'Linux'
        run: |
          sudo apt-get update
          sudo apt-get install -y \
            libgtk-3-dev \
            libwebkit2gtk-4.1-dev \
            libayatana-appindicator3-dev \
            librsvg2-dev \
            patchelf

      - run: npm ci

      - name: Build Tauri app (unsigned)
        uses: tauri-apps/tauri-action@v0
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        with:
          tagName: nightly
          releaseName: 'Nightly Build'
          releaseBody: '自动夜间构建，未经签名，仅用于测试。'
          releaseDrafts: false
          prerelease: true
          args: --target ${{ matrix.target }}
          # nightly 不签名，仅验证构建链路
```

### 11.2.4 release.yml（正式发布）

```yaml
name: Release

on:
  push:
    tags: ['v*']

permissions:
  contents: write

jobs:
  release:
    strategy:
      fail-fast: false
      matrix:
        include:
          - os: ubuntu-22.04
            target: x86_64-unknown-linux-gnu
            label: linux-x64
          - os: macos-14
            target: aarch64-apple-darwin
            label: macos-arm64
          - os: macos-13
            target: x86_64-apple-darwin
            label: macos-x64
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            label: windows-x64
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - uses: swatinem/rust-cache@v2
        with:
          key: ${{ matrix.target }}

      - uses: actions/setup-node@v4
        with:
          node-version: '20'
          cache: 'npm'

      - name: Install Linux system deps
        if: runner.os == 'Linux'
        run: |
          sudo apt-get update
          sudo apt-get install -y \
            libgtk-3-dev \
            libwebkit2gtk-4.1-dev \
            libayatana-appindicator3-dev \
            librsvg2-dev \
            patchelf

      - run: npm ci

      # ============ 构建并发布 ============
      # 不签名（macOS ad-hoc / Windows 不签名），仅 Tauri updater 签名
      - name: Build and release
        uses: tauri-apps/tauri-action@v0
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          # Tauri updater 签名密钥（免费 Ed25519，唯一需要的签名密钥）
          TAURI_PRIVATE_KEY: ${{ secrets.TAURI_PRIVATE_KEY }}
          TAURI_KEY_PASSWORD: ${{ secrets.TAURI_KEY_PASSWORD }}
        with:
          tagName: ${{ github.ref_name }}
          releaseName: 'ClipSync ${{ github.ref_name }}'
          releaseBody: |
            ## 下载说明

            ### macOS
            首次打开会提示「未验证开发者」，请：
            1. 右键点击应用 -> 「打开」->「打开」
            2. 或终端执行：`xattr -dr com.apple.quarantine /Applications/ClipSync.app`

            ### Windows
            首次运行会看到 SmartScreen 警告，点击「更多信息」->「仍要运行」。

            ### Linux
            AppImage 直接运行；deb/rpm 用对应包管理器安装。

            ### 完整性校验
            下载 SHA256SUMS.txt，执行 `shasum -a 256 -c SHA256SUMS.txt` 校验。
          releaseDraft: true
          prerelease: false
          args: --target ${{ matrix.target }}

      # ============ 生成 SHA256 校验文件 ============
      - name: Generate SHA256 checksums
        if: always()
        shell: bash
        run: |
          cd src-tauri/target
          find . -type f \( -name "*.dmg" -o -name "*.msi" -o -name "*-setup.exe" \
            -o -name "*.AppImage" -o -name "*.deb" -o -name "*.rpm" \) \
            -exec shasum -a 256 {} \; > SHA256SUMS-${{ matrix.label }}.txt
          cat SHA256SUMS-${{ matrix.label }}.txt

      - name: Upload checksums
        if: always()
        uses: softprops/action-gh-release@v2
        with:
          files: src-tauri/target/SHA256SUMS-${{ matrix.label }}.txt
```

**与签名相关的关键差异：**

- 没有 macOS 证书导入步骤，Tauri 默认 ad-hoc 签名
- 没有 Windows 证书导入步骤，安装包不签名
- 仅注入 `TAURI_PRIVATE_KEY` / `TAURI_KEY_PASSWORD` 用于 updater 签名
- 末尾生成 SHA256 校验文件并上传到 release

### 11.2.5 security.yml（安全审计）

```yaml
name: Security

on:
  schedule:
    - cron: '0 6 * * 1'   # 每周一 06:00 UTC
  pull_request:
    paths:
      - '**/Cargo.toml'
      - '**/Cargo.lock'
      - '**/package.json'
      - '**/package-lock.json'
  workflow_dispatch:

jobs:
  cargo-audit:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - uses: rustsec/audit-check@v2.0.0
        with:
          token: ${{ secrets.GITHUB_TOKEN }}

  cargo-deny:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2
        with:
          arguments: --all-features

  npm-audit:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: '20'
      - run: npm audit --audit-level=high
```

### 11.2.6 所需 GitHub Secrets

项目不购买付费签名证书，所需 Secrets 极少：

| Secret | 用途 | 配置方式 |
|---|---|---|
| `TAURI_PRIVATE_KEY` | Tauri updater 签名私钥（Ed25519，免费生成） | `npm run tauri signer generate` 生成后粘贴 |
| `TAURI_KEY_PASSWORD` | Tauri updater 私钥密码 | 生成时设置的密码 |

`GITHUB_TOKEN` 由 Actions 自动注入，无需手动配置。

**完整配置流程（约 5 分钟）：**

```bash
# 1. 本地生成 Tauri updater 密钥对
npm run tauri signer generate -- -w ~/.tauri/clipsync.key
# 输出公钥（用于 tauri.conf.json）和私钥（用于 Secret）

# 2. 公钥写入 src-tauri/tauri.conf.json
#    plugins.updater.pubkey = "<生成的公钥>"

# 3. 私钥与密码填入 GitHub 仓库 Settings -> Secrets and variables -> Actions
#    TAURI_PRIVATE_KEY = "<生成的私钥>"
#    TAURI_KEY_PASSWORD = "<你设置的密码>"

# 4. 推送 tag 触发 release
git tag v0.1.0
git push origin v0.1.0
```
### 11.2.7 缓存与构建优化

- **Cargo 缓存：** `swatinem/rust-cache@v2` 自动缓存 `target/`，按 OS + target + Cargo.lock 哈希分桶
- **npm 缓存：** `actions/setup-node@v4` 内置 `cache: 'npm'`
- **并行矩阵：** `fail-fast: false` 确保一个平台失败不取消其他平台
- **构建超时：** 单 job 上限 60 分钟，超时自动取消
- **产物保留：** nightly 7 天，release 永久

### 11.2.8 本地复现 CI

`scripts/local-ci.sh` 提供与 CI 等价的本地验证脚本，开发者提交前应运行：

```bash
#!/usr/bin/env bash
set -euo pipefail

echo "[1/5] cargo fmt --check"
cargo fmt --all -- --check

echo "[2/5] cargo clippy"
cargo clippy --all-targets --all-features -- -D warnings

echo "[3/5] cargo test"
cargo test --all --all-features

echo "[4/5] npm lint"
npm run lint

echo "[5/5] npm build"
npm run build

echo "All checks passed."
```

配合 git pre-commit hook 自动调用，确保不合规代码无法提交。

## 11.3 签名方案（仅免费证书）

项目不上架 App Store / Mac App Store，不购买付费代码签名证书。采用以下免费方案：

| 平台 | 签名方式 | 用户首次安装体验 |
|---|---|---|
| macOS | Ad-hoc 签名（`codesign -s -`） | 需右键打开或 `xattr -d` 去隔离属性 |
| Windows | 不签名，附 SHA256 校验 | SmartScreen 警告，点击「仍要运行」 |
| Linux | 不签名，附 SHA256 校验 | 无警告 |
| 跨平台 | Tauri updater Ed25519 签名 | 自动更新校验通过 |

### 11.3.1 macOS（Ad-hoc 签名）

- **签名方式：** Ad-hoc，无身份证书，仅做完整性签名
- **配置：** `tauri.macos.conf.json` 中不配置 `signingIdentity`，Tauri 默认 ad-hoc 签名
- **Gatekeeper 绕过：** 用户首次打开需：
  - 右键点击应用 -> 「打开」->「打开」
  - 或终端执行：`xattr -dr com.apple.quarantine /Applications/ClipSync.app`
- **README 必须说明：** 在下载说明中明确告知用户如何绕过 Gatekeeper
- **Hardened Runtime：** 不启用（无公证需求）
- **Entitlements：** 仍需声明网络权限（`com.apple.security.network.client/server`），但不启用 sandbox

> 注：macOS 公证（notarization）必须付费 Apple Developer Program（$99/年），本项目不做公证。用户首次运行会有「未验证开发者」提示，需手动绕过。这是不上架 App Store 的免费工具的常见取舍。

### 11.3.2 Windows（不签名 + SHA256 校验）

- **签名方式：** 不签名。Windows 受信任 CA 颁发的代码签名证书均为付费，本项目不购买
- **SmartScreen 警告：** 用户首次运行会看到「Windows 已保护你的电脑」提示，点击「更多信息」->「仍要运行」
- **README 必须说明：** 在下载说明中提供 SmartScreen 绕过截图或步骤
- **完整性校验：** CI 在 release 资产为 `.msi` / `.exe` 附带 `.sha256` 文件
- **后续选项：** 如未来用户量增长，可通过 [SignPath.io](https://signpath.org) 申请免费代码签名（仅限开源项目，需项目维护 6 个月以上）

### 11.3.3 Linux（不签名 + SHA256 校验）

- **打包格式：** 优先 AppImage（无需安装、跨发行版），补充 deb / rpm
- **签名：** AppImage / deb / rpm 均不签名，Linux 桌面无强制签名要求
- **完整性校验：** CI 在 release 资产为每个包附带 `.sha256` 文件
- **Flatpak：** 后续可选上传 Flathub（免费，需通过审核），不在 GitHub Actions 内
- **Snap：** 后续可选上传 Snap Store（免费），不在 GitHub Actions 内

### 11.3.4 Tauri Updater 签名（免费，必须配置）

自动更新使用 Tauri 内置的 Ed25519 签名验证，**密钥对免费生成，是项目唯一需要的签名密钥**：

```bash
# 本地生成密钥对
npm run tauri signer generate -- -w ~/.tauri/clipsync.key
# 输出：
#   公钥（写入 tauri.conf.json 的 plugins.updater.pubkey）
#   私钥（填入 GitHub Secret: TAURI_PRIVATE_KEY）
#   密码（填入 GitHub Secret: TAURI_KEY_PASSWORD）

# 验证签名
npm run tauri signer verify -- ~/.tauri/clipsync.key.pub
```

**为什么 updater 签名重要：** 即使安装包本身不签名，自动更新时客户端会校验 Ed25519 签名，防止更新源被篡改后注入恶意更新。这是免费方案下最关键的安全保障。

### 11.3.5 完整性校验生成

CI 在 release 工作流末尾为每个产物生成 SHA256：

```yaml
- name: Generate SHA256 checksums
  shell: bash
  run: |
    cd src-tauri/target
    find . -type f \( -name "*.dmg" -o -name "*.msi" -o -name "*-setup.exe" \
      -o -name "*.AppImage" -o -name "*.deb" -o -name "*.rpm" \) \
      -exec shasum -a 256 {} \; > SHA256SUMS.txt

    - name: Upload checksums
      uses: softprops/action-gh-release@v2
      with:
        files: src-tauri/target/SHA256SUMS.txt
```

用户下载后可校验：

```bash
shasum -a 256 -c SHA256SUMS.txt
```

## 11.4 自动更新

使用 Tauri 的 `updater` 插件：

- **更新源：** GitHub Releases，每个 release 附带 `latest.json` 元数据
- **签名校验：** 更新包使用项目公钥签名，客户端校验签名后才安装
- **增量更新：** 暂不实现，全量替换
- **回滚：** 安装失败自动回滚到上一版本
- **更新策略：** 默认检查更新但需用户确认，可在设置中改为自动安装

## 11.5 可观测性

### 11.5.1 日志

- **库：** `tracing` + `tracing-appender`
- **轮转：** 按天滚动，保留最近 7 天
- **位置：**
  - macOS: `~/Library/Logs/clipsync/`
  - Windows: `%APPDATA%\clipsync\logs\`
  - Linux: `~/.local/share/clipsync/logs/`
- **敏感信息脱敏：** 文件名、路径、设备名在日志中可能保留（便于调试），剪贴板内容**绝不**记录
- **UI 导出：** 设置面板提供「导出日志」按钮，打包最近 7 天日志为 zip

### 11.5.2 性能指标

本地内存中维护，不外传：

- 同步成功率（最近 24 小时）
- 平均同步延迟
- 文件传输吞吐量
- 内存占用
- 连接状态

UI 设置面板显示，便于用户自检。

### 11.5.3 崩溃上报

- **库：** `crash-handler` + `minidump`
- **生成 minidump：** 崩溃时写入本地 `crash_dumps/` 目录
- **用户授权上报：** 崩溃后下次启动时弹窗询问是否上报，**默认不上报**
- **上报目标：** 可配置，默认无（用户可自建 Sentry）
- **隐私：** minidump 可能包含内存片段，上报前弹窗明确告知

## 11.6 开发规范（CI 兼容性要求）

为使 GitHub Actions 多平台构建稳定可靠，开发阶段必须遵守以下规范。任何违反规范的 PR 将被 CI 自动拒绝。

### 11.6.1 Cargo Workspace 约束

**单工作区，所有 Rust 代码在 `src-tauri/Cargo.toml`**，避免多 crate 管理开销。

```toml
# src-tauri/Cargo.toml
[package]
name = "clipsync"
version = "0.1.0"
edition = "2021"
rust-version = "1.75"   # 锁定最低 Rust 版本，与 rust-toolchain.toml 一致

[dependencies]
# 平台无关依赖直接列
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
anyhow = "1"
thiserror = "1"

# 平台特定依赖通过 cfg 区分
[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.58", features = [...] }

[target.'cfg(target_os = "macos")'.dependencies]
objc2 = "0.2"
objc2-app-kit = "0.2"
security-framework = "3"

[target.'cfg(target_os = "linux")'.dependencies]
x11rb = "0.13"
smithay-client-toolkit = "0.18"
dbus-secret-service = "4"
```

**强制规则：**

- ❌ 禁止在公共代码中 `#[cfg(target_os = "...")]`，必须通过 trait 抽象后写到 `windows.rs` / `macos.rs` / `linux.rs`
- ❌ 禁止运行时平台判断（如 `std::env::consts::OS`）做核心逻辑分支，仅用于日志/UI 显示
- ✅ 平台特定模块在 `mod.rs` 中通过 `cfg` 选择性编译：
  ```rust
  // clipboard/mod.rs
  #[cfg(target_os = "windows")]
  mod windows;
  #[cfg(target_os = "macos")]
  mod macos;
  #[cfg(target_os = "linux")]
  mod linux;

  #[cfg(target_os = "windows")]
  pub use windows::WindowsClipboard as PlatformClipboard;
  #[cfg(target_os = "macos")]
  pub use macos::MacosClipboard as PlatformClipboard;
  #[cfg(target_os = "linux")]
  pub use linux::LinuxClipboard as PlatformClipboard;
  ```

### 11.6.2 Feature Flags 设计

feature flag 必须满足：**默认 feature 集合在所有平台都能编译通过**。

```toml
[features]
default = ["mdns", "image-sync"]
mdns = ["dep:mdns"]
image-sync = []
wayland = ["smithay-client-toolkit"]   # Linux 专用，非默认
```

**规则：**

- `cargo build --no-default-features` 必须在所有平台编译通过（CI 中验证）
- 平台专用 feature（如 `wayland`）不放入 `default`，由 `tauri.conf.json` 的 features 字段按平台启用
- 避免 feature 之间的隐式依赖，显式声明 `dep:` 前缀

### 11.6.3 跨平台依赖审查

新增依赖前必须验证三平台支持：

| Crate | Windows | macOS | Linux | 备注 |
|---|---|---|---|---|
| `windows` | ✅ | ❌ | ❌ | 仅 Windows |
| `objc2-app-kit` | ❌ | ✅ | ❌ | 仅 macOS |
| `x11rb` | ❌ | ❌ | ✅ | 仅 Linux |
| `tokio` | ✅ | ✅ | ✅ | 跨平台 |
| `aes-gcm` | ✅ | ✅ | ✅ | 跨平台，纯 Rust |

**禁止使用的依赖类型：**

- ❌ 依赖 C 库且未在所有平台提供预编译 binary 的 crate（如 `openssl`，改用 `ring`）
- ❌ 依赖系统包管理器的 crate（如 `apt` / `brew` 安装的库）
- ❌ 闭源或已停止维护的 crate

**Linux 系统依赖白名单**（CI 中预装）：

- `libgtk-3-dev`
- `libwebkit2gtk-4.1-dev`
- `libayatana-appindicator3-dev`
- `librsvg2-dev`
- `patchelf`

新增 Linux 系统依赖必须在 PR 中说明，并同步更新 `ci.yml` / `nightly.yml` / `release.yml` 三个工作流的安装步骤。

### 11.6.4 版本管理

**单一真相源：** `src-tauri/Cargo.toml` 的 `version` 字段为唯一版本号。

**同步机制：** `build.rs` 中读取 `Cargo.toml` 版本号写入 `tauri.conf.json`：

```rust
// src-tauri/build.rs
fn main() {
    let version = env!("CARGO_PKG_VERSION");
    println!("cargo:rustc-env=APP_VERSION={}", version);
    tauri_build::build();
}
```

前端通过 Tauri command 获取版本：

```rust
#[tauri::command]
fn get_version() -> &'static str {
    env!("APP_VERSION")
}
```

**打 tag 流程：**

```bash
# 使用 cargo-release 自动化
cargo install cargo-release
cargo release patch --no-publish --execute   # 自动 bump 版本 + commit + tag
git push --follow-tags
# GitHub Actions release.yml 自动触发
```

**版本号规则：** 遵循 SemVer

- `0.x.y`：MVP 阶段，API 不稳定
- `1.0.0`：正式发布
- `1.x.0`：向后兼容的新功能
- `1.0.x`：bug 修复

### 11.6.5 提交与分支规范

**分支策略（trunk-based）：**

- `main`：稳定主干，所有 release 从 main 打 tag
- 短期 feature 分支：`feat/xxx`、`fix/xxx`，命名遵循 `<type>/<scope>`
- 长期 feature 用 feature flag 控制，不合入主干时也能跑

**Commit message（Conventional Commits）：**

```
<type>(<scope>): <subject>

<body>

<footer>
```

| type | 含义 |
|---|---|
| `feat` | 新功能 |
| `fix` | bug 修复 |
| `refactor` | 重构 |
| `perf` | 性能优化 |
| `docs` | 文档 |
| `test` | 测试 |
| `chore` | 构建/工具 |
| `ci` | CI 配置 |

**示例：**

```
feat(clipboard): 实现 Windows IStream 延迟渲染

实现 CFSTR_FILEDESCRIPTOR + CFSTR_FILECONTENTS 的 COM 接口，
支持大文件按需分片拉取。STA 线程模型，30s 超时保护。

Closes #42
```

CI 中通过 `commitlint` 强制校验格式，不合规 PR 直接拒绝。

### 11.6.6 代码风格与 Lint

**Rust：**

- `rust-toolchain.toml` 锁定工具链：
  ```toml
  [toolchain]
  channel = "1.75.0"
  components = ["rustfmt", "clippy"]
  profile = "minimal"
  ```
- `clippy.toml` 严格配置：
  ```toml
  msrv = "1.75"
  too-many-arguments-threshold = 8
  ```
- `.rustfmt.toml`：
  ```toml
  edition = "2021"
  max_width = 100
  use_field_init_shorthand = true
  use_try_shorthand = true
  ```

**TypeScript：**

- ESLint + Prettier，`.eslintrc.json` + `.prettierrc` 配置提交到仓库
- `npm run lint` 必须零 warning 零 error

**强制 Lint 规则（CI 拒绝）：**

- `cargo fmt --check` 必须通过
- `cargo clippy -- -D warnings` 必须通过（warning 即 error）
- `npm run lint` 必须通过

### 11.6.7 Pre-commit Hook

通过 `core.hooksPath` 或 `husky` 安装，提交前自动运行快速检查：

```bash
# .githooks/pre-commit
#!/usr/bin/env bash
set -e

echo "[pre-commit] cargo fmt --check"
cargo fmt --all -- --check

echo "[pre-commit] cargo clippy (changed files only)"
cargo clippy --all-targets -- -D warnings

echo "[pre-commit] npm lint"
npm run lint --silent

echo "OK"
```

```bash
# 启用
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit
```

完整 CI 验证（`scripts/local-ci.sh`）在 push 前手动运行，pre-commit 只跑快速子集避免阻塞。

### 11.6.8 错误处理规范

**对外 API 必须用 `thiserror` 定义类型化错误：**

```rust
// error.rs
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard locked by another process")]
    Locked,
    #[error("unsupported content type: {0}")]
    UnsupportedType(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ClipboardError>;
```

**内部逻辑可用 `anyhow`，但跨模块边界必须转 `thiserror`：**

```rust
// 内部
fn read_file(path: &Path) -> anyhow::Result<Vec<u8>> { ... }

// 边界
pub fn export_clipboard(content: ClipboardContent) -> Result<()> {
    let data = read_file(&path).map_err(|e| ClipboardError::Io(e))?;
    Ok(())
}
```

**禁止：**

- ❌ `unwrap()` / `expect()` 在非测试代码中（除非有静态保证）
- ❌ `panic!()` 在剪贴板/网络回调中（会崩溃整个程序）
- ❌ 忽略 `Result`（用 `let _ = ...` 显式忽略并加注释）

### 11.6.9 测试规范

**目录约定：**

- 单元测试：与源码同文件 `#[cfg(test)] mod tests`
- 集成测试：`src-tauri/tests/` 目录，每个文件独立可执行
- 平台特定测试：`#[cfg(target_os = "...")]` 包裹

**Mock 优先：**

- 平台剪贴板通过 `ClipboardProvider` trait 抽象，测试用 `MockClipboardProvider`
- 网络层通过 `Transport` trait 抽象，测试用 `MockTransport`
- 文件系统用 `tempfile` 创建临时目录

**CI 中必须通过：**

- `cargo test --all --all-features`
- `cargo test --all --no-default-features`（验证 feature 隔离）
- 集成测试需要三平台 runner 都跑过

### 11.6.10 文件与目录规范

**禁止提交到仓库的内容：**

- `.env` / `.env.local`
- `*.p12` / `*.pfx` / `*.key`（签名密钥）
- `target/` / `node_modules/` / `dist/`
- IDE 配置（`.idea/` / `.vscode/` 除非为团队共享配置）
- 系统文件（`.DS_Store` / `Thumbs.db`）

`.gitignore` 必须包含以上条目，CI 中通过 `git diff --check` 验证无意外提交。

**新增文件命名：**

- Rust 模块文件：`snake_case.rs`
- 前端组件：`PascalCase.tsx`
- 配置文件：`kebab-case.json`
- 文档：`kebab-case.md`

### 11.6.11 PR 检查清单

PR 模板（`.github/pull_request_template.md`）：

```markdown
## 变更说明
<!-- 简述本次变更 -->

## 变更类型
- [ ] feat: 新功能
- [ ] fix: bug 修复
- [ ] refactor: 重构
- [ ] perf: 性能优化
- [ ] docs: 文档
- [ ] ci: CI 配置

## 检查清单
- [ ] 代码通过 `cargo fmt --check`
- [ ] 代码通过 `cargo clippy -- -D warnings`
- [ ] 通过 `cargo test --all --all-features`
- [ ] 通过 `npm run lint`
- [ ] 通过 `npm run build`
- [ ] 新增依赖已在三平台验证
- [ ] 涉及 Linux 系统依赖变更已更新 CI 工作流
- [ ] commit message 符合 Conventional Commits
- [ ] 不包含敏感信息（密钥、证书、个人数据）

## 测试方式
<!-- 描述如何测试本次变更 -->

## 关联 Issue
<!-- Closes #xxx -->
```

CI 自动检查以上清单项，未勾选或检查失败将阻塞 PR 合并。

# 十二、分阶段开发路线

## 12.1 开发策略

不建议上来就全平台延迟渲染，推荐**分阶段迭代**，先跑通基础同步，再逐步增强体验。每个阶段都有可交付的可用版本，降低开发风险。

## 12.2 阶段一：基础同步 MVP（1-2 周）

**目标：** 跑通三端文本剪贴板同步，验证基础架构与配对流程。

- 项目初始化：Tauri + Rust 项目搭建，基础 UI 框架
- **CI/CD 接入（第一天）：** `ci.yml` 工作流跑通三平台 lint + test + build 验证，pre-commit hook 配置完成
- **开发规范落地：** `rust-toolchain.toml` / `clippy.toml` / `.rustfmt.toml` / ESLint 配置提交
- 剪贴板模块：使用 `arboard` 库实现文本读写和监听
- 防回环机制：自定义格式标记 + 内容哈希去重
- 网络传输：WebSocket 直连，手动输入地址连接
- **配对流程（精简版）：** SPAKE2 配对 + 长期密钥交换 + 系统密钥链存储
- **加密：** 配对完成后所有通信加密
- 系统托盘：基础托盘菜单，窗口显示/隐藏控制

**交付物：** 可在两台设备间同步文本的可用版本，手动连接，端到端加密。CI 三平台通过。

**为何 MVP 即包含 CI 与配对：**

- CI 第一天接入，避免技术债积累后回头补
- 配对后续加加密会触发大量代码返工（消息格式、连接管理、UI 流程），MVP 阶段跑通最简配对 + 加密通道，后续迭代只扩展内容类型

## 12.3 阶段二：自动发现 + 文件基础同步（2-3 周）

**目标：** 实现局域网自动发现，支持文件同步（完整传输模式）。

- 设备发现：mDNS 局域网自动发现，设备列表 UI
- 地址簿：保存常用对端地址
- 文件同步：完整传输模式，文件写入临时目录后写入剪贴板路径
- 图片同步：支持图片剪贴板同步
- 配置管理：用户设置持久化，同步开关配置
- 进度提示：文件传输进度显示
- 历史记录：剪贴板历史记录查看（基础版）

**交付物：** 局域网内自动发现设备，支持文本、图片、文件同步。

## 12.4 阶段三：Windows 延迟渲染（2 周）

**目标：** 实现 Windows 文件延迟渲染，体验接近原生。

- Windows 延迟渲染：实现 `IStream` + `IDataObject` 接口
- `CFSTR_FILEDESCRIPTOR` + `CFSTR_FILECONTENTS` 支持
- 文件夹支持：递归目录结构同步
- COM 线程模型：STA + 隐藏窗口消息泵
- 文件流式传输：按需分片拉取，边传边写
- 异常处理：对端离线、传输中断的降级处理
- 超时保护：`Read` 超时返回错误，避免资源管理器挂起

**交付物：** Windows 支持文件延迟渲染，复制大文件零等待。

## 12.5 阶段四：macOS 延迟渲染（2 周）

**目标：** 实现 macOS 文件延迟渲染。

- macOS 延迟渲染：实现 `NSPasteboardItemDataProvider` 协议
- 引用计数：使用 `objc2::rc::Retained` 正确管理对象生命周期
- 本地文件缓存：LRU 缓存策略，已传输文件复用
- 线程模型：主线程回调 + 后台传输线程
- 超时保护与降级

**交付物：** macOS 支持文件延迟渲染，体验与 Windows 一致。

## 12.6 阶段五：Linux 优化（1-2 周）

**目标：** 完善 Linux 体验。

- X11 延迟渲染：`x11rb` 实现 `SelectionRequest` 处理
- Wayland 基础支持：`smithay-client-toolkit` 实现 `wl_data_source`
- primary selection 支持（可选）
- 桌面环境兼容性测试：GNOME、KDE、XFCE、Sway
- 降级方案：延迟渲染不可用时自动降级为完整传输

**交付物：** Linux 三大桌面环境可用，X11 延迟渲染，Wayland 降级方案。

## 12.7 阶段六：体验优化与发布（持续迭代）

- **发布流程跑通：** 配置 Tauri updater Ed25519 签名密钥到 GitHub Secrets，`release.yml` 全流程跑通，生成 SHA256 校验文件
- **nightly 通道：** `nightly.yml` 自动构建夜间版本，供早期用户测试
- **自动更新：** Tauri updater 验签通过，`latest.json` 自动发布
- **下载说明文档：** README 提供 macOS Gatekeeper 绕过、Windows SmartScreen 绕过的图文指引
- 性能优化：减少内存占用和 CPU 使用率
- 错误处理：完善异常场景的用户提示
- 多语言：国际化支持
- 主题：明暗主题切换
- 黑白名单：应用级或内容级同步过滤
- **安全审计：** 内部代码审计（FFI、unsafe、加密代码），`security.yml` 跑通依赖审计
- 文档：用户手册和开发文档

# 十三、关键难点与解决方案

## 13.1 同步回调 vs 异步传输的矛盾

**问题：** Windows `IStream`、macOS Data Provider 的回调都是**同步阻塞**的，而网络传输是异步的。如果在回调中直接等待网络数据，会阻塞系统线程，导致超时或界面卡死。

**解决方案：**

- **小文件：** 回调内用 `block_on` 阻塞等待传输完成，体验无感知（< 100ms）
- **大文件：** 采用「预缓存 + 流式读取」策略，后台提前缓冲前几 MB 数据，回调时优先返回缓存
- **超时保护：** 所有阻塞等待设置超时（30s），超时返回错误，避免系统挂起
- **Windows 进阶：** 实现异步 `IStream`，配合系统异步读取（兼容性略差）
- **macOS 进阶：** 在回调中立即返回一个空的 `NSData` 占位，后台异步填充后调用 `noteChangesForTypes:` 通知系统更新

## 13.2 程序退出后延迟粘贴失效

**问题：** 延迟渲染依赖程序进程存活，程序退出后注册的延迟数据提供者失效，粘贴会失败。

**解决方案：**

- **后台常驻：** 程序默认后台运行，关闭窗口不退出，通过系统托盘控制
- **开机自启：** 默认开启开机自启动，保证程序始终运行
- **降级提示：** 检测到程序即将退出时，如果有未完成的延迟粘贴，提示用户
- **自动切换：** 对于已完整缓存的文件，自动切换为「真实文件路径」模式，不依赖进程

## 13.3 大文件内存占用

**问题：** 如果一次性读取整个文件到内存，大文件（如几 GB 的视频）会造成内存暴涨。

**解决方案：**

- **流式读取：** 发送端按分片从磁盘读取，不加载整个文件
- **流式写入：** 接收端按分片写入临时文件，边收边写
- **窗口控制：** 限制同时在途的分片数量（默认 9 个），避免内存堆积
- **内存映射：** 超大文件使用 `memmap2` crate 内存映射文件
- **大小限制：** 单文件默认上限 10GB（可配置），超过拒绝同步并提示

## 13.4 多设备同步冲突

**问题：** 三台以上设备时，A → B 和 A → C 同时进行，或者 B 和 C 同时复制不同内容，可能导致剪贴板内容混乱。

**解决方案：** 见第九章「冲突与一致性」。

- **逻辑时钟：** Lamport 时钟，不依赖系统时钟同步
- **冲突解决：** `(lamport, device_id)` 字典序，最大者胜出
- **历史保留：** 败者内容保留在历史记录中
- **最终一致性：** 明确语义，不保证严格顺序

## 13.5 Windows 剪贴板占用冲突

**问题：** Windows 剪贴板是系统独占资源，其他程序正在访问时 `OpenClipboard` 会失败。

**解决方案：**

- **重试机制：** 间隔 10ms 重试，最多重试 10 次
- **指数退避：** 重试间隔逐步增加，避免频繁争抢
- **失败跳过：** 多次重试失败则跳过本次，下次变化再处理
- **延迟读取：** 监听到变化后等待 100ms 再读取，避开写入高峰期

## 13.6 Linux 桌面环境兼容性

**问题：** Linux 桌面环境众多（GNOME、KDE、XFCE 等），各环境对文件剪贴板的支持不一致。

**解决方案：**

- **优先标准格式：** 优先使用 `text/uri-list` 标准格式，各桌面都支持
- **扩展格式兼容：** 检测桌面环境，对应支持扩展格式（如 GNOME 的 `x-special/gnome-copied-files`）
- **自动检测：** 运时检测当前桌面环境，选择最优实现
- **降级方案：** 延迟渲染不可用时，自动降级为完整传输模式

## 13.7 跨网 P2P 连接稳定性

**问题：** 用户手动输入外部地址后，跨网连接可能因防火墙、网络抖动等不稳定。

**解决方案：**

- **重连机制：** 指数退避重连（1s、2s、4s...，上限 60s）
- **单连接简化：** 仅一个 WebSocket 连接，重连逻辑统一，无需协调多通道状态
- **连接状态 UI：** 实时显示对端连接状态（在线/离线/重连中）
- **手动重连按钮：** UI 提供手动触发重连
- **地址验证：** 输入地址时自动测试连通性，提示端口是否可达

# 十四、参考开源项目

## 14.1 RustDesk

**项目地址：** https://github.com/rustdesk/rustdesk

**简介：** 纯 Rust 实现的全平台远程桌面软件，其剪贴板模块完整实现了三端文件延迟渲染，是最权威的参考实现。

**参考价值：**

- Windows 下 `IStream` 接口的完整实现
- macOS 下 `NSPasteboard` 的封装与 Data Provider 实现
- Linux X11/Wayland 双协议适配
- 文件流式传输与缓存机制

**核心代码路径：** `libs/clipboard/src/` 下分平台的实现文件。

## 14.2 arboard

**项目地址：** https://github.com/1Password/arboard

**简介：** 1Password 开源的跨平台剪贴板库，支持文本和图片，是 Rust 生态最成熟的剪贴板库。

**参考价值：**

- 三平台剪贴板基础操作的最佳实践
- 各平台剪贴板监听的实现方式
- 错误处理和重试机制

**局限：** 不支持文件操作和延迟渲染，本项目文件部分需要自行实现。

## 14.3 clipboard-rs

**项目地址：** https://github.com/leexgone/clipboard-rs

**简介：** Rust 跨平台剪贴板库，支持 Windows 延迟渲染。

**参考价值：**

- Windows `CFSTR_FILEDESCRIPTOR` + `CFSTR_FILECONTENTS` 实现
- COM 接口的 Rust 封装
- 比 `arboard` 更适合本项目的文件场景

## 14.4 CopyQ

**项目地址：** https://github.com/hluk/CopyQ

**简介：** Qt 开发的高级剪贴板管理器，支持多种同步后端和插件系统。

**参考价值：**

- 剪贴板历史记录的设计思路
- 多平台剪贴板格式处理经验
- 插件系统架构

## 14.5 SyncClipboard

**项目地址：** https://github.com/Jeric-X/SyncClipboard

**简介：** C# + .NET 实现的跨平台剪贴板同步工具，支持 WebDAV 后端。

**参考价值：**

- 剪贴板内容格式统一设计
- 多设备同步的架构思路

## 14.6 LocalSend

**项目地址：** https://github.com/localsend/localsend

**简介：** Flutter 开发的局域网文件共享工具，mDNS 自动发现，跨平台支持。

**参考价值：**

- mDNS 设备发现的实现细节
- 局域网文件传输协议设计
- 跨平台 UI 设计参考

## 14.7 Tauri 官方示例

**项目地址：** https://github.com/tauri-apps/tauri

**简介：** Tauri 官方仓库，包含大量示例和最佳实践。

**参考价值：**

- 系统托盘的实现方式
- 窗口管理和后台运行配置
- 前后端通信最佳实践
- 三端打包配置
- updater 插件用法

## 14.8 Matrix vodozemac

**项目地址：** https://github.com/matrix-org/vodozemac

**简介：** Matrix 协议的端到端加密实现，Rust 编写。

**参考价值：**

- X25519 + Ed25519 密钥管理
- AES-256-GCM 加密消息封装
- 设备指纹与信任模型

> （注：部分内容可能由 AI 生成）
