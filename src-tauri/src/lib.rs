//! Local Console Hub — backend entry point.
//!
//! Module layout mirrors the boundaries frozen in
//! `docs/MVP_IMPLEMENTATION_SPEC.md` §3. Each module below owns one
//! responsibility; the boundary rules from that spec apply:
//!
//! - UI does not own process truth;
//! - PTY code does not decide logging policy;
//! - Logging code does not decide session lifecycle;
//! - Tray actions call the same Session Core APIs as the main window;
//! - Session Core is the single source of lifecycle truth.
//!
//! As of T09 every module in `docs/MVP_IMPLEMENTATION_SPEC.md` §3 is filled;
//! none is a placeholder any more.

// Public for the same reason as the layers below it, and for one more: the
// end-to-end matrix (`tests/mvp_matrix.rs`) drives the real "add an
// application" operation against a real config file and a real process, and an
// integration test can only reach what the crate exports. Widening the app
// layer is the honest way to let that composition be tested; the alternative —
// a test that rebuilds the operation out of its parts — would pin the test's
// copy of it rather than the one the app runs (#64).
pub mod app;
pub mod config;
// The console this process is attached to, and the borrow of another one's the
// window lookup and the graceful stop both need (D-037). Windows-only and
// crate-private: the console model is Windows', both callers are in Windows
// backends, and nothing above those layers names a console.
#[cfg(windows)]
mod console;
/// The native message box, for the two moments there is no window to draw in
/// (`docs/MVP_IMPLEMENTATION_SPEC.md` §11; #60).
mod dialog;
mod health;
mod instance;
mod ipc;
/// Listening ports on this machine and the process that owns each one (#96).
/// A read of the OS tables, not the configured-port TCP reachability probe
/// (`health`).
pub mod listen;
pub mod logging;
pub mod process;
pub mod pty;
/// CPU and memory of one managed session's own process, and of processes in
/// its tree whose identity still matches (#108). A reading only: it does not
/// look at arbitrary processes, and it does not end one.
pub mod resources;
// Release-manifest guards (T12, #13). Test-only: they pin facts about the
// packaging that no production code reads, and they need `ipc::APP_NAME` and
// `config::APP_DIR_NAME`, which are not public API.
#[cfg(test)]
mod release;
pub mod session;
pub mod shell;
mod tray;
// Pub for the same reason as `process` and `pty`: the standalone-window
// application tests (#66) exercise real windows of a real run, and the window
// selection rule is a product rule worth asserting directly.
pub mod window;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use std::sync::Arc;

    // `manage` lives on the `Manager` trait, not on `App` itself.
    use tauri::Manager;

    // The trait, not the type: what this launch asks for is parsed from the
    // command line, and the request itself is only ever handed on.
    use instance::RequestHandler;

    // What this launch is asking for. The entry decides: the Hub's own shortcut
    // opens it, and the user's PowerShell shortcut (#63) asks for a terminal —
    // possibly in the directory that shortcut names. An argument this build
    // does not understand stops the launch with the reason, rather than being
    // ignored: the user asked for something specific and would otherwise get a
    // window and no explanation.
    let request = match instance::protocol::request_from_env() {
        Ok(request) => request,
        Err(reason) => {
            dialog::report(&reason);
            std::process::exit(instance::EXIT_NOT_DELIVERED);
        }
    };

    // Before anything else — before a registry, a window or a supervisor
    // exists. A process that is not the Hub must never start supervising the
    // user's sessions, because the Hub it is handing off to already is
    // (spec §2: 单实例确认先于建立另一份监管器).
    let primary = match instance::claim() {
        // A Hub is already running: give it the request, report what really
        // happened, and end. Nothing below this line is reached — no window, no
        // registry, no second tray icon.
        Ok(instance::Role::HandOff(hand_off)) => match hand_off.deliver(request) {
            Ok(_) => std::process::exit(instance::EXIT_HANDED_OFF),
            Err(error) => {
                dialog::report(error.message());
                std::process::exit(instance::EXIT_NOT_DELIVERED);
            }
        },
        Ok(instance::Role::Primary(primary)) => primary,
        Err(error) => {
            dialog::report(error.message());
            std::process::exit(instance::EXIT_NOT_DELIVERED);
        }
    };

    // The request cannot be answered until the window exists, and the pipe that
    // carries requests has to be listening before it does: the launches that
    // arrive during this Hub's own start-up are exactly the ones a cold-start
    // race produces (spec §5), and they must wait rather than be lost.
    let hub = Arc::new(app::launch::Hub::new());
    primary.serve(hub.clone());

    tauri::Builder::default()
        // Closing the main window hides it; the tray is the way back and the
        // only way out (D-006, spec §11).
        .on_window_event(tray::on_window_event)
        // The session registry is built once, at startup, and shared by every
        // caller. It is not started from here: registering a session is not a
        // lifecycle operation, and nothing gets a process until something asks
        // (spec §3, "UI does not own process truth" — nor does startup).
        .setup(move |app| {
            let (core, config_report) = app::bootstrap(app.handle().clone());
            app.manage(core);
            // The report is also where the config *path* lives: it is the one
            // place that records which file this process read at startup, and
            // the writer has to reach the same one (#64).
            app.manage(config_report);
            // The window reads the launch request's selection from this same
            // value (`ipc::launch`), whichever process made the request: a
            // hand-off arriving on the pipe and this process's own start-up
            // both land in this one hub.
            app.manage(Arc::clone(&hub));
            // After `manage`, so the tray's first menu is built from the real
            // registry instead of briefly showing an empty one.
            tray::install(app.handle())?;
            // Last, and only now: the window exists, so a request that was
            // waiting on this can be answered. Before this line the honest
            // answer is "the Hub is not ready", which is what `hub` gives.
            hub.publish(app.handle().clone());
            // This launch's own request, through the same path a handed-off one
            // takes. It is the first thing the pipeline does for real, and the
            // reason the pipeline is not dead code until a second click.
            //
            // A request this Hub could not carry out is reported here, because
            // there is no other process to report it: a normal open has nothing
            // to fail at, and a terminal the shortcut asked for can fail on the
            // directory it named or on a machine with no PowerShell. The window
            // is already up at this point (the request above brought it
            // forward), so what the user gets is the Hub plus the reason
            // (story 17, H06).
            let response = hub.handle(request);
            if !response.delivered {
                if let Some(message) = response.message {
                    dialog::report(&message);
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ipc::discovery::pick_application_path,
            ipc::discovery::scan_application_directory,
            ipc::ping,
            ipc::launch::take_launch_focus,
            ipc::session::get_config_report,
            ipc::session::list_sessions,
            ipc::session::list_session_configs,
            ipc::session::create_temporary_terminal,
            ipc::session::remove_session,
            ipc::session::remove_application,
            ipc::session::add_application,
            ipc::session::update_application,
            ipc::session::recommend_display,
            ipc::session::save_terminal_config,
            ipc::session::activate_session,
            ipc::session::resolve_session_open,
            ipc::session::get_session,
            ipc::session::start_session,
            ipc::session::stop_session,
            ipc::session::force_stop_session,
            ipc::session::restart_session,
            ipc::session::open_session_url,
            ipc::session::open_session_cwd,
            ipc::terminal::attach_terminal,
            ipc::terminal::terminal_write,
            ipc::terminal::terminal_resize,
            ipc::logs::get_log_info,
            ipc::logs::get_run_history,
            ipc::logs::save_run_log,
            ipc::logs::set_log_recording,
            ipc::logs::open_log_file,
            ipc::logs::open_log_folder,
            ipc::logs::preview_log_cleanup,
            ipc::logs::cleanup_logs,
            ipc::listen::list_listeners,
            ipc::listen::port_occupancy,
            ipc::confirm_end::confirm_end_occupant,
            ipc::resources::session_resources,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Local Console Hub");
}
