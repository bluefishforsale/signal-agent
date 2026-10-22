// The commands the setup page may call. build.rs turns each into an
// `allow-<name>` permission, and capabilities/default.json grants them to the
// local main window only, so the remote captcha page can call none of them.
const APP_COMMANDS: &[&str] = &[
    "current",
    "send",
    "accounts",
    "add_account",
    "open_captcha",
    "needs_move",
    "move_to_applications",
    "update_offer",
    "install_update",
    "uninstall",
    "quit",
];
