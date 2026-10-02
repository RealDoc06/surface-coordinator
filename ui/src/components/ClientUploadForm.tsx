import { useId, useState, type FormEvent } from 'react'
import { api, errorMessage } from '../api/client'
import type { Updates } from '../api/types'
import { ErrorNote } from './bits'

const FILE_NAME = /^surface-coordinator-client-(\d+\.\d+\.\d+)-windows-x86_64\.exe$/i
const VERSION = /^\d+\.\d+\.\d+$/

/** The version a release file name carries, or null for any other name. */
export function versionFromFileName(name: string): string | null {
  return FILE_NAME.exec(name)?.[1] ?? null
}

type Props = {
  /** Called with the refreshed page data after a successful upload. */
  onUploaded: (updates: Updates) => void
}

/**
 * Uploads a client build the update key has signed. The executable goes to the
 * coordinator as the raw request body; the signature is base64 text, typed or
 * taken from the `.sig` file next to the build.
 */
export function ClientUploadForm({ onUploaded }: Props) {
  const ids = useId()
  const [file, setFile] = useState<File | null>(null)
  const [signature, setSignature] = useState('')
  const [version, setVersion] = useState('')
  const [notes, setNotes] = useState('')
  const [uploading, setUploading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [done, setDone] = useState<string | null>(null)
  // Changing the key empties the file inputs, which cannot be set from React.
  const [formKey, setFormKey] = useState(0)

  const versionValid = VERSION.test(version)
  const ready = file !== null && signature.trim() !== '' && versionValid && !uploading

  function chooseFile(chosen: File | null) {
    setFile(chosen)
    setDone(null)
    const found = chosen ? versionFromFileName(chosen.name) : null
    if (found) setVersion(found)
  }

  async function chooseSignature(chosen: File | undefined) {
    if (!chosen) return
    try {
      setSignature(await chosen.text())
      setError(null)
    } catch {
      setError('The signature file could not be read.')
    }
  }

  async function submit(event: FormEvent) {
    event.preventDefault()
    if (!ready || !file) return
    setUploading(true)
    setError(null)
    setDone(null)
    try {
      // Base64 has no whitespace; a pasted or file-borne signature often ends in a newline.
      const updates = await api.uploadClient({
        file,
        version,
        signature: signature.replace(/\s+/g, ''),
        notes: notes.trim() || undefined,
      })
      onUploaded(updates)
      setDone(`Uploaded ${version}.`)
      setFile(null)
      setSignature('')
      setVersion('')
      setNotes('')
      setFormKey((k) => k + 1)
    } catch (err) {
      setError(errorMessage(err))
    } finally {
      setUploading(false)
    }
  }

  return (
    <form key={formKey} onSubmit={(e) => void submit(e)} aria-busy={uploading}>
      <div className="field">
        <label htmlFor={`${ids}-file`}>Client executable</label>
        <input
          id={`${ids}-file`}
          type="file"
          accept=".exe"
          disabled={uploading}
          onChange={(e) => chooseFile(e.target.files?.[0] ?? null)}
        />
      </div>
      <div className="field">
        <label htmlFor={`${ids}-sig-file`}>Signature file (.sig)</label>
        <input
          id={`${ids}-sig-file`}
          type="file"
          accept=".sig"
          disabled={uploading}
          onChange={(e) => void chooseSignature(e.target.files?.[0])}
        />
      </div>
      <div className="field">
        <label htmlFor={`${ids}-sig`}>Signature (base64)</label>
        <textarea
          id={`${ids}-sig`}
          className="mono"
          rows={2}
          spellCheck={false}
          value={signature}
          disabled={uploading}
          onChange={(e) => setSignature(e.target.value)}
        />
        <p className="hint">Choose the .sig file above, or paste its contents.</p>
      </div>
      <div className="field">
        <label htmlFor={`${ids}-version`}>Version</label>
        <input
          id={`${ids}-version`}
          type="text"
          className="input-narrow"
          value={version}
          placeholder="1.2.3"
          spellCheck={false}
          autoComplete="off"
          disabled={uploading}
          aria-invalid={version !== '' && !versionValid}
          onChange={(e) => setVersion(e.target.value.trim())}
        />
        {version !== '' && !versionValid ? <p className="hint">Three numbers separated by dots, like 1.2.3.</p> : null}
      </div>
      <div className="field">
        <label htmlFor={`${ids}-notes`}>Notes (optional)</label>
        <textarea
          id={`${ids}-notes`}
          rows={2}
          value={notes}
          disabled={uploading}
          onChange={(e) => setNotes(e.target.value)}
        />
      </div>
      <p className="hint">
        Platform: <span className="mono">windows-x86_64</span>
      </p>
      <ErrorNote message={error} />
      {done ? (
        <p className="success" role="status">
          {done}
        </p>
      ) : null}
      <div className="form-actions">
        <button type="submit" className="primary" disabled={!ready}>
          {uploading ? 'Uploading…' : 'Upload'}
        </button>
      </div>
    </form>
  )
}
