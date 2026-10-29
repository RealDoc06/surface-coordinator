import '@testing-library/jest-dom/vitest'
import { cleanup } from '@testing-library/react'
import { afterEach, beforeEach, vi } from 'vitest'

// jsdom has no viewport scrolling; navigation tests assert this call directly.
beforeEach(() => vi.spyOn(window, 'scrollTo').mockImplementation(() => {}))

afterEach(() => {
  cleanup()
})
