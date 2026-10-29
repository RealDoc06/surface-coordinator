# Surface Coordinator 0.2.3 function audit

Test date: **3 October 2026**. **Verdict: not ready for broad rollout.** Most tested functions work, but inspecting a library script can unexpectedly expand the selected targets to the entire fleet. Several dialogs let Cancel dismiss a write that continues running. Installed-service CMD scripts fail with canonical Windows paths, and unsupported sleep is incorrectly reported as succeeded. No application fixes were applied during this audit.

This report distinguishes fresh real execution, simulated execution, previous evidence and unverified behavior. It inventories the user-facing functions; it does not claim that every code branch, operating system or hardware combination was tested.

## Results at a glance

| Check | Fresh result | What this establishes |
| --- | --- | --- |
| Existing Rust suite | **547 passed**, 0 failed; 2 hardware tests ignored | Client 208, coordinator-core 182, Manager 65, protocol 75, release tool 17; individual tests use real components or explicit test doubles |
| Existing UI suite | **383 passed**, 0 failed, 26 files | React navigation/forms/validation/events with simulated HTTP and WebSocket |
| Manager launcher suite | **7 passed**, 0 failed | Launcher JavaScript with simulated native bridge; concurrency, progress and recovery behavior |
| Baseline total | **937 passing tests** | This total excludes the additional scenarios and grouped runtime checks below |
| Additional UI scenarios | **7 passed, 7 failed** | Failures reproduce two issue categories; four dialog families share the second issue |
| Static checks | TypeScript and workspace Clippy **passed** | `npm run typecheck`; `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| Manager packaging/signatures | **22 packaging checks passed**; real shipping signature accepted and altered bytes/version rejected | Packaging tests use mocked command boundaries; separate read-only verifier checks the actual 0.2.3 artifact |
| Release bundle integrity | **66 manifest hashes passed** in both folder and ZIP | ZIP contains 67 files including the checksum manifest itself; no files were repackaged |
| Packaged release API audit | **54 grouped checks passed**, 471 requests | Shipping executables, temporary databases and real console-client operations; unsafe device operations used a synthetic client |
| Release restart/reconnect scenario | **7 checks passed** | Real client enrollment, PowerShell output, cancellation, offline queue, identity-preserving reconnect and running-job replay after coordinator restart |
| Linux upgrade/restore | **33 checks passed** | Docker coordinator 0.2.0 → 0.2.3 → backup restore to 0.2.0, with a real Windows console client |
| Fleet correctness | **300 devices, 4,500 targets passed**; additional **54,000 output chunks** preserved | Synthetic clients, actual coordinator, reconnect/replay/cancellation/persistence; zero unexpected disconnects |
| Physical Surface settings | **5 setting jobs and 6 readback jobs passed** | Real volume, mute and brightness effects, followed by exact restoration |
| Installed Windows client | Core lifecycle **15 checks passed**; extended native checks **not all passing** | Install, service identity, restart/reinstall, PowerShell, user-session app launch and uninstall verified; CMD issue below |
| Signed client update fixture | **48/48 checks passed**, no cleanup errors | Disposable versions 90.0.1 → 90.0.2, real service replacement, rejection/failure injection and rollback; not a public release update |
| Installed Windows Manager | Installation, local start/open, Live UI, Change coordinator and stop **passed** | Actual shipping Windows application in disposable VM |
| Signed Manager update fixture | Rejection, install/restart and manual rollback **passed**, with retained data | Separate test product/key/feed, versions 0.2.1 → 0.2.2 → 0.2.1; not an installation from the production feed |
| Release-browser output/layout | **Passed sampled checks** | 600 Unicode output lines, live follow/pause/resume, modal focus behavior and 390×844 job layout |
| Manager public update feed | **Unavailable** | Manual check displays “the update server has no release information”; a public-feed installation was not possible |

Counts above are different levels of evidence, not numbers to add into a single percentage. The two ignored Rust tests touch real audio or scan Wi-Fi; they remain ignored in that suite even though separate native audio tests passed. Detailed sources: [Rust summary](../demo-data/function-audit-20261003/rust/summary.json), [UI summary](../demo-data/function-audit-20261003/ui/results.json), [Surface summary](../demo-data/function-audit-20261003/surface/summary.json).

## What does not work correctly

| ID / severity | Reproduction and observed result | Impact / next action |
| --- | --- | --- |
| **UI-01 · High** | Select one device → More actions → Run script from library → click the script **name** to inspect it → Run. The composer changes to all devices and submits `targets: {type: 'all'}`. Both the additional React test and the actual release browser reproduced this. | Merely inspecting a script broadens its execution scope. Preserve the selection when entering/leaving the script editor. The row's direct Run button already preserves selection. |
| **UI-02 · Medium** | Submit Change password, Rename group, Add/remove membership or Save to library while the response is pending, then click Cancel. The dialog closes and the write continues. Membership/library-save failures disappear without a visible error. A delayed real group rename also completed after its dialog was cancelled. | Operators cannot tell whether an operation was cancelled, completed or failed. Keep the dialog/result visible and disable dismissal while the write is pending. |
| **WIN-01 · High** | Four trivial CMD jobs, echo and `whoami /user`, failed in both System and Logged-in user modes through the shipping client service. An independent comparison ran the same batch file through CMD: canonical `\\?\C:\...` path failed with exit 1; ordinary `C:\...` path succeeded with `audit-batch-ok`. | Installed-service CMD scripting is broken for this path form. Normalize script paths for CMD without removing the install-path safety checks, then retest both scopes. |
| **WIN-02 · Medium** | Sleep on the VM returned API state succeeded, message “going to sleep”. The client then logged `SetSuspendState failed` with OS error 50, request not supported; the guest remained awake and executed another script. | The operator sees success for an action the OS refused. Reject unsupported power actions before reporting success, and make deferred failures visible. This is a reporting defect; lack of VM sleep hardware alone is not. |

UI-01 source: [ScriptsPage.tsx](../ui/src/pages/ScriptsPage.tsx), line 136, omits the current target state on the script-name link. The editor defaults missing target state to all devices. The changed count is visible in the composer, but the operator never requested the broader scope.

UI-02 source: [AccountsPage.tsx](../ui/src/pages/AccountsPage.tsx), line 231; [GroupNameDialog.tsx](../ui/src/components/GroupNameDialog.tsx), line 47; [GroupMembershipDialog.tsx](../ui/src/components/GroupMembershipDialog.tsx), line 50; [ScriptLibraryDialogs.tsx](../ui/src/components/ScriptLibraryDialogs.tsx), line 115. These modal callers omit the pending flag and leave Cancel enabled. Shared destructive confirmation dialogs already prevent pending dismissal. Membership Add was dynamically reproduced; Remove shares that component and was inspected, rather than separately delayed.

WIN-01 relevant source: [Windows script runner](../crates/client/src/platform/windows.rs), line 45, calls the `SC_SCRIPT` batch path through CMD; the installed path resolution uses canonical paths in [install.rs](../crates/client/src/install.rs). WIN-02 relevant source: [jobs.rs](../crates/client/src/jobs.rs), line 460, logs a power failure after it has already been reported; [power.rs](../crates/client/src/platform/windows/power.rs), line 93, captures the native suspend error.

Reproductions: [additional test source](../demo-data/function-audit-20261003/ui/extra.audit.test.tsx), [failure report](../demo-data/function-audit-20261003/ui/extra-audit-results.json), [real browser report](../demo-data/function-audit-20261003/integration/browser-report.json), [CMD path diagnostics](../demo-data/function-audit-20261003/native/shared/results/power-session/diagnostics.json), [sleep result/readback](../demo-data/function-audit-20261003/native/shared/results/power-session/unsupported-sleep-recovery.json), [sleep client log](../demo-data/function-audit-20261003/native/shared/results/power-session/unsupported-sleep-client.log).

Screenshots: [selected targets before inspecting](screenshots/function-audit-0.2.3/selected-targets-before.png), [expanded targets afterward](screenshots/function-audit-0.2.3/selected-targets-after.png), [rename completed after Cancel](screenshots/function-audit-0.2.3/pending-rename-cancel.png).

## Function-by-function results

**Real** means an actual release process, browser, service or device performed the action. **Simulated** means the UI/API/state machine was exercised with a fake client, platform or transport. A successful submitted job alone is not counted as proof of its physical effect.

| Function group | What was exercised | Result and limits |
| --- | --- | --- |
| Startup, health, session | Health endpoint, anonymous session, token exchange, invalid token, URL-fragment removal, account/token mode isolation, initial error/retry | **Pass**, release API and UI tests |
| Sign-in and access control | Correct/incorrect credentials, case-insensitive username, throttling, logout, cookie/session invalidation, anonymous protected-method rejection, same-origin write restrictions | **Pass**, real API and release browser normal login/logout; public TLS/cookie behavior behind a deployed proxy not rerun |
| Accounts | Create/list, invalid username/password, duplicate refusal, password reset, login using changed password, delete/cancel, self-delete protection, session revocation | **Pass** for normal real browser/API flows. **UI-02** affects pending password-change cancellation |
| Enrollment keys | Generate/list/import/export/reveal/copy/hide/revoke; invalid and duplicate refusal; secrets absent from list response | **Pass**, real API and browser. Revoking an established connection leaves it online until its next handshake, which is then refused and logged as designed |
| Devices | Enrollment, mutual authentication, online/offline/network metadata, detail/recent jobs, search and status/group filters, selection, refresh, forget/history/group removal | **Pass**, real API/client plus UI tests. Forgetting a running valid client does not uninstall it; it can enroll again |
| Rejected connections | Wrong/revoked key and protocol rejection, records/labels, live unseen badge and acknowledgement on viewing | **Pass**, real handshake/API and simulated UI events |
| Groups | Create/list/rename/delete, name rules, membership replacement/deduplication, invalid-member atomic refusal, selected-group targets, save/revert/filter controls | **Pass** normal API/browser/UI flows. **UI-02** affects pending rename/membership dismissal |
| Script library | Create/edit/duplicate/delete, names/body validation, built-in immutability, load/save from composer, earlier job payload remains independent | **Pass** for ordinary CRUD and direct row Run. **UI-01** affects inspecting before Run; **UI-02** affects pending Save to library |
| Built-in scripts | Disk space; Installed updates (last 20); Logged-in user and battery; Network configuration; System summary | **Pass**, executed by the shipping Windows console client |
| PowerShell execution | System and logged-in-user scope, stdout/stderr, Unicode, nonzero exit, terminating error, timeout and running cancellation | **Pass** in real console-client audit. Installed-service System/user identity, timeout/nonzero and corrected UTF-8 Unicode request passed; release browser preserved 600 sequential Unicode lines with exit 0 |
| CMD execution | Console-client output; installed-service System/user echo and identity probes; independent path comparison | Console **passed**. Installed-service canonical batch paths **fail, WIN-01** |
| App launch | Path/arguments/limits, real hidden-process launch, logged-in-user session/identity from service, exact process cleanup, missing executable and no-user errors | **Pass** for exercised paths. Locked screen and fast user switching were not freshly exercised |
| Job targets and forms | All six kinds; selected/all/group targets, empty/missing/unknown targets, action-tab selection retention, restricted clients, rerun prefills, confirmation/cancel | **Pass** baseline/UI/API validation with **UI-01** exception. Power/update/Wi-Fi tests include simulated dispatch, not necessarily native effect |
| Job queue and cancellation | Offline queue/reconnect delivery, queued and running cancellation, concurrency admission, duplicate delivery, replay/acknowledgement, expiry, durable state | **Pass**, real local coordinator/client and synthetic fleet checks; cancellation in fleet run completed in 902 ms |
| Job history and output | Paging, selected output, Unicode offsets, live events, crossing/stale response reconciliation, metadata excludes output, 1 MiB truncation, final state and persistence | **Pass**, real API/client and UI tests. Real visible-browser output followed, stayed fixed while reading earlier lines, and resumed at bottom. Sampled 390×844 job layout had no outer horizontal overflow |
| Settings | All 12 catalogue entries, type/range validation and delivery; actual settings/readbacks listed below | Catalogue/UI/API **pass**. Native detail is per-setting, not inferred from dispatch |
| Restart/shutdown/sleep | API validation/dispatch, confirmation, acknowledgement ordering, duplicate suppression; actual Windows actions | Restart and shutdown/recovery **passed** with same device identity and a new successful job. Supported S3 suspend **verified**, wake-up **unverified due to VM failure**. Unsupported sleep produces **false success, WIN-02** |
| Manual Wi-Fi move | SSID/password/coordinator validation, secret redaction, job/rerun payload, move-to-manager Wi-Fi confirmation and changed-destination conflict | **Pass UI/API/state-machine tests**. No fresh physical transfer in this audit; prior physical evidence is listed separately |
| Wi-Fi preservation/recovery | Active/connecting/unknown/manual selection protection, failed handoff recovery, Location-withheld state handling, reconnect paths | **Pass state-machine tests with simulated network state**. Surface reported online before and after settings tests, but that is not a fresh outage/handoff preservation experiment |
| Hotspot | Active-key choice, start/stop status, startup polling, revoked/missing key errors, copy details, unsupported/manual platform presentation, repeat-start rules | **Pass UI and Manager integration tests with fake network control**. Actual fresh hotspot activation was not performed |
| Client update controls | Cached-version display, signed EXE upload/download, invalid signature/tamper/version/platform/duplicate rejection, manual target selection, downgrade rules, restricted client access | Real release upload/download/verification **passed**. Separate real installed-service test-key update/recovery/rollback fixture **48/48 passed** |
| Automatic client rollout | Stored switch, canary success/failure, halt/recovery, counts, live refresh/coalescing | **Pass source/integration tests using synthetic clients**. No production fleet rollout occurred |
| Manager local operation | Shipping NSIS install/start, local coordinator start/open, authenticated Live UI, native Change coordinator menu, status refresh, stop, close/uninstall | **Pass in actual Windows VM**; [native browser evidence](../demo-data/function-audit-20261003/native/manager-browser.json), [close/uninstall run](../demo-data/function-audit-20261003/native/shared/results/004-manager-close-uninstall.json) |
| Manager remote operation and key sync | Address validation, stored selection, key-sync/status and failure logic, request boundary tests | Empty-address native validation **passed**; broader remote/sync behavior covered by tests. Public-host connectivity and every sync conflict not freshly exercised through native UI |
| Manager updates | Launcher check/install mutual exclusion, progress, failed-install retry, backend updater decisions/recovery; actual native signed fixture install and rollback | Tests and isolated native update **pass**. Production feed reports no release information; public update installation **unverified/unavailable**. Manager applies an update only after Install and restart |
| Hosted coordinator deployment | Linux image startup, real client, stopped-server backup, upgrade, preserved sessions/accounts/keys/groups/scripts/history/output, reconnect, restore and runnable old image | **33 real Docker checks passed**. Hosted coordinator still requires deployment; it has no self-updater |
| Storage/protocol/signing | Migrations, replay and selected-output persistence, wire formats/HMAC/signatures/version rules, real signing CLI with disposable keys | **Pass** automated and runtime checks; production signing secrets and publishing workflows were not changed |
| Global UI/error recovery | Navigation/menu capability gating, live reconnect banner, Reconnect now, initial load retry, session-expiry redirect, loading/empty/error states, keyboard menu/modal controls | **Pass covered UI tests and sampled release-browser flows**, with **UI-02** exception. Real modal initial focus, Shift-Tab containment and Escape/focus return passed. No claim of exhaustive accessibility or all-screen-size coverage |

The [complete UI inventory](../demo-data/function-audit-20261003/ui/coverage.md) maps all 19 route patterns plus the fallback, individual controls and validation gaps. The [backend function map](../demo-data/function-audit-20261003/rust/coverage-map.json) maps 26 function groups to exact test names and source locations.

## All 12 settings

All rows passed fresh API acceptance, invalid-value rejection and dispatch to a synthetic client. The UI separately passed generic percentage/integer/boolean/text editing and validation. Native readbacks below establish more than API dispatch, with their limits stated explicitly.

| Setting | Fresh native result | Evidence strength / limit |
| --- | --- | --- |
| `audio.volume` | **Pass**, Surface requested 45%; VM requested 37%; readback matched | Real default audio endpoint. Surface original level restored exactly |
| `audio.mute` | **Pass**, mute true and false; readback matched on Surface and VM | Real default audio endpoint; original Surface mute restored |
| `display.brightness` | **Pass**, Surface changed 61 → 56 → 61 with WMI readback | Real built-in display. VM correctly reports unsupported virtual display; that error is not counted as a hardware success |
| `power.display_timeout_ac` | **Pass**, VM set 31 minutes | `powercfg` readback 1,860 seconds; actual idle wait not performed |
| `power.display_timeout_battery` | **Pass**, VM set 32 minutes | `powercfg` readback 1,920 seconds; physical battery discharge/idle wait not performed |
| `power.sleep_timeout_ac` | **Pass**, VM set 33 minutes | `powercfg` readback 1,980 seconds; does not prove native sleep itself |
| `power.sleep_timeout_battery` | **Pass**, VM set 34 minutes | `powercfg` readback 2,040 seconds; does not prove physical battery sleep |
| `desktop.wallpaper` | **Pass**, VM applied a test image and read back its path; missing file fails clearly | User-context setting/readback; no claim of every image format or multi-monitor visual effect |
| `windows_update.pause_days` | **Pass registry write/readback**, pause 2 writes expected timestamps; resume clears them | Initial readback helper error was corrected; independent readback found no remaining pause properties. No actual Windows Update scan observed |
| `taskbar.locked` | **Pass policy write/readback**, true then false in VM | Visual enforcement after future sign-in was not observed |
| `start.layout_locked` | **Pass policy write/readback**, true then false in VM | Visual enforcement after future sign-in was not observed |
| `time.zone` | **Pass**, Pacific Standard Time → UTC → Pacific Standard Time; readback matched | Invalid Windows zone id fails with explanatory error |

Evidence: [Surface hardware summary and restoration](../demo-data/function-audit-20261003/surface/summary.json), [VM setting and script details](../demo-data/function-audit-20261003/native/shared/results/production-lifecycle/evidence/all-settings.json), [corrected Unicode/Windows Update readback and CMD diagnostics](../demo-data/function-audit-20261003/native/shared/results/power-session/diagnostics.json). The first VM detail file retains `passed: false` because it includes the confirmed CMD failures and superseded helper errors. The passing core lifecycle report must not be substituted for its result. Setting-effect limits remain per row; this table does not label all Windows behaviors fully verified.

Surface setting jobs took **355–659 ms on the device**, measured from target start to finish and excluding submission/queue time. That demonstrates responsiveness for these five setting operations, not for every job, network or fleet size.

## Responsiveness and fleet behavior

The separate one-client release run measured dispatch acknowledgements of **3–8 ms**, PowerShell completion of **766–1,457 ms**, and device-list API p95 of **0.56 ms** on localhost. These are small local samples, not fleet or network promises. [Release integration evidence](../demo-data/prod-validation/20261003-193229-2c672a/report.json).

The final run used **300 synthetic mutually authenticated clients**, **15 jobs / 4,500 targets**, then a controlled output stream of **54,000 chunks / 27,648,000 bytes**. Every target's state and sequenced output matched the API and the live events. All 300 clients finished online, with **zero unexpected disconnects** and no failed HTTP requests. Duplicate delivery did not produce duplicate synthetic execution. Data survived coordinator process exit and SQLite integrity checking.

| Measurement | Result |
| --- | --- |
| Active fleet cancellation | 902 ms |
| Controlled output chunks sent / observed | 54,000 / 54,000 |
| Target results acknowledged | 300 / 300 |
| Result drain after a target finished local emission | Median 18.601 s; p95 19.840 s; maximum **20.341 s** |
| Full controlled-stream completion | Maximum 66.869 s |

**This does not meet a blanket “semi-instant” claim.** Correctness and connection stability passed, but completion acknowledgements took substantial time to drain under this load. Fresh Windows VM provisioning was running concurrently, so this is not a quiet-host performance benchmark and does not isolate the bottleneck. [Final fleet summary and raw evidence links](../demo-data/function-audit-20261003/rust/fleet-final-summary.json).

An earlier fleet attempt had too little fixture hold time left when an additional review job was submitted, and the simulated clients intentionally rejected it. The evidence remains preserved, but it is classified as a test-harness timing error and superseded by the complete final run. Likewise, an earlier application-launch probe used invalid nested CMD redirection quoting; direct execution failed identically, and the corrected PowerShell probe passed. Neither is counted as an application defect. The installed-service CMD canonical-path failure above is a separate confirmed issue.

## Native Windows validation

The shipping 0.2.3 client installed as an automatic LocalSystem service in SC-VALIDATION, executed System and logged-in-user PowerShell, preserved identity across restart/reinstall, launched an application in the actual user's session, cleaned up that process, and uninstalled/purged its dedicated paths. These **15 lifecycle checks passed** with no cleanup errors. [Core native report](../demo-data/function-audit-20261003/native/shared/results/production-lifecycle/report.json) and [combined native results](../demo-data/function-audit-20261003/native/native-summary.json).

| Native scenario | Result and evidence |
| --- | --- |
| Installed-service CMD | **Fail, WIN-01**, four failing jobs and independent canonical-versus-normal path comparison |
| Unicode and Windows Update resume | **Pass after correcting test helpers**. UTF-8 request returned exact `audit-caffè-東京`; registry readback found no remaining pause properties. Initial HTTP 400 and missing-property errors belonged to the test helpers |
| No signed-in user | **8 checks passed**. Seven user-scoped operations returned `no_user_session`; a System script still ran. [Session evidence](../demo-data/function-audit-20261003/native/shared/results/power-session/no-session.json) |
| Restart | **Pass**. Actual boot changed, Windows power events appeared, service restarted automatically with the same device identity, and another System job succeeded. [Recovery](../demo-data/function-audit-20261003/native/shared/results/power-session/restart-recovery.json) |
| Shutdown | **Pass**. Guest/container exited after the client action. Starting the same disk recovered the same identity, automatic service and successful job. [Recovery](../demo-data/function-audit-20261003/native/shared/results/power-session/shutdown-recovery.json) |
| Unsupported sleep | **Fail, WIN-02**. Success was reported before the OS rejected sleep |
| Supported S3 sleep | **Suspend verified**, QEMU entered suspended state. **Resume unverified**: `system_wakeup` terminated the guest/container; cold-boot recovery does not establish resume. [Suspend evidence](../demo-data/function-audit-20261003/native/supported-sleep-monitor.json), [wake failure](../demo-data/function-audit-20261003/native/supported-sleep-wakeup-failure.json) |
| Signed client installation/update/rollback | **48/48 passed**, disposable versions 90.0.1 → 90.0.2: signature rejection, locked-config rollback, replacement/rename faults, successful swap and preserved identity, fourth-start recovery, real 180-second deadline rollback, exact-byte restoration and uninstall. [Fixture report](../demo-data/function-audit-20261003/native/shared/results/signed-client-lifecycle/report.json). Recovery-state files were injected for marker/deadline cases; these are not uncontrolled corrupt-executable crash tests |

The client update fixture's installer/update source matches the current source. Of 116 snapshotted source files, the later differences are Windows settings, Wi-Fi and the Manager validation example; the fixture does not retest those later changes. [Source comparison](../demo-data/function-audit-20261003/native/fixture-source-comparison.json).

The VM clock shifted by seven hours after a cold start because of its RTC/timezone configuration. Native power checks therefore used event-record sequence numbers, boot changes and actual VM/container state, not timestamps alone. Raw guest timestamps were retained. This report's test date and external timings come from the execution host. Locked-screen and fast-user-switching behavior were not freshly exercised.

The actual installed Manager was separately exercised through its native window: Start local coordinator opened the authenticated Devices page showing **Live** and **v0.2.3**; Change coordinator returned to the correct Running state; Stop returned to Not running. An empty remote address was rejected clearly. Manual Check for updates displayed the feed error instead of freezing. [Manager native browser record](../demo-data/function-audit-20261003/native/manager-browser.json). Screenshots: [local running](screenshots/function-audit-0.2.3/manager-local.png), [stopped](screenshots/function-audit-0.2.3/manager-stopped.png), [update feed unavailable](screenshots/function-audit-0.2.3/manager-update-unavailable.png), [mobile job view](screenshots/function-audit-0.2.3/mobile-job.png).

The separate **Manager Update Test** application was freshly installed from the retained 0.2.1 fixture, using a loopback update feed and test signing key. Its real native UI rejected tampered bytes and an announced version that disagreed with the signed payload, then successfully installed 0.2.2 and restarted. A group named `audit`, created through the UI beforehand, remained after the update. Reinstalling the 0.2.1 fixture manually rolled the Manager back; the group remained again. This is manual rollback, not an automatic Manager crash rollback. [Native UI evidence](../demo-data/function-audit-20261003/native/manager-fixture-browser.json), [fixture summary](../demo-data/function-audit-20261003/native/manager-fixture-summary.json), [native evidence index](../demo-data/function-audit-20261003/native/evidence-index.json).

The updated executable's hash matched a fresh extraction from the signed 0.2.2 installer, and its process/version changed. An initially mislabeled reference payload was caught and the check repeated against the actual installer; that observer failure was not treated as a product failure. The updater source matches all four retained source snapshots, but no manifest binds these reused fixture binaries to one exact snapshot. This is fresh fixture evidence, not proof that the shipping 0.2.3 application updated through its public feed. [Source comparison and limits](../demo-data/function-audit-20261003/native/manager-updater-source-comparison.json).

Screenshots: [tampered update rejected](screenshots/function-audit-0.2.3/manager-tamper-rejected.png), [version mismatch rejected](screenshots/function-audit-0.2.3/manager-wrong-version-rejected.png), [update completed](screenshots/function-audit-0.2.3/manager-update-complete.png), [data retained after update](screenshots/function-audit-0.2.3/manager-update-data-retained.png), [manual rollback completed](screenshots/function-audit-0.2.3/manager-rollback-complete.png), [data retained after rollback](screenshots/function-audit-0.2.3/manager-rollback-data-retained.png).

## Test machines, release identity and cleanup

| Execution / target | Use and isolation |
| --- | --- |
| **Seba-work-laptop** (`SEBA-WORK-LAPTO`) | Execution host, source tests, release console tests, browser control and isolated temporary coordinators. These localhost URLs refer to this machine |
| **SC-VALIDATION**, disposable local Windows VM | Shipping client service and Manager installer/native UI checks; additional signed-update fixtures. Test applications/services, owned container and disk removed after testing |
| **Docker Desktop** on the execution host | Isolated Linux coordinator upgrade/backup/restore and synthetic fleet fixtures; audit containers/volumes/client processes were removed |
| **Surface User, 10.10.10.186** | Only reversible audio/mute/brightness jobs and readback. Existing service was not stopped, replaced or upgraded; no Wi-Fi, hotspot or power action was sent in this audit |
| **network-vm, 10.10.10.110** | Hosted the isolated Surface test coordinator and artifact server in an owned temporary directory, subsequently removed. The existing coordinator carried guarded bootstrap/readback/cleanup jobs; it was not upgraded |

The tested shipping Windows artifacts report **0.2.3**. Full filenames, lengths and SHA-256 values are in the [shipping artifact manifest](../demo-data/function-audit-20261003/native/shipping-artifacts.json); identifying prefixes are:

| Artifact | SHA-256 prefix |
| --- | --- |
| Client executable | `1052BE4E57C3E983` |
| Windows coordinator executable | `EA6799C2F7A79964` |
| Manager NSIS installer | `6360A9842CE667C4` |

The Surface test used the same client hash and a Linux coordinator whose separate hash is recorded in its summary. Source tests ran the current working tree at Git HEAD `0825ca740f7075a40d7424962559c58409776bff`, including existing uncommitted work; source tests and packaged-binary checks are reported separately. Audit scripts/evidence and this report were added; application source was not fixed and no release was published or deployed to the fleet. [Shipping signatures and complete bundle integrity verification](../demo-data/function-audit-20261003/rust/shipping-package-verification.json) passed. The Manager's updater signature is valid; its Windows Authenticode status is **NotSigned**. These are separate mechanisms, and no SmartScreen reputation is established. The existing 0.2.3 bundle remains unchanged and has the defects documented here.

Confirmed cleanup: release API fixture processes stopped; ports released; Docker test containers/volumes removed; final fleet coordinator stopped and private access file removed. Surface volume/mute/brightness returned to their exact original values, and its temporary tasks/processes/directories were removed. The original service reported Running with unchanged PID **11308** and the device reported online before and after testing; the final check was at **17:42:32 UTC**. WLAN SSID readback was denied by Windows Location permissions; the reported IP remained 10.10.10.186. [Restoration details](../demo-data/function-audit-20261003/surface/summary.json).

The separate release-browser fixture's two owned processes also stopped and port 18141 was released. Its private access file remains in the ignored test directory because automatic approval review blocked its deletion, with the reason “blocked by policy”; this audit does **not** claim that every private access file was removed. [Browser fixture cleanup record](../demo-data/function-audit-20261003/integration/cleanup.json).

Final native cleanup passed: the test client service, Manager installations/processes and guest listeners were removed; the owned VM container and disk are absent; the console port is closed; temporary KVM modules were unloaded; and the guest credential file was removed. All **294 hashed evidence files** remained intact through cleanup. The final test container was deliberately stopped during cleanup, so its recorded exit 143 is expected and distinct from the earlier successful shutdown-action test. [Native cleanup proof](../demo-data/function-audit-20261003/native/cleanup.json).

## Previous evidence and remaining gaps

The earlier [0.2.3 validation report](validation-0.2.3.md) records physical Wi-Fi/hotspot transfers, manual-network preservation observations and signed updater fixture exercises. Those checks are **previous evidence**, even where their date is also 3 October. They were not rerun by the fresh settings-only Surface audit and are not included in the fresh counts above.

An earlier deliberately malformed MZ installer stalled during native launch; its cause remains unresolved and that scenario was not rerun or counted as passed. Recovery after an authenticated unsupported installer format has previous passing evidence, but was not freshly repeated. These limits are separate from the successful genuine signed NSIS update and rollback above.

Remaining limits are: fresh physical Wi-Fi wrong-password/unreachable-destination recovery; hardware hotspot behavior; all Linux/macOS Manager native functions and installers; public GitHub release/publish workflow and update feed; external TLS/reverse-proxy behavior and long-duration soak; full keyboard/accessibility/small-screen coverage; visual enforcement of Windows policies after sign-in; actual Windows Update scans; native resume from supported sleep; locked-screen and fast-user-switching behavior. No line/branch coverage percentage was measured.

Before broad rollout, fix and retest **UI-01**, **UI-02**, **WIN-01** and **WIN-02**, and make a signed release available through the real update feed. A separate quiet-host performance run is required before describing fleet operation as semi-instant. This audit supersedes earlier readiness conclusions for the unchanged 0.2.3 bundle; earlier test observations remain historical evidence.
