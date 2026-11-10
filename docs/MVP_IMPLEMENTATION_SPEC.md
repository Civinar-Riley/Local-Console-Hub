# MVP Implementation Spec

> **Status:** implementation baseline  
> **Target:** Local Console Hub v0.1.0 / Windows-first MVP  
> **Related:** [PRODUCT_SPEC.md](./PRODUCT_SPEC.md), [LOGGING.md](./LOGGING.md), [DEVELOPMENT.md](./DEVELOPMENT.md), [DECISIONS.md](./DECISIONS.md), [UI_STYLE_GUIDE.md](./UI_STYLE_GUIDE.md)

This document is the implementation contract for contributors and coding agents. It freezes the important boundaries, observable behavior, interfaces and acceptance criteria so work can be split without incompatible implementations.

## 1. MVP outcome

The MVP is complete when a Windows user can:

1. configure local services and interactive shells;
2. start, stop, restart and switch between them from one desktop window;
3. interact with a real terminal session, including stdin and Ctrl+C;
4. hide the main window to the tray without stopping managed sessions;
5. see service name, state and optional port/URL without dashboard clutter;
6. use selective logging without every terminal automatically producing files;
7. locate any persisted log by session and run;
8. stop one managed session without killing unrelated processes.

The approved V2 UI references are:

- assets/ui/ui-v2-service.png
- assets/ui/ui-v2-terminal.png
- docs/UI_STYLE_GUIDE.md

## 2. Technical baseline

Unless an explicit design decision changes this spec:

- Desktop shell: **Tauri 2**
- Frontend: **React + TypeScript + Vite**
- Terminal renderer: **xterm.js**
- Backend: **Rust**
- Async runtime: Tokio where needed
- Serialization: Serde
- Configuration: human-readable YAML
- PTY: backend-owned abstraction; Windows implementation may begin with portable-pty, but must satisfy ConPTY behavior in the acceptance tests

If portable-pty cannot satisfy the Windows requirements, replace only the PTY backend with direct Windows ConPTY integration. Do not change the frontend/session contracts to work around it.

Windows data locations should follow OS app-data conventions:

~~~text
%APPDATA%\LocalConsoleHub\config.yaml
%LOCALAPPDATA%\LocalConsoleHub\logs\...
%LOCALAPPDATA%\LocalConsoleHub\metadata\...
%LOCALAPPDATA%\LocalConsoleHub\cache\...
~~~

Runtime logs must not be written beside the executable by default.

## 3. Module boundaries

The exact filenames may vary, but responsibility boundaries must remain equivalent.

~~~text
src/
├─ app/
├─ components/
│  ├─ sidebar/
│  ├─ session-header/
│  └─ terminal/
├─ state/                  # frontend view state only
└─ types/

src-tauri/src/
├─ config/                 # schema, validation, defaults
├─ session/                # Session Core + state machine
├─ pty/                    # PTY abstraction and Windows backend
├─ process/                # process supervision / stop / kill tree
├─ logging/                # buffer, captured logs, external logs
├─ shell/                  # handing a resolved path to the OS (T10)
├─ health/                 # port/HTTP checks
├─ tray/                   # native tray lifecycle
├─ ipc/                    # Tauri commands/events/DTO mapping
└─ app/
~~~

Boundary rules:

- UI does not own process truth.
- PTY code does not decide logging policy.
- Logging code does not decide session lifecycle.
- Tray actions call the same Session Core APIs as the main window.
- Session Core is the single source of lifecycle truth.

## 4. Core data contracts

### SessionConfig

Conceptual example:

~~~yaml
id: sillytavern
name: SillyTavern
type: service
cwd: D:/Tools/SillyTavern
command: node server.js
url: http://127.0.0.1:8000
port: 8000
purpose: 聊天前端
close_impact: 可停止；网页会失联
logging:
  mode: auto
  source: captured
~~~

Required fields:

- id: stable, unique, filesystem-safe identifier
- name
- type: service or terminal

Optional fields both types own:

- cwd
- purpose
- close_impact

Optional service fields:

- command
- url
- port

Optional terminal fields:

- shell
- initial command

`purpose` and `close_impact` are free text with no runtime meaning: they say
what a session is for and what stopping it costs, and the header, the Details
card and the search filter read them without consulting the session type
(D-027). Every other field belongs to exactly one type; validation rejects it
on the other rather than dropping it quietly.

Service entries also carry **where they are displayed** and **who ends them**
(#66, DECISIONS.md D-034):

~~~yaml
display: internal        # or `window`: keep the application's own console
lifecycle: managed       # or `independent`, for an application that ends itself
~~~

Both are optional and both are service-only. Absent means `internal` and, from
there, `managed` — which is what every entry written before those keys existed
meant. A `window` entry without a `lifecycle` is `independent`: leaving the Hub
must not close an application nobody asked the Hub to manage. `internal` with
`independent` is refused (the Hub-hosted console *is* the run's owner), and a
`window` entry cannot ask for `source: captured`, because it has no Hub-side
console to capture from.

Logging fields follow LOGGING.md.

### SessionStatus

MVP lifecycle states:

~~~text
Stopped
Starting
Running
Stopping
Exited
Error
~~~

Optional runtime flags may supplement lifecycle state:

~~~text
busy: bool | unknown
ready: bool | unknown
~~~

Busy/ready must not replace lifecycle state.

### SessionRuntime

At minimum:

- session id
- lifecycle status
- PID when applicable
- run id
- start time
- exit code when known
- PTY attachment state
- whether the run is one the Hub did **not** start (#67)
- effective logging mode
- terminal buffer reference
- last structured error

The "did not start it" flag is the third answer beside D-034's two. Both of those
are the Hub's — it started them and holds a handle on the tree — while an
instance the user was already running is only *reported* by the Hub, and nothing
that acts on a run may act on it (D-036). It rides the snapshot because the
decision it guards is made in two places that only see snapshots: the tray's bulk
actions and the window's action availability.

The buffer reference is a **summary** (bytes held, lines held, bytes discarded),
not the scrollback itself. The scrollback is read on demand, so a session with a
full buffer does not make every state change expensive (§14). A UI that needs the
content asks for it; a UI that needs "is there anything to show, and was any of
it lost?" reads it from the snapshot it already has.

### RunRecord

Each managed start creates one run record containing:

- run id
- session id
- started/ended timestamps
- exit code
- PID
- logging source/mode
- persisted log path when one exists

## 5. State machine

Normal transitions:

~~~text
Stopped -> Starting -> Running
Running -> Stopping -> Exited/Stopped
Running -> Exited
Starting -> Error
Running -> Error
Stopping -> Error
Exited -> Starting
Error -> Starting
Error -> Stopping
~~~

`Error -> Stopping` is the recovery path for a stop that could not confirm the
run's process tree was gone. That `Error` still owns the run (the handle is what
accounts for a tree that may be alive), so retrying the stop ladder — including
force stop — must be possible; otherwise the only way out would be a restart,
which would have to launch a second run while the first tree is unaccounted for.

One move is deliberately **not** in this table: a session with nothing of the
Hub's running may become `Running` because the user associated an instance they
were already running (#67). The table describes runs the Hub creates, and that
move creates none — it changes only what the Hub accounts for, and D-036 records
the reasoning. It comes from the same three states a start may come from
(`Stopped`, `Exited`, `Error`), and from those alone.

Rules:

1. Start while Starting/Running is rejected or a clear no-op.
2. Restart cannot launch a replacement until the previous managed process is confirmed stopped.
3. State changes originate in Session Core, not UI assumptions.
4. Unexpected exit updates state while the UI is hidden.
5. Exit code and error context remain available in the run record.

## 6. PTY contract

Interactive terminal support is a **release blocker**.

The PTY layer must support:

- spawn shell/command in configured working directory;
- bidirectional byte stream;
- resize;
- Unicode input/output;
- ANSI output;
- Ctrl+C behavior expected by common Windows CLI applications;
- terminal session remaining alive while hidden or while another session is selected.

xterm.js renders the terminal; it does not own the process or PTY.

Conceptual backend operations:

~~~text
spawn(session_id, cols, rows)
write(session_id, bytes)
resize(session_id, cols, rows)
close(session_id)
subscribe_output(session_id)
~~~

Output transport must be bounded/backpressured so a noisy process cannot grow memory without limit.

## 7. Process supervision contract

The process layer must:

- know exactly which process belongs to a managed session;
- track enough descendant/process-tree information for safe shutdown;
- distinguish graceful stop from force kill;
- never kill unrelated processes based only on executable name;
- start a run on a console nothing on the desktop can show, so hosting a run
  never puts a window on screen (D-035).

Default stop path:

~~~text
request graceful stop
        ↓
wait configured timeout
        ↓
confirm process exit
        ↓
if still alive -> explicit force-kill path
~~~

The graceful request is a `CTRL_BREAK` aimed at the run's process group, so a run
needs a console for it to travel through. What a run must not have is a console
*window*: a console allocated for it is what a desktop terminal application turns
into a stray window titled after the program being run (D-035). The run therefore
shares the Hub's console when the Hub has one, and is started with
`CREATE_NO_WINDOW` — a console with no window — when it does not.

Console borrowing is process-wide (D-037). Window lookup and both graceful-stop
attempts share one console claim, and return the caller to its original console
through surviving original members, including on failed attachment and unwind.
Borrowing is refused when the caller has a console window, is its console's sole
member, or cannot obtain a complete bounded membership list. If every original
peer leaves during a borrow, restoration can fail; it is diagnosed and not
reported as a successful borrow. Run preparation (including D-035's console
observation) and actual process creation take the same claim, so they never see
a transient borrowed console. The creation-flag rules and stop ladder are unchanged.

Graceful-stop acceptance must wait for a live child to register its actual
control handler. Startup failure is not graceful delivery. The isolated console
measurement requires `graceful_delivered=true`, `Exited`, and the handler's exit
code 0, while counting only newly observed console-host window identities.
See D-035 and VERIFICATION §6 for both console shapes and measurement limits.

For terminal sessions, Ctrl+C is not the same action as closing the session.

## 8. Logging contract

LOGGING.md is normative.

Keep separate:

1. **Terminal Buffer** — bounded in-memory scrollback
2. **Hub Captured Log** — optional persistent stdout/stderr
3. **External Log** — application-owned file referenced by Hub

Defaults:

- interactive terminal: no persistent log
- stdin: never persisted by default
- application with its own log: reference it rather than duplicate it
- service without external logs: auto may resolve to on_error
- UI must reveal whether persistence is active and where it writes

For on_error, retain a bounded pre-error buffer so an abnormal exit can preserve useful context.

## 9. IPC contract

Keep the frontend/backend contract explicit.

MVP command equivalents:

~~~text
list_sessions
list_session_configs
get_config_report
get_session
start_session
stop_session
force_stop_session
restart_session
create_temporary_terminal
remove_session
add_application
recommend_display
save_terminal_config
activate_session
resolve_session_open
attach_terminal
terminal_write
terminal_resize
open_session_url
open_session_cwd
get_run_history
get_log_info
save_run_log
set_log_recording
open_log_file
open_log_folder
preview_log_cleanup
cleanup_logs
~~~

`save_run_log` commits an `on_error` run's log on demand and `set_log_recording`
switches a `manual` run's recording on or off (LOGGING.md §3). Both name a
single operation; neither is a generic "do something to this session".

`list_session_configs` answers the half of a session that is not its snapshot —
what it *is* (name, type, purpose, close impact, port, cwd, shell) — from the
same registry the snapshots come from, so a row the window can render is one
Session Core can act on.

`get_config_report` is read-only startup diagnostics: it reports whether the
config file was loaded, missing, unreadable or unavailable, plus its path,
validated sessions and file/per-session errors. A read or parse error never
prevents the app from opening, and a bad session never hides valid sessions.

`create_temporary_terminal` and `remove_session` are the two operations that
make the session list something other than a startup snapshot (D-031). The
first creates, registers and starts an interactive terminal from the window —
no form, no config file, answering with both halves of the session it made —
and the second takes a temporary session that has ended out of the registry.
The shell and the directory are the session layer's to resolve (PowerShell 7,
else Windows PowerShell; the user's home directory unless an entry names one),
and a creation that cannot resolve or start one leaves no row behind. A
configured session is refused by `remove_session`: it lives in the config file,
and this command is not a way to delete one.

`add_application` is the secondary entry ("添加应用", D-032): it validates one
form against the config layer, saves it into the user's `config.yaml` as an
appended entry, and registers the session so the window lists it immediately.
It carries the two display dimensions (#66) when the user chose them, and
omits them when the form was left alone.

`update_application` is that entry's edit half: the same form, opened on what
the file holds, validated the same way, and applied to the registered session
and to its one block in `config.yaml`. The session id is not editable — it
keys the registry, the file and the log directory — and every other key the
build does not know stays in place. The edit is refused while the session still
has a live run: the configuration is what the run was started from, and the
changed values apply to the next start.

`recommend_display` is the form's one read-only question (#66): given a
command and a working directory, it answers what this build can *confirm*
about that launch method's display — the executable the command resolves to,
and whether it gets a console of its own — as advice the user may accept or
change. It never reads the entry's name (spec #59 decision 9).
The save is an edit of the user's own text — unrelated entries, ordering and
comments survive, and a file this build cannot safely extend (broken YAML, an
unknown root key, an inline `sessions: []`) is refused rather than rewritten.
`activate_session` is the one way an entry opens an application: start it when
nothing is running, answer with the run it already has when something is, and
refuse while it is stopping. It is deliberately not `restart_session`. Since
#66 its answer also carries what happened to the application's **own** window —
focused, refused by Windows, or no window to bring forward — for entries that
keep one, so a click whose window did not come forward says so instead of
starting a second copy (D-034).

`resolve_session_open` is the answer to a question `activate_session` asked
(#67, D-036): when an entry that keeps its own window has nothing running in the
Hub, that command first looks for an instance already running outside it, and
what it cannot decide it asks about rather than guessing. Its `associate` half
takes the process the user picked — pid *and* creation time, re-verified against
the process table before anything is believed about it — and its `new` half
starts the Hub's own copy and leaves the other instance alone. Neither changes
the configuration file.

`save_terminal_config` is the other half of the quick entry (D-033): it writes
a temporary terminal's launch method — the shell and directory it is already
running, plus the name the user gives it — into `config.yaml` through the same
append-and-refuse save `add_application` uses, and marks that session as saved.
The session keeps its id and its run: no process is started or copied, nothing
the user typed is stored, and the entry carries no `initial_command` and no
`logging:` block, so a saved terminal persists no more than the temporary one
did.

`attach_terminal` is what a terminal view calls when it appears: it answers with
the retained scrollback, the byte offset that scrollback reaches, and the run
they belong to. `terminal_write` carries input bytes (base64) and
`terminal_resize` the view's geometry — which is remembered for a session that
has not started yet, so a shell begins at the size its view already has.
Terminal output arrives as events, not as a return value (§6).

`open_log_file` and `open_log_folder` hand one file — or the folder holding it —
to the OS, and `preview_log_cleanup` / `cleanup_logs` are the two halves of
retention (LOGGING.md §9, §10). They take a session id and an optional run id,
never a path: Session Core resolves which file a session owns, and the command
cannot be used to open anything else (DECISIONS.md D-021).

Do not introduce one generic "execute arbitrary backend action" command.

MVP event equivalents:

~~~text
session-state-changed
session-created
session-saved
session-removed
run-record-updated
app-summary-changed
terminal-output
~~~

Every session event includes the session id.

`session-created` carries the session's configuration — the half a state event
cannot, and the reason a listener can render a row it has never seen before
without re-reading a list (D-031). It is published before the session starts,
so every lifecycle event that follows belongs to a session the listener already
knows. `session-saved` carries the same pair about a session that was already
there, whose configuration became one the config file describes (D-033); a
listener applies it to the row it has, and the two payloads are deliberately
one shape, because the fact they ask a listener to apply is the same one. A
save is published only after the file holds the entry. `session-removed` says a
session left the registry, after which nothing about that id is published
again.

Terminal output should be batched enough that high-volume output does not create an expensive UI event per line.

## 10. UI contract

UI_STYLE_GUIDE.md is normative.

The main window must preserve:

- compact left session list;
- status dots instead of decorative per-session icons;
- selected session name + optional port;
- compact Open / Restart / Stop / More actions;
- Terminal / Logs as the primary content switch;
- terminal as the dominant region;
- interactive input behavior;
- low-density status bar.

Do not reintroduce:

- permanent right-side information dashboard;
- redundant global Logs navigation;
- repeated state/port/logging information;
- decorative app icons in the session list.

For stopped services, Start replaces the relevant running action state.

For terminal sessions, PTY-native interaction is required. A fake command textbox cannot replace real terminal input if it breaks full-screen/interactive CLI applications.

For an entry configured as `display: window` (#66) the content region states
where the application's console is and offers the one action that works —
opening it, which brings its own window forward — instead of drawing a terminal
that would stay empty forever. The header's primary control on such an entry is
that open action; stop and restart are shown only for a run the Hub owns.

## 11. Tray contract

Clicking the main window close button hides the window to tray by default.

Tray behavior must provide the functional equivalent of:

- app title/icon;
- summary such as "3 running (1 busy)";
- compact running-session list;
- Show Window;
- Restart Failed;
- Stop All;
- Exit.

If managed sessions are active, Exit must not silently destroy them.

MVP acceptable behavior:

- confirm "Stop managed sessions and exit" or Cancel.

Detach-and-leave-running is optional and must not be faked before technically safe.

## 12. Health/status checks

MVP needs a small abstraction only:

- process alive;
- TCP port open;
- optional HTTP GET.

Checks must be low-frequency, cancellable with session lifecycle, non-blocking to UI, and separate from lifecycle truth.

## 13. Configuration validation

Validate before launch.

Reject with actionable messages:

- duplicate session id;
- unsupported type;
- missing command/shell;
- invalid required working directory;
- invalid port;
- malformed URL;
- incompatible logging settings.

One bad session must not hide unrelated valid sessions. Report per-session config errors.

## 14. Performance requirements

- hidden-to-tray idle CPU should remain effectively near idle;
- no high-frequency global process scanning;
- terminal scrollback must be bounded;
- log writes should be buffered;
- invisible terminals should not continuously re-render;
- one high-output session must not freeze the whole UI.

## 15. Security/privacy requirements

Must not:

- record stdin by default;
- dump full environment variables into logs/metadata;
- upload logs automatically;
- scan/capture every console process;
- kill by executable-name pattern;
- silently delete external application logs.

## 16. MVP test matrix

### Interactive terminal

- PowerShell starts
- commands can be typed
- Unicode works
- Ctrl+C interrupts a long-running command
- resize works
- switching sessions keeps PTY alive
- tray hide/restore keeps PTY alive
- closing a terminal ends the shell's process tree, not just the shell (#61, D-028)

### Service

- long-running service starts
- stdout/stderr visible
- configured port/URL visible
- restart waits for previous process to exit
- graceful stop works
- force stop affects only the managed tree

### Logging

- terminal mode off creates no persistent log
- captured always creates a run-specific log
- on_error preserves pre-error context
- external log references the original file without duplicate capture

### Multi-session

- at least three concurrent sessions
- stopping one leaves others alive
- noisy output in one does not make another unusable

### Tray

- X hides window
- sessions continue
- Show Window restores
- summary reflects runtime changes
- Exit never silently destroys running work

## 17. Definition of Done for v0.1.0

v0.1.0 reaches release-candidate status only when:

- all P0/P1 tickets in EXECUTION_PLAN.md are complete;
- hard dependencies are resolved;
- the Windows MVP test matrix passes;
- no known issue can terminate unrelated processes;
- PTY interaction is real, not read-only emulation;
- logging defaults conform to LOGGING.md;
- UI preserves the approved V2 information hierarchy;
- live managed sessions expose clear close-impact context before destructive actions;
- Terminal / Logs / Details keep interactive, persisted and low-frequency information separated;
- tray behavior is implemented;
- a fresh user can configure a SillyTavern/ComfyUI-like service and a PowerShell terminal without modifying source code.

## 18. Change control

An implementation agent must not silently change:

- tech stack;
- config semantics;
- PTY ownership;
- logging defaults;
- session state model;
- process stop semantics;
- V2 UI information hierarchy.

If a blocker requires such a change:

1. stop dependent work;
2. document the blocker on the relevant ticket;
3. propose the smallest change;
4. update DECISIONS.md and this spec if accepted;
5. then resume dependent work.
