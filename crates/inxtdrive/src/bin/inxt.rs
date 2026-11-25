//! Command-line companion to the InxtDrive tray app.

use std::io::{BufRead, Write};
use std::time::Duration;

use inxtdrive::config::Config;
use inxtdrive::core::{AppCore, Phase};
use inxtdrive::{ipc, logging, paths, secrets};

const USAGE: &str = "\
inxt - command line for InxtDrive (unofficial Internxt client)

USAGE:
    inxt status                  state of the running InxtDrive app
    inxt login                   sign in through the browser and save the session
    inxt login --password        sign in with email, password and 2FA code
    inxt logout                  remove the saved session from this PC
    inxt mount                   run the drive in this console until Ctrl+C
    inxt pin <path>              always keep a file or folder on this PC
    inxt unpin <path>            let it be removed from the local copy again
    inxt free <path>             remove the local copy now (stays in the cloud)
    inxt paths                   show where settings, data and logs are kept
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime")
}

fn run(args: &[String]) -> anyhow::Result<()> {
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    match cmd {
        "status" => {
            let r = runtime().block_on(ipc::send("status"))?;
            println!("{}", r.strip_prefix("ok ").unwrap_or(&r));
        }
        "quit" => {
            let r = runtime().block_on(ipc::send("quit"))?;
            println!("{r}");
        }
        "pin" | "unpin" | "free" => {
            let path = args.get(1).ok_or_else(|| anyhow::anyhow!("missing path"))?;
            let full = std::path::absolute(path).unwrap_or_else(|_| path.into());
            let r = runtime().block_on(ipc::send(&format!("{cmd} {}", full.display())))?;
            println!("{r}");
        }
        "paths" => {
            println!("settings: {}", paths::config_file().display());
            println!("data:     {}", paths::data_dir().display());
            println!("logs:     {}", paths::logs_dir().display());
        }
        "logout" => {
            secrets::clear_session();
            println!("Saved session removed. Local data for the account is kept until you sign out from the app.");
        }
        "login" => login(args)?,
        "mount" => mount()?,
        _ => print!("{USAGE}"),
    }
    Ok(())
}

fn prompt(label: &str) -> anyhow::Result<String> {
    print!("{label}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

fn login(args: &[String]) -> anyhow::Result<()> {
    logging::init_console("warn");
    let cfg = Config::load();
    let core = AppCore::new(cfg, false)?;
    let client = core.client().clone();
    let creds = if args.iter().any(|a| a == "--password") {
        let email = prompt("Email: ")?;
        let password = rpassword::prompt_password("Password: ")?;
        let first = core.runtime().block_on(client.login_password(&email, &password, None));
        match first {
            Err(e) if e.to_string().contains("two-factor") => {
                let code = prompt("Two-factor code: ")?;
                core.runtime().block_on(client.login_password(&email, &password, Some(&code)))?
            }
            other => other?,
        }
    } else if args.iter().any(|a| a == "--token-stdin") {
        // Development aid: reads a session token and the recovery words from stdin.
        let mut lines = std::io::stdin().lock().lines();
        let token = lines.next().transpose()?.unwrap_or_default();
        let mnemonic = lines.next().transpose()?.unwrap_or_default();
        let sso = inxt_api::auth::SsoResult { token: token.trim().into(), mnemonic: mnemonic.trim().into() };
        core.runtime().block_on(client.complete_sso(sso))?
    } else {
        println!("Opening your browser to sign in…");
        core.runtime().block_on(inxtdrive::login::browser_login(client.clone(), inxtdrive::core::open_target))?
    };
    secrets::save_session(&creds)?;
    println!("Signed in as {}.", creds.email);
    Ok(())
}

fn mount() -> anyhow::Result<()> {
    let cfg = Config::load();
    logging::init_console(&cfg.log_level);
    let core = AppCore::new(cfg, false)?;
    core.start();
    std::thread::sleep(Duration::from_millis(200));
    if matches!(core.snapshot().phase, Phase::SignedOut) {
        println!("Not signed in; opening your browser…");
        core.sign_in();
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let stop = stop.clone();
        core.runtime().spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    let mut last = String::new();
    while !stop.load(std::sync::atomic::Ordering::SeqCst) && !core.quit_requested() {
        let s = core.snapshot();
        let line = match (&s.phase, &s.status) {
            (Phase::Ready, Some(st)) => format!(
                "ready on {} | items {} | uploads pending {} active {} | ops {} | {} | cache {} MB{}",
                s.mount_point.clone().unwrap_or_default(),
                st.items,
                st.pending_uploads,
                st.uploading,
                st.pending_ops,
                if st.online { "online" } else { "offline" },
                st.cache_bytes >> 20,
                st.last_error.as_ref().map(|e| format!(" | last error: {e}")).unwrap_or_default()
            ),
            (Phase::Failed(e), _) => {
                eprintln!("failed: {e}");
                break;
            }
            (p, _) => format!("{p:?}"),
        };
        if line != last {
            println!("{line}");
            last = line;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("Unmounting…");
    core.shutdown();
    Ok(())
}
