// The launcher is the only page that can call into the manager. Once a
// coordinator is chosen the window navigates to that coordinator's own UI.
const { invoke, Channel } = window.__TAURI__.core;

const address = document.getElementById('address');
const remoteForm = document.getElementById('remote');
const remoteOpen = document.getElementById('remote-open');
const remoteError = document.getElementById('remote-error');
const localOpen = document.getElementById('local-open');
const localStop = document.getElementById('local-stop');
const localStatus = document.getElementById('local-status');
const localError = document.getElementById('local-error');
const keySync = document.getElementById('key-sync');

let checking = false;
let installing = false;
let commandRunning = false;

function showError(element, message) {
  element.textContent = message;
  element.hidden = !message;
}

// One quiet line on how the enrollment keys were copied from the online
// coordinator into the local one; nothing before the first attempt.
function keySyncText(status) {
  const sync = status.key_sync;
  if (!status.local_running || !sync.at) {
    return '';
  }
  if (sync.error) {
    return `${sync.error}.`;
  }
  const from = status.remote_url ? new URL(status.remote_url).host : 'the online coordinator';
  return `Enrollment keys synced from ${from} (${sync.imported} new).`;
}

async function refresh() {
  const status = await invoke('status');
  document.getElementById('version').textContent = `Manager ${status.version}`;
  if (status.remote_url && !address.value) {
    address.value = status.remote_url;
  }
  if (status.local_running) {
    localStatus.textContent = `Running. Devices connect to this computer on port ${status.local_client_port}.`;
    localOpen.textContent = 'Open local coordinator';
  } else {
    localStatus.textContent = 'Not running.';
    localOpen.textContent = 'Start local coordinator';
  }
  showText(keySync, keySyncText(status));
  localStop.hidden = !status.local_running;
  document.getElementById('update-hint').hidden = !status.local_running;
}

// Runs a manager command with the button disabled; reports a failure in
// `errorElement`. On success the window usually navigates away.
async function run(button, busyText, errorElement, command, args) {
  if (installing || commandRunning) {
    return false;
  }
  const label = button.textContent;
  commandRunning = true;
  updateControls();
  button.textContent = busyText;
  showError(errorElement, '');
  try {
    await invoke(command, args);
  } catch (error) {
    showError(errorElement, String(error));
  } finally {
    commandRunning = false;
    updateControls();
    button.textContent = label;
  }
  return true;
}

remoteForm.addEventListener('submit', (event) => {
  event.preventDefault();
  run(remoteOpen, 'Connecting…', remoteError, 'open_remote', { address: address.value });
});

localOpen.addEventListener('click', async () => {
  if (await run(localOpen, 'Starting…', localError, 'open_local')) {
    refresh();
  }
});

localStop.addEventListener('click', async () => {
  if (await run(localStop, 'Stopping…', localError, 'stop_local')) {
    refresh();
  }
});

// ---- Self-update ----------------------------------------------------------
// The manager checks the repository's latest release. A failed check (no
// internet, e.g. in hotspot mode) is only mentioned when the operator asked.

const updateBanner = document.getElementById('update');
const updateTitle = document.getElementById('update-title');
const updateNotes = document.getElementById('update-notes');
const updateProgress = document.getElementById('update-progress');
const updateError = document.getElementById('update-error');
const updateInstall = document.getElementById('update-install');
const updateLater = document.getElementById('update-later');
const updateCheck = document.getElementById('update-check');
const updateMessage = document.getElementById('update-message');

// "Later" holds for this session only; the launcher is reloaded every time
// the operator comes back to it.
const DISMISSED = 'manager-update-dismissed';

function showText(element, text) {
  element.textContent = text;
  element.hidden = !text;
}

function showUpdate(result) {
  updateTitle.textContent = `Manager ${result.version} is available`;
  updateTitle.dataset.version = result.version;
  // Release notes are text from the release page: never parsed as HTML.
  showText(updateNotes, result.notes || '');
  showText(updateProgress, '');
  showError(updateError, '');
  updateBanner.hidden = false;
}

async function checkForUpdates(asked) {
  if (checking || installing || commandRunning) {
    return;
  }
  checking = true;
  updateControls();
  if (asked) {
    updateMessage.textContent = 'Checking…';
  }
  let result;
  try {
    result = await invoke('check_update');
  } catch (error) {
    result = { configured: true, available: false, error: String(error) };
  } finally {
    checking = false;
    updateControls();
  }
  if (result.available) {
    updateMessage.textContent = '';
    if (asked || sessionStorage.getItem(DISMISSED) !== result.version) {
      showUpdate(result);
    }
  } else if (!asked) {
    updateMessage.textContent = '';
  } else if (!result.configured) {
    updateMessage.textContent = 'Updates are not configured in this build.';
  } else if (result.error) {
    updateMessage.textContent = `Could not check for updates: ${result.error}.`;
  } else {
    updateMessage.textContent = 'You are up to date.';
  }
  if (!result.available && !result.error) {
    updateBanner.hidden = true;
  }
}

// While installing nothing else may be started: the local coordinator is
// about to be stopped and the app restarted.
function updateControls() {
  const busy = installing || commandRunning;
  for (const control of document.querySelectorAll('button, input')) {
    control.disabled = busy;
  }
  // A completed check may replace the pending release. Keep its existing
  // banner unchanged until the check finishes so it cannot be installed
  // or dismissed against a different backend result.
  updateCheck.disabled = busy || checking;
  updateInstall.disabled = busy || checking;
  updateLater.disabled = busy || checking;
}

updateInstall.addEventListener('click', async () => {
  if (checking || installing || commandRunning || updateBanner.hidden) {
    return;
  }
  installing = true;
  updateControls();
  showError(updateError, '');
  showText(updateProgress, 'Preparing…');
  const progress = new Channel();
  progress.onmessage = (text) => {
    if (installing) {
      showText(updateProgress, text);
    }
  };
  try {
    // On success the manager restarts and this page goes away.
    await invoke('install_update', { onProgress: progress });
  } catch (error) {
    showText(updateProgress, '');
    showError(updateError, String(error));
    installing = false;
    updateControls();
    refresh();
  }
});

updateLater.addEventListener('click', () => {
  if (checking || installing || commandRunning) {
    return;
  }
  sessionStorage.setItem(DISMISSED, updateTitle.dataset.version || '');
  updateBanner.hidden = true;
});

updateCheck.addEventListener('click', () => checkForUpdates(true));

// Coming back with the window's Back button restores the page from cache;
// the status may have changed meanwhile. A fresh load also checks for
// updates, in the background so the page is usable at once.
window.addEventListener('pageshow', async (event) => {
  try {
    await refresh();
  } finally {
    if (!event.persisted) {
      checkForUpdates(false);
    }
  }
});
