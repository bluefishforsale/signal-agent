//! Setup in a plain terminal, for Linux and headless boxes reached over SSH:
//! the same states the desktop window shows, drawn as text, with the link QR
//! code in block characters and every answer typed in.

use std::{
    io::{self, BufRead, Write},
    path::Path,
};

use anyhow::{Context, bail};
use async_trait::async_trait;
use qrcode::{QrCode, render::unicode::Dense1x2};
use relay_core::prelude::*;
use relay_secrets::StoreKey;
use relay_setup::{
    claude::{ClaudeClient, Clients, ServerEntry},
    driver::{Driver, HistoryImport, LoginItem, OpenProvisioner, Services},
    prelude::*,
};
use relay_signal::{PresageProvisioner, Provisioner, Servers};

/// Runs setup for `profile` on this terminal until it is done or the user quits.
pub async fn run(profile: ProfileName) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("unable to locate this executable")?;
    let server = ServerEntry::for_profile(&profile, &exe, ["mcp", "--profile", &profile])?;
    let services = Services {
        signal: Box::new(Presage),
        secrets: relay_secrets::platform_store()?,
        history: Box::new(NoHistoryYet),
        login: Box::new(AgentAtLogin),
        clients: Clients::system(),
    };
    let paths = ProfilePaths::for_profile(&profile)?;
    let start = SetupState::start(&computer_name());
    let mut driver = Driver::new(start, profile, paths, server, services);
    drive(&mut driver, &mut terminal_input()?, &mut io::stdout()).await
}

/// Shows each state and feeds the user's answers to `driver` until done.
async fn drive(
    driver: &mut Driver,
    input: &mut impl BufRead,
    out: &mut (impl Write + Send),
) -> anyhow::Result<()> {
    if let Some(caveat) = driver.key_caveat() {
        writeln!(out, "Note: {caveat}\n")?;
    }
    render(driver.state(), out)?;
    let mut offered_restart = false;
    loop {
        let event = match driver.state() {
            SetupState::Done { clients } => {
                let desktop = clients
                    .iter()
                    .any(|c| c.client == ClaudeClient::Desktop && c.result.is_ok());
                if !desktop || offered_restart {
                    return Ok(());
                }
                offered_restart = true;
                if !ask_yes(
                    input,
                    out,
                    "Restart Claude Desktop now so it picks up Signal?",
                )? {
                    return Ok(());
                }
                SetupEvent::RestartDesktop
            }
            state => {
                // A failed restart retries back to done, which offers it again.
                offered_restart &= !matches!(state, SetupState::Failed { .. });
                match ask(state, input, out)? {
                    Some(event) => event,
                    None => return Ok(()),
                }
            }
        };
        let mut failed = None;
        driver
            .dispatch(event, &mut |state| {
                if let Err(e) = render(state, out) {
                    failed.get_or_insert(e);
                }
            })
            .await;
        if let Some(e) = failed {
            return Err(e).context("unable to write to the terminal");
        }
    }
}

/// Draws what the user should see on entering `state`.
fn render(state: &SetupState, out: &mut impl Write) -> io::Result<()> {
    match state {
        SetupState::ChooseAccount { device_name } => writeln!(
            out,
            "MCP Relay for Signal lets Claude read, search and send your Signal messages.\n\
             This computer will appear in Signal as \"{device_name}\".\n\n\
             How should Claude reach Signal?\n  \
             1) Link to my Signal account by scanning a QR code with my phone (recommended)\n  \
             2) Give Claude its own phone number"
        ),
        SetupState::Linking { uri: None, .. } => {
            writeln!(out, "\nAsking Signal for a link code...")
        }
        SetupState::Linking {
            uri: Some(uri),
            device_name,
        } => {
            // Clear the screen so each refreshed code replaces the expired one.
            write!(out, "\x1b[2J\x1b[H")?;
            writeln!(
                out,
                "On your phone, open Signal, go to Settings > Linked devices > Link new device,\n\
                 and scan this code. It refreshes on its own until you do.\n\n{}\n\
                 It will be listed as \"{device_name}\".",
                qr(uri)
            )
        }
        SetupState::EnterNumber => writeln!(
            out,
            "\nThe number must be able to receive an SMS or call, and must not already be\n\
             used by another Signal app."
        ),
        SetupState::RequestingCode { number } => {
            writeln!(out, "Asking Signal to send a code to {number}...")
        }
        SetupState::SolveCaptcha { .. } => writeln!(
            out,
            "\nSignal wants a captcha first. On any computer, open\n  {}\n\
             solve it, then copy the \"Open Signal\" link (it starts with signalcaptcha://).",
            relay_setup::CAPTCHA_PAGE
        ),
        SetupState::EnterCode { number } => {
            writeln!(out, "Signal sent a verification code to {number}.")
        }
        SetupState::VerifyingCode { .. } => writeln!(out, "Checking the code..."),
        SetupState::ChooseAccess { kind } => writeln!(
            out,
            "\n{}. One question about what Claude may do:\n\
             Yes lets Claude send, react and delete as you; no lets it only read and search.",
            match kind {
                ProfileKind::Linked => "Linked",
                ProfileKind::Primary => "Registered",
            }
        ),
        SetupState::ImportingHistory {
            imported: 0,
            total: None,
            ..
        } => writeln!(
            out,
            "\nImporting your recent messages. Signal sends about the last 45 days."
        ),
        SetupState::ImportingHistory {
            imported, total, ..
        } => match total {
            Some(total) => writeln!(out, "  {imported} of {total} messages"),
            None => writeln!(out, "  {imported} messages"),
        },
        SetupState::ConnectingClients { .. } => {
            writeln!(
                out,
                "\nStarting the background agent and adding it to Claude..."
            )
        }
        SetupState::Done { clients } => render_done(clients, out),
        SetupState::Failed { message, .. } => writeln!(out, "\nThat didn't work: {message}"),
    }
}

fn render_done(clients: &[ClientOutcome], out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "\nSetup is done.")?;
    if clients.is_empty() {
        return writeln!(
            out,
            "No Claude app was found. Install Claude Code; the agent adds itself to it the\n\
             next time it starts."
        );
    }
    for outcome in clients {
        match &outcome.result {
            Ok(()) => writeln!(out, "  {}: connected", outcome.client)?,
            Err(e) => writeln!(out, "  {}: not connected: {e}", outcome.client)?,
        }
    }
    Ok(())
}

/// `uri` as a QR code in Unicode half blocks, two modules per character cell.
// NOTE: dark modules are drawn as blanks and light ones as solid blocks, which
// shows the right way round on the usual dark terminal background. Drawn the
// other way, phones see an inverted code and most refuse to scan it.
fn qr(uri: &str) -> String {
    match QrCode::new(uri) {
        Ok(code) => code
            .render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .build(),
        // A link URI is far below QR capacity; say so rather than draw nothing.
        Err(e) => format!("(unable to draw the QR code: {e}; link with this URI: {uri})"),
    }
}

/// Prompts for `state`'s answer until one parses. `None` means the user quit.
fn ask(
    state: &SetupState,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> anyhow::Result<Option<SetupEvent>> {
    let event = match state {
        SetupState::ChooseAccount { .. } => {
            let choice = prompt(input, out, "Choice [1]: ", parse_account_choice)?;
            SetupEvent::AccountChosen { choice }
        }
        SetupState::EnterNumber => {
            let number = prompt(
                input,
                out,
                "Phone number, with country code (e.g. +15555550100): ",
                parse_number,
            )?;
            SetupEvent::NumberEntered { number }
        }
        SetupState::SolveCaptcha { .. } => {
            let token = prompt(
                input,
                out,
                "Paste the link (or its token) here: ",
                parse_captcha,
            )?;
            SetupEvent::CaptchaSolved { token }
        }
        SetupState::EnterCode { .. } => {
            let code = prompt(input, out, "Code: ", parse_code)?;
            SetupEvent::CodeEntered { code }
        }
        SetupState::ChooseAccess { .. } => {
            let access = if ask_yes(input, out, "Let Claude send messages as you?")? {
                AccessMode::Full
            } else {
                AccessMode::ReadOnly
            };
            SetupEvent::AccessChosen { access }
        }
        SetupState::Failed { .. } => {
            let retry = prompt(
                input,
                out,
                "Press Enter to try again, or type q to quit: ",
                parse_retry,
            )?;
            if !retry {
                return Ok(None);
            }
            SetupEvent::Retry
        }
        state => bail!("setup stopped while it was busy: {state:?}"),
    };
    Ok(Some(event))
}

/// A yes-or-no question where Enter means yes.
fn ask_yes(input: &mut impl BufRead, out: &mut impl Write, question: &str) -> anyhow::Result<bool> {
    prompt(input, out, &format!("{question} [Y/n]: "), parse_yes)
}

/// Asks `question` until `parse` accepts the answer, showing why it didn't.
fn prompt<T>(
    input: &mut impl BufRead,
    out: &mut impl Write,
    question: &str,
    parse: impl Fn(&str) -> Result<T, &'static str>,
) -> anyhow::Result<T> {
    loop {
        write!(out, "{question}")?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            bail!("the terminal closed before setup finished");
        }
        match parse(line.trim()) {
            Ok(answer) => return Ok(answer),
            Err(hint) => writeln!(out, "{hint}")?,
        }
    }
}

fn parse_account_choice(answer: &str) -> Result<AccountChoice, &'static str> {
    match answer {
        "" | "1" => Ok(AccountChoice::LinkExisting),
        "2" => Ok(AccountChoice::OwnNumber),
        _ => Err("Type 1 or 2, or press Enter for 1."),
    }
}

fn parse_yes(answer: &str) -> Result<bool, &'static str> {
    match answer.to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => Err("Type y or n, or press Enter for yes."),
    }
}

fn parse_retry(answer: &str) -> Result<bool, &'static str> {
    match answer.to_ascii_lowercase().as_str() {
        "" => Ok(true),
        "q" | "quit" => Ok(false),
        _ => Err("Press Enter to try again, or type q to quit."),
    }
}

/// A number in international form, with the spaces, dashes and brackets people
/// type removed.
fn parse_number(answer: &str) -> Result<String, &'static str> {
    let number: String = answer
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
        .collect();
    let digits = number.strip_prefix('+').unwrap_or_default();
    if digits.len() < 7 || digits.len() > 15 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err("Start with + and the country code, e.g. +15555550100.");
    }
    Ok(number)
}

/// The token Signal needs, from a pasted `signalcaptcha://` link or the bare
/// token people copy out of it.
fn parse_captcha(answer: &str) -> Result<String, &'static str> {
    relay_setup::pasted_captcha_token(answer)
        .ok_or("That isn't a whole captcha link or token; copy the link again.")
}

/// The verification code, accepting the dash Signal's SMS shows in it.
fn parse_code(answer: &str) -> Result<String, &'static str> {
    let code: String = answer.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err("The code is 6 digits.");
    }
    Ok(code)
}

/// Where answers come from: the terminal itself on Unix, even when stdin is
/// the pipe from `curl ... | sh`, and stdin elsewhere.
fn terminal_input() -> anyhow::Result<Box<dyn BufRead>> {
    #[cfg(unix)]
    {
        let tty = std::fs::File::open("/dev/tty")
            .context("setup needs a terminal to ask questions; run it from an interactive shell")?;
        Ok(Box::new(io::BufReader::new(tty)))
    }
    #[cfg(not(unix))]
    {
        Ok(Box::new(io::stdin().lock()))
    }
}

/// This machine's name, for the device name the phone lists.
fn computer_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this computer".to_owned())
}

/// Creates the account in the profile's presage store on Signal's production
/// service.
struct Presage;

#[async_trait]
impl OpenProvisioner for Presage {
    async fn open(&self, path: &Path, key: &StoreKey) -> anyhow::Result<Box<dyn Provisioner>> {
        let provisioner = PresageProvisioner::open(path, key.as_bytes(), Servers::Production)
            .await
            .with_context(|| format!("unable to open the Signal store \"{}\"", path.display()))?;
        Ok(Box::new(provisioner))
    }
}

// Integration seam: link-and-sync import (lane L7) needs the archive and key
// from linking, which the Provisioner contract doesn't carry yet (issue #34).
// Until then a linked device starts with an empty history.
struct NoHistoryYet;

#[async_trait]
impl HistoryImport for NoHistoryYet {
    async fn import(&self, _: &mut (dyn FnMut(u64, Option<u64>) + Send)) -> anyhow::Result<()> {
        Ok(())
    }
}

struct AgentAtLogin;

impl LoginItem for AgentAtLogin {
    fn register(&self, profile: &ProfileName) -> anyhow::Result<()> {
        relay_agent::login::register(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(state: &SetupState) -> String {
        let mut out = Vec::new();
        render(state, &mut out).expect("render");
        String::from_utf8(out).expect("utf-8")
    }

    /// Reads the QR code back out of rendered text as rows of modules, `true`
    /// for dark, undoing the half-block packing.
    fn scan(text: &str) -> Vec<Vec<bool>> {
        let mut rows = Vec::new();
        let qr_line = |l: &&str| l.starts_with(['█', '▀']) && l.chars().all(|c| "█▀▄ ".contains(c));
        for line in text.lines().filter(qr_line) {
            let (mut top, mut bottom) = (Vec::new(), Vec::new());
            for c in line.chars() {
                let (t, b) = match c {
                    '█' => (false, false),
                    '▀' => (false, true),
                    '▄' => (true, false),
                    ' ' => (true, true),
                    other => panic!("unexpected {other:?} in the QR code"),
                };
                top.push(t);
                bottom.push(b);
            }
            rows.push(top);
            rows.push(bottom);
        }
        rows
    }

    /// The code drawn must be the URI's own QR code, dark modules dark, inside
    /// a light quiet zone, or a phone either can't scan it or links elsewhere.
    #[test]
    fn the_link_qr_scans_as_the_uri_with_a_quiet_zone() {
        let uri = "sgnl://linkdevice?uuid=abc&pub_key=xyz";
        let code = QrCode::new(uri).expect("qr");
        let width = code.width();
        let modules = code.to_colors();
        let text = rendered(&SetupState::Linking {
            device_name: "Claude (box)".into(),
            uri: Some(uri.into()),
        });
        let rows = scan(&text);
        let margin = (rows.first().map_or(0, Vec::len) - width) / 2;
        assert!(margin >= 2, "quiet zone of {margin} modules is too thin");
        // The last character row's lower half is padding when the height is odd.
        for (y, row) in rows.iter().take(width + 2 * margin).enumerate() {
            for (x, &dark) in row.iter().enumerate() {
                let inside =
                    (margin..margin + width).contains(&x) && (margin..margin + width).contains(&y);
                let want =
                    inside && modules[(y - margin) * width + (x - margin)] == qrcode::Color::Dark;
                assert_eq!(dark, want, "module ({x}, {y}) is wrong");
            }
        }
        assert!(
            rows.len() >= width + 2 * margin,
            "QR code cut short at {} rows:\n{text}",
            rows.len()
        );
    }

    /// A refreshed URI must replace the expired code on screen, not scroll a
    /// second code below it that the phone might scan first.
    #[test]
    fn a_new_link_uri_clears_the_screen_and_draws_the_new_code() {
        let state = |uri: &str| SetupState::Linking {
            device_name: "Claude (box)".into(),
            uri: Some(uri.into()),
        };
        let (a, b) = (rendered(&state("sgnl://a")), rendered(&state("sgnl://b")));
        assert!(a.starts_with("\x1b[2J\x1b[H"), "screen not cleared: {a:?}");
        assert_ne!(scan(&a), scan(&b), "the refreshed code was not redrawn");
    }

    /// Enter takes the recommended answer at every question with one: linking,
    /// and full access ("Yes preselected").
    #[test]
    fn enter_takes_the_recommended_answers() {
        assert_eq!(parse_account_choice(""), Ok(AccountChoice::LinkExisting));
        assert_eq!(parse_account_choice("2"), Ok(AccountChoice::OwnNumber));
        assert_eq!(parse_yes(""), Ok(true));
        assert_eq!(parse_yes("N"), Ok(false));
        assert_eq!(parse_retry(""), Ok(true));
        assert_eq!(parse_retry("q"), Ok(false));
        for typo in ["3", "maybe"] {
            assert!(parse_account_choice(typo).is_err(), "{typo:?} accepted");
            assert!(parse_yes(typo).is_err(), "{typo:?} accepted");
        }
    }

    /// Numbers come in however people write them, but Signal needs the bare
    /// international form; anything else would cost an SMS request to reject.
    #[test]
    fn phone_numbers_are_normalized_or_refused_before_reaching_signal() {
        assert_eq!(parse_number("+1 (555) 555-0100"), Ok("+15555550100".into()));
        assert_eq!(parse_number("+44 7700.900123"), Ok("+447700900123".into()));
        for bad in ["5555550100", "+1", "+1555abc0100", "+1234567890123456", ""] {
            assert!(parse_number(bad).is_err(), "{bad:?} accepted");
        }
    }

    /// The pasted captcha link yields just its token; a partial paste is
    /// caught here instead of failing at Signal.
    #[test]
    fn a_pasted_captcha_link_yields_its_token() {
        assert_eq!(
            parse_captcha("signalcaptcha://signal-hcaptcha.5fad.registration.P1_eyJ"),
            Ok("signal-hcaptcha.5fad.registration.P1_eyJ".into())
        );
        for bad in ["signalcaptcha://", "signalcaptcha://signal-hcaptcha.5f ad"] {
            assert!(parse_captcha(bad).is_err(), "{bad:?} accepted");
        }
    }

    /// People often copy just the token out of the link; that works too, but
    /// not any stray text that isn't a token.
    #[test]
    fn a_pasted_bare_captcha_token_is_accepted() {
        assert_eq!(
            parse_captcha("signal-recaptcha-v2.6LfB.registration.03AF"),
            Ok("signal-recaptcha-v2.6LfB.registration.03AF".into())
        );
        for bad in ["+15555550100", "signal-hcaptcha.5f ad", "hcaptcha.5fad"] {
            assert!(parse_captcha(bad).is_err(), "{bad:?} accepted");
        }
    }

    /// The SMS shows the code as 123-456; both forms verify, and a short code
    /// is refused before it burns a verification attempt.
    #[test]
    fn verification_codes_accept_the_sms_dash() {
        assert_eq!(parse_code("123-456"), Ok("123456".into()));
        assert_eq!(parse_code(" 123456 "), Ok("123456".into()));
        assert!(parse_code("12345").is_err());
        assert!(parse_code("12345a").is_err());
    }

    /// A wrong answer re-asks with a hint rather than ending setup.
    #[test]
    fn a_bad_answer_is_asked_again_with_a_hint() {
        let mut input = io::Cursor::new("+1\n+15555550100\n");
        let mut out = Vec::new();
        let event = ask(&SetupState::EnterNumber, &mut input, &mut out).expect("ask");
        assert_eq!(
            event,
            Some(SetupEvent::NumberEntered {
                number: "+15555550100".into()
            })
        );
        let out = String::from_utf8(out).expect("utf-8");
        assert_eq!(out.matches("Phone number").count(), 2, "{out}");
        assert!(out.contains("country code, e.g."), "no hint: {out}");
    }

    /// A closed terminal must end setup with an error, not spin re-asking.
    #[test]
    fn a_closed_terminal_ends_setup_with_an_error() {
        let mut input = io::Cursor::new("");
        let result = ask(&SetupState::EnterNumber, &mut input, &mut Vec::new());
        assert!(result.is_err(), "{result:?}");
    }

    /// Typing q at a failure quits instead of retrying.
    #[test]
    fn q_at_a_failure_quits() {
        let failed = SetupState::Failed {
            message: "down".into(),
            retry: Box::new(SetupState::EnterNumber),
        };
        let mut input = io::Cursor::new("q\n");
        let event = ask(&failed, &mut input, &mut Vec::new()).expect("ask");
        assert_eq!(event, None);
    }

    struct Phone;

    #[async_trait]
    impl Provisioner for Phone {
        fn link(
            &self,
            _: String,
        ) -> futures::stream::BoxStream<'static, anyhow::Result<relay_signal::LinkEvent>> {
            use relay_signal::{Account, LinkEvent};
            let linked = LinkEvent::Linked {
                account: Account {
                    service_id: ServiceId::new(uuid::Uuid::from_u128(1)),
                    phone_number: None,
                    kind: ProfileKind::Linked,
                    device_name: "Claude (box)".into(),
                },
            };
            let uri = LinkEvent::Uri {
                uri: "sgnl://linkdevice?uuid=1".into(),
            };
            Box::pin(futures::stream::iter([Ok(uri), Ok(linked)]))
        }
        async fn request_code(
            &self,
            _: &str,
            _: Option<&str>,
            _: bool,
        ) -> anyhow::Result<relay_signal::RegistrationStep> {
            bail!("not scripted")
        }
        async fn verify_code(&self, _: &str) -> anyhow::Result<relay_signal::RegistrationStep> {
            bail!("not scripted")
        }
    }

    #[async_trait]
    impl OpenProvisioner for Phone {
        async fn open(&self, _: &Path, _: &StoreKey) -> anyhow::Result<Box<dyn Provisioner>> {
            Ok(Box::new(Phone))
        }
    }

    /// The headless Linux fallback: a key file, with its caveat.
    #[derive(Default)]
    struct KeyFile(relay_secrets::MemorySecretStore);

    impl relay_secrets::SecretStore for KeyFile {
        fn get(&self, p: &ProfileName) -> anyhow::Result<Option<StoreKey>> {
            self.0.get(p)
        }
        fn set(&self, p: &ProfileName, k: &StoreKey) -> anyhow::Result<()> {
            self.0.set(p, k)
        }
        fn delete(&self, p: &ProfileName) -> anyhow::Result<()> {
            self.0.delete(p)
        }
        fn caveat(&self) -> Option<&'static str> {
            Some("no keyring here, so the key is in a file only you can read")
        }
    }

    struct Agent;

    impl LoginItem for Agent {
        fn register(&self, _: &ProfileName) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct NoClaude;

    impl relay_setup::claude::Runner for NoClaude {
        fn run(&self, _: &Path, _: &[String]) -> anyhow::Result<relay_setup::claude::RunOutput> {
            bail!("no Claude on this box")
        }
    }

    /// Runs setup in `dir` against fakes, answering from `answers`.
    async fn run_scripted(dir: &Path, clients: Clients, answers: &str) -> (Driver, String) {
        let profile = ProfileName::default_profile();
        let server =
            ServerEntry::for_profile(&profile, &dir.join("signal-agent"), ["mcp"]).expect("entry");
        let services = Services {
            signal: Box::new(Phone),
            secrets: Box::new(KeyFile::default()),
            history: Box::new(NoHistoryYet),
            login: Box::new(Agent),
            clients,
        };
        let paths = ProfilePaths::under(dir.to_path_buf(), &profile);
        let mut driver = Driver::new(SetupState::start("box"), profile, paths, server, services);
        let mut input = io::Cursor::new(answers.to_owned());
        let mut out = Vec::new();
        drive(&mut driver, &mut input, &mut out)
            .await
            .expect("setup");
        (driver, String::from_utf8(out).expect("utf-8"))
    }

    /// A headless box over SSH, start to finish from typed answers: the key
    /// file caveat comes before anything is stored, the QR code is drawn, the
    /// access question defaults to yes, and done says no Claude app was found.
    #[tokio::test]
    async fn a_headless_link_runs_from_typed_answers_to_done() {
        let dir = tempfile::tempdir().expect("tempdir");
        let clients = Clients::new(
            relay_setup::claude::Platform::Linux,
            Box::new(NoClaude),
            None,
            None,
        );
        let (driver, out) = run_scripted(dir.path(), clients, "\n\n").await;
        assert!(
            out.starts_with("Note: no keyring here"),
            "caveat not shown first: {out}"
        );
        assert!(!scan(&out).is_empty(), "no QR code drawn: {out}");
        assert!(
            out.contains("Let Claude send messages as you? [Y/n]"),
            "{out}"
        );
        assert!(out.contains("No Claude app was found"), "{out}");
        assert!(
            matches!(driver.state(), SetupState::Done { .. }),
            "{:?}",
            driver.state()
        );
        let paths = ProfilePaths::under(dir.path().to_path_buf(), &ProfileName::default_profile());
        assert_eq!(paths.load_access().expect("access saved"), AccessMode::Full);
    }

    /// Claude Desktop on macOS: osascript quits it, and fails the first time.
    #[derive(Default)]
    struct FlakyDesktop(std::sync::Mutex<u32>);

    impl relay_setup::claude::Runner for FlakyDesktop {
        fn run(
            &self,
            program: &Path,
            _: &[String],
        ) -> anyhow::Result<relay_setup::claude::RunOutput> {
            let mut quits = self.0.lock().expect("quits");
            if program == Path::new("osascript") {
                *quits += 1;
            }
            Ok(relay_setup::claude::RunOutput {
                success: *quits != 1,
                ..Default::default()
            })
        }
    }

    /// A failed Claude Desktop restart, retried, must offer the restart again
    /// rather than end setup with Desktop still running its old config.
    #[tokio::test]
    async fn a_failed_desktop_restart_is_offered_again_after_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let desktop = dir.path().join("Claude");
        std::fs::create_dir_all(&desktop).expect("desktop dir");
        let runner = std::sync::Arc::new(FlakyDesktop::default());
        let clients = Clients::new(
            relay_setup::claude::Platform::MacOs,
            Box::new(Shared(runner.clone())),
            None,
            Some(desktop),
        );
        let (_, out) = run_scripted(dir.path(), clients, "\n\n\n\n\n").await;
        assert_eq!(
            out.matches("Restart Claude Desktop now").count(),
            2,
            "{out}"
        );
        assert_eq!(*runner.0.lock().expect("quits"), 2, "{out}");
    }

    struct Shared(std::sync::Arc<FlakyDesktop>);

    impl relay_setup::claude::Runner for Shared {
        fn run(
            &self,
            program: &Path,
            args: &[String],
        ) -> anyhow::Result<relay_setup::claude::RunOutput> {
            self.0.run(program, args)
        }
    }
}
