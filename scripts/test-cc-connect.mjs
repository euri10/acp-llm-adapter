#!/usr/bin/env node
// Real cc-connect + adapter; synthetic loopback chat and provider, never Feishu.
// Usage: node scripts/test-cc-connect.mjs /absolute/path/to/cc-connect
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, readFile, writeFile, readdir, access } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
assert(process.argv[2], 'Usage: node scripts/test-cc-connect.mjs /path/to/cc-connect');
const binary = resolve(process.argv[2]);
const run = await mkdtemp(join(tmpdir(), 'acp-cc-connect-check.'));
const workspace = join(run, 'project');
await mkdir(workspace);
await writeFile(join(workspace, 'notes.txt'), 'fixture-note\n');
const inbound = [], outbound = [], requests = [];
let registered = false, callId = 0, messageId = 0, child, fixtureError;
let logs = '';
const version = execFileSync(binary, ['--version'], { encoding: 'utf8' }).trim();
const stable = version.startsWith('cc-connect v1.5.0\n');
assert(stable || version.startsWith('cc-connect v1.5.2-beta.1\n'), 'Update the pinned-version expectations before checking another release');
const say = (s) => console.log(s.replaceAll(run, '<disposable>'));
async function handleRequest(req, res) {
  let body = '';
  for await (const part of req) body += part;
  const send = (value) => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  if (req.url === '/cloud-web/v1/register') {
    registered = true;
    return send({ type: 'register_ack', ok: true, capabilities: ['text', 'file', 'image', 'buttons', 'typing', 'preview', 'update_message'] });
  }
  if (req.url === '/cloud-web/v1/events') {
    await delay(50);
    return send({ events: inbound.splice(0) });
  }
  if (req.url === '/cloud-web/v1/send') {
    const msg = JSON.parse(body);
    outbound.push(msg);
    if (msg.type === 'preview_start') inbound.push({ type: 'preview_ack', ref_id: msg.ref_id, preview_handle: `preview-${outbound.length}` });
    return send({ ok: true });
  }
  if (req.url === '/models') return send({ data: [{ id: 'local-fixture' }] });
  if (req.url !== '/chat/completions') { res.writeHead(404); return res.end(); }
  assert.equal(req.headers.authorization, 'Bearer local-fixture-key');
  const request = JSON.parse(body);
  requests.push(request);
  const last = request.messages.at(-1);
  res.writeHead(200, { 'Content-Type': 'text/event-stream' });
  const chunk = (delta, finish_reason = null) => res.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason }] })}\n\n`);
  if (last.role === 'user' && last.content.startsWith('fixture-tool ')) {
    const [, name, args] = last.content.match(/^fixture-tool (\w+) (.*)$/s);
    chunk({ tool_calls: [{ index: 0, id: `fixture-${++callId}`, type: 'function', function: { name, arguments: '' } }] });
    chunk({ tool_calls: [{ index: 0, function: { arguments: args } }] });
    chunk({}, 'tool_calls');
  } else {
    if (last.content === 'thought-check') chunk({ reasoning_content: 'SYNTHETIC_THOUGHT ' });
    const reply = last.role === 'tool' ? `TOOL_RESULT ${last.content}` : `ANSWER ${last.content}`;
    for (const text of reply.match(/.{1,24}/gs)) {
      chunk({ content: text });
      await delay(30);
    }
    chunk({}, 'stop');
  }
  res.end('data: [DONE]\n\n');
}
const server = createServer((req, res) => {
  handleRequest(req, res).catch(error => {
    fixtureError = error;
    res.destroy();
  });
});
await new Promise((done) => server.listen(0, '127.0.0.1', done));
const base = `http://127.0.0.1:${server.address().port}`;
const template = await readFile(join(root, 'examples/cc-connect.toml'), 'utf8');
// Keep the maintained agent configuration intact; replace only the account transport.
const config = template.split('[[projects.platforms]]')[0] + `
[[projects.platforms]]
type = "cloud_web"
[projects.platforms.options]
transport = "long_poll"
base_url = "${base}"
token = "local-chat-fixture"
allow_from = "pilot"
`;
await writeFile(join(run, 'config.toml'), config);
const env = {
  PATH: process.env.PATH, LANG: 'C.UTF-8',
  CC_PILOT_STATE: join(run, 'cc-state'), CC_PILOT_WORKSPACE: workspace,
  ACP_ADAPTER_BIN: join(root, 'target/debug/acp-llm-adapter'),
  DEEPSEEK_API_KEY: 'local-fixture-key', LLM_BASE_URL: base, LLM_MODEL: 'local-fixture',
  XDG_STATE_HOME: join(run, 'state'), XDG_CONFIG_HOME: join(run, 'config'),
  XDG_DATA_HOME: join(run, 'data'), XDG_CACHE_HOME: join(run, 'cache'),
  ACP_LOG: '1', ACP_LOG_UNREDACTED: '1', RUST_LOG: 'error',
};
const waitFor = async (predicate, label) => {
  const deadline = Date.now() + 15000;
  while (!await predicate()) {
    if (fixtureError) throw fixtureError;
    if (Date.now() > deadline) throw new Error(`Timed out: ${label}`);
    if (child?.exitCode != null) throw new Error(`cc-connect exited: ${logs}`);
    await delay(50);
  }
};
const enqueue = (content, extra = {}) => inbound.push({ type: 'message', msg_id: `msg-${++messageId}`, session_key: 'cloud_web:pilot:pilot', user_id: 'pilot', chat_id: 'pilot', reply_ctx: 'pilot', content, ...extra });
const textSince = (start) => outbound.slice(start).map(m => m.content ?? m.full_text ?? '').join('\n');
const turn = async (prompt, expected, extra = {}) => {
  const start = outbound.length;
  enqueue(prompt, extra);
  await waitFor(() => textSince(start).includes(expected), expected);
  await waitFor(() => outbound.slice(start).some(m => m.type === 'typing_stop'), 'turn complete');
  return textSince(start);
};
const permission = async (name, args, choice) => {
  const start = outbound.length;
  enqueue(`fixture-tool ${name} ${JSON.stringify(args)}`);
  await waitFor(() => outbound.slice(start).some(m => JSON.stringify(m).includes('perm:allow')), 'permission buttons');
  inbound.push({ type: 'card_action', session_key: 'cloud_web:pilot:pilot', reply_ctx: 'pilot', action: `perm:${choice}` });
  await waitFor(() => textSince(start).includes('TOOL_RESULT'), 'tool result');
  await waitFor(() => outbound.slice(start).some(m => m.type === 'typing_stop'), 'tool turn complete');
};
const exists = async (path) => access(path).then(() => true, () => false);
const wire = async () => {
  const dir = join(run, 'state/acp-llm-adapter');
  const names = await readdir(dir, { recursive: true });
  const records = await Promise.all(names.filter(n => n.endsWith('.jsonl')).map(n => readFile(join(dir, n), 'utf8')));
  return records.join('\n').split('\n').filter(Boolean).map(s => JSON.parse(s));
};
try {
  say(version);
  say(`Disposable workspace: ${workspace}`);
  child = spawn(binary, ['--config', join(run, 'config.toml')], { env, cwd: workspace, stdio: ['ignore', 'pipe', 'pipe'] });
  child.stdout.on('data', b => { logs += b; });
  child.stderr.on('data', b => { logs += b; });
  await waitFor(() => registered, 'local chat registration');
  await turn('hello-local', 'ANSWER hello-local');
  say('PASS initialization + streamed provider answer through real cc-connect');
  await turn('fixture-tool read_file {"path":"notes.txt"}', 'fixture-note');
  say('PASS read_file returns fixture-note');
  await permission('write_file', { path: 'approved.txt', content: 'approved\n' }, 'allow');
  assert.equal(await readFile(join(workspace, 'approved.txt'), 'utf8'), 'approved\n');
  say('PASS approved write_file creates approved.txt');
  await permission('write_file', { path: 'denied.txt', content: 'denied\n' }, 'deny');
  assert.equal(await exists(join(workspace, 'denied.txt')), false);
  say('PASS denied write_file leaves no denied.txt');
  const started = outbound.length;
  enqueue('fixture-tool run_command {"command":"printf started > started.txt; sleep 2; printf late > cancelled.txt"}');
  await waitFor(() => outbound.slice(started).some(m => JSON.stringify(m).includes('perm:allow')), 'command permission');
  inbound.push({ type: 'card_action', session_key: 'cloud_web:pilot:pilot', reply_ctx: 'pilot', action: 'perm:allow' });
  await waitFor(() => exists(join(workspace, 'started.txt')), 'command started');
  const stopped = outbound.length;
  enqueue('/stop');
  await waitFor(() => /stopped/i.test(textSince(stopped)), 'stop reply');
  await waitFor(async () => (await wire()).some(r => r.payload?.result?.stopReason === 'cancelled'), 'cancelled ACP result');
  await delay(2200);
  assert.equal(await exists(join(workspace, 'cancelled.txt')), false);
  await turn('after-cancel', 'ANSWER after-cancel');
  say('PASS /stop cancels a running command; continuation succeeds; no cancelled.txt');
  const thought = await turn('thought-check', 'ANSWER thought-check');
  assert.equal(thought.includes('SYNTHETIC_THOUGHT'), stable, 'version-specific thought rendering changed');
  say(`OBSERVED thinking_messages=false: thought in reply=${thought.includes('SYNTHETIC_THOUGHT')}`);
  await turn('attachment-check', 'ANSWER attachment-check', { files: [{ file_name: 'sample.txt', mime_type: 'text/plain', data: Buffer.from('attached-fixture').toString('base64') }] });
  const attached = requests.at(-1).messages.at(-1).content;
  assert.equal(typeof attached, 'string');
  assert.match(attached, /sample\.txt/);
  assert(!attached.includes('attached-fixture'));
  say('PASS text attachment becomes local path in text prompt, not inline file contents');
  await turn('image-check', 'ANSWER image-check', { images: [{ file_name: 'pixel.png', mime_type: 'image/png', data: Buffer.from('synthetic-image-bytes').toString('base64') }] });
  const imagePrompt = requests.at(-1).messages.at(-1).content;
  assert.equal(typeof imagePrompt, 'string');
  assert.match(imagePrompt, /Image files saved locally:/);
  say('PASS image attachment becomes local path text; no image/vision input');
  const beforeSwitch = await wire();
  assert(beforeSwitch.some(r => r.payload?.params?.update?.sessionUpdate === 'agent_thought_chunk'), 'fixture must emit a real ACP thought chunk');
  assert(beforeSwitch.some(r => r.payload?.method === 'initialize' && r.payload.params.protocolVersion === 1));
  assert(beforeSwitch.some(r => r.payload?.method === 'session/set_mode' && r.payload.params.modeId === 'ask'));
  const originalID = beforeSwitch.find(r => r.payload?.method === 'session/prompt').payload.params.sessionId;
  const promptIDs = beforeSwitch.filter(r => r.payload?.method === 'session/prompt').map(r => r.payload.params.sessionId);
  assert(promptIDs.every(id => id === originalID));
  say('PASS same ACP session retained across turns and cancellation');
  const listed = outbound.length;
  enqueue('/list');
  await waitFor(() => textSince(listed).includes('hello-local'), 'session list');
  const switched = outbound.length;
  enqueue(`/switch ${originalID}`);
  await waitFor(() => /switched/i.test(textSince(switched)), 'session switch');
  await turn('after-load', 'ANSWER after-load');
  const afterSwitch = await wire();
  const restoreMethod = afterSwitch.find(r => ['session/load', 'session/resume'].includes(r.payload?.method))?.payload.method;
  assert(restoreMethod, 'host must attempt load or resume');
  const loadedPrompt = afterSwitch.find(r => r.payload?.method === 'session/prompt' && r.payload.params.prompt.some(p => p.text === 'after-load'));
  const retained = loadedPrompt.payload.params.sessionId === originalID;
  assert.equal(retained, !stable, 'version-specific restore behavior changed');
  say(`OBSERVED /list + /switch: ${restoreMethod}, original ACP session retained=${retained}`);
  const historyRetained = requests.at(-1).messages.some(m => m.role === 'user' && m.content === 'hello-local');
  assert.equal(historyRetained, retained);
  const previewCount = outbound.filter(m => m.type === 'preview_start').length;
  assert(previewCount > 0);
  say(`PASS client preview output (${previewCount} starts); platform rendering still requires real Feishu`);
  await writeFile(join(run, 'wire.json'), JSON.stringify(await wire(), null, 2));
  say('LOCAL VALIDATION COMPLETE; synthetic chat/provider, not Feishu or user-adoption evidence');
} finally {
  if (child && child.exitCode === null) {
    child.kill('SIGTERM');
    const killed = setTimeout(() => child.kill('SIGKILL'), 4000);
    await once(child, 'exit');
    clearTimeout(killed);
  }
  await writeFile(join(run, 'cc-connect.log'), logs);
  await writeFile(join(run, 'chat.json'), JSON.stringify(outbound, null, 2));
  server.closeAllConnections();
  await new Promise(done => server.close(done));
  console.log(`Fixture evidence retained: ${run}`);
}
