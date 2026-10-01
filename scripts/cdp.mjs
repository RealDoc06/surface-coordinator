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

const [port, command, argument, argument2] = process.argv.slice(2);

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

// Emulation only lasts as long as this connection, so to look at a page in the
// other colour scheme: CDP_MEDIA=light node scripts/cdp.mjs 9444 screenshot x.png
if (process.env.CDP_MEDIA) {
  await send('Emulation.setEmulatedMedia', {
    features: [{ name: 'prefers-color-scheme', value: process.env.CDP_MEDIA }],
  });
}

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
} else if (command === 'click') {
  // Real mouse click on the centre of the first element matching a CSS
  // selector, or (with "text=Some label") the first button/link/label whose
  // text is exactly that.
  const found = await send('Runtime.evaluate', {
    expression: `(() => {
      const spec = ${JSON.stringify(argument)};
      let el;
      if (spec.startsWith('text=')) {
        const text = spec.slice(5);
        el = [...document.querySelectorAll('button, a, label, summary, [role=button]')]
          .find((e) => e.textContent.replace(/\\s+/g, ' ').trim() === text);
      } else {
        el = document.querySelector(spec);
      }
      if (!el) return null;
      el.scrollIntoView({ block: 'center' });
      const r = el.getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    })()`,
    returnByValue: true,
  });
  const at = found.result?.result?.value;
  if (!at) {
    console.error(`no element for ${argument}`);
    process.exitCode = 1;
  } else {
    for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased']) {
      await send('Input.dispatchMouseEvent', { type, x: at.x, y: at.y, button: 'left', clickCount: 1 });
    }
    console.log('clicked');
  }
} else if (command === 'type') {
  // Focus the element matching the selector, replace its content, type the text.
  const focused = await send('Runtime.evaluate', {
    expression: `(() => {
      const el = document.querySelector(${JSON.stringify(argument)});
      if (!el) return false;
      el.focus();
      el.select?.();
      return true;
    })()`,
    returnByValue: true,
  });
  if (!focused.result?.result?.value) {
    console.error(`no element for ${argument}`);
    process.exitCode = 1;
  } else {
    await send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Backspace', code: 'Backspace', windowsVirtualKeyCode: 8 });
    await send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Backspace', code: 'Backspace', windowsVirtualKeyCode: 8 });
    await send('Input.insertText', { text: argument2 ?? '' });
    console.log('typed');
  }
} else if (command === 'key') {
  await send('Input.dispatchKeyEvent', { type: 'keyDown', key: argument, code: argument, windowsVirtualKeyCode: argument === 'Enter' ? 13 : 0, text: argument === 'Enter' ? '\r' : undefined });
  await send('Input.dispatchKeyEvent', { type: 'keyUp', key: argument, code: argument, windowsVirtualKeyCode: argument === 'Enter' ? 13 : 0 });
  console.log('pressed');
} else if (command === 'media') {
  // media light|dark: emulate prefers-color-scheme.
  await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: argument }] });
  console.log(`prefers-color-scheme: ${argument}`);
} else if (command === 'clearcookies') {
  await send('Network.clearBrowserCookies');
  console.log('cookies cleared');
} else if (command === 'goto') {
  await send('Page.navigate', { url: argument });
  console.log(`navigated to ${argument}`);
} else {
  console.error(
    'usage: cdp.mjs <port> eval <expression> | screenshot <file> | url | click <selector|text=Label> | type <selector> <text> | key <Enter|Escape|...> | media <light|dark> | goto <url> | clearcookies',
  );
  process.exitCode = 1;
}
socket.close();
