import { describe, expect, it } from 'vitest'
import { MAX_ARGUMENT_CHARS, MAX_ARGUMENTS, MAX_PATH_CHARS, validateLaunch } from './launch'

describe('validateLaunch', () => {
  it('requires a path', () => {
    const { payload, errors } = validateLaunch({ path: '   ', arguments: [] })
    expect(payload).toBeNull()
    expect(errors.path).toMatch(/Enter the path/)
  })

  it('builds the payload, dropping blank argument rows and omitting an empty list', () => {
    expect(validateLaunch({ path: ' C:\\a.exe ', arguments: [] }).payload).toEqual({ path: 'C:\\a.exe' })
    expect(validateLaunch({ path: 'x', arguments: ['-a', '', '  ', 'b c'] }).payload).toEqual({
      path: 'x',
      arguments: ['-a', 'b c'],
    })
  })

  it('limits the path to 1024 characters', () => {
    expect(validateLaunch({ path: 'a'.repeat(MAX_PATH_CHARS), arguments: [] }).payload).not.toBeNull()
    expect(validateLaunch({ path: 'a'.repeat(MAX_PATH_CHARS + 1), arguments: [] }).errors.path).toMatch(/at most 1024/)
  })

  it('limits each argument to 1024 characters and says which row', () => {
    const { payload, errors } = validateLaunch({ path: 'x', arguments: ['ok', 'b'.repeat(MAX_ARGUMENT_CHARS + 1)] })
    expect(payload).toBeNull()
    expect(errors.arguments?.[1]).toMatch(/at most 1024/)
    expect(errors.arguments?.[0]).toBeUndefined()
  })

  it('counts characters, not UTF-16 units', () => {
    const emoji = '\u{1F600}'.repeat(MAX_ARGUMENT_CHARS)
    expect(validateLaunch({ path: 'x', arguments: [emoji] }).payload).not.toBeNull()
  })

  it('limits the number of arguments to 64', () => {
    const many = Array.from({ length: MAX_ARGUMENTS + 1 }, (_, i) => `a${i}`)
    expect(validateLaunch({ path: 'x', arguments: many.slice(0, MAX_ARGUMENTS) }).payload).not.toBeNull()
    expect(validateLaunch({ path: 'x', arguments: many }).errors.count).toMatch(/At most 64/)
  })
})
