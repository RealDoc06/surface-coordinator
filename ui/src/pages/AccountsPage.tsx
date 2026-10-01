import { useCallback, useEffect, useState, type FormEvent } from 'react'
import { api, errorMessage } from '../api/client'
import type { Account } from '../api/types'
import { ErrorNote } from '../components/bits'
import { ConfirmDialog, Modal } from '../components/Modal'
import { formatDateTime } from '../lib/format'
import { useSession } from '../session/SessionProvider'
import { useStore } from '../state/StoreProvider'

const USERNAME_PATTERN = /^[A-Za-z0-9._-]{1,64}$/
const MIN_PASSWORD_LENGTH = 10

function checkPassword(password: string): string | null {
  return password.length < MIN_PASSWORD_LENGTH ? `Passwords need at least ${MIN_PASSWORD_LENGTH} characters.` : null
}

export function AccountsPage() {
  const session = useSession()
  const { state } = useStore()
  const [accounts, setAccounts] = useState<Account[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [creating, setCreating] = useState(false)
  const [createError, setCreateError] = useState<string | null>(null)

  const [changing, setChanging] = useState<Account | null>(null)
  const [newPassword, setNewPassword] = useState('')
  const [changeBusy, setChangeBusy] = useState(false)
  const [changeError, setChangeError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)

  const [deleting, setDeleting] = useState<Account | null>(null)
  const [deleteBusy, setDeleteBusy] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  const available = session.capabilities.accounts

  const load = useCallback(async () => {
    try {
      const { accounts: list } = await api.listAccounts()
      setAccounts(list)
      setError(null)
    } catch (err) {
      setError(errorMessage(err))
    }
  }, [])

  useEffect(() => {
    if (available) void load()
  }, [available, load, state.syncEpoch])

  if (!available) {
    return (
      <div className="empty">
        <h2>Accounts are not used here</h2>
        <p>This coordinator runs inside the manager and has no accounts.</p>
      </div>
    )
  }

  async function onCreate(event: FormEvent) {
    event.preventDefault()
    if (creating) return
    if (!USERNAME_PATTERN.test(username)) {
      setCreateError('Usernames are 1–64 characters: letters, digits, dot, underscore and hyphen.')
      return
    }
    const weak = checkPassword(password)
    if (weak) {
      setCreateError(weak)
      return
    }
    setCreating(true)
    setCreateError(null)
    setNotice(null)
    try {
      await api.createAccount(username, password)
      setNotice(`Account ${username} created.`)
      setUsername('')
      setPassword('')
      await load()
    } catch (err) {
      setCreateError(errorMessage(err))
    } finally {
      setCreating(false)
    }
  }

  async function onChangePassword(event: FormEvent) {
    event.preventDefault()
    if (!changing || changeBusy) return
    const weak = checkPassword(newPassword)
    if (weak) {
      setChangeError(weak)
      return
    }
    setChangeBusy(true)
    setChangeError(null)
    try {
      await api.setAccountPassword(changing.id, newPassword)
      setNotice(`Password of ${changing.username} changed.`)
      closeChange()
    } catch (err) {
      setChangeError(errorMessage(err))
    } finally {
      setChangeBusy(false)
    }
  }

  function closeChange() {
    setChanging(null)
    setNewPassword('')
    setChangeError(null)
  }

  async function onDelete() {
    if (!deleting) return
    setDeleteBusy(true)
    setDeleteError(null)
    try {
      await api.deleteAccount(deleting.id)
      setNotice(`Account ${deleting.username} deleted.`)
      setDeleting(null)
      await load()
    } catch (err) {
      setDeleteError(errorMessage(err))
    } finally {
      setDeleteBusy(false)
    }
  }

  return (
    <>
      <div className="page-head">
        <h1>Accounts</h1>
      </div>
      <p className="lead">Every account has full access to this coordinator.</p>
      <ErrorNote message={error} onRetry={() => void load()} />
      {notice ? (
        <p className="success" role="status">
          {notice}
        </p>
      ) : null}

      <section className="panel">
        <form className="inline-form" onSubmit={(e) => void onCreate(e)} noValidate>
          <div className="field">
            <label htmlFor="account-username">New username</label>
            <input
              id="account-username"
              type="text"
              autoComplete="off"
              autoCapitalize="off"
              spellCheck={false}
              maxLength={64}
              value={username}
              onChange={(e) => setUsername(e.target.value)}
            />
          </div>
          <div className="field">
            <label htmlFor="account-password">Password (at least {MIN_PASSWORD_LENGTH} characters)</label>
            <input
              id="account-password"
              type="password"
              autoComplete="new-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          </div>
          <button type="submit" className="primary" disabled={creating}>
            Create account
          </button>
        </form>
        <ErrorNote message={createError} />
      </section>

      {accounts === null ? (
        error ? null : (
          <p className="muted">Loading accounts&hellip;</p>
        )
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th>Username</th>
              <th>Created</th>
              <th>
                <span className="visually-hidden">Actions</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {accounts.map((account) => {
              const self = account.id === session.account?.id
              return (
                <tr key={account.id}>
                  <td>
                    {account.username}
                    {self ? <span className="muted"> (you)</span> : null}
                  </td>
                  <td className="nowrap">{formatDateTime(account.created_at)}</td>
                  <td className="row-actions">
                    <button
                      type="button"
                      onClick={() => setChanging(account)}
                      aria-label={`Change password of ${account.username}`}
                    >
                      Change password
                    </button>
                    <button
                      type="button"
                      className="danger"
                      disabled={self}
                      title={self ? 'You cannot delete the account you are signed in with.' : undefined}
                      onClick={() => setDeleting(account)}
                      aria-label={`Delete ${account.username}`}
                    >
                      Delete
                    </button>
                  </td>
                </tr>
              )
            })}
          </tbody>
        </table>
      )}

      {changing ? (
        <Modal title={`Change password of ${changing.username}`} onClose={closeChange}>
          <form onSubmit={(e) => void onChangePassword(e)} noValidate>
            <div className="field">
              <label htmlFor="change-password">New password (at least {MIN_PASSWORD_LENGTH} characters)</label>
              <input
                id="change-password"
                type="password"
                autoComplete="new-password"
                data-autofocus
                value={newPassword}
                onChange={(e) => setNewPassword(e.target.value)}
              />
            </div>
            <ErrorNote message={changeError} />
            <div className="modal-actions">
              <button type="button" onClick={closeChange}>
                Cancel
              </button>
              <button type="submit" className="primary" disabled={changeBusy}>
                Change password
              </button>
            </div>
          </form>
        </Modal>
      ) : null}

      {deleting ? (
        <ConfirmDialog
          title={`Delete account ${deleting.username}?`}
          confirmLabel="Delete account"
          danger
          busy={deleteBusy}
          error={deleteError}
          onConfirm={() => void onDelete()}
          onCancel={() => {
            setDeleting(null)
            setDeleteError(null)
          }}
        >
          <p>{deleting.username} will no longer be able to sign in.</p>
        </ConfirmDialog>
      ) : null}
    </>
  )
}
