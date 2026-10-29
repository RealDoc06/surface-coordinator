#!/usr/bin/env node
// Isolated protocol/SQLite load validation. Never runs commands on real devices.
// Node 24+, a release coordinator, and the existing ui dependencies are required.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash, createHmac, randomBytes, timingSafeEqual } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { copyFile, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { createRequire } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import { monitorEventLoopDelay, performance } from 'node:perf_hooks';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
if (process.argv.includes('--help')) {
  console.log(`Usage: node scripts/fleet-validation.mjs [options]
  --devices 100       Synthetic authenticated clients (1..2000)
  --seconds 45        Workload duration, excluding setup/verification (30..600)
  --port 18083        Loopback-only port; refuses an occupied port
  --label validation Short evidence label, e.g. baseline or candidate
  --binary PATH      Coordinator binary (default: target/release/coordinator.exe)
  --hostLoadContext TEXT  Operator-supplied concurrent-load context (default: unknown)
  --hold-secs 0      Keep the app online for controlled UI review after checks (0..600)
  --review-auto      Submit one 45-second controlled stream when hold begins
  --intervalMs 2000  Delay after each fleet job submission
  --chunks 4         Sequenced output chunks per target
  --chunkBytes 512   ASCII bytes per chunk (at least 128)

Run from any directory. Requires Node 24+, installed ui dependencies, and a
built coordinator. Creates a fresh demo-data/fleet-validation/<timestamp>/
with evidence.json, requests.json, resources.json, coordinator.log and SQLite.
API token/enrollment secret are ephemeral and omitted from reports. The isolated
SQLite database still contains the synthetic enrollment key. No real devices,
Windows commands, Wi-Fi or services are exercised. With --hold-secs, the ignored
private-access.json supplies local UI credentials during the review window only.
review-job.json is a ready-to-POST /api/v1/jobs payload. Its comment-only body is:
  # fleet-review {"duration_secs":45,"chunks_per_second":4,"chunk_bytes":512}
Only during hold, synthetic clients accept up to three such jobs, one active per
device. Bounds: 30..60 seconds, 1..8 chunks/sec, 128..1024 bytes/chunk. These jobs
stream synthetic output and support cancellation; they never execute commands.
Preserve measured devices/jobs. All child processes stop after the optional hold.
With --review-auto, use --hold-secs 60 or longer. Keep the host quiet until
[review-auto-complete]; send/ACK/resource measurements are frozen before
[hold-ready] permits browser inspection. Acknowledgements may drain for 120s
after local emission ends. review-stream.json contains this separate snapshot.
Nonzero exit means validation failed.`);
  process.exit(0);
}
const options = { devices: 100, seconds: 45, port: 18083, label: 'validation', intervalMs: 2000, chunks: 4, chunkBytes: 512, binary: '', hostLoadContext: 'unknown', holdSecs: 0, reviewAuto: false };
for (let index = 2; index < process.argv.length; index++) {
  const requestedName = process.argv[index].replace(/^--/, '');
  const name = ({ 'hold-secs': 'holdSecs', 'review-auto': 'reviewAuto' })[requestedName] ?? requestedName;
  if (name === 'reviewAuto') { options.reviewAuto = true; continue; }
  const value = process.argv[++index];
  assert(Object.hasOwn(options, name) && value, `Unknown or missing option: --${requestedName}`);
  options[name] = typeof options[name] === 'number' ? Number(value) : value;
}
for (const name of ['devices', 'seconds', 'port', 'intervalMs', 'chunks', 'chunkBytes']) {
  assert(Number.isSafeInteger(options[name]) && options[name] > 0, `${name} must be a positive integer`);
}
assert(options.devices <= 2000 && options.seconds >= 30 && options.seconds <= 600, 'Use 1..2000 devices and 30..600 seconds');
assert(options.port >= 1024 && options.port <= 65535, 'Invalid port');
assert(Number.isSafeInteger(options.holdSecs) && options.holdSecs >= 0 && options.holdSecs <= 600, 'hold-secs must be 0..600');
assert(!options.reviewAuto || options.holdSecs >= 60, 'review-auto requires hold-secs of at least 60');
assert(options.chunkBytes >= 128, 'chunkBytes must be at least 128');
assert(options.chunks * options.chunkBytes <= 1024 * 1024, 'Output must fit the protocol cap');
assert(/^[a-zA-Z0-9_-]{1,48}$/.test(options.label), 'Use a short alphanumeric label');
const runId = `${new Date().toISOString().replace(/[:.]/g, '-')}-${options.label}-${randomBytes(3).toString('hex')}`;
const runDir = path.join(root, 'demo-data', 'fleet-validation', runId);
const privateAccessPath = path.join(runDir, 'private-access.json');
const reviewPayloadPath = path.join(runDir, 'review-job.json');
const reviewPrefix = '# fleet-review ';
const reviewPayload = {
  kind: 'script.run',
  payload: { shell: 'powershell', body: reviewPrefix + JSON.stringify({ duration_secs: 45, chunks_per_second: 4, chunk_bytes: 512 }), timeout_secs: 90 },
  targets: { type: 'all' },
};
const sourceBinary = options.binary ? path.resolve(options.binary) : path.join(root, 'target', 'release', process.platform === 'win32' ? 'coordinator.exe' : 'coordinator');
const binary = path.join(runDir, path.basename(sourceBinary));
const base = `http://127.0.0.1:${options.port}`;
const wsBase = `ws://127.0.0.1:${options.port}`;
const token = randomBytes(32).toString('hex');
const environment = Object.fromEntries(Object.entries(process.env).filter(([name]) => !/^SC_/i.test(name)));
const requireUi = createRequire(path.join(root, 'ui', 'package.json'));
// jsdom already depends on undici. Its WebSocket supports authenticated headers.
const { WebSocket: AuthenticatedWebSocket } = requireUi('undici');
const metrics = [];
const phases = [];
const samples = [];
const jobs = [];
const clients = [];
const failures = [];
const checks = [];
const dispatchMs = [];
const resultAckMs = [];
const acknowledgedAt = new Map();
const reconnectMs = [];
const eventCounts = {};
const eventOutputs = new Map();
const eventTargets = new Map();
const eventJobs = new Set();
const eventErrors = [];
const sockets = new Set();
const tasks = new Set();
const reviewJobIds = new Set();
const acceptedReviewJobs = new Map();
const reviewExecutions = new Map();
const reviewSamples = [];
const reviewLoopDelay = monitorEventLoopDelay({ resolution: 20 });
const reviewStats = { acceptedTargets: 0, rejectedTargets: 0, duplicateDeliveries: 0, outputChunksSent: 0, outputBytesSent: 0, resultAcks: 0, cancellationsReceived: 0, errors: [] };
const counters = { executed: 0, duplicateDeliveries: 0, resultsReplayed: 0, outputChunksSent: 0, resultAcks: 0, heartbeatAcks: 0, unexpectedDisconnects: 0, cancellationsReceived: 0 };
const loopDelay = monitorEventLoopDelay({ resolution: 20 });
let coordinator;
let sampler;
let coordinatorLog;
let uiSocket;
let enrollment;
let phase = 'setup';
let polling = false;
let stopping = false;
let latestJob;
let workloadStart;
let workloadEnd;
let snapshotHash;
let version;
let ownListenerReady = false;
let persistedAfterProcessExit = false;
let success = false;
let fatal;
let pollTasks = [];
let measurement;
let review;

function markPhase(name) {
  if (phases.length) phases.at(-1).endedAt = Date.now();
  phase = name;
  phases.push({ name, startedAt: Date.now() });
  console.log(`[${name}] ${options.devices} synthetic clients, ${runId}`);
}

function closeCurrentPhase() {
  if (phases.length) phases.at(-1).endedAt = Date.now();
}

// Browser inspection must not add idle heartbeats, requests, or resource samples
// to the workload that has already passed its checks.
function captureMeasurement() {
  return structuredClone({ metrics, phases, samples, jobs, failures, dispatchMs, resultAckMs, acknowledgedAt, reconnectMs, eventCounts, eventErrors, counters });
}

async function holdForUiReview() {
  if (!options.holdSecs) return;
  const startedAt = Date.now();
  const expiresAt = startedAt + options.holdSecs * 1000;
  review = { startedAt, expiresAt, seconds: options.holdSecs, privateAccessFile: privateAccessPath, payloadFile: reviewPayloadPath };
  await writeFile(reviewPayloadPath, JSON.stringify(reviewPayload, null, 2));
  await writeFile(privateAccessPath, JSON.stringify({ baseUrl: base, apiToken: token, coordinatorPid: coordinator.pid, harnessPid: process.pid, startedAt, expiresAt, reviewPayloadFile: reviewPayloadPath, reviewPayload, scope: 'UI review and controlled synthetic output jobs; preserve measured devices/jobs' }, null, 2), { mode: 0o600 });
  phase = 'hold';
  startSampler(coordinator.pid, reviewSamples, reviewStats.errors);
  reviewLoopDelay.enable();
  console.log(JSON.stringify({ measuredChecksPassed: true, targets: measurement.jobs.length * options.devices, uiPolls: summarizeRequests(measurement.metrics.filter(entry => entry.purpose === 'ui-poll'), workloadEnd - workloadStart) }));
  if (options.reviewAuto) {
    const submittedAt = Date.now();
    const { job } = await api('/jobs', { body: reviewPayload, measure: false });
    review.automaticJob = { id: job.id, submittedAt, postCompletedAt: Date.now() };
    assert.equal(job.targets.length, options.devices, 'Automatic review did not target the full fleet');
    console.log(`[review-auto-start] job=${job.id} targets=${job.targets.length} keep-host-quiet=true`);
    try {
      await waitFor('automatic review local emission finished', () => job.targets.every(target => reviewExecutions.get(target.id)?.result), 75000);
      review.automaticJob.localEmissionFinishedAt = Date.now();
      await waitFor('automatic review results acknowledged', () => job.targets.every(target => reviewExecutions.get(target.id)?.acknowledged), 120000);
      await verifyReviewApiAndEvents([job.id]);
      assert.equal(reviewStats.rejectedTargets, 0, 'Automatic review rejected targets');
      assert.equal(reviewStats.acceptedTargets, options.devices, 'Automatic review did not execute every target');
      assert(reviewSamples.length >= 10 || process.platform !== 'win32', 'Insufficient review coordinator resource samples');
      review.automaticJob.apiAndEventsVerified = true;
    } catch (error) {
      reviewStats.errors.push(error.message);
      review.automaticJob.apiAndEventsVerified = false;
    }
    await stopChild(sampler);
    reviewLoopDelay.disable();
    review.streamMeasurement = captureReviewMeasurement(new Set([job.id]));
    await writeFile(path.join(runDir, 'review-stream.json'), JSON.stringify(review.streamMeasurement, null, 2));
    console.log(`[review-auto-complete] verified=${review.automaticJob.apiAndEventsVerified} snapshot=${path.join(runDir, 'review-stream.json')}`);
  }
  if (Date.now() < expiresAt) console.log(`[hold-ready] started=${new Date(startedAt).toISOString()} expires=${new Date(expiresAt).toISOString()} base=${base} access=${privateAccessPath}`);
  while (Date.now() < expiresAt && !stopping && coordinator.exitCode === null && coordinator.signalCode === null) await delay(250);
  review.endedAt = Date.now();
  review.interrupted = review.endedAt < expiresAt;
  phase = 'review-drain';
  try {
    if (!options.reviewAuto || review.automaticJob.apiAndEventsVerified) await waitFor('controlled review results acknowledged', () => [...reviewExecutions.values()].every(execution => execution.acknowledged), 120000);
    await verifyReviewApiAndEvents(reviewJobIds);
    review.apiAndEventsVerified = true;
  } catch (error) {
    reviewStats.errors.push(error.message);
    review.apiAndEventsVerified = false;
  }
  if (!options.reviewAuto) {
    await stopChild(sampler);
    reviewLoopDelay.disable();
    review.streamMeasurement = captureReviewMeasurement(reviewJobIds);
    await writeFile(path.join(runDir, 'review-stream.json'), JSON.stringify(review.streamMeasurement, null, 2));
  }
  review.errors = [...reviewStats.errors, ...failures.slice(measurement.failures.length)];
  review.uiEventErrors = eventErrors.slice(measurement.eventErrors.length);
  review.unexpectedDisconnects = counters.unexpectedDisconnects - measurement.counters.unexpectedDisconnects;
  review.onlineClientsAtEnd = clients.filter(client => client.online).length;
  review.controlledJobs = [...acceptedReviewJobs].map(([id, instruction]) => ({ id, ...instruction }));
  review.controlledTraffic = { ...reviewStats, errors: undefined };
  review.success = review.apiAndEventsVerified && review.errors.length === 0 && review.uiEventErrors.length === 0 && review.unexpectedDisconnects === 0 && review.onlineClientsAtEnd === options.devices;
}

async function verifyReviewApiAndEvents(ids) {
  for (const id of ids) {
    const { job } = await api(`/jobs/${id}`, { measure: false });
    assert(eventJobs.has(id), 'Missing review job.created UI event');
    assert.equal(job.targets.length, [...reviewExecutions.values()].filter(execution => execution.jobId === id).length, 'Review targets missing from synthetic clients');
    for (const target of job.targets) {
      verifyReviewTarget(target, reviewExecutions.get(target.id));
      assert.equal(eventOutputs.get(target.id) ?? '', target.output, 'Review UI output differs from API');
      assert.deepEqual(eventTargets.get(target.id), { state: target.state, reason: target.reason }, 'Review UI state differs from API');
    }
  }
}

function captureReviewMeasurement(ids) {
  const executions = [...reviewExecutions.values()].filter(execution => ids.has(execution.jobId));
  const targets = executions.map(execution => ({
    id: execution.id, jobId: execution.jobId, deviceId: execution.deviceId,
    receivedAt: execution.receivedAt, firstOutputSentAt: execution.firstOutputSentAt, lastOutputSentAt: execution.lastOutputSentAt,
    resultSentAt: execution.resultSentAt, resultAckAt: execution.resultAckAt,
    firstOutputEventAt: execution.firstOutputEventAt, lastOutputEventAt: execution.lastOutputEventAt,
    outputChunksSent: execution.chunks.length, outputBytesSent: execution.chunks.reduce((total, data) => total + Buffer.byteLength(data), 0),
    outputEventsObserved: execution.outputEventsObserved ?? 0, acknowledged: execution.acknowledged, status: execution.result?.status,
  }));
  const times = name => targets.map(target => target[name]).filter(Number.isFinite);
  const firstSentAt = times('firstOutputSentAt').length ? Math.min(...times('firstOutputSentAt')) : null;
  const lastSentAt = times('lastOutputSentAt').length ? Math.max(...times('lastOutputSentAt')) : null;
  const lastAckAt = times('resultAckAt').length ? Math.max(...times('resultAckAt')) : null;
  const sentChunks = targets.reduce((total, target) => total + target.outputChunksSent, 0);
  const acked = targets.filter(target => target.acknowledged);
  const ackedChunks = acked.reduce((total, target) => total + target.outputChunksSent, 0);
  const bufferedByDevice = clients.map(client => ({ deviceId: client.id, maxBufferedAmountBytes: client.maxReviewBufferedAmountBytes }));
  return structuredClone({
    frozenAt: Date.now(), automaticJob: review.automaticJob, targets,
    emissionElapsedMs: stats(targets.map(target => target.lastOutputSentAt - target.firstOutputSentAt).filter(Number.isFinite)),
    resultDrainMs: stats(targets.map(target => target.resultAckAt - target.resultSentAt).filter(Number.isFinite)),
    fullCompletionMs: stats(targets.map(target => target.resultAckAt - target.receivedAt).filter(Number.isFinite)),
    aggregate: { firstOutputSentAt: firstSentAt, lastOutputSentAt: lastSentAt, lastResultAckAt: lastAckAt, outputChunksSent: sentChunks, acknowledgedTargets: acked.length, acknowledgedOutputChunks: ackedChunks,
      sentChunksPerSecond: firstSentAt !== null && lastSentAt > firstSentAt ? sentChunks * 1000 / (lastSentAt - firstSentAt) : null,
      persistedAcknowledgedChunksPerSecond: firstSentAt !== null && lastAckAt > firstSentAt ? ackedChunks * 1000 / (lastAckAt - firstSentAt) : null,
      persistedAcknowledgedTargetsPerSecond: firstSentAt !== null && lastAckAt > firstSentAt ? acked.length * 1000 / (lastAckAt - firstSentAt) : null },
    socketBuffering: { maxBufferedAmountBytes: Math.max(0, ...bufferedByDevice.map(device => device.maxBufferedAmountBytes)), byDevice: bufferedByDevice },
    coordinatorResources: summarizeResources(reviewSamples),
    harnessEventLoopDelayMs: { p50: reviewLoopDelay.percentile(50) / 1e6, p95: reviewLoopDelay.percentile(95) / 1e6, p99: reviewLoopDelay.percentile(99) / 1e6, max: reviewLoopDelay.max / 1e6 },
    errors: [...reviewStats.errors], uiEventErrors: eventErrors.slice(measurement.eventErrors.length),
    notes: ['WebSocket.send timestamps mean enqueued by the synthetic client, not persisted.', 'bufferedAmount covers the client WebSocket queue, not kernel buffers or pending coordinator work.', 'Result acknowledgements follow durable coordinator commit; acknowledged chunk rate covers complete acknowledged targets.', 'Resource and event-loop samples cover the review stream, drain, and API verification, separately from the frozen short-job workload.'],
  });
}

function verifyReviewTarget(target, execution) {
  assert(execution?.result && execution.acknowledged, `Unacknowledged review target: ${target.id}`);
  assert.equal(target.state, execution.result.status, `Wrong review state: ${target.id}`);
  assert.equal(target.reason, execution.result.reason ?? null);
  assert.equal(target.output, execution.chunks.join(''), `Wrong review output: ${target.id}`);
  assert.notEqual(target.output_truncated, true, 'Controlled review output was unexpectedly truncated');
  if (execution.result.status === 'succeeded') assert.equal(execution.chunks.length, execution.instruction.duration_secs * execution.instruction.chunks_per_second);
}

function parseReviewInstruction(message) {
  assert.equal(phase, 'hold', 'Controlled review jobs are accepted only during the hold');
  assert.equal(message.kind, 'script.run', 'Only controlled script.run review jobs are accepted');
  assert(typeof message.payload.body === 'string' && message.payload.body.startsWith(reviewPrefix), 'Expected the documented # fleet-review JSON comment');
  const instruction = JSON.parse(message.payload.body.slice(reviewPrefix.length));
  assert(instruction && typeof instruction === 'object' && !Array.isArray(instruction), 'Invalid review instruction');
  const bounds = { duration_secs: [30, 60], chunks_per_second: [1, 8], chunk_bytes: [128, 1024] };
  assert(Object.keys(instruction).every(key => Object.hasOwn(bounds, key)), 'Unexpected review instruction field');
  for (const [key, [minimum, maximum]] of Object.entries(bounds)) assert(Number.isInteger(instruction[key]) && instruction[key] >= minimum && instruction[key] <= maximum, `${key} must be ${minimum}..${maximum}`);
  assert(Date.now() + instruction.duration_secs * 1000 + 5000 <= review.expiresAt, 'Insufficient hold time remains for this review job');
  assert(acceptedReviewJobs.has(message.job_id) || acceptedReviewJobs.size < 3, 'At most three controlled review jobs are allowed');
  return instruction;
}

function track(promise) {
  tasks.add(promise);
  promise.catch(error => failures.push(error.message)).finally(() => tasks.delete(promise));
}

async function waitFor(description, predicate, timeoutMs = 30000) {
  const started = performance.now();
  while (performance.now() - started < timeoutMs) {
    if (stopping) throw new Error('Validation interrupted');
    if (failures.length) throw new Error(failures[0]);
    if (await predicate()) return;
    await delay(50);
  }
  throw new Error(`Timed out: ${description}`);
}

async function api(endpoint, { body, method = body ? 'POST' : 'GET', measure = true, purpose = 'control' } = {}) {
  const started = performance.now();
  const entry = { endpoint: endpoint.replace(/\/jobs\/[a-f0-9-]+/g, '/jobs/:id'), method, purpose, phase, at: Date.now() };
  try {
    const response = await fetch(`${base}/api/v1${endpoint}`, {
      method,
      headers: { Authorization: `Bearer ${token}`, ...(body ? { 'Content-Type': 'application/json' } : {}) },
      body: body ? JSON.stringify(body) : undefined,
      signal: AbortSignal.timeout(15000),
    });
    entry.status = response.status;
    const text = await response.text();
    assert(response.ok, `${method} ${entry.endpoint}: HTTP ${response.status}`);
    return text ? JSON.parse(text) : null;
  } catch (error) {
    entry.error = error.message;
    throw error;
  } finally {
    entry.ms = performance.now() - started;
    if (measure) metrics.push(entry);
  }
}

function mac(role, clientNonce, serverNonce) {
  return createHmac('sha256', enrollment.secret)
    .update(['surface-coordinator/handshake/v1', role, clientNonce, serverNonce].join('\0')).digest('hex');
}

function outputChunk(deviceId, targetId, sequence, bytes) {
  const prefix = `${deviceId}/${targetId}/${sequence}:`;
  return prefix + 'x'.repeat(Math.max(0, bytes - prefix.length - 1)) + '\n';
}

function expectedChunks(deviceId, targetId) {
  return Array.from({ length: options.chunks }, (_, index) => outputChunk(deviceId, targetId, index + 1, options.chunkBytes));
}

class SyntheticDevice {
  constructor(index) {
    this.id = `fleet-${String(index).padStart(4, '0')}`;
    this.executed = new Map();
    this.pending = new Map();
    this.connections = 0;
    this.online = false;
    this.maxReviewBufferedAmountBytes = 0;
  }

  async connect() {
    const started = performance.now();
    const nonce = randomBytes(32).toString('hex');
    const socket = new WebSocket(`${wsBase}/client/ws`);
    sockets.add(socket);
    this.socket = socket;
    this.online = false;
    this.connections++;
    let plannedClose = false;
    let welcomed = false;
    socket.plannedClose = () => { plannedClose = true; };
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error(`Handshake timeout: ${this.id}`)), 15000);
      socket.addEventListener('open', () => socket.send(JSON.stringify({
        type: 'hello', protocol_version: 1, device_id: this.id, hostname: this.id,
        os: 'windows', client_version: version.replace(/^coordinator\s+/, ''), key_id: enrollment.id, nonce,
        network: { ssid: 'SYNTHETIC-LOOPBACK', ip: '127.0.0.1' },
      })));
      socket.addEventListener('message', event => {
        try {
          const message = JSON.parse(event.data);
          if (message.type === 'challenge') {
            const expected = Buffer.from(mac('coordinator', nonce, message.nonce), 'hex');
            const received = Buffer.from(message.mac, 'hex');
            assert(received.length === expected.length && timingSafeEqual(expected, received), 'Coordinator HMAC mismatch');
            socket.send(JSON.stringify({ type: 'auth', mac: mac('client', nonce, message.nonce) }));
          } else if (message.type === 'welcome') {
            assert(!message.restricted, 'Unexpected restricted device');
            clearTimeout(timeout);
            welcomed = true;
            this.online = true;
            this.heartbeat = setInterval(() => this.send({ type: 'heartbeat' }), Math.min(message.heartbeat_interval_ms, 1000));
            reconnectMs.push({ phase, ms: performance.now() - started });
            // Let the coordinator redeliver unresolved work before replaying results.
            if (this.connections > 1) track(delay(350).then(() => this.replay()));
            resolve();
          } else if (message.type === 'job') {
            this.receiveJob(message);
          } else if (message.type === 'job.result_ack') {
            counters.resultAcks++;
            const pending = this.pending.get(message.job_target_id);
            if (pending) {
              resultAckMs.push(performance.now() - pending.sentAt);
              if (!acknowledgedAt.has(message.job_target_id)) acknowledgedAt.set(message.job_target_id, Date.now());
              if (pending.execution.review) {
                pending.execution.acknowledged = true;
                pending.execution.resultAckAt = Date.now();
                reviewStats.resultAcks++;
              }
            }
            this.pending.delete(message.job_target_id);
          } else if (message.type === 'job.cancel') {
            const execution = this.executed.get(message.job_target_id);
            if (execution && !execution.result) {
              counters.cancellationsReceived++;
              if (execution.review) reviewStats.cancellationsReceived++;
              this.finish(execution, true);
            }
          } else if (message.type === 'heartbeat_ack') {
            counters.heartbeatAcks++;
          } else if (message.type === 'reject') {
            throw new Error(`Handshake rejected: ${message.reason}`);
          }
        } catch (error) {
          failures.push(`${this.id}: ${error.message}`);
          clearTimeout(timeout);
          reject(error);
        }
      });
      socket.addEventListener('error', () => {
        clearTimeout(timeout);
        if (!stopping && !plannedClose) failures.push(`WebSocket error: ${this.id}`);
        reject(new Error(`WebSocket error: ${this.id}`));
      });
      socket.addEventListener('close', () => {
        clearTimeout(timeout);
        sockets.delete(socket);
        if (this.socket === socket) {
          this.online = false;
          clearInterval(this.heartbeat);
        }
        if (!stopping && !plannedClose) counters.unexpectedDisconnects++;
        if (!welcomed) reject(new Error(`Socket closed before welcome: ${this.id}`));
      });
    });
  }

  send(message) {
    if (this.socket?.readyState !== WebSocket.OPEN) return false;
    this.socket.send(JSON.stringify(message));
    if (review) this.maxReviewBufferedAmountBytes = Math.max(this.maxReviewBufferedAmountBytes, this.socket.bufferedAmount);
    return true;
  }

  receiveJob(message) {
    const existing = this.executed.get(message.job_target_id);
    if (existing) {
      counters.duplicateDeliveries++;
      if (existing.review) reviewStats.duplicateDeliveries++;
      return; // Just as a real client's durable executed-target journal does.
    }
    if (measurement) {
      this.receiveReviewJob(message);
      return;
    }
    assert(message.kind === 'script.run' && typeof message.payload.body === 'string' && message.payload.body.startsWith('# fleet-validation '), 'Unexpected instruction during measured workload');
    const instruction = JSON.parse(message.payload.body.slice('# fleet-validation '.length));
    dispatchMs.push(Date.now() - instruction.submittedAt);
    const execution = { id: message.job_target_id, mode: instruction.mode, chunks: expectedChunks(this.id, message.job_target_id) };
    this.executed.set(execution.id, execution);
    counters.executed++;
    this.send({ type: 'job.started', job_target_id: execution.id });
    this.sendOutput(execution);
    if (execution.mode === 'replay') {
      this.finish(execution, false, true);
    } else if (execution.mode !== 'cancel') {
      track(delay(100).then(() => this.finish(execution)));
    }
  }

  receiveReviewJob(message) {
    reviewJobIds.add(message.job_id);
    const execution = { id: message.job_target_id, jobId: message.job_id, deviceId: this.id, receivedAt: Date.now(), review: true, chunks: [], acknowledged: false };
    this.executed.set(execution.id, execution);
    reviewExecutions.set(execution.id, execution);
    this.send({ type: 'job.started', job_target_id: execution.id });
    try {
      const instruction = parseReviewInstruction(message);
      assert(![...this.executed.values()].some(other => other !== execution && other.review && !other.result), 'A controlled review job is already active on this device');
      execution.instruction = instruction;
      acceptedReviewJobs.set(message.job_id, instruction);
      reviewStats.acceptedTargets++;
      track(this.streamReviewOutput(execution));
    } catch (error) {
      reviewStats.rejectedTargets++;
      reviewStats.errors.push(`${this.id}: ${error.message}`);
      console.error(`[review-rejected] ${this.id}: ${error.message}`);
      execution.failure = { reason: 'invalid_payload', message: error.message };
      this.finish(execution);
    }
  }

  async streamReviewOutput(execution) {
    const { duration_secs, chunks_per_second, chunk_bytes } = execution.instruction;
    for (let sequence = 1; sequence <= duration_secs * chunks_per_second; sequence++) {
      // A delayed client never catches up by dumping a burst of overdue chunks.
      await delay(sequence === 1 ? 1 : 1000 / chunks_per_second);
      if (stopping || execution.result) return;
      const data = outputChunk(this.id, execution.id, sequence, chunk_bytes);
      assert(this.send({ type: 'job.output', job_target_id: execution.id, seq: sequence, stream: 'stdout', data }), 'Review device disconnected while streaming output');
      const sentAt = Date.now();
      execution.firstOutputSentAt ??= sentAt;
      execution.lastOutputSentAt = sentAt;
      execution.chunks.push(data);
      reviewStats.outputChunksSent++;
      reviewStats.outputBytesSent += data.length;
    }
    this.finish(execution);
  }

  sendOutput(execution) {
    execution.chunks.forEach((data, index) => {
      if (this.send({ type: 'job.output', job_target_id: execution.id, seq: index + 1, stream: 'stdout', data })) counters.outputChunksSent++;
    });
  }

  finish(execution, cancelled = false, hold = false) {
    if (execution.result) return;
    const failure = execution.failure ?? (cancelled ? { reason: 'cancelled', message: 'Synthetic cancellation acknowledged' } : null);
    execution.result = {
      type: 'job.result', job_target_id: execution.id, status: failure ? 'failed' : 'succeeded',
      ...(failure ?? { exit_code: 0 }),
    };
    const pending = { execution, sentAt: performance.now() };
    this.pending.set(execution.id, pending);
    if (!hold) {
      if (execution.review) execution.resultSentAt = Date.now();
      this.send(execution.result);
    }
  }

  replay() {
    for (const pending of this.pending.values()) {
      // Resend output with the same sequence numbers as well as the final result.
      this.sendOutput(pending.execution);
      pending.sentAt = performance.now();
      this.send(pending.execution.result);
      counters.resultsReplayed++;
    }
  }

  async disconnect() {
    const socket = this.socket;
    clearInterval(this.heartbeat);
    if (!socket || socket.readyState === WebSocket.CLOSED) return;
    socket.plannedClose();
    const closed = new Promise(resolve => socket.addEventListener('close', resolve, { once: true }));
    socket.close(1000, 'Synthetic reconnect burst');
    await Promise.race([closed, delay(2500)]);
  }
}

async function connectUi() {
  uiSocket = new AuthenticatedWebSocket(`${wsBase}/api/v1/events`, { headers: { Authorization: `Bearer ${token}` } });
  sockets.add(uiSocket);
  uiSocket.addEventListener('close', () => {
    sockets.delete(uiSocket);
    if (!stopping) eventErrors.push('UI WebSocket closed during workload');
  });
  uiSocket.addEventListener('message', event => {
    try {
      const message = JSON.parse(event.data);
      eventCounts[message.type] = (eventCounts[message.type] ?? 0) + 1;
      if (message.type === 'resync') {
        eventErrors.push('UI events overflowed: resync required');
      } else if (message.type === 'job.created') {
        eventJobs.add(message.job.id);
        if (measurement && !measurement.jobs.some(job => job.id === message.job.id)) reviewJobIds.add(message.job.id);
      } else if (message.type === 'job.target.updated') {
        eventTargets.set(message.target.id, { state: message.target.state, reason: message.target.reason });
      } else if (message.type === 'job.output') {
        const output = eventOutputs.get(message.job_target_id) ?? '';
        assert(message.offset === output.length, `Output event offset mismatch: ${message.job_target_id}`);
        eventOutputs.set(message.job_target_id, output + message.data);
        const execution = reviewExecutions.get(message.job_target_id);
        if (execution) {
          const receivedAt = Date.now();
          execution.firstOutputEventAt ??= receivedAt;
          execution.lastOutputEventAt = receivedAt;
          execution.outputEventsObserved = (execution.outputEventsObserved ?? 0) + 1;
        }
      }
    } catch (error) {
      eventErrors.push(error.message);
    }
  });
  await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error('UI WebSocket did not open')), 15000);
    uiSocket.addEventListener('open', () => { clearTimeout(timeout); resolve(); }, { once: true });
    uiSocket.addEventListener('error', () => { clearTimeout(timeout); reject(new Error('UI WebSocket failed')); }, { once: true });
  });
}

async function createJob(mode = 'normal') {
  const { job } = await api('/jobs', { body: {
    kind: 'script.run', payload: { shell: 'powershell', body: `# fleet-validation ${JSON.stringify({ mode, submittedAt: Date.now() })}`, timeout_secs: 300 },
    targets: { type: 'all' },
  } });
  jobs.push({ id: job.id, mode });
  latestJob = job.id;
  assert.equal(job.targets.length, options.devices);
  return job;
}

async function poll(worker) {
  while (polling && !stopping) {
    const endpoint = worker === 0 ? '/devices' : worker === 1 ? '/jobs?limit=50' : latestJob ? `/jobs/${latestJob}` : '/groups';
    const started = performance.now();
    try { await api(endpoint, { purpose: 'ui-poll' }); } catch { /* Report all failures, keep sampling. */ }
    await delay(Math.max(1, 100 - (performance.now() - started)));
  }
}

async function jobRunning(job) {
  await waitFor('all job targets running', async () => {
    const result = await api(`/jobs/${job.id}`);
    return result.job.targets.every(target => target.state === 'running');
  });
}

async function replayBurst() {
  const job = await createJob('replay');
  await jobRunning(job);
  await Promise.all(clients.map(client => client.disconnect()));
  await delay(150);
  await Promise.all(clients.map(client => client.connect()));
  await waitFor('replayed results acknowledged', () => clients.every(client => !client.pending.has(job.targets.find(target => target.device_id === client.id).id)));
  checks.push('Reconnect burst: all clients reauthenticated, replayed pending results, and received acknowledgements');
}

async function cancelActive() {
  const job = await createJob('cancel');
  await jobRunning(job);
  const started = performance.now();
  await api(`/jobs/${job.id}/cancel`, { method: 'POST' });
  await waitFor('active cancellation acknowledged', async () => {
    const result = await api(`/jobs/${job.id}`);
    return result.job.targets.every(target => target.state === 'failed' && target.reason === 'cancelled');
  });
  checks.push(`Active fleet cancellation completed in ${Math.round(performance.now() - started)} ms`);
}

async function assertPortFree() {
  const guard = createServer();
  await new Promise((resolve, reject) => {
    guard.once('error', () => reject(new Error(`127.0.0.1:${options.port} is occupied; refusing to touch it`)));
    guard.listen({ host: '127.0.0.1', port: options.port, exclusive: true }, resolve);
  });
  await new Promise(resolve => guard.close(resolve));
}

function startSampler(pid, output = samples, errors = failures) {
  if (process.platform !== 'win32') return;
  const powershell = path.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe');
  const command = `$ErrorActionPreference='Stop'; while ($true) { try { $sampleProcess=Get-Process -Id ${pid}; [pscustomobject]@{at=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();rssBytes=$sampleProcess.WorkingSet64;privateBytes=$sampleProcess.PrivateMemorySize64;cpuSeconds=$sampleProcess.TotalProcessorTime.TotalSeconds;threads=$sampleProcess.Threads.Count;handles=$sampleProcess.HandleCount} | ConvertTo-Json -Compress; Start-Sleep -Milliseconds 1000 } catch { break } }`;
  sampler = spawn(powershell, ['-NoLogo', '-NoProfile', '-NonInteractive', '-Command', command], { windowsHide: true, env: environment, stdio: ['ignore', 'pipe', 'pipe'] });
  let buffer = '';
  sampler.stdout.setEncoding('utf8');
  sampler.stdout.on('data', chunk => {
    buffer += chunk;
    let newline;
    while ((newline = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (line) {
        try { output.push(JSON.parse(line)); } catch { errors.push('Invalid process sampler output'); }
      }
    }
  });
  sampler.on('error', error => errors.push(`Sampler failed: ${error.message}`));
}

function stats(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const percentile = fraction => sorted[Math.max(0, Math.ceil(sorted.length * fraction) - 1)] ?? null;
  return { count: sorted.length, p50: percentile(.5), p95: percentile(.95), p99: percentile(.99), max: sorted.at(-1) ?? null };
}

function summarizeRequests(requests, elapsedMs) {
  return { latencyMs: stats(requests.map(entry => entry.ms)), requests: requests.length, failures: requests.filter(entry => entry.error).length, requestsPerSecond: requests.length * 1000 / elapsedMs };
}

function summarizeResources(entries) {
  const first = entries[0];
  const last = entries.at(-1);
  const cpuSeconds = last && first ? last.cpuSeconds - first.cpuSeconds : null;
  const sampledSeconds = last && first ? (last.at - first.at) / 1000 : null;
  const cpuPercentOneCore = entries.slice(1).map((entry, index) => (entry.cpuSeconds - entries[index].cpuSeconds) * 100000 / (entry.at - entries[index].at));
  return { samples: entries.length, rssBytes: stats(entries.map(entry => entry.rssBytes)), privateBytes: stats(entries.map(entry => entry.privateBytes)), maxThreads: entries.length ? Math.max(...entries.map(entry => entry.threads)) : null,
    cpuSeconds, sampledSeconds, averageCpuPercentOneCore: sampledSeconds ? cpuSeconds * 100 / sampledSeconds : null, averageCpuPercentHost: sampledSeconds ? cpuSeconds * 100 / sampledSeconds / os.cpus().length : null, sampledCpuPercentOneCore: stats(cpuPercentOneCore) };
}

async function stopChild(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise(resolve => child.once('exit', resolve));
  child.kill();
  await Promise.race([exited, delay(3000)]);
  if (child.exitCode === null && child.signalCode === null) {
    child.kill('SIGKILL');
    await Promise.race([exited, delay(3000)]);
  }
}

async function verifyPersistedDatabase() {
  assert(coordinator.exitCode !== null || coordinator.signalCode !== null, 'Owned coordinator did not stop');
  // This read happens after forceful Windows process termination, not through the API.
  const { DatabaseSync } = await import('node:sqlite');
  const database = new DatabaseSync(path.join(runDir, 'coordinator', 'coordinator.db'), { readOnly: true });
  try {
    assert.deepEqual(database.prepare('PRAGMA quick_check').all().map(row => row.quick_check), ['ok']);
    const hasOutputChunks = database.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'job_target_output_chunks'").get();
    const targetsForJob = database.prepare(hasOutputChunks
      ? "SELECT t.id, t.job_id, t.device_id, t.state, t.reason, t.output || COALESCE((SELECT group_concat(data, '') FROM (SELECT data FROM job_target_output_chunks WHERE target_id = t.id ORDER BY seq)), '') AS output FROM job_targets t WHERE t.job_id = ?"
      : 'SELECT id, job_id, device_id, state, reason, output FROM job_targets WHERE job_id = ?');
    const stored = jobs.flatMap(job => targetsForJob.all(job.id));
    const modes = new Map(jobs.map(job => [job.id, job.mode]));
    assert.equal(stored.length, jobs.length * options.devices);
    for (const target of stored) {
      assert(modes.has(target.job_id), 'Unexpected persisted job');
      assert.equal(target.state, modes.get(target.job_id) === 'cancel' ? 'failed' : 'succeeded');
      assert.equal(target.reason, modes.get(target.job_id) === 'cancel' ? 'cancelled' : null);
      assert.equal(target.output, expectedChunks(target.device_id, target.id).join(''));
    }
    persistedAfterProcessExit = true;
    checks.push('After coordinator termination, SQLite quick_check passed and every target result/output remained persisted');
    if (review) {
      try {
        for (const jobId of reviewJobIds) {
          const targets = targetsForJob.all(jobId);
          assert.equal(targets.length, [...reviewExecutions.values()].filter(execution => execution.jobId === jobId).length, 'Persisted review target count differs');
          for (const target of targets) verifyReviewTarget(target, reviewExecutions.get(target.id));
        }
        review.persistedAfterProcessExit = true;
      } catch (error) {
        review.persistedAfterProcessExit = false;
        review.success = false;
        review.errors.push(`Review persistence check: ${error.message}`);
      }
    }
  } finally {
    database.close();
  }
}

for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => {
  stopping = true;
  for (const socket of sockets) socket.close();
  coordinator?.kill();
  sampler?.kill();
});

try {
  await assertPortFree();
  await mkdir(path.join(runDir, 'coordinator'), { recursive: true });
  await copyFile(sourceBinary, binary); // Snapshot permits parallel release rebuilds without changing this run.
  snapshotHash = createHash('sha256').update(await readFile(binary)).digest('hex');
  version = await new Promise((resolve, reject) => {
    const child = spawn(binary, ['--version'], { windowsHide: true, env: environment });
    let output = '';
    child.stdout.on('data', chunk => { output += chunk; });
    child.once('error', reject);
    child.once('exit', code => code === 0 ? resolve(output.trim()) : reject(new Error('Binary --version failed')));
  });
  coordinatorLog = createWriteStream(path.join(runDir, 'coordinator.log'));
  coordinator = spawn(binary, ['serve', '--data-dir', path.join(runDir, 'coordinator'), '--bind', `127.0.0.1:${options.port}`, '--auth', 'token', '--no-update-check', '--heartbeat-interval-secs', '5'], {
    env: { ...environment, SC_API_TOKEN: token }, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'],
  });
  coordinator.stdout.pipe(coordinatorLog, { end: false });
  coordinator.stderr.pipe(coordinatorLog, { end: false });
  let startupOutput = '';
  coordinator.stdout.on('data', chunk => {
    if (!ownListenerReady) {
      startupOutput += chunk.toString();
      ownListenerReady = startupOutput.includes(`Web UI and API on ${base}`);
    }
  });
  coordinator.on('error', error => failures.push(`Coordinator failed: ${error.message}`));
  coordinator.on('exit', code => { if (!stopping) failures.push(`Coordinator exited during validation: ${code}`); });
  startSampler(coordinator.pid);
  // Establish that our child owns the listener before issuing any API writes.
  await waitFor('owned coordinator listener', () => ownListenerReady);
  await waitFor('coordinator startup', async () => {
    try { await api('/health', { measure: false }); return true; } catch { return false; }
  });
  enrollment = (await api('/enrollment-keys', { body: { label: 'Isolated synthetic fleet' } })).key;
  await connectUi();
  markPhase('connect');
  for (let index = 0; index < options.devices; index++) clients.push(new SyntheticDevice(index));
  await Promise.all(clients.map(client => client.connect()));
  await waitFor('all devices online', async () => (await api('/devices')).devices.filter(device => device.online).length === options.devices);
  checks.push(`${options.devices} mutually authenticated devices online`);
  markPhase('steady');
  workloadStart = Date.now();
  loopDelay.enable();
  polling = true;
  pollTasks = [0, 1, 2].map(poll);
  const totalMs = options.seconds * 1000;
  let nextJobAt = workloadStart;
  let replayed = false;
  let cancelled = false;
  while (Date.now() - workloadStart < totalMs) {
    if (failures.length || stopping) throw new Error(failures[0] ?? 'Interrupted');
    const elapsed = Date.now() - workloadStart;
    if (!replayed && elapsed >= totalMs / 3) {
      markPhase('reconnect');
      await replayBurst();
      replayed = true;
    }
    if (!cancelled && elapsed >= totalMs * 2 / 3) {
      markPhase('cancellation');
      await cancelActive();
      cancelled = true;
    }
    if (Date.now() >= nextJobAt) {
      await createJob();
      nextJobAt = Date.now() + options.intervalMs;
    }
    await delay(25);
  }
  assert(replayed && cancelled, 'Workload ended before reconnect/cancellation phases');
  workloadEnd = Date.now();
  polling = false;
  await stopChild(sampler);
  await Promise.all(pollTasks);
  loopDelay.disable();
  markPhase('verify');
  await waitFor('all results acknowledged', () => acknowledgedAt.size === jobs.length * options.devices && clients.every(client => client.pending.size === 0));
  for (const job of jobs) {
    const result = (await api(`/jobs/${job.id}`)).job;
    assert.equal(result.targets.length, options.devices);
    assert(eventJobs.has(job.id), 'Missing job.created UI event');
    for (const target of result.targets) {
      const cancelledTarget = job.mode === 'cancel';
      assert.equal(target.state, cancelledTarget ? 'failed' : 'succeeded', `Wrong final state: ${target.id}`);
      assert.equal(target.reason, cancelledTarget ? 'cancelled' : null);
      assert.equal(target.output, expectedChunks(target.device_id, target.id).join(''), `Persisted output mismatch: ${target.id}`);
      assert.equal(target.output_truncated, false);
      assert.equal(eventOutputs.get(target.id), target.output, `UI output differs from API: ${target.id}`);
      assert.deepEqual(eventTargets.get(target.id), { state: target.state, reason: target.reason }, `UI final state differs: ${target.id}`);
    }
  }
  assert.equal(counters.executed, jobs.length * options.devices, 'Targets executed more than once or skipped');
  assert.equal(acknowledgedAt.size, jobs.length * options.devices, 'Some target results were not acknowledged');
  assert(counters.resultsReplayed >= options.devices, 'Pending results were not replayed for the full fleet');
  assert(counters.duplicateDeliveries >= options.devices, 'Unresolved jobs were not redelivered to the full fleet');
  assert.equal(counters.cancellationsReceived, options.devices);
  assert.equal(counters.unexpectedDisconnects, 0);
  assert.equal(metrics.filter(entry => entry.error).length, 0);
  assert.equal(eventErrors.length, 0, eventErrors[0]);
  assert((eventCounts.heartbeat ?? 0) >= 2, 'UI heartbeat was not observed');
  assert.equal((await api('/devices')).devices.filter(device => device.online).length, options.devices);
  assert(samples.length >= 10 || process.platform !== 'win32', 'Insufficient coordinator resource samples');
  checks.push('Exactly one synthetic execution per target despite duplicate delivery');
  checks.push('Every persisted target has the expected final state and byte-for-byte sequenced output');
  checks.push('UI job creation, output offsets/content, final states, and heartbeats match the API without resync');
  checks.push('No unexpected device disconnects or failed HTTP requests; all devices online at finish');
  closeCurrentPhase();
  measurement = captureMeasurement();
  success = true;
  await holdForUiReview();
} catch (error) {
  success = false;
  fatal = error.message;
  console.error(`Validation failed: ${fatal}`);
} finally {
  stopping = true;
  polling = false;
  loopDelay.disable();
  reviewLoopDelay.disable();
  for (const client of clients) clearInterval(client.heartbeat);
  for (const socket of sockets) {
    try { socket.close(); } catch { /* Coordinator termination also releases connections. */ }
  }
  await stopChild(coordinator);
  await stopChild(sampler);
  await Promise.allSettled(pollTasks);
  await Promise.allSettled([...tasks]);
  if (coordinatorLog) await new Promise(resolve => coordinatorLog.end(resolve));
  if (review) {
    try { await rm(privateAccessPath, { force: true }); review.privateAccessFileDeleted = true; } catch (error) {
      review.privateAccessFileDeleted = false;
      review.privateAccessCleanupError = error.message;
    }
  }
  if (success) {
    try { await verifyPersistedDatabase(); } catch (error) {
      success = false;
      fatal = `Post-exit persistence check: ${error.message}`;
    }
  }
  if (review?.success === false) {
    success = false;
    fatal ??= 'Controlled UI review failed; inspect review diagnostics';
  }
  if (!measurement) closeCurrentPhase();
  const { metrics, phases, samples, jobs, failures, dispatchMs, resultAckMs, acknowledgedAt, reconnectMs, eventCounts, eventErrors, counters } = measurement ?? captureMeasurement();
  const elapsedMs = (workloadEnd ?? Date.now()) - (workloadStart ?? Date.now());
  const polls = metrics.filter(entry => entry.purpose === 'ui-poll');
  const mutations = metrics.filter(entry => entry.method === 'POST');
  const completedDuringWorkload = [...acknowledgedAt.values()].filter(at => at >= workloadStart && at <= workloadEnd).length;
  const firstSample = samples[0];
  const lastSample = samples.at(-1);
  const cpuSeconds = lastSample && firstSample ? lastSample.cpuSeconds - firstSample.cpuSeconds : null;
  const sampledSeconds = lastSample && firstSample ? (lastSample.at - firstSample.at) / 1000 : null;
  for (let index = 0; index < samples.length; index++) {
    const sample = samples[index];
    sample.phase = phases.find(item => sample.at >= item.startedAt && sample.at < item.endedAt)?.name ?? 'setup';
    if (index) {
      const previous = samples[index - 1];
      sample.cpuPercentOneCore = (sample.cpuSeconds - previous.cpuSeconds) * 100000 / (sample.at - previous.at);
    }
  }
  const report = {
    success, measuredChecksPassed: Boolean(measurement), fatal, label: options.label, generatedAt: new Date().toISOString(),
    host: { hostname: os.hostname(), platform: os.platform(), release: os.release(), architecture: os.arch(), logicalCpus: os.cpus().length, cpuModel: os.cpus()[0]?.model, totalMemoryBytes: os.totalmem(), node: process.version, concurrentLoadContext: options.hostLoadContext, loadContextSource: 'Operator supplied, not an idle-host guarantee' },
    binary: { source: sourceBinary, sha256: snapshotHash, version },
    isolation: { bind: `127.0.0.1:${options.port}`, dataDirectory: path.join(runDir, 'coordinator'), authMode: 'token', inheritedScVariables: 'removed', updates: 'disabled', automaticRollout: false, cleanup: 'Sockets closed; owned coordinator and sampler processes terminated' },
    workload: { ...options, elapsedMs, jobs: jobs.length, targets: jobs.length * options.devices, targetsAcknowledgedDuringWorkload: completedDuringWorkload, targetCompletionsPerSecond: completedDuringWorkload * 1000 / elapsedMs, outputBytesPerTarget: options.chunks * options.chunkBytes, syntheticHeartbeatIntervalMs: 1000, coordinatorHeartbeatIntervalMs: 5000, pollConcurrency: 3, pollIntervalMs: 100 },
    api: { uiPolls: summarizeRequests(polls, elapsedMs), byEndpoint: Object.fromEntries([...new Set(polls.map(entry => entry.endpoint))].map(endpoint => [endpoint, summarizeRequests(polls.filter(entry => entry.endpoint === endpoint), elapsedMs)])), postByEndpoint: Object.fromEntries([...new Set(mutations.map(entry => entry.endpoint))].map(endpoint => [endpoint, summarizeRequests(mutations.filter(entry => entry.endpoint === endpoint), elapsedMs)])), byPhase: Object.fromEntries(phases.filter(item => ['steady', 'reconnect', 'cancellation'].includes(item.name)).map(item => [item.name, summarizeRequests(polls.filter(entry => entry.phase === item.name), item.endedAt - item.startedAt)])), jobSubmissionMs: stats(metrics.filter(entry => entry.method === 'POST' && entry.endpoint === '/jobs').map(entry => entry.ms)) },
    protocol: { ...counters, dispatchMs: stats(dispatchMs), resultAckMs: stats(resultAckMs), handshakeMsByPhase: Object.fromEntries([...new Set(reconnectMs.map(entry => entry.phase))].map(name => [name, stats(reconnectMs.filter(entry => entry.phase === name).map(entry => entry.ms))])) },
    uiEvents: { counts: eventCounts, errors: eventErrors },
    coordinatorResources: { samples: samples.length, rssBytes: stats(samples.map(sample => sample.rssBytes)), privateBytes: stats(samples.map(sample => sample.privateBytes)), maxThreads: samples.length ? Math.max(...samples.map(sample => sample.threads)) : null, cpuSeconds, sampledSeconds, averageCpuPercentOneCore: sampledSeconds ? cpuSeconds * 100 / sampledSeconds : null, averageCpuPercentHost: sampledSeconds ? cpuSeconds * 100 / sampledSeconds / os.cpus().length : null, sampledCpuPercentOneCore: stats(samples.flatMap(sample => sample.cpuPercentOneCore === undefined ? [] : [sample.cpuPercentOneCore])) },
    harnessEventLoopDelayMs: { p50: loopDelay.percentile(50) / 1e6, p95: loopDelay.percentile(95) / 1e6, p99: loopDelay.percentile(99) / 1e6, max: loopDelay.max / 1e6 },
    persistedAfterProcessExit, checks, failures, phases, review,
    limitations: ['Synthetic Windows protocol clients on loopback, not physical Surface Wi-Fi or Windows service execution.', 'Coordinator and harness share a host; CPU, storage, and scheduler contention affect timings.', 'Closed-loop job submission waits for each POST and then intervalMs; slower servers receive fewer jobs in the same duration.', 'Fresh SQLite database; this does not establish performance for long-lived fleet history.', 'API polling and UI event reconstruction exercise server delivery, not browser rendering performance.', 'No claims about slow-reader backpressure, packet loss, or power/Wi-Fi changes are made by this workload.'],
  };
  await mkdir(runDir, { recursive: true });
  await writeFile(path.join(runDir, 'evidence.json'), JSON.stringify(report, null, 2));
  await writeFile(path.join(runDir, 'requests.json'), JSON.stringify(metrics, null, 2));
  await writeFile(path.join(runDir, 'resources.json'), JSON.stringify(samples, null, 2));
  if (review) await writeFile(path.join(runDir, 'review-resources.json'), JSON.stringify(reviewSamples, null, 2));
  console.log(JSON.stringify({ success, evidence: path.join(runDir, 'evidence.json'), uiPolls: report.api.uiPolls, targets: report.workload.targets, maxCoordinatorRssMiB: report.coordinatorResources.rssBytes.max / 1024 / 1024, fatal }, null, 2));
  process.exitCode = success ? 0 : 1;
}
