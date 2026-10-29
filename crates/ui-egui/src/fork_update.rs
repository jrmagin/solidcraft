//! Fork (jrmagin): updates for this self-built copy. The release updater ([`crate::update`]) never
//! checks a build from source, so a copy built from a git checkout (`<repo>/target/release/`) gets
//! its own "Nach Updates suchen" pill in the application bar instead. It fetches the original
//! repository's `main` (the `upstream` remote), lists what is new, and on confirmation closes
//! SolidCraft and runs a script in a console window: merge `upstream/main`, build, push to the
//! fork (`origin`), start the new build. A failed step leaves the previous build in place and
//! starts it again.
//!
//! The texts are German only: this module exists only in the fork.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};

use crate::SolveApp;
use crate::theme::Tokens;
use crate::ui_kit;

/// The original repository's GitHub API, for the release notes of a new version number.
const RELEASE_API: &str = "https://api.github.com/repos/tangency-labs/solidcraft/releases/tags/v";
/// At most this many changes are listed; the rest are counted.
const MAX_LISTED: usize = 60;
/// Room the pill's long label leaves for the document tabs (as the release updater's).
const TAB_ROOM: f32 = 300.0;

/// What a check found.
#[derive(Clone)]
struct Found {
    /// Commits on `upstream/main` that this checkout lacks.
    behind: usize,
    /// Their subjects (first parent, generated-file commits left out), newest first.
    subjects: Vec<String>,
    /// The version number on `upstream/main`.
    upstream_version: Option<String>,
    /// The release notes of that version, when it differs from this build's.
    notes: Option<String>,
    /// Tracked files changed but not committed: the merge would refuse or mix them in.
    dirty: Vec<String>,
    /// Claude Code's German translation and rating of `subjects` (None: nothing to rate).
    assessment: Option<Result<Assessment, String>>,
}

/// The changes in German, each rated, and an overall verdict.
#[derive(Clone)]
struct Assessment {
    verdict: String,
    /// (rating, text): "wichtig", "nützlich" or "Kleinkram", most important first.
    items: Vec<(String, String)>,
}

enum Phase {
    Idle,
    Checking(Receiver<Result<Found, String>>),
    Done(Result<Found, String>),
}

struct State {
    open: bool,
    phase: Phase,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State { open: false, phase: Phase::Idle }))
}

/// The git checkout this executable was built in: `<repo>/target/release/solidcraft.exe`, with
/// `.git` and `Cargo.toml` at `<repo>`. None for an installed copy.
fn repo() -> Option<&'static Path> {
    static REPO: OnceLock<Option<PathBuf>> = OnceLock::new();
    REPO.get_or_init(|| {
        if !cfg!(windows) || cfg!(target_arch = "wasm32") {
            return None;
        }
        let exe = std::env::current_exe().ok()?;
        let target = exe.parent()?.parent()?;
        let root = target.parent()?;
        (target.file_name()? == "target" && root.join(".git").exists() && root.join("Cargo.toml").exists()).then(|| root.to_path_buf())
    })
    .as_deref()
}

/// `git -C <repo> <args>`, without a console window: its standard output, or why it failed.
fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let out = command.output().map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// The workspace version in a `Cargo.toml`: its first `version = "…"` line.
fn cargo_version(toml: &str) -> Option<String> {
    toml.lines().find_map(|l| l.trim().strip_prefix("version").map(str::trim)?.strip_prefix('=').map(|v| v.trim().trim_matches('"').to_string()))
}

/// The release notes of `version` from GitHub, when the host can fetch and the release exists.
fn release_notes(http: Option<&crate::update::HttpGet>, version: &str) -> Option<String> {
    if !version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')) {
        return None;
    }
    let body = (http?)(&format!("{RELEASE_API}{version}")).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&body).ok()?;
    let notes = json.get("body")?.as_str()?.trim();
    (!notes.is_empty()).then(|| notes.chars().take(64 * 1024).collect())
}

/// Fetch `upstream` and compare it with this checkout (runs on a background thread).
fn check(repo: &Path, http: Option<&crate::update::HttpGet>) -> Result<Found, String> {
    git(repo, &["fetch", "upstream", "--quiet"])?;
    let behind = git(repo, &["rev-list", "--count", "HEAD..upstream/main"])?.trim().parse::<usize>().map_err(|e| e.to_string())?;
    let subjects: Vec<String> = git(repo, &["log", "--first-parent", "--format=%s", "HEAD..upstream/main"])?
        .lines()
        .filter(|s| !s.contains("[skip ci]"))
        .map(str::to_string)
        .collect();
    let dirty = git(repo, &["status", "--porcelain", "--untracked-files=no"])?.lines().map(|l| l.trim().to_string()).collect();
    let upstream_version = git(repo, &["show", "upstream/main:Cargo.toml"]).ok().as_deref().and_then(cargo_version);
    let notes = upstream_version.as_deref().filter(|v| *v != crate::update::current_version()).and_then(|v| release_notes(http, v));
    let assessment = (!subjects.is_empty()).then(|| assess(&subjects));
    Ok(Found { behind, subjects, upstream_version, notes, dirty, assessment })
}

/// Claude Code as the Claude desktop app ships it: the newest
/// `%APPDATA%\Claude\claude-code\<version>\<build>\claude.exe`.
fn claude_exe() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("APPDATA")?).join("Claude").join("claude-code");
    let newest = std::fs::read_dir(&root)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| Some((semver::Version::parse(&e.file_name().to_string_lossy()).ok()?, e.path())))
        .max_by(|a, b| a.0.cmp(&b.0))?
        .1;
    std::fs::read_dir(newest).ok()?.filter_map(|e| e.ok()).map(|e| e.path().join("claude.exe")).find(|p| p.exists())
}

/// How long Claude Code may take to answer before it is stopped.
const CLAUDE_TIMEOUT_S: u64 = 180;

/// The answer's shape, as Claude Code's `--json-schema` checks it.
const ASSESSMENT_SCHEMA: &str = r#"{"type":"object","properties":{"urteil":{"type":"string"},"aenderungen":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"stufe":{"type":"string","enum":["wichtig","nützlich","Kleinkram"]}},"required":["text","stufe"]}}},"required":["urteil","aenderungen"]}"#;

/// Have Claude Code (headless, no tools, no settings or MCP servers, nothing kept) translate the
/// change titles into German and rate them for someone who designs parts for a 3D printer.
fn assess(subjects: &[String]) -> Result<Assessment, String> {
    use std::io::{Read as _, Write as _};
    let exe = claude_exe().ok_or("Claude Code wurde nicht gefunden (es kommt mit der Claude-App).")?;
    let list: Vec<&str> = subjects.iter().take(MAX_LISTED).map(String::as_str).collect();
    let prompt = format!(
        "Du bewertest Änderungen an SolidCraft, einem 3D-CAD-Programm. Der Nutzer konstruiert damit Teile für seinen 3D-Drucker und spricht Deutsch. \
         Übersetze jede der folgenden Änderungen (eine je Zeile, englische Commit-Titel) in einen kurzen, verständlichen deutschen Satz ohne Fachjargon und ohne Ticketnummern, \
         und ordne sie ein: wichtig (behebt Fehler oder bringt etwas, das man beim Konstruieren merkt), nützlich, oder Kleinkram (Übersetzungen in andere Sprachen, interne Technik). \
         Schreib dazu ein Urteil in 2 bis 3 Sätzen: Lohnt sich das Update jetzt? Gleiche Reihenfolge wie die Eingabe.\n\n{}",
        list.join("\n")
    );
    let work = std::env::temp_dir().join("solidcraft-claude");
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let mut command = Command::new(&exe);
    command
        .args(["-p", "--output-format", "json", "--json-schema", ASSESSMENT_SCHEMA, "--tools", "", "--setting-sources", ""])
        .args(["--strict-mcp-config", "--no-session-persistence"])
        .current_dir(&work)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(|e| format!("Claude Code: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).map_err(|e| e.to_string())?;
    }
    // Drain both pipes while waiting, so a long answer can't block Claude Code on a full pipe.
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>));
    let started = std::time::Instant::now();
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if started.elapsed().as_secs() > CLAUDE_TIMEOUT_S {
            let _ = child.kill();
            return Err("Claude Code hat nicht rechtzeitig geantwortet.".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let _ = stderr.join();
    parse_assessment(&stdout.join().map_err(|_| "Claude Code: Antwort nicht lesbar.".to_string())?)
}

/// Claude Code's `--output-format json` answer: `structured_output` holds the schema's object; an
/// error (not logged in, …) is in `result`.
fn parse_assessment(stdout: &[u8]) -> Result<Assessment, String> {
    let answer: serde_json::Value = serde_json::from_slice(stdout).map_err(|_| "Claude Code gab keine lesbare Antwort.".to_string())?;
    if answer.get("is_error").and_then(serde_json::Value::as_bool).unwrap_or(false) {
        let why = answer.get("result").and_then(serde_json::Value::as_str).unwrap_or("unbekannter Fehler");
        return Err(if why.contains("Not logged in") {
            "Claude Code ist nicht angemeldet. Doppelklick auf „Claude Code anmelden“ auf dem Desktop, dann /login.".into()
        } else {
            format!("Claude Code: {why}")
        });
    }
    let data = answer.get("structured_output").cloned().unwrap_or(serde_json::Value::Null);
    let verdict = data.get("urteil").and_then(serde_json::Value::as_str).ok_or("Claude Code lieferte kein Urteil.")?.trim().to_string();
    let rank = |s: &str| match s {
        "wichtig" => 0,
        "nützlich" => 1,
        _ => 2,
    };
    let mut items: Vec<(String, String)> = data
        .get("aenderungen")
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(|c| Some((c.get("stufe")?.as_str()?.to_string(), c.get("text")?.as_str()?.trim().to_string()))).collect())
        .unwrap_or_default();
    items.sort_by_key(|(stufe, _)| rank(stufe));
    Ok(Assessment { verdict, items })
}

fn start_check(app: &SolveApp, s: &mut State) {
    let Some(repo) = repo() else { return };
    let http = app.services.http_get.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(check(repo, http.as_ref()));
    });
    s.phase = Phase::Checking(rx);
}

/// The application bar's "Nach Updates suchen" pill, at the right of `span`, and its window.
/// None (nothing drawn) for a copy not built in a git checkout.
pub fn button(app: &mut SolveApp, ui: &mut egui::Ui, span: egui::Rect, labeled: bool) -> Option<egui::Rect> {
    repo()?;
    let (rect, response) =
        crate::toolbar::offer_pill(ui, span, labeled, &["Nach Updates suchen", "Updates"], TAB_ROOM, "update", "titlebar:fork_update")?;
    let clicked = response.on_hover_text("Im Original nach neuen Änderungen suchen und die eigene Fassung damit neu bauen").clicked();
    if let Ok(mut s) = state().lock() {
        if clicked {
            s.open = !s.open;
            if s.open && !matches!(s.phase, Phase::Checking(_)) {
                start_check(app, &mut s);
            }
        }
        if s.open {
            window(app, ui.ctx(), &mut s);
        }
    }
    Some(rect)
}

fn window(app: &mut SolveApp, ctx: &egui::Context, s: &mut State) {
    if let Phase::Checking(rx) = &s.phase {
        match rx.try_recv() {
            Ok(result) => s.phase = Phase::Done(result),
            Err(TryRecvError::Disconnected) => s.phase = Phase::Done(Err("Die Suche wurde abgebrochen.".into())),
            Err(TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(200)),
        }
    }
    let found = match &s.phase {
        Phase::Done(Ok(f)) => Some(f.clone()),
        _ => None,
    };
    let offer = found.as_ref().is_some_and(|f| f.behind > 0);
    let can_update = found.as_ref().is_some_and(|f| f.behind > 0 && f.dirty.is_empty());
    let (ok, cancel) = if offer { ("Jetzt aktualisieren", "Später") } else { ("", "Schließen") };
    let t = Tokens::get();
    let id = egui::Id::new("sc_fork_update");
    let (closed, answer) =
        ui_kit::Window::new(id, "update", "Updates für die eigene Fassung").cards(1.7).actions(ok, cancel, None).show(ctx, can_update, |ui| {
            let current = crate::update::current_version();
            match &s.phase {
                Phase::Idle | Phase::Checking(_) => {
                    ui_kit::text(
                        ui,
                        "Suche im Original nach neuen Änderungen und lasse sie von Claude übersetzen und bewerten (bis zu einer Minute) …",
                        ui_kit::Text::Report("checking", false),
                    );
                }
                Phase::Done(Err(e)) => {
                    ui_kit::text(ui, &format!("Die Suche ist fehlgeschlagen: {e}"), ui_kit::Text::Report("failed", true));
                }
                Phase::Done(Ok(f)) if f.behind == 0 => {
                    ui_kit::text(ui, &format!("Deine Fassung ist auf dem neuesten Stand des Originals (Version {current})."), ui_kit::Text::Report("current", false));
                }
                Phase::Done(Ok(f)) => {
                    ui_kit::text(ui, &format!("Im Original gibt es {} neue Änderungen seit deinem Stand.", f.behind), ui_kit::Text::Report("available", false));
                    let version = match f.upstream_version.as_deref() {
                        Some(v) if v != current => format!("Neue Versionsnummer: {v} (deine Fassung: {current})"),
                        _ => format!("Die Versionsnummer bleibt {current}."),
                    };
                    ui_kit::text(ui, &version, ui_kit::Text::Note("version"));
                    let rated = match &f.assessment {
                        Some(Ok(a)) => {
                            ui_kit::text(ui, &format!("Einschätzung von Claude: {}", a.verdict), ui_kit::Text::Report("verdict", false));
                            Some(a)
                        }
                        Some(Err(e)) => {
                            ui_kit::text(ui, &format!("Keine deutsche Bewertung: {e}"), ui_kit::Text::Note("no_verdict"));
                            None
                        }
                        None => None,
                    };
                    ui_kit::list_frame(ui, "fork_changes", |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                            if let Some(notes) = &f.notes {
                                crate::update::markdown(ui, notes, &t);
                                ui.add_space(8.0);
                            }
                            match rated {
                                Some(a) => {
                                    for (stufe, text) in &a.items {
                                        let mark = match stufe.as_str() {
                                            "wichtig" => "★ Wichtig",
                                            "nützlich" => "● Nützlich",
                                            _ => "· Kleinkram",
                                        };
                                        ui_kit::text(ui, &format!("{mark}: {text}"), ui_kit::Text::Note("change"));
                                    }
                                }
                                None => {
                                    for subject in f.subjects.iter().take(MAX_LISTED) {
                                        ui_kit::text(ui, &format!("• {subject}"), ui_kit::Text::Note("change"));
                                    }
                                }
                            }
                            if f.subjects.len() > MAX_LISTED {
                                ui_kit::text(ui, &format!("… und {} weitere", f.subjects.len() - MAX_LISTED), ui_kit::Text::Note("more"));
                            }
                        });
                    });
                    if f.dirty.is_empty() {
                        ui_kit::text(
                            ui,
                            "Jetzt aktualisieren schließt SolidCraft (ungespeicherte Konstruktionen werden vorher abgefragt). Ein Fenster zeigt dann den Fortschritt: Änderungen übernehmen, neu bauen (10 bis 40 Minuten), in deinen Fork auf GitHub hochladen. Danach startet die neue Fassung. Geht etwas schief, bleibt die bisherige Fassung erhalten.",
                            ui_kit::Text::Note("how"),
                        );
                    } else {
                        ui_kit::text(
                            ui,
                            &format!(
                                "Aktualisieren ist gesperrt: In {} sind {} Dateien geändert, aber nicht gesichert (committet). Sichere sie zuerst.",
                                repo().map_or_else(String::new, |r| r.display().to_string()),
                                f.dirty.len()
                            ),
                            ui_kit::Text::Report("dirty", true),
                        );
                    }
                }
            }
        });
    match answer {
        ui_kit::Action::Ok if can_update => {
            s.open = false;
            if let Err(e) = start_update(app) {
                s.phase = Phase::Done(Err(e));
                s.open = true;
            }
        }
        ui_kit::Action::Cancel => s.open = false,
        _ if closed => s.open = false,
        _ => {}
    }
}

/// Write the update script and have the host run it as SolidCraft exits.
fn start_update(app: &mut SolveApp) -> Result<(), String> {
    let repo = repo().ok_or("Kein Git-Klon gefunden.")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let cargo = std::env::var_os("USERPROFILE")
        .map(|home| PathBuf::from(home).join(".cargo").join("bin").join("cargo.exe"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("cargo"));
    let script = std::env::temp_dir().join("solidcraft-eigene-fassung-update.cmd");
    let text = script_text(repo, &exe, &cargo, std::process::id());
    std::fs::write(&script, text).map_err(|e| format!("{}: {e}", script.display()))?;
    crate::update::launch_on_exit(
        app,
        PathBuf::from("cmd.exe"),
        // `cmd /c` inside `start`, so the window closes once the script ends (a bare script would
        // keep it open as `cmd /k`).
        vec!["/c".into(), "start".into(), "SolidCraft aktualisieren".into(), "cmd".into(), "/c".into(), script.display().to_string()],
    );
    crate::documents::request_quit(app);
    Ok(())
}

/// The console script (CRLF, ASCII only): wait for this process to end, merge, build, push, start.
fn script_text(repo: &Path, exe: &Path, cargo: &Path, pid: u32) -> String {
    let (repo, exe, cargo) = (repo.display(), exe.display(), cargo.display());
    let lines = [
        "@echo off".to_string(),
        "title SolidCraft - eigene Fassung aktualisieren".into(),
        format!("cd /d \"{repo}\""),
        "echo Warte, bis SolidCraft geschlossen ist ...".into(),
        ":warten".into(),
        format!("tasklist /FI \"PID eq {pid}\" /NH 2>nul | find \"{pid}\" >nul"),
        "if not errorlevel 1 (".into(),
        "  timeout /t 1 /nobreak >nul".into(),
        "  goto warten".into(),
        ")".into(),
        "echo.".into(),
        "echo [1/4] Neue Aenderungen vom Original holen ...".into(),
        "git fetch upstream || goto fehler".into(),
        "echo.".into(),
        "echo [2/4] Aenderungen uebernehmen ...".into(),
        "git merge --no-edit upstream/main".into(),
        "if errorlevel 1 (".into(),
        "  git merge --abort".into(),
        "  echo Die Aenderungen des Originals kollidieren mit deinen eigenen. Es wurde nichts veraendert.".into(),
        "  goto fehler".into(),
        ")".into(),
        "echo.".into(),
        "echo [3/4] Neu bauen - das dauert 10 bis 40 Minuten ...".into(),
        format!("\"{cargo}\" build --release -p solidcraft || goto fehler"),
        "echo.".into(),
        "echo [4/4] Neuen Stand in deinen Fork auf GitHub hochladen ...".into(),
        "git push origin HEAD:main || echo Hinweis: Hochladen fehlgeschlagen. Lokal ist alles da; beim naechsten Update wird es nachgeholt.".into(),
        "echo.".into(),
        "echo Fertig. Die neue Fassung startet.".into(),
        format!("start \"\" \"{exe}\""),
        "timeout /t 5 >nul".into(),
        "exit /b 0".into(),
        ":fehler".into(),
        "echo.".into(),
        "echo FEHLER: Das Update wurde nicht abgeschlossen. Die bisherige Fassung bleibt erhalten und startet wieder.".into(),
        "pause".into(),
        format!("start \"\" \"{exe}\""),
        "exit /b 1".into(),
    ];
    lines.join("\r\n") + "\r\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_workspace_version() {
        assert_eq!(cargo_version("[workspace.package]\nedition = \"2024\"\nversion = \"0.25.1\"\n").as_deref(), Some("0.25.1"));
        assert_eq!(cargo_version("[package]\nname = \"x\"\n"), None);
    }

    #[test]
    fn reads_claudes_rating_most_important_first() {
        let ok = r#"{"is_error":false,"structured_output":{"urteil":"Lohnt sich.","aenderungen":[{"text":"A","stufe":"Kleinkram"},{"text":"B","stufe":"wichtig"},{"text":"C","stufe":"nützlich"}]}}"#;
        let a = parse_assessment(ok.as_bytes()).ok().map(|a| (a.verdict, a.items));
        assert_eq!(
            a,
            Some(("Lohnt sich.".into(), vec![("wichtig".into(), "B".into()), ("nützlich".into(), "C".into()), ("Kleinkram".into(), "A".into())]))
        );
        let logged_out = r#"{"is_error":true,"result":"Not logged in · Please run /login"}"#;
        assert!(parse_assessment(logged_out.as_bytes()).err().is_some_and(|e| e.contains("nicht angemeldet")));
        assert!(parse_assessment(b"kein json").is_err());
    }

    #[test]
    fn the_script_uses_crlf_and_waits_for_this_process() {
        let text = script_text(Path::new("C:\\Projekte\\SolidCraft"), Path::new("C:\\x\\solidcraft.exe"), Path::new("cargo"), 4711);
        assert!(text.lines().all(|l| l.is_ascii()));
        assert!(!text.replace("\r\n", "").contains('\n'));
        assert!(text.contains("PID eq 4711"));
        assert!(text.contains("git merge --no-edit upstream/main"));
    }
}
