# cc-connect pilot / cc-connect 试用

English and Chinese use the same [configuration](../examples/cc-connect.toml).
The [short demo](cc-connect-demo.txt) is a real cc-connect process driving real
adapter tools, with a **synthetic local chat transport and provider**. Feishu
account delivery, phone rendering, and live-provider usefulness are not yet
verified. This is an invitation to a small pilot, not a claim of adoption.

中英文共用一份[配置](../examples/cc-connect.toml)。[简短演示](cc-connect-demo.txt)
运行了真实 cc-connect 和适配器工具，但聊天传输与模型服务均为本地模拟。
尚未验证飞书账号收发、手机端显示或真实模型的实用性；欢迎参与小范围试用。

## Versions and observed behavior / 版本与验证结果

Tested 2026-11-06 on Linux x86_64 (kernel `7.2.6+deb13-amd64`), adapter
`0.10.0`, source `fdf40ac`, default features, built with
`cargo build --locked --bin acp-llm-adapter`. cc-connect used the upstream
Linux amd64 release archive, verified against its published SHA-256 checksum.

Use **[cc-connect v1.5.2-beta.1](https://github.com/chenhg5/cc-connect/releases/tag/v1.5.2-beta.1)**
(`10442325`) for this recipe. The tested archive SHA-256 is
`cdffb845337df1d7ac46f978feb44e48a7a0e4de223e368a02a1ca67f6be4243`.
This is a prerelease; the evidence below is limited to this version and scenario.

| Surface / 功能 | v1.5.2-beta.1, local fixture / 本地验证 |
| --- | --- |
| Initialize / 初始化 | ACP v1; `serve --backend deepseek`; local HTTP/SSE provider |
| Streaming / 流式回复 | ACP text deltas and cc-connect preview messages observed |
| Tools / 工具 | `read_file`; approved `write_file`; denied write leaves no file |
| Mode / 权限模式 | `ask` applied via `session/set_mode`; explicit Allow/Deny |
| Cancel / 取消 | `/stop` cancels running shell; delayed write never happens; next turn works |
| Continue / 连续对话 | Same ACP session across prompts and cancellation |
| Restore / 恢复 | `/list`, `/switch <id>`, next prompt → `session/resume`; same id and history |
| Thoughts / 思考内容 | Actual `agent_thought_chunk` emitted; hidden with `thinking_messages=false` |
| Attachments / 附件 | File/image saved locally; ACP receives a text path, not inline content or vision input |
| Feishu / 飞书 | Prepared configuration only; account interaction still needed |

Stable `v1.5.0` (`17c61062`) passed the tool/cancel checks but exposed thought
text as an answer and lost the conversation after `/switch` → `session/load`.
Both failures were reproduced before selecting the beta. See
[upstream thought report #1940](https://github.com/chenhg5/cc-connect/issues/1940)
and the pinned [restore implementation](https://github.com/chenhg5/cc-connect/blob/10442325984a4e228222dec3532cab8c31f7caaa/agent/acp/session.go).
The adapter does not fix these host behaviors. Long-history restoration was
not tested; do not infer it from the short scenario.

稳定版 `v1.5.0` 的工具与取消测试通过，但关闭思考显示仍会泄露到答案，切换会话后
也会丢失上下文。因此本配方固定使用上述 beta；没有修改适配器来绕过客户端行为。
只验证了短对话恢复，未验证长历史。

## English: first Feishu task

1. Install the pinned cc-connect release above and the
   [verified Linux x86_64 adapter binary](installation.md) (Cargo alternative
   also documented). Put `$HOME/.local/bin` on `PATH`. No native cc-connect backend is needed:
   the maintained example uses `type = "acp"`.
2. Configure an enterprise Feishu bot using cc-connect's
   [Feishu guide](https://github.com/chenhg5/cc-connect/blob/10442325984a4e228222dec3532cab8c31f7caaa/docs/feishu.md).
   Use its account/platform setup sections; our ACP example replaces its Claude
   Code agent configuration, so Claude Code is not required for this recipe.
   Publish it for your pilot user and enable its documented message events,
   permissions and long connection. Supply `FEISHU_APP_ID`, `FEISHU_APP_SECRET`
   and that user's open ID as `FEISHU_PILOT_USER_ID` in the launching environment.
   Supply your provider key as `DEEPSEEK_API_KEY`; the example maps it to the
   adapter's `LLM_API_KEY`. This uses the adapter's direct provider API, with no
   Claude Code/Anthropic compatibility shim. No `auth_method` is needed.
3. Run this from the adapter checkout in a shell with those variables exported:

```sh
acp-llm-adapter --version
export ACP_ADAPTER_BIN="$(command -v acp-llm-adapter)"
export CC_PILOT_WORKSPACE="$(mktemp -d)"
export CC_PILOT_STATE="$(mktemp -d)"
export XDG_STATE_HOME="$CC_PILOT_STATE/adapter"
printf 'A small pilot project.\n' > "$CC_PILOT_WORKSPACE/notes.txt"
: "${DEEPSEEK_API_KEY:?Set the provider key in the environment}"
: "${FEISHU_APP_ID:?Set the Feishu app id}"
: "${FEISHU_APP_SECRET:?Set the Feishu app secret}"
: "${FEISHU_PILOT_USER_ID:?Set the allowed pilot user open id}"
unset LLM_BASE_URL LLM_MODEL ACP_LOG ACP_LOG_UNREDACTED
cc-connect --config "$PWD/examples/cc-connect.toml"
```

The absolute `work_dir` is the disposable project, not this source checkout.
cc-connect state and adapter sessions live under the disposable state directory.
The initial mode is `ask`. Built-in file tools stay within approved roots;
`run_command` is permission-gated host execution, **not a filesystem sandbox**.
Approve only the requested test operation; use Allow rather than Allow All.
Keep the existing `error` log level: host info/debug logs can contain tool inputs.

Send this first task to your bot (mention it when required in a group):

```text
Read notes.txt. Propose a one-line summary. Then ask permission to create
pilot-summary.txt with that summary. Do not run shell commands or modify
any other file.
```

Approve the write and inspect the file on the host. Next ask it to create
`denied.txt`, choose Deny, and confirm that file is absent. Try a follow-up
question, `/list` and `/switch <session-id>`, then another follow-up. Use `/stop`
to cancel a running turn. The automated demo proves the deterministic tool
path; a real model may choose a different sequence. If no permission appears,
check `/mode` and stop before proceeding. Attach a small `.txt` file and ask
the agent to read its saved path; image attachment paths do not provide vision.
Do not paste the mock backend's `!tool` directives into chat: cc-connect treats
the leading `!` as its own shell command syntax.

Then choose **one real task of your own** in a disposable copy of a project.
Report OS, both versions, install route, setup friction, task outcome, and what
was better or worse than your usual workflow. A failed installation, preference
for another harness, or later repeat use is useful evidence. Share no keys,
private conversations or project contents.

## 中文：第一个飞书任务

1. 安装上面固定版本的 cc-connect 和[已验证的 Linux x86_64 适配器二进制](installation.md)，
   将 `$HOME/.local/bin` 加入 `PATH`；不需要 Rust、Python 或 Node 运行时，仍可选择 Cargo 安装。
   在适配器源码目录中执行上面的命令块以使用配置示例。配置通过通用 `type = "acp"` 启动，无需新增原生后端。
2. 按[飞书指南](https://github.com/chenhg5/cc-connect/blob/10442325984a4e228222dec3532cab8c31f7caaa/docs/feishu.md)
   创建并发布机器人，配置消息权限、事件订阅及长连接，把试用用户加入可用范围。
   只需参考账号及平台设置部分；以本配方的 ACP 配置替换其中的 Claude Code 配置，
   无需安装 Claude Code。
   在启动进程的环境中设置 `FEISHU_APP_ID`、`FEISHU_APP_SECRET`、
   `FEISHU_PILOT_USER_ID`（该用户的 open ID）和 `DEEPSEEK_API_KEY`。
   同一份 TOML 将后者映射为 `LLM_API_KEY`，直连模型服务，不经过 Claude Code 的
   Anthropic 兼容接口，也不需要 `auth_method`。
3. 执行上面的命令块。工作目录和状态目录都是一次性临时目录；权限模式为 `ask`。
   文件工具受允许目录限制，但 shell 命令不是操作系统沙箱。只允许当前操作，
   不选择 Allow All。保留 `error` 日志级别，避免 host info/debug 日志记录工具输入。

向机器人发送以下内容（群聊按需 @机器人）：

```text
读取 notes.txt，提出一句话摘要。然后请求许可，将摘要写入
pilot-summary.txt。不要运行 shell 命令，也不要修改其他文件。
```

点击 Allow 并在主机检查文件；再要求创建 `denied.txt`，点击 Deny 并确认文件不存在。
继续追问，尝试 `/list`、`/switch <会话ID>` 后再次追问，用 `/stop` 取消正在运行的任务。
真实模型的步骤可能不同于确定性的模拟演示。若没有许可提示，先查看 `/mode` 并停止操作。
可以上传小型 `.txt` 文件并要求读取保存路径；图片仅传路径，不代表支持视觉输入。
不要向聊天发送 mock 后端的 `!tool` 指令，cc-connect 会把开头的 `!` 当作 shell 命令。

随后请在项目的一次性副本上，选择**一个自己的真实任务**。欢迎反馈系统、两个版本、
安装方式、配置卡点、任务结果，以及相对原工作流的优缺点。安装失败、选择其他工具、
之后是否再次使用都值得记录。无需分享密钥、私人聊天或项目内容。

## Reproduce the local check / 重现本地检查

Requires Node.js 24 (tested `24.16.0`), Rust, and the downloaded cc-connect binary.
From the checkout:

```sh
cargo build --locked --bin acp-llm-adapter
node scripts/test-cc-connect.mjs /absolute/path/to/cc-connect-v1.5.2-beta.1-linux-amd64
```

The script consumes the **unchanged agent section** of the maintained TOML and
replaces only the Feishu transport with a loopback `cloud_web` fixture. It supplies
a dummy key and local model/base URL in a clean child environment. It asserts
file effects, permissions, ACP mode, cancellation, thoughts and restored history.
It owns/reaps cc-connect and retains synthetic evidence in a printed temporary
directory. No live provider or chat account is called. It also accepts `v1.5.0`
and asserts its known failures as characterization evidence, not as a passing
pilot configuration. No adapter runtime code changed; no artificial red test
was needed for the documentation.

脚本直接使用同一 TOML 的 agent 配置，仅将飞书替换为本地聊天夹具，使用假密钥及本地
模型服务。它不会调用真实模型或聊天账号，保留模拟证据，并结束自己启动的进程。
稳定版的已知失败只是对照证据，不能当作试用成功。
