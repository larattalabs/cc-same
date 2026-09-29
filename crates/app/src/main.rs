#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    // The updater checks that a new copy starts and is the version it expects.
    if std::env::args_os().nth(1).is_some_and(|a| a == cc_same_app::update::PRINT_VERSION) {
        println!("{} {}", cc_same_app::APP_NAME, cc_same_app::update::VERSION);
        return;
    }
    // The background agent is this same binary started with `watch`: no window, no UI.
    if std::env::args_os().skip(1).any(|a| a == "watch") {
        cc_same_core::watch::run_agent(std::env::args_os().skip(1));
        return;
    }
    cc_same_app::run();
}
