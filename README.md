# Ducky

Ducky 将 ASRInput 语音输入和 SwiftClip 剪贴板历史/同步整合到同一个 Windows 进程，使用同一个 GPUI 应用。启动 `ducky.exe` 后，语音和剪贴板分别有托盘入口；任一入口退出会结束整个应用。剪贴板窗口默认隐藏，可通过其托盘菜单或原快捷键打开。请先退出独立版 SwiftClip，以免单实例冲突。

剪贴板数据保存在 `%LOCALAPPDATA%\Ducky\clipboard`。不会自动迁移旧版数据；如需迁移，请退出两个程序，备份后将旧 SwiftClip 的 `data` 目录内容复制到此目录。语音配置继续使用原 ASRInput 目录。同步凭据依赖 Windows 用户账户，请在迁移后检查并重新输入。

GitHub Actions 的 **Windows build** 工作流在推送后构建，完成后可在对应运行页面下载 `Ducky-windows-x64` artifact。本地构建：`cargo build --locked --release`（Rust 1.99.1）。以下保留原语音功能说明；其中旧的可执行文件名应以 `ducky.exe` 为准。

## 语音输入

Windows 原生语音输入工具，使用 Rust、GPUI 和 Ely GPUI Components。

按住 **右 Ctrl** 录音，松开后由 MiMo 专用 ASR、MiMo 多模态或 OpenRouter 音频模型识别，可选调用独立的 OpenAI 兼容模型润色，最后向原来的输入位置发送粘贴。

## 使用

1. 启动 `asr-input.exe`，在侧栏“API 配置”选择识别方式。普通 ASR 使用 MiMo `mimo-v2.5-asr`，填写 MiMo API Key，并选择自动、中文或英文。新安装默认普通 ASR。
2. 多模态可选择 MiMo 或 OpenRouter。MiMo 默认 `mimo-v2.6-flash`，与普通 ASR 共用 Key。OpenRouter 使用独立 Key，刷新并搜索支持音频的模型，也可手动填写完整模型 ID。
3. 如需润色，在“文本润色”右侧打开开关，填写接口地址、API Key、模型和提示词，再保存。它使用独立配置；若也使用 MiMo，需要在润色 API Key 栏填写相应密钥。本地无鉴权服务可以留空。
4. 将光标放在其他应用的输入框，按住右 Ctrl 说话，松开等待上屏。

窗口使用 Windows 原生标题栏，支持拖动、双击标题栏最大化/还原、窗口边缘调整大小，以及标准最小化。最小化保留在任务栏；关闭或点击“收起到托盘”才隐藏主窗口，录音快捷键继续工作。点击系统托盘的彩球图标重新打开；右键托盘图标选择“退出”才会结束进程。隐藏最大化窗口后从托盘打开会保留最大化状态。使用 `asr-input.exe --background` 可以直接启动到托盘。

侧栏底部新增“开机静默启动”开关，开启或关闭后立即生效，无需再保存 API 设置。默认关闭；开启后，登录当前 Windows 用户时以 `--background` 启动，仅显示托盘图标，不弹主窗口，右 Ctrl 录音快捷键照常工作。后台启动遇到重复实例会安静退出；如后台启动失败，手动启动可查看错误。

启动项仅写入当前用户的 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，无需管理员权限；关闭时只移除 ASRInput 自己的条目。使用当前程序的绝对路径并正确处理空格、中文。移动程序或改用另一版本目录后，在新位置关闭再开启一次开关以更新路径。Windows 可能延迟启动登录程序，详见 [Microsoft Run 启动项说明](https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys)。

右 Ctrl 被专用为录音键；左 Ctrl 与其他按键照常使用。识别或润色期间再次按右 Ctrl 不会排队录音。

- 按住时间 **小于 1 秒**，或者麦克风实际录得的音频不足 1 秒，直接丢弃，不请求模型。
- 恰好 1 秒且有效音频不少于 1 秒可以识别。
- 使用 Windows 默认麦克风，仅录音时打开设备。实时混为单声道并转换为 16 kHz / 16-bit WAV。
- 单次录音最多 3 分钟；超时本次丢弃，松开再试。避免误触后无限录音和内存增长。
- 润色失败或返回空文本时使用识别原文。
- 默认打开历史记录页，按新到旧保存识别原文和最终文本。点击内容展开全文与原文，支持逐条复制、删除和确认后清空。
- 历史最多保留 500 条，原文和正文合计不超过 6 MiB，超过自动移除最旧记录；不保存录音文件。

## 提示词与动画

多模态的识别提示词和润色提示词支持直接编辑、展开编辑、恢复默认。MiMo 专用 ASR 不发送提示词，只使用音频与语言参数。切换组合保留草稿，保存后对下一次录音生效，进行中的录音使用开始时的配置。底部显示未保存状态，支持撤销更改；允许先保存未填完的配置，开始录音前会检查缺项。

标题栏、侧栏、内容区、表单和设置分区使用统一奶油底色，以细分割线组织区域；选中和焦点状态保留灰绿色提示，不使用异色卡片。

应用界面使用系统安装的 MiSans 字体，不在程序内重复打包字体文件。

“测试识别”发送内置合成语音，不录麦克风；“测试润色”发送内置示例文稿。测试使用当前表单草稿，可能产生服务费用，不修改正常历史或向其他软件粘贴。支持取消、查看耗时、复制结果；修改相应配置后原测试结论会标记过期。测试成功只说明该测试样本成功，不代表所有录音均可识别。

“动画设置”可以启用或关闭全局 GIF 提示，选择内置动画或自己的 GIF、位置、大小，以及识别和润色阶段是否继续显示。预览持续 3 秒，不需要 API Key。动画设置可单独保存。

提示层使用独立 Win32 layered 窗口播放 GIF，没有标题栏和边框，不抢焦点、可点击穿透，也不显示在任务栏。主窗口打开、最小化或隐藏时均可播放。隐藏后停止动画定时器。支持透明 GIF；自定义文件上限 8 MiB、512×512、120 帧，解码缓存上限 16 MiB。内置 GIF 为 24 帧、50ms/帧，通过 cargo run --example make_gifs 可重复生成。GIF 循环播放，不随实时音量改变。

## 上屏方式

识别结果先写入系统剪贴板，再发送 Ctrl+V。窗口或原生焦点发生变化、仍有修饰键按下、或当前目标就是设置窗口时，只复制结果，不自动粘贴。剪贴板保留结果，方便手动粘贴；不自动恢复旧剪贴板。

发送成功表示 Windows 接受了模拟按键，不能证明目标软件已经插入文字。管理员窗口、部分终端和自定义输入控件可能需要手动粘贴。结果会同时保留在历史记录中，可点击复制图标。

## 模型接口

- **MiMo 多模态**：`https://api.xiaomimimo.com/v1/chat/completions`，使用 `api-key` 认证、WAV Base64 data URL 和识别提示词，关闭思考模式。
- **MiMo 普通 ASR**：同一 Chat Completions 地址，固定 `mimo-v2.5-asr`，只发送单条音频及 `asr_options.language`，不走 `/audio/transcriptions`，不附加提示词和思考字段。
- **OpenRouter 多模态**：按当前官方音频输入文档使用 `https://openrouter.ai/api/v1/responses`，Bearer 认证，原始 WAV Base64 与识别提示词，单独解析 Responses 正文与流式事件。模型列表只按音频输入和文本输出初筛，具体模型权限和能力需测试确认。

三种组合分别记忆非流式或流式设置。流式是松开按键、上传完整录音后分段返回文字，不是边录边上传；正常输入仍等待完整结果及可选润色后一次性上屏。断流、截断或服务错误不会把半句话当成功结果；模型返回空正文不会直接判断为没有人声。测试响应队列有界，避免流式文本堆积。

润色支持实现 `/chat/completions` 的 OpenAI 兼容接口，包括自定义地址和模型。地址可以填写 `https://example.com/v1` 或完整 `/chat/completions` 路径。不额外适配供应商专有协议。

官方接口依据：[MiMo 音频理解](https://mimo.mi.com/docs/en-US/quick-start/usage-guide/multimodal-understanding/audio-understanding)、[MiMo 专用 ASR](https://mimo.mi.com/docs/zh-CN/api/audio/Speech-Recognition)、[OpenRouter 音频](https://openrouter.ai/docs/guides/overview/multimodal/audio)。模型可用性以自己的账户为准。

## 配置与资源

配置保存在 `%LOCALAPPDATA%\ASRInput\config.json`，历史文本保存在同目录的 `history.json`。API Key 使用当前 Windows 用户的 DPAPI 加密；复制配置到其他账户不能解密。应用不输出密钥日志。升级自动补齐新设置，保留旧密钥及润色提示词。

0.3 的通用 ASR 已由 MiMo 专用 ASR 替代，旧通用 ASR 字段和加密 Key 保留；检测到旧通用 ASR 地址时生成 `config.pre-0.4.json` 备份，不把第三方 Key 自动当成 MiMo Key。OpenRouter 模型列表缓存于同目录 `openrouter-models.json`。

常驻部分为原生 UI、右 Ctrl 事件线程、托盘消息线程、GIF 播放线程和后台工作线程。没有 WebView、网页前端、轮询键盘或常驻录音流。只依赖 Ely 的 `ely-core`，关闭可选字体包，不引入整套业务组件。录音缓冲上限约 5.76 MB；HTTP 请求构造时会额外占用 WAV、Base64 和 JSON 内存，完成后释放。

## 构建

需要 Windows、Rust 1.98+（MSVC 工具链）、Visual Studio C++ Build Tools 和 Windows SDK。

```powershell
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo run --locked
cargo build --locked --release
```

或者执行 `powershell -File scripts/package.ps1`，产物位于 `dist/ASRInput-0.4.1/`。关闭旧程序后再启动新版，避免两个版本争用快捷键。

开发验证：`cargo run -- --smoke-ui` 使用独立测试配置，验证原生标题栏、最大化还原、最小化任务栏、关闭到托盘、GIF 绘帧，以及 API 页的保存、撤销、服务商切换、模型搜索和提示词编辑，结果写入 `target/ui-smoke-report.json`；不会访问真实密钥或历史，不调用外部模型。`--preview-ui` 提供同样隔离的界面预览；`--preview-api` 直接预览 API 页，3 分钟后自动退出。

`--smoke-ui --background` 验证静默启动时主窗口和动画均隐藏，结果写入 `target/background-smoke-report.json`。预览与自检中的开机开关仅模拟状态，不修改真实启动项；注册表读写测试使用独立临时测试键。

## 参考与依赖

- [Ely GPUI Components](https://github.com/ELY-Open-Source/Ely-GPUI-Components)：实际使用其输入框、密码框、按钮、开关和焦点组件，依赖固定到提交 `dbfdf0ce7ff2d4328f00ea5b00bfa8f3aa504e42`。
- [OpenLess](https://github.com/Open-Less/openless)：参考其音频 Chat Completions 协议、“剪贴板 + 粘贴、失败保留结果”的实现思路，以及声波胶囊与处理中扫光的视觉效果；用 GPUI 独立实现，没有引入其 WebView、egui 或 OpenGL 动画运行时。
- [GPUI](https://www.gpui.rs/)：原生 UI 框架。

应用图标使用用户提供的玻璃彩球，内置 GIF 为独立生成的声波提示；图标与 GIF 均嵌入可执行文件。`scripts/prepare-icon.ps1` 从 `assets/logo-source.png` 生成多尺寸 ICO，不改变图案。

测试覆盖音频重采样与 WAV、录音时长阈值、HTTP 音频请求和润色请求、异常响应、Windows 密钥加密、旧配置迁移、提示词持久化及历史读写。真实麦克风、物理右 Ctrl 和各目标软件的粘贴行为仍需在实际使用环境验证。
