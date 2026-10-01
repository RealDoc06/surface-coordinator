import { describe, expect, it } from 'vitest'
import type { JobOutputEvent, Rejected } from '../api/types'
import { device, job, target } from '../test/helpers'
import {
  appendOutput,
  applyEvent,
  codePointLength,
  initialState,
  listedJobs,
  mergeBuffered,
  mergeBufferedEvents,
  reducer,
  type Action,
  type State,
} from './store'

function run(state: State, ...actions: Action[]): State {
  return actions.reduce(reducer, state)
}

function output(targetId: string, data: string, truncated = false): JobOutputEvent {
  return { type: 'job.output', job_id: 'j1', job_target_id: targetId, data, truncated }
}

/** An event from a coordinator that sends offsets. */
function outputAt(targetId: string, offset: number, data: string, truncated = false): JobOutputEvent {
  return { ...output(targetId, data, truncated), offset }
}

function rejected(id: number): Rejected {
  return { id, at: id * 1000, peer: '10.0.0.9:5000', device_id: null, hostname: null, key_id: null, reason: 'unknown key' }
}

/** A job detail that has been requested and loaded, as the job page does. */
function withLoadedJob(stored: Record<string, string>): State {
  const targets = Object.entries(stored).map(([id, text], i) =>
    target(id, `d${i + 1}`, { state: 'running', output: text }),
  )
  return run(
    initialState,
    { type: 'job.detail.requested', jobId: 'j1' },
    { type: 'job.detail.loaded', job: job('j1', targets) },
  )
}

describe('job output', () => {
  it('appends job.output events in order, reproducing the stored output', () => {
    const chunks = ['line 1\r\n', 'line ', '2\r\n', '', 'ünïcödé ✓\n', 'last']
    let state = withLoadedJob({ t1: 'already stored\n' })
    for (const chunk of chunks) state = applyEvent(state, output('t1', chunk))

    // What the coordinator has stored after appending the same chunks.
    const storedOnServer = 'already stored\n' + chunks.join('')
    expect(state.outputs.t1).toEqual({ text: storedOnServer, truncated: false })
  })

  it('keeps the outputs of different targets apart', () => {
    let state = withLoadedJob({ t1: '', t2: '' })
    state = applyEvent(state, output('t1', 'a1 '))
    state = applyEvent(state, output('t2', 'b1 '))
    state = applyEvent(state, output('t1', 'a2'))
    expect(state.outputs.t1?.text).toBe('a1 a2')
    expect(state.outputs.t2?.text).toBe('b1 ')
  })

  it('records truncation, including the marker carried in data', () => {
    let state = withLoadedJob({ t1: 'x' })
    state = applyEvent(state, output('t1', '\n[output truncated]\n', true))
    expect(state.outputs.t1).toEqual({ text: 'x\n[output truncated]\n', truncated: true })
  })

  it('takes output_truncated from the loaded job', () => {
    const state = run(
      initialState,
      { type: 'job.detail.requested', jobId: 'j1' },
      { type: 'job.detail.loaded', job: job('j1', [target('t1', 'd1', { output: 'x', output_truncated: true })]) },
    )
    expect(state.outputs.t1?.truncated).toBe(true)
  })

  it('does not collect output for jobs that are not being viewed', () => {
    const state = applyEvent(initialState, output('t1', 'ignored'))
    expect(state).toBe(initialState)
  })

  it('does not keep output in the job list entries', () => {
    const state = withLoadedJob({ t1: 'text' })
    expect(state.jobs.j1?.targets[0]).not.toHaveProperty('output')
  })

  it('applies events that arrive while the job is being loaded, without duplicating', () => {
    // The response was produced after "one " was stored but before "two".
    const state = run(
      initialState,
      { type: 'job.detail.requested', jobId: 'j1' },
      { type: 'event', event: output('t1', 'one ') },
      { type: 'event', event: output('t1', 'two') },
      { type: 'job.detail.loaded', job: job('j1', [target('t1', 'd1', { output: 'zero one ' })]) },
    )
    expect(state.outputs.t1?.text).toBe('zero one two')
    expect(state.pendingOutput).toEqual({})
  })

  it('continues appending after a reload replaced the output', () => {
    let state = withLoadedJob({ t1: 'a' })
    state = applyEvent(state, output('t1', 'b'))
    state = run(
      state,
      { type: 'job.detail.requested', jobId: 'j1' },
      { type: 'event', event: output('t1', 'c') },
      { type: 'job.detail.loaded', job: job('j1', [target('t1', 'd1', { output: 'abc' })]) },
      { type: 'event', event: output('t1', 'd') },
    )
    expect(state.outputs.t1?.text).toBe('abcd')
  })

  it('drops the output when the job view is left', () => {
    let state = withLoadedJob({ t1: 'text' })
    state = reducer(state, { type: 'job.detail.released', jobId: 'j1' })
    expect(state.outputs).toEqual({})
    expect(applyEvent(state, output('t1', 'more')).outputs).toEqual({})
  })
})

describe('job output offsets', () => {
  it('counts code points, not UTF-16 code units', () => {
    expect(codePointLength('')).toBe(0)
    expect(codePointLength('abc')).toBe(3)
    expect(codePointLength('héllo ✓')).toBe(7)
    // U+1F600 is one code point but two UTF-16 code units.
    expect('😀'.length).toBe(2)
    expect(codePointLength('😀')).toBe(1)
    expect(codePointLength('a😀𝄞b')).toBe(4)
    // A lone surrogate is a unit of its own rather than half of a pair.
    expect(codePointLength('�')).toBe(1)
    expect(codePointLength('�x')).toBe(2)
  })

  it('appends an event that starts exactly at the end of the output held', () => {
    expect(appendOutput('abc', { data: 'd', offset: 3 })).toBe('abcd')
    expect(appendOutput('', { data: 'a', offset: 0 })).toBe('a')
  })

  it('ignores an event that starts before the end of the output held', () => {
    expect(appendOutput('abc', { data: 'c', offset: 2 })).toBeNull()
    expect(appendOutput('abc', { data: 'abc', offset: 0 })).toBeNull()
  })

  it('measures the output held in code points when comparing with the offset', () => {
    const held = '😀😀' // 2 code points, 4 UTF-16 code units
    expect(appendOutput(held, { data: 'x', offset: 2 })).toBe(held + 'x')
    expect(appendOutput(held, { data: '😀', offset: 1 })).toBeNull()
    expect(appendOutput(held, { data: 'y', offset: 2 })).toBe(held + 'y')
  })

  it('applies live events by offset: duplicates are ignored, the rest appended in order', () => {
    // The job was loaded with "héllo " (6 code points, one of them non-ASCII).
    let state = withLoadedJob({ t1: 'héllo ' })
    state = applyEvent(state, outputAt('t1', 3, 'lo ')) // already in the loaded output
    state = applyEvent(state, outputAt('t1', 6, '😀')) // new
    state = applyEvent(state, outputAt('t1', 7, 'w')) // new, after an astral character
    state = applyEvent(state, outputAt('t1', 6, '😀')) // delivered twice
    expect(state.outputs.t1?.text).toBe('héllo 😀w')
  })

  it('does not take truncation from an ignored event', () => {
    let state = withLoadedJob({ t1: 'abcdef' })
    state = applyEvent(state, outputAt('t1', 2, 'cdef', true))
    expect(state.outputs.t1).toEqual({ text: 'abcdef', truncated: false })
  })

  it('places events that crossed the load by offset, even when the content repeats', () => {
    // Stored output at the time of the response: "a". The event at offset 1 is
    // new and has the same content; matching by content would drop it.
    const state = run(
      initialState,
      { type: 'job.detail.requested', jobId: 'j1' },
      { type: 'event', event: outputAt('t1', 1, 'a') },
      { type: 'job.detail.loaded', job: job('j1', [target('t1', 'd1', { output: 'a' })]) },
    )
    expect(state.outputs.t1?.text).toBe('aa')
  })

  it('drops buffered events the response already contains, counting astral characters', () => {
    const stored = '😀😀b' // 3 code points
    const state = run(
      initialState,
      { type: 'job.detail.requested', jobId: 'j1' },
      { type: 'event', event: outputAt('t1', 1, '😀b') }, // included
      { type: 'event', event: outputAt('t1', 3, 'c') }, // new
      { type: 'event', event: outputAt('t1', 4, 'd') }, // new
      { type: 'job.detail.loaded', job: job('j1', [target('t1', 'd1', { output: stored })]) },
      { type: 'event', event: outputAt('t1', 5, 'e') },
    )
    expect(state.outputs.t1?.text).toBe(stored + 'cde')
  })

  it('merges buffered events by offset only when all of them have one', () => {
    expect(mergeBufferedEvents('abc', [{ data: 'c', offset: 2 }, { data: 'd', offset: 3 }])).toBe('abcd')
    // Without offsets the old rule applies: leading chunks the output ends with are included.
    expect(mergeBufferedEvents('abc', [{ data: 'c' }, { data: 'd' }])).toBe('abcd')
    expect(mergeBufferedEvents('a', [{ data: 'a' }])).toBe('a')
  })

  it('still appends events without offsets (a coordinator older than milestone 3)', () => {
    let state = withLoadedJob({ t1: 'x' })
    state = applyEvent(state, output('t1', 'y'))
    state = applyEvent(state, output('t1', 'y'))
    expect(state.outputs.t1?.text).toBe('xyy')
  })
})

describe('mergeBuffered', () => {
  it('appends chunks the stored output does not contain', () => {
    expect(mergeBuffered('abc', ['d', 'e'])).toBe('abcde')
  })
  it('skips leading chunks the stored output already ends with', () => {
    expect(mergeBuffered('abcd', ['c', 'd', 'e'])).toBe('abcde')
    expect(mergeBuffered('abcde', ['c', 'd', 'e'])).toBe('abcde')
  })
  it('handles no chunks and empty output', () => {
    expect(mergeBuffered('abc', [])).toBe('abc')
    expect(mergeBuffered('', ['a'])).toBe('a')
  })
})

describe('events', () => {
  it('ignores unknown event types and things that are not events', () => {
    const state = reducer(initialState, { type: 'devices.loaded', devices: [device('a')] })
    expect(applyEvent(state, { type: 'hotspot.changed', enabled: true })).toBe(state)
    expect(applyEvent(state, { type: 42 })).toBe(state)
    expect(applyEvent(state, null)).toBe(state)
    expect(applyEvent(state, 'resync')).toBe(state)
    expect(applyEvent(state, [])).toBe(state)
  })

  it('survives a malformed event of a known type', () => {
    expect(applyEvent(initialState, { type: 'device.updated' })).toBe(initialState)
    expect(applyEvent(initialState, { type: 'job.created', job: { id: 'j' } })).toBe(initialState)
  })

  it('asks for a refetch on resync', () => {
    const state = applyEvent(initialState, { type: 'resync' })
    expect(state.syncEpoch).toBe(initialState.syncEpoch + 1)
  })

  it('asks for a refetch whenever the socket opens, not when it drops', () => {
    let state = reducer(initialState, { type: 'connection', status: 'open' })
    expect(state.syncEpoch).toBe(1)
    state = reducer(state, { type: 'connection', status: 'reconnecting' })
    expect(state.syncEpoch).toBe(1)
    expect(state.connection).toBe('reconnecting')
    state = reducer(state, { type: 'connection', status: 'open' })
    expect(state.syncEpoch).toBe(2)
  })

  it('adds, updates and removes devices', () => {
    let state = reducer(initialState, { type: 'devices.loaded', devices: [device('a', { online: false })] })
    state = applyEvent(state, { type: 'device.updated', device: device('a', { online: true }) })
    state = applyEvent(state, { type: 'device.updated', device: device('b') })
    expect(state.devices.a?.online).toBe(true)
    expect(Object.keys(state.devices)).toEqual(['a', 'b'])

    state = applyEvent(state, { type: 'device.removed', device_id: 'a' })
    expect(Object.keys(state.devices)).toEqual(['b'])
  })

  it('removes a forgotten device from the jobs it was part of', () => {
    let state = applyEvent(initialState, {
      type: 'job.created',
      job: job('j1', [target('t1', 'a'), target('t2', 'b')]),
    })
    state = applyEvent(state, { type: 'device.removed', device_id: 'a' })
    expect(state.jobs.j1?.targets.map((t) => t.id)).toEqual(['t2'])
  })

  it('removes a job that is left without any device', () => {
    let state = applyEvent(initialState, {
      type: 'job.created',
      job: job('only-a', [target('t1', 'a')]),
    })
    state = applyEvent(state, { type: 'job.created', job: job('both', [target('t2', 'a'), target('t3', 'b')]) })
    state = applyEvent(state, { type: 'device.removed', device_id: 'a' })
    expect(Object.keys(state.jobs)).toEqual(['both'])
    expect(state.jobs.both?.targets.map((t) => t.id)).toEqual(['t3'])
  })

  it('adds created jobs and updates their targets', () => {
    let state = applyEvent(initialState, { type: 'job.created', job: job('j1', [target('t1', 'a'), target('t2', 'b')]) })
    state = applyEvent(state, {
      type: 'job.target.updated',
      job_id: 'j1',
      target: target('t2', 'b', { state: 'failed', exit_code: 3, reason: 'nonzero_exit' }),
    })
    expect(state.jobs.j1?.targets.map((t) => t.state)).toEqual(['queued', 'failed'])
    expect(state.jobs.j1?.targets[1]?.exit_code).toBe(3)
  })

  it('ignores target updates for jobs it has not loaded', () => {
    const state = applyEvent(initialState, { type: 'job.target.updated', job_id: 'old', target: target('t', 'a') })
    expect(state).toBe(initialState)
  })

  it('keeps the recent jobs of a viewed device up to date', () => {
    let state = reducer(initialState, { type: 'device.detail.loaded', device: device('a'), targets: [] })
    state = applyEvent(state, { type: 'job.created', job: job('j1', [target('t1', 'a'), target('t2', 'b')]) })
    expect(state.deviceTargets.a?.map((t) => [t.id, t.state, t.job.id])).toEqual([['t1', 'queued', 'j1']])

    state = applyEvent(state, {
      type: 'job.target.updated',
      job_id: 'j1',
      target: target('t1', 'a', { state: 'succeeded', exit_code: 0 }),
    })
    expect(state.deviceTargets.a?.map((t) => [t.id, t.state])).toEqual([['t1', 'succeeded']])
    expect(state.deviceTargets.b).toBeUndefined()
  })

  it('does not let a stale response move a target backwards', () => {
    let state = applyEvent(initialState, { type: 'job.created', job: job('j1', [target('t1', 'a')]) })
    state = applyEvent(state, {
      type: 'job.target.updated',
      job_id: 'j1',
      target: target('t1', 'a', { state: 'succeeded', exit_code: 0 }),
    })
    state = reducer(state, {
      type: 'jobs.loaded',
      jobs: [job('j1', [target('t1', 'a', { state: 'running' })])],
      pageSize: 50,
      append: false,
    })
    expect(state.jobs.j1?.targets[0]?.state).toBe('succeeded')
  })
})

describe('events that cross a fetch', () => {
  it('are applied on top of a response that predates them', () => {
    const state = run(
      initialState,
      { type: 'base.requested' },
      { type: 'event', event: { type: 'device.updated', device: device('b') } },
      { type: 'event', event: { type: 'device.removed', device_id: 'c' } },
      { type: 'event', event: { type: 'connection.rejected', rejected: rejected(5) } },
      { type: 'base.loaded', devices: [device('a'), device('c')], keys: [], rejected: [rejected(4)] },
    )
    expect(Object.keys(state.devices).sort()).toEqual(['a', 'b'])
    expect(state.rejected.map((r) => r.id)).toEqual([5, 4])
    expect(state.rejectedUnseen).toBe(1)
    expect(state.baseBuffer).toBeNull()
  })

  it('fills in the recent jobs of a device from jobs already known', () => {
    let state = applyEvent(initialState, {
      type: 'job.created',
      job: job('j2', [target('t2', 'a', { state: 'running' })], { created_at: 900 }),
    })
    state = applyEvent(state, {
      type: 'job.created',
      job: job('j1', [target('t1', 'a', { state: 'succeeded' })], { created_at: 100 }),
    })
    state = reducer(state, {
      type: 'device.detail.loaded',
      device: device('a'),
      // Produced before j2 existed and before t1 finished.
      targets: [{ ...target('t1', 'a', { state: 'running' }), job: { id: 'j1', kind: 'script.run', created_at: 100 } }],
    })
    expect(state.deviceTargets.a?.map((t) => [t.id, t.state])).toEqual([
      ['t2', 'running'],
      ['t1', 'succeeded'],
    ])
    expect(state.devices.a).toEqual(device('a'))
  })
})

describe('rejected connections', () => {
  it('puts new ones first and counts them until they are seen', () => {
    let state = reducer(initialState, { type: 'rejected.loaded', rejected: [rejected(2), rejected(1)] })
    expect(state.rejectedUnseen).toBe(0)

    state = applyEvent(state, { type: 'connection.rejected', rejected: rejected(3) })
    state = applyEvent(state, { type: 'connection.rejected', rejected: rejected(3) })
    expect(state.rejected.map((r) => r.id)).toEqual([3, 2, 1])
    expect(state.rejectedUnseen).toBe(1)

    state = reducer(state, { type: 'rejected.seen' })
    expect(state.rejectedUnseen).toBe(0)
  })

  it('counts the ones found by a refetch after missed events', () => {
    let state = reducer(initialState, { type: 'rejected.loaded', rejected: [rejected(1)] })
    state = reducer(state, { type: 'rejected.loaded', rejected: [rejected(3), rejected(2), rejected(1)] })
    expect(state.rejectedUnseen).toBe(2)
  })
})

describe('job list paging', () => {
  it('shows the loaded pages newest first, and newly created jobs on top', () => {
    let state = reducer(initialState, {
      type: 'jobs.loaded',
      jobs: [job('j3', [], { created_at: 300 }), job('j2', [], { created_at: 200 })],
      pageSize: 2,
      append: false,
    })
    expect(state.jobList).toEqual({ loaded: true, cursor: 200, hasMore: true })

    state = applyEvent(state, { type: 'job.created', job: job('j4', [], { created_at: 400 }) })
    expect(listedJobs(state).map((j) => j.id)).toEqual(['j4', 'j3', 'j2'])

    state = reducer(state, { type: 'jobs.loaded', jobs: [job('j1', [], { created_at: 100 })], pageSize: 2, append: true })
    expect(state.jobList).toEqual({ loaded: true, cursor: 100, hasMore: false })
    expect(listedJobs(state).map((j) => j.id)).toEqual(['j4', 'j3', 'j2', 'j1'])
  })

  it('does not list an old job that was only opened directly while pages are missing', () => {
    let state = reducer(initialState, {
      type: 'jobs.loaded',
      jobs: [job('j3', [], { created_at: 300 }), job('j2', [], { created_at: 200 })],
      pageSize: 2,
      append: false,
    })
    state = reducer(state, { type: 'job.detail.loaded', job: job('j0', [], { created_at: 1 }) })
    expect(listedJobs(state).map((j) => j.id)).toEqual(['j3', 'j2'])
  })
})
