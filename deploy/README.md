# Running the coordinator on a Linux server

This directory ships in the release tarball
`surface-coordinator-coordinator-<version>-linux-x86_64.tar.gz` together with
the `coordinator` binary:

| File | Purpose |
|---|---|
| `coordinator` | the server (the web UI is built in) |
| `coordinator.service` | systemd unit |
| `coordinator.env.example` | settings for the unit |
| `Caddyfile.example` | TLS reverse proxy |
| `README.md` | this file |

The complete `surface-coordinator-0.2.4.zip` bundle places the native Linux
files in `linux/`. Use that directory for the native installation steps below,
or follow the bundle's `START-HERE.md` for its prebuilt Docker image.

The server speaks plain HTTP and WebSocket and listens on `127.0.0.1:8080`.
Clients (WebSocket) and the web UI and API (HTTP) share that one port. You put
a TLS-terminating reverse proxy in front of it; the steps below use Caddy.

Everything below runs as root (use `sudo -i` or prefix each command with
`sudo`). GitHub release binaries built on Ubuntu 22.04 need glibc 2.35 or newer.
The locally validated 0.2.4 bundle is built on Debian Bookworm and needs glibc
2.36 or newer. Its included Docker image supplies the matching runtime.

## 1. Install

Download the tarball from the
[latest release](https://github.com/RealDoc06/surface-coordinator/releases/latest)
and unpack it:

```
tar -xzf surface-coordinator-coordinator-<version>-linux-x86_64.tar.gz
cd surface-coordinator-coordinator-<version>-linux-x86_64
```

Create the service user and install the files:

```
useradd --system --home-dir /var/lib/surface-coordinator --shell /usr/sbin/nologin surface-coordinator

install -m 0755 coordinator /usr/local/bin/coordinator
install -m 0644 coordinator.service /etc/systemd/system/coordinator.service

install -d -m 0755 /etc/surface-coordinator
install -m 0640 -o root -g surface-coordinator coordinator.env.example /etc/surface-coordinator/coordinator.env
```

(On some distributions the nologin shell is `/sbin/nologin`.)

Edit `/etc/surface-coordinator/coordinator.env`. The defaults suit the Caddy
setup below. The settings, taken from `coordinator serve --help`:

| Variable | Meaning | Default |
|---|---|---|
| `SC_BIND` | address for clients, and for the UI and API unless `SC_UI_BIND` is set | `127.0.0.1:8080` |
| `SC_UI_BIND` | serve the UI and API on a separate address | unset |
| `SC_DATA_DIR` | database directory | `/var/lib/surface-coordinator` (set by the unit) |
| `SC_AUTH` | `accounts` or `token` | `accounts` |
| `SC_API_TOKEN` | the token with `SC_AUTH=token` | random per start, printed to the journal |
| `SC_SECURE_COOKIES` | mark the session cookie Secure; `true` behind HTTPS | `false` in the unit, `true` in the example file |
| `SC_HEARTBEAT_INTERVAL_SECS` | heartbeat interval; offline after three missed intervals | `30` in the binary, `10` in the example file |
| `SC_NO_UPDATE_CHECK` | use uploaded client builds without polling GitHub | `true` in the example file |
| `SC_UI_DIR` | serve the UI from a directory instead of the built-in copy | unset |

`--queued-expiry-hours` and `--verbose` exist
only as command-line flags. To change them, run `systemctl edit coordinator`
and override `ExecStart=` (first an empty `ExecStart=` line, then the full
command, for example
`ExecStart=/usr/local/bin/coordinator serve --verbose`).

Start it:

```
systemctl daemon-reload
systemctl enable --now coordinator
systemctl status coordinator
journalctl -u coordinator -f
```

The first start creates `/var/lib/surface-coordinator` (owned by
`surface-coordinator`, mode 0750) and the database. With `SC_AUTH=accounts` the
journal says that no accounts exist yet; the next step fixes that.

## 2. First account and first enrollment key

The administration commands work on the data directory directly. Run them as
the service user so the database files keep the right owner:

```
sudo -u surface-coordinator /usr/local/bin/coordinator account create \
    --data-dir /var/lib/surface-coordinator <username>
```

It asks for the password twice without echo. To script it, set
`SC_ACCOUNT_PASSWORD` in that command's environment instead.

Create an enrollment key. Devices use it to enroll and it also determines the
hotspot name and passphrase:

```
sudo -u surface-coordinator /usr/local/bin/coordinator key create \
    --data-dir /var/lib/surface-coordinator --label "Classroom 1"
```

It prints `key id` and `key secret`. The secret is shown once; store it
safely. Hand both to the device installer together with the coordinator URL
(`wss://<your hostname>` once Caddy is set up).

Other administration commands:

```
coordinator account list|set-password|delete ...   # same --data-dir option
coordinator key list|revoke ...                    # same --data-dir option
```

## 3. TLS with Caddy

Install Caddy from your distribution or from <https://caddyserver.com/docs/install>.
Then:

1. Point a DNS name at the server and make sure ports 80 and 443 are open.
2. Copy `Caddyfile.example` to `/etc/caddy/Caddyfile` (or import it), and
   replace `coordinator.example.com` with your hostname.
3. `systemctl reload caddy`. Caddy fetches a certificate on its own.
4. Make sure `SC_SECURE_COOKIES=true` is set in the environment file, then
   `systemctl restart coordinator`.

Now the web UI is at `https://<hostname>` and clients connect to
`wss://<hostname>`. Caddy proxies WebSocket connections without extra
configuration. Check with `curl -i https://<hostname>/api/v1/health`.

## 4. Firewall

- Open 80/tcp and 443/tcp (Caddy: certificate issuance and the service).
- Do not open 8080. The coordinator listens on loopback only unless you change
  `SC_BIND`; if you do bind it to another address, restrict who can reach it.
- Without a proxy (a test setup) clients can connect to `ws://<host>:8080`
  directly, which sends all job data and web UI logins unencrypted. Do not do
  that over the internet.
- With GitHub update checks enabled, the client update cache needs outbound
  HTTPS to github.com and its download hosts. Signed client builds uploaded
  from the UI or `scripts/publish-client.ps1` work without that connection.

## 5. Upgrade

The coordinator never updates itself. For each release:

```
systemctl stop coordinator
# back up first, see below
tar -xzf surface-coordinator-coordinator-<new version>-linux-x86_64.tar.gz
install -m 0755 surface-coordinator-coordinator-<new version>-linux-x86_64/coordinator /usr/local/bin/coordinator
systemctl start coordinator
journalctl -u coordinator -n 50
```

The database migrates itself on start. Migrations are one-way: to go back to
an older version, restore the backup you took before upgrading. The tarball
also contains an updated unit, environment example and Caddyfile; compare them
with your installed copies (`diff`) and, if the unit changed, copy it and run
`systemctl daemon-reload`.

Update the devices separately, from the web UI, once the coordinator runs the
new version. A coordinator accepts clients on its own protocol version and the
previous one; clients on any other version can only be sent `client.update`.

## 6. Backup

The database is the SQLite file `coordinator.db` in the data directory. While
the service runs, SQLite also keeps `coordinator.db-wal` and
`coordinator.db-shm` next to it; the three belong together.

- **Simple and safe**: stop the service, copy the whole data directory, start
  it again.

  ```
  systemctl stop coordinator
  tar -czf /root/coordinator-backup-$(date +%F).tar.gz -C /var/lib surface-coordinator
  systemctl start coordinator
  ```

- **Without stopping**, if the `sqlite3` command-line tool is installed:

  ```
  sudo -u surface-coordinator sqlite3 /var/lib/surface-coordinator/coordinator.db \
      ".backup '/var/lib/surface-coordinator/backup.db'"
  ```

  Move `backup.db` off the server afterwards and delete it from the data
  directory. Do not copy just `coordinator.db` while the service is running;
  without the `-wal` file you may lose recent changes.

To restore: stop the service, put the files back into
`/var/lib/surface-coordinator` (delete stale `coordinator.db-wal` and
`coordinator.db-shm` first when restoring a single `.db` file), make sure
`surface-coordinator` owns them, start the service.

The backup contains the enrollment keys, the account password hashes and the
job history, including script text and output. Treat it as confidential.
