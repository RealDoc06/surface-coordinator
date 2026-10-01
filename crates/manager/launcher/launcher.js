// The launcher is the only page that can call into the manager. Once a
// coordinator is chosen the window navigates to that coordinator's own UI.
const { invoke } = window.__TAURI__.core;

const address = document.getElementById('address');
const remoteForm = document.getElementById('remote');
const remoteOpen = document.getElementById('remote-open');
const remoteError = document.getElementById('remote-error');
const localOpen = document.getElementById('local-open');
const localStop = document.getElementById('local-stop');
const localStatus = document.getElementById('local-status');
const localError = document.getElementById('local-error');

function showError(element, message) {
  element.textContent = message;
  element.hidden = !message;
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
  localStop.hidden = !status.local_running;
}

// Runs a manager command with the button disabled; reports a failure in
// `errorElement`. On success the window usually navigates away.
async function run(button, busyText, errorElement, command, args) {
  const label = button.textContent;
  button.disabled = true;
  button.textContent = busyText;
  showError(errorElement, '');
  try {
    await invoke(command, args);
  } catch (error) {
    showError(errorElement, String(error));
  } finally {
    button.disabled = false;
    button.textContent = label;
  }
}

remoteForm.addEventListener('submit', (event) => {
  event.preventDefault();
  run(remoteOpen, 'Connecting…', remoteError, 'open_remote', { address: address.value });
});

localOpen.addEventListener('click', async () => {
  await run(localOpen, 'Starting…', localError, 'open_local');
  refresh();
});

localStop.addEventListener('click', async () => {
  await run(localStop, 'Stopping…', localError, 'stop_local');
  refresh();
});

// Coming back with the window's Back button restores the page from cache;
// the status may have changed meanwhile.
window.addEventListener('pageshow', refresh);
