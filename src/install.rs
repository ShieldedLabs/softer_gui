//! Installing the running program into the host system so it shows up in
//! application search with its icon.
//!
//! This is the half of the icon problem that no window protocol can solve. A
//! window icon lives as long as the window; appearing in GNOME's overview, KDE's
//! Kickoff or Spotlight means writing files into the user's home directory in a
//! layout the desktop already watches. There is no in-band way to do it and no
//! amount of cleverness at the binary level avoids it — a single-file program
//! must, at some point, become more than one file.
//!
//! So this is deliberately an EXPLICIT, CALLED API. Nothing here runs on its
//! own. A program that scatters files through $HOME because it happened to be
//! launched is a program people uninstall. Wire it to a `--install` flag, or to
//! a button the user presses, and give them [`uninstall`] too.
//!
//! Everything written is confined to the user's own data directories; nothing
//! needs root and nothing touches /usr.

use crate::icon::{self, OwnedIcon};
use std::path::{Path, PathBuf};

/// What to install. `id` is the spine of the whole thing.
pub struct AppInfo<'a> {
    /// Reverse-DNS identifier, e.g. "lol.softer.demo". It becomes the desktop
    /// file's basename, the icon file's name, the Wayland app_id and the X11
    /// WM_CLASS — they must all agree or the desktop cannot connect a running
    /// window to its launcher entry, and you get a generic second taskbar group.
    /// Pass this same string as [`crate::Config::app_id`].
    pub id: &'a str,
    /// Human-readable, shown in the launcher. Also names the macOS bundle.
    pub name: &'a str,
    /// One-line description; the launcher shows it under the name.
    pub comment: &'a str,
    /// freedesktop categories, semicolon-terminated, e.g. "Utility;Graphics;".
    /// Ignored on macOS.
    pub categories: &'a str,
    /// Icon sizes to install. 16/32/48/64/128/256 covers every desktop; macOS
    /// additionally uses 512 and 1024 and ignores sizes it has no slot for.
    pub icons: &'a [OwnedIcon],
    /// The binary to launch. None means the running executable.
    pub exec: Option<&'a Path>,
    /// Launch inside a terminal window. Linux only.
    pub terminal: bool,
}

impl<'a> AppInfo<'a> {
    /// Minimal form: everything else empty or defaulted.
    pub fn new(id: &'a str, name: &'a str, icons: &'a [OwnedIcon]) -> AppInfo<'a> {
        AppInfo { id, name, comment: "", categories: "Utility;", icons, exec: None, terminal: false }
    }
}

/// What an install did, so a caller can report it and so [`uninstall`] has
/// something to check against.
#[derive(Debug, Default)]
pub struct Installed {
    pub files: Vec<PathBuf>,
}

pub type Error = String;

// ---- shared helpers --------------------------------------------------------

/// An id we are willing to build paths from. This is the security boundary of
/// the whole module: `id` reaches the filesystem, so anything that could climb
/// out of the data directory is rejected rather than sanitised. Sanitising
/// invites a silent mismatch between the installed name and the app_id the
/// window reports, which breaks icon association in a way nobody can see.
fn check_id(id: &str) -> Result<(), Error> {
    if id.is_empty() || id.len() > 255 { return Err(format!("bad app id {id:?}: empty or over 255 bytes")); }
    if !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-' || c == b'_') {
        return Err(format!("bad app id {id:?}: use only [A-Za-z0-9._-]"));
    }
    if id.starts_with('.') || id.contains("..") { return Err(format!("bad app id {id:?}: leading dot or '..'")); }
    Ok(())
}

fn home() -> Result<PathBuf, Error> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| "HOME is not set".to_string())
}

fn exec_path(info: &AppInfo) -> Result<PathBuf, Error> {
    match info.exec {
        Some(p) => Ok(p.to_path_buf()),
        // canonicalize so the entry survives the user's $PATH changing or the
        // launcher running with a different working directory.
        None => std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .map_err(|e| format!("cannot find the running executable: {e}")),
    }
}

fn write(path: &Path, bytes: &[u8], out: &mut Installed) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    out.files.push(path.to_path_buf());
    Ok(())
}

/// Best-effort: tell the desktop to re-read what we just wrote so the entry
/// appears now rather than at the next login. Failure is not an error — the
/// files are correct either way, and these tools are frequently absent.
#[cfg(target_os = "linux")]
fn refresh_caches(data: &Path) {
    use std::process::{Command, Stdio};
    let quiet = |c: &mut Command| { c.stdout(Stdio::null()).stderr(Stdio::null()); };
    let mut c = Command::new("update-desktop-database");
    quiet(&mut c);
    let _ = c.arg(data.join("applications")).status();
    let mut c = Command::new("gtk-update-icon-cache");
    quiet(&mut c);
    let _ = c.args(["-t", "-f"]).arg(data.join("icons/hicolor")).status();
}

// ============================================================================
// Public API
// ============================================================================

/// Install the program into the user's desktop so it appears in app search.
///
/// Linux: writes `$XDG_DATA_HOME/applications/<id>.desktop` and one PNG per
/// icon size under `icons/hicolor/<N>x<N>/apps/<id>.png`.
///
/// macOS: writes `~/Applications/<Name>.app` — Info.plist, an .icns, and a
/// SYMLINK to the real binary, so the bundle keeps working when the binary is
/// rebuilt in place and nothing is duplicated.
///
/// Overwrites an existing install of the same `id` in place.
pub fn install(info: &AppInfo) -> Result<Installed, Error> {
    check_id(info.id)?;
    if info.name.is_empty() { return Err("app name is empty".into()); }
    #[cfg(target_os = "linux")]
    { install_linux(info) }
    #[cfg(target_os = "macos")]
    { install_macos(info) }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    { let _ = info; Err("install is implemented for Linux and macOS only".into()) }
}

/// Remove what [`install`] wrote for `id`/`name`. Missing files are not an
/// error — uninstall is idempotent on purpose, so a caller can offer it
/// unconditionally. `name` is only needed on macOS, where it names the bundle.
pub fn uninstall(id: &str, name: &str) -> Result<Installed, Error> {
    check_id(id)?;
    #[cfg(target_os = "linux")]
    { let _ = name; uninstall_linux(id) }
    #[cfg(target_os = "macos")]
    { let _ = id; uninstall_macos(name) }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    { let _ = (id, name); Err("uninstall is implemented for Linux and macOS only".into()) }
}

/// Whether an install for this id/name is present. Cheap; a stat, not a parse.
pub fn is_installed(id: &str, name: &str) -> bool {
    #[cfg(target_os = "linux")]
    { let _ = name; data_dir().map(|d| d.join("applications").join(format!("{id}.desktop")).exists()).unwrap_or(false) }
    #[cfg(target_os = "macos")]
    { let _ = id; home().map(|h| h.join("Applications").join(format!("{name}.app")).exists()).unwrap_or(false) }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    { let _ = (id, name); false }
}

// ============================================================================
// Linux — desktop entry + icon theme
// ============================================================================

#[cfg(target_os = "linux")]
fn data_dir() -> Result<PathBuf, Error> {
    // XDG says an empty or relative XDG_DATA_HOME is invalid and the default applies.
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(d);
        if p.is_absolute() { return Ok(p); }
    }
    Ok(home()?.join(".local/share"))
}

/// Quote a path for a desktop-entry Exec key. The spec reserves a pile of
/// characters; anything not plainly safe gets the full quoted treatment, where
/// backslash and double quote are backslash-escaped.
#[cfg(target_os = "linux")]
fn exec_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    let safe = !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"/._-+".contains(&c));
    if safe { return s.into_owned(); }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' || c == '`' || c == '$' { out.push('\\'); }
        out.push(c);
    }
    out.push('"');
    out
}

/// Desktop-entry values are single-line; a newline would start a bogus key.
#[cfg(target_os = "linux")]
fn one_line(s: &str) -> String { s.replace(['\n', '\r'], " ") }

#[cfg(target_os = "linux")]
fn install_linux(info: &AppInfo) -> Result<Installed, Error> {
    let data = data_dir()?;
    let exe = exec_path(info)?;
    let mut out = Installed::default();

    for img in info.icons {
        let path = data.join(format!("icons/hicolor/{0}x{0}/apps/{1}.png", img.side, info.id));
        write(&path, &icon::encode_png(&img.as_image()), &mut out)?;
    }

    // StartupWMClass is what ties a mapped window back to this entry. It must
    // equal the WM_CLASS the X11 backend sets and the app_id the Wayland
    // backend sends — all three are `id`. Without it the launcher shows a
    // separate, iconless entry beside the pinned one whenever the app runs.
    let mut d = String::new();
    d.push_str("[Desktop Entry]\n");
    d.push_str("Type=Application\n");
    d.push_str("Version=1.5\n");
    d.push_str(&format!("Name={}\n", one_line(info.name)));
    if !info.comment.is_empty() { d.push_str(&format!("Comment={}\n", one_line(info.comment))); }
    d.push_str(&format!("Exec={}\n", exec_quote(&exe)));
    // Icon= without a path is looked up in the theme, which is what lets the
    // desktop pick the right size per context instead of rescaling one file.
    d.push_str(&format!("Icon={}\n", info.id));
    d.push_str(&format!("StartupWMClass={}\n", info.id));
    d.push_str("StartupNotify=true\n");
    d.push_str(&format!("Terminal={}\n", info.terminal));
    if !info.categories.is_empty() { d.push_str(&format!("Categories={}\n", one_line(info.categories))); }

    let desktop = data.join("applications").join(format!("{}.desktop", info.id));
    write(&desktop, d.as_bytes(), &mut out)?;

    // The spec wants desktop files executable; most desktops no longer care,
    // but the ones that do refuse to show a non-executable entry at all.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&desktop, std::fs::Permissions::from_mode(0o755));

    refresh_caches(&data);
    Ok(out)
}

#[cfg(target_os = "linux")]
fn uninstall_linux(id: &str) -> Result<Installed, Error> {
    let data = data_dir()?;
    let mut out = Installed::default();
    let desktop = data.join("applications").join(format!("{id}.desktop"));
    if std::fs::remove_file(&desktop).is_ok() { out.files.push(desktop); }
    // Sweep every theme size rather than remembering which were installed: the
    // set may differ from what this build would write, and a stale 48x48 left
    // behind is exactly the kind of thing that makes an "uninstall" not one.
    let hicolor = data.join("icons/hicolor");
    if let Ok(entries) = std::fs::read_dir(&hicolor) {
        for e in entries.flatten() {
            let p = e.path().join("apps").join(format!("{id}.png"));
            if std::fs::remove_file(&p).is_ok() { out.files.push(p); }
        }
    }
    refresh_caches(&data);
    Ok(out)
}

// ============================================================================
// macOS — a bundle around the same binary
// ============================================================================

#[cfg(target_os = "macos")]
fn plist_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn install_macos(info: &AppInfo) -> Result<Installed, Error> {
    let exe = exec_path(info)?;
    // ~/Applications, not /Applications: no admin rights, and Spotlight indexes
    // it just the same. Created if the user has never had one.
    let root = home()?.join("Applications").join(format!("{}.app", info.name));
    let contents = root.join("Contents");
    let mut out = Installed::default();

    let icns = icon::encode_icns(info.icons);
    let has_icon = !icns.is_empty();
    if has_icon { write(&contents.join(format!("Resources/{}.icns", info.id)), &icns, &mut out)?; }

    let bin_name = exe.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| info.id.to_string());
    let mut pl = String::new();
    pl.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    pl.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    pl.push_str("<plist version=\"1.0\">\n<dict>\n");
    let mut kv = |k: &str, v: &str| { pl.push_str(&format!("\t<key>{k}</key>\n\t<string>{v}</string>\n")); };
    kv("CFBundleName", &plist_escape(info.name));
    kv("CFBundleDisplayName", &plist_escape(info.name));
    kv("CFBundleIdentifier", &plist_escape(info.id));
    kv("CFBundleExecutable", &plist_escape(&bin_name));
    kv("CFBundlePackageType", "APPL");
    kv("CFBundleInfoDictionaryVersion", "6.0");
    kv("CFBundleShortVersionString", "1.0");
    kv("CFBundleVersion", "1");
    if has_icon { kv("CFBundleIconFile", &plist_escape(info.id)); }
    // Without this the window is scaled up from 1x and every pixel we so
    // carefully placed is drawn as a 2x2 block on a Retina display.
    pl.push_str("\t<key>NSHighResolutionCapable</key>\n\t<true/>\n");
    pl.push_str("</dict>\n</plist>\n");
    write(&contents.join("Info.plist"), pl.as_bytes(), &mut out)?;
    write(&contents.join("PkgInfo"), b"APPL????", &mut out)?;

    // A SYMLINK, not a copy: the bundle then tracks a rebuilt binary instead of
    // silently launching a stale duplicate, and there is only ever one program
    // on disk. Replace any previous link first — symlink() will not overwrite.
    let link = contents.join("MacOS").join(&bin_name);
    std::fs::create_dir_all(link.parent().unwrap()).map_err(|e| format!("mkdir MacOS: {e}"))?;
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&exe, &link).map_err(|e| format!("symlink {}: {e}", link.display()))?;
    out.files.push(link);

    // Launch Services notices a bundle appearing in a watched directory, but it
    // watches lazily; bumping the mtime of the bundle root prompts a re-scan.
    let _ = std::fs::File::open(&root).and_then(|f| f.sync_all());
    Ok(out)
}

#[cfg(target_os = "macos")]
fn uninstall_macos(name: &str) -> Result<Installed, Error> {
    if name.is_empty() || name.contains('/') { return Err(format!("bad app name {name:?}")); }
    let root = home()?.join("Applications").join(format!("{name}.app"));
    let mut out = Installed::default();
    if root.exists() {
        std::fs::remove_dir_all(&root).map_err(|e| format!("remove {}: {e}", root.display()))?;
        out.files.push(root);
    }
    Ok(out)
}
