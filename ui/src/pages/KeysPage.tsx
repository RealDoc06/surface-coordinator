import { useState, type FormEvent } from 'react'
import { api, errorMessage } from '../api/client'
import type { Key } from '../api/types'
import { CopyField, ErrorNote } from '../components/bits'
import { ConfirmDialog } from '../components/Modal'
import { formatDateTime, plural } from '../lib/format'
import { useStore } from '../state/StoreProvider'

/** A secret being shown: just created, imported, or revealed on request. */
type Shown = { id: string; label: string; secret: string; origin: 'created' | 'imported' | 'revealed' }

export function KeysPage() {
  const { state, reloadKeys } = useStore()
  const [shown, setShown] = useState<Shown | null>(null)
  const [error, setError] = useState<string | null>(null)

  const [label, setLabel] = useState('')
  const [creating, setCreating] = useState(false)
  const [createError, setCreateError] = useState<string | null>(null)

  const [importing, setImporting] = useState(false)
  const [importOpen, setImportOpen] = useState(false)
  const [importForm, setImportForm] = useState({ label: '', id: '', secret: '' })
  const [importError, setImportError] = useState<string | null>(null)

  const [revoking, setRevoking] = useState<Key | null>(null)
  const [revokeBusy, setRevokeBusy] = useState(false)
  const [revokeError, setRevokeError] = useState<string | null>(null)

  async function refresh() {
    try {
      await reloadKeys()
      setError(null)
    } catch (err) {
      setError(errorMessage(err))
    }
  }

  async function onCreate(event: FormEvent) {
    event.preventDefault()
    if (creating) return
    if (label.trim() === '') {
      setCreateError('Give the key a label, for example the site or classroom it is for.')
      return
    }
    setCreating(true)
    setCreateError(null)
    try {
      const { key } = await api.createKey(label.trim())
      setShown({ id: key.id, label: key.label, secret: key.secret, origin: 'created' })
      setLabel('')
      await refresh()
    } catch (err) {
      setCreateError(errorMessage(err))
    } finally {
      setCreating(false)
    }
  }

  async function onImport(event: FormEvent) {
    event.preventDefault()
    if (importing) return
    const key = { label: importForm.label.trim(), id: importForm.id.trim(), secret: importForm.secret.trim() }
    if (key.label === '' || key.id === '' || key.secret === '') {
      setImportError('Enter the label, the key id and the key secret.')
      return
    }
    setImporting(true)
    setImportError(null)
    try {
      await api.importKey(key)
      setImportForm({ label: '', id: '', secret: '' })
      setImportOpen(false)
      await refresh()
    } catch (err) {
      setImportError(errorMessage(err))
    } finally {
      setImporting(false)
    }
  }

  async function reveal(key: Key) {
    setError(null)
    try {
      const { id, secret } = await api.exportKeySecret(key.id)
      setShown({ id, label: key.label, secret, origin: 'revealed' })
    } catch (err) {
      setError(errorMessage(err))
    }
  }

  async function revoke() {
    if (!revoking) return
    setRevokeBusy(true)
    setRevokeError(null)
    try {
      await api.revokeKey(revoking.id)
      setRevoking(null)
      await refresh()
    } catch (err) {
      setRevokeError(errorMessage(err))
    } finally {
      setRevokeBusy(false)
    }
  }

  const keys = state.keys

  return (
    <>
      <div className="page-head">
        <h1>Enrollment keys</h1>
      </div>
      <p className="lead">
        A client proves it belongs to this coordinator with an enrollment key: an id and a secret, given to the client
        when it is installed. To use the same devices with another coordinator, export the key here and import it there.
      </p>
      <ErrorNote message={error} />

      {shown ? (
        <section className="panel secret-panel" aria-label="Key secret">
          <h2>
            {shown.origin === 'created' ? 'Key created' : 'Key secret'}: {shown.label}
          </h2>
          <p className="notice" role="note">
            {shown.origin === 'created'
              ? 'Copy both values now. The client needs the key id and the secret; the secret is not shown again unless you reveal it.'
              : 'The client needs both the key id and the secret. Anyone with both can enroll devices.'}
          </p>
          <CopyField label="Key id" value={shown.id} />
          <CopyField label="Key secret" value={shown.secret} />
          <div className="form-actions">
            <button type="button" onClick={() => setShown(null)}>
              Hide
            </button>
          </div>
        </section>
      ) : null}

      <section className="panel">
        <form className="inline-form" onSubmit={(e) => void onCreate(e)} noValidate>
          <div className="field">
            <label htmlFor="key-label">New key label</label>
            <input id="key-label" type="text" value={label} maxLength={100} onChange={(e) => setLabel(e.target.value)} />
          </div>
          <button type="submit" className="primary" disabled={creating}>
            Create key
          </button>
          <button type="button" onClick={() => setImportOpen((open) => !open)} aria-expanded={importOpen}>
            Import a key&hellip;
          </button>
        </form>
        <ErrorNote message={createError} />

        {importOpen ? (
          <form className="import-form" onSubmit={(e) => void onImport(e)} noValidate>
            <h2>Import a key</h2>
            <p className="hint">Use the id and secret exported from the other coordinator.</p>
            <div className="form-row">
              <div className="field">
                <label htmlFor="import-label">Label</label>
                <input
                  id="import-label"
                  type="text"
                  value={importForm.label}
                  onChange={(e) => setImportForm({ ...importForm, label: e.target.value })}
                />
              </div>
              <div className="field grow">
                <label htmlFor="import-id">Key id</label>
                <input
                  id="import-id"
                  type="text"
                  className="mono"
                  spellCheck={false}
                  autoComplete="off"
                  value={importForm.id}
                  onChange={(e) => setImportForm({ ...importForm, id: e.target.value })}
                />
              </div>
              <div className="field grow">
                <label htmlFor="import-secret">Key secret</label>
                <input
                  id="import-secret"
                  type="password"
                  className="mono"
                  spellCheck={false}
                  autoComplete="off"
                  value={importForm.secret}
                  onChange={(e) => setImportForm({ ...importForm, secret: e.target.value })}
                />
              </div>
            </div>
            <ErrorNote message={importError} />
            <div className="form-actions">
              <button type="submit" className="primary" disabled={importing}>
                Import key
              </button>
              <button type="button" onClick={() => setImportOpen(false)}>
                Cancel
              </button>
            </div>
          </form>
        ) : null}
      </section>

      {keys === null ? (
        <p className="muted">Loading keys&hellip;</p>
      ) : keys.length === 0 ? (
        <p className="muted">No enrollment keys yet. Create one to enroll devices.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Label</th>
              <th>Key id</th>
              <th>Created</th>
              <th>Status</th>
              <th>Devices</th>
              <th>
                <span className="visually-hidden">Actions</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {keys.map((key) => (
              <tr key={key.id} className={key.revoked_at !== null ? 'dimmed' : undefined}>
                <td>{key.label}</td>
                <td className="mono">{key.id}</td>
                <td className="nowrap">{formatDateTime(key.created_at)}</td>
                <td className="nowrap">
                  {key.revoked_at !== null ? (
                    <span className="badge state-failed" title={formatDateTime(key.revoked_at)}>
                      revoked
                    </span>
                  ) : (
                    <span className="badge state-succeeded">active</span>
                  )}
                </td>
                <td>{plural(key.device_count, 'device')}</td>
                <td className="row-actions">
                  <button type="button" onClick={() => void reveal(key)} aria-label={`Reveal secret of ${key.label}`}>
                    Reveal secret
                  </button>
                  {key.revoked_at === null ? (
                    <button
                      type="button"
                      className="danger"
                      onClick={() => setRevoking(key)}
                      aria-label={`Revoke ${key.label}`}
                    >
                      Revoke
                    </button>
                  ) : null}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {revoking ? (
        <ConfirmDialog
          title={`Revoke “${revoking.label}”?`}
          confirmLabel="Revoke key"
          danger
          busy={revokeBusy}
          error={revokeError}
          onConfirm={() => void revoke()}
          onCancel={() => {
            setRevoking(null)
            setRevokeError(null)
          }}
        >
          <p>
            {revoking.device_count > 0
              ? `${plural(revoking.device_count, 'device')} enrolled with this key will no longer be able to connect.`
              : 'Clients using this key will no longer be able to connect.'}{' '}
            This cannot be undone.
          </p>
        </ConfirmDialog>
      ) : null}
    </>
  )
}
