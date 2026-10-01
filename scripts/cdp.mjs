// Minimal Chrome DevTools Protocol client used to drive the manager's
// webview (WebView2) in checks:
//
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9333'
//   target\debug\manager.exe
//   node scripts/cdp.mjs 9333 eval "document.title"
//   node scripts/cdp.mjs 9333 screenshot out.png
//
// `eval` awaits promises and prints the JSON result.
import fs from 'node:fs';

const [port, command, argument] = process.argv.slice(2);

async function target() {
  const deadline = Date.now() + 20000;
  for (;;) {
    try {
      const pages = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
      const page = pages.find((p) => p.type === 'page');
      if (page) return page;
    } catch {
      // not up yet
    }
    if (Date.now() > deadline) throw new Error('no debuggable page found');
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
}

const page = await target();
const socket = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  socket.onopen = resolve;
  socket.onerror = reject;
});
let nextId = 1;
const pending = new Map();
socket.onmessage = (event) => {
  const message = JSON.parse(event.data);
  if (message.id && pending.has(message.id)) {
    pending.get(message.id)(message);
    pending.delete(message.id);
  }
};
const send = (method, params = {}) =>
  new Promise((resolve) => {
    const id = nextId++;
    pending.set(id, resolve);
    socket.send(JSON.stringify({ id, method, params }));
  });

if (command === 'eval') {
  const reply = await send('Runtime.evaluate', {
    expression: argument,
    awaitPromise: true,
    returnByValue: true,
  });
  if (reply.result?.exceptionDetails) {
    console.error(JSON.stringify(reply.result.exceptionDetails.exception ?? reply.result.exceptionDetails));
    process.exitCode = 1;
  } else {
    console.log(JSON.stringify(reply.result?.result?.value));
  }
} else if (command === 'screenshot') {
  const reply = await send('Page.captureScreenshot', { format: 'png' });
  fs.writeFileSync(argument, Buffer.from(reply.result.data, 'base64'));
  console.log(`saved ${argument}`);
} else if (command === 'url') {
  console.log(page.url);
} else {
  console.error('usage: cdp.mjs <port> eval <expression> | screenshot <file> | url');
  process.exitCode = 1;
}
socket.close();
