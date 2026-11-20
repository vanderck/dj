//! Build tasks. Run from anywhere in the workspace:
//!
//!     cargo xtask package-macos [options]
//!
//! Builds dj in release mode and packages it as `dj.app` plus a `.dmg` in
//! `target/package/`. See `cargo xtask help` and packaging/macos/README.md.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const APP_NAME: &str = "dj";
const EXECUTABLE: &str = "dj";
const CARGO_BIN: &str = "dj";
const DEFAULT_BUNDLE_ID: &str = "com.github.vanderck.dj";
/// The statically linked ONNX Runtime (stem separation) is built for macOS 13.4.
const DEFAULT_MIN_MACOS: &str = "13.4";
/// App icon used when --icon is not given.
const DEFAULT_ICON: &str = "packaging/macos/AppIcon.icns";

const USAGE: &str = "\
cargo xtask package-macos [options]

Builds dj (release, Apple Silicon) and packages target/package/dj.app and a .dmg.

Options:
  --icon <file>          App icon: .icns, or a 1024x1024 .png (converted with sips/iconutil).
                         Default: packaging/macos/AppIcon.icns
  --sign <identity>      Code-signing identity, e.g. \"Developer ID Application: Name (TEAMID)\".
                         Default: $MACOS_SIGN_IDENTITY, else ad-hoc (\"-\")
  --no-sign              Skip code signing
  --notarize             Notarize and staple the .dmg (needs a Developer ID identity and either
                         $NOTARY_KEYCHAIN_PROFILE or $APPLE_ID, $APPLE_TEAM_ID, $APPLE_APP_PASSWORD)
  --no-dmg               Only build the .app
  --features <list>      Extra cargo features for the app (comma separated)
  --no-default-features  Build the app without its default features
  --bundle-id <id>       CFBundleIdentifier (default com.github.vanderck.dj)
  --min-macos <version>  LSMinimumSystemVersion / deployment target (default 13.4)
  --binary <path>        Package an existing binary instead of building (works on any OS;
                         signing and the .dmg still need macOS)
  --out <dir>            Output folder (default target/package)
";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("package-macos") => Options::parse(&args[1..]).and_then(|o| package_macos(&o)),
        Some("help") | Some("--help") | Some("-h") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown task `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Default)]
struct Options {
    icon: Option<PathBuf>,
    sign: Option<String>,
    no_sign: bool,
    notarize: bool,
    dmg: bool,
    features: Vec<String>,
    no_default_features: bool,
    bundle_id: String,
    min_macos: String,
    binary: Option<PathBuf>,
    out: PathBuf,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut o = Options {
            dmg: true,
            bundle_id: DEFAULT_BUNDLE_ID.into(),
            min_macos: DEFAULT_MIN_MACOS.into(),
            out: workspace_root().join("target/package"),
            sign: env::var("MACOS_SIGN_IDENTITY")
                .ok()
                .filter(|s| !s.is_empty()),
            ..Default::default()
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let mut value = |name: &str| {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match a.as_str() {
                "--universal" => return Err("Intel Macs aren't supported any more: dj is built for Apple Silicon only".into()),
                "--icon" => o.icon = Some(value(a)?.into()),
                "--sign" => o.sign = Some(value(a)?),
                "--no-sign" => o.no_sign = true,
                "--notarize" => o.notarize = true,
                "--no-dmg" => o.dmg = false,
                "--features" => o.features.extend(
                    value(a)?
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                ),
                "--no-default-features" => o.no_default_features = true,
                "--bundle-id" => o.bundle_id = value(a)?,
                "--min-macos" => o.min_macos = value(a)?,
                "--binary" => o.binary = Some(value(a)?.into()),
                "--out" => o.out = value(a)?.into(),
                other => return Err(format!("unknown option `{other}`\n\n{USAGE}")),
            }
        }
        if o.icon.is_none() {
            let default = workspace_root().join(DEFAULT_ICON);
            // A .png needs sips/iconutil, i.e. macOS; elsewhere the bundle is built without it.
            o.icon = (default.is_file() && on_macos()).then_some(default);
        }
        if o.notarize && (o.no_sign || o.identity() == "-") {
            return Err(
                "--notarize needs a Developer ID signing identity (--sign or $MACOS_SIGN_IDENTITY)"
                    .into(),
            );
        }
        for (what, p) in [
            ("icon", &o.icon),
            ("binary", &o.binary),
        ] {
            if let Some(p) = p {
                if !p.is_file() {
                    return Err(format!("{what} file not found: {}", p.display()));
                }
            }
        }
        Ok(o)
    }

    fn identity(&self) -> &str {
        self.sign.as_deref().unwrap_or("-")
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .to_path_buf()
}

fn version() -> &'static str {
    env!("CARGO_PKG_VERSION") // xtask inherits the workspace version
}

fn on_macos() -> bool {
    cfg!(target_os = "macos")
}

fn package_macos(o: &Options) -> Result<(), String> {
    fs::create_dir_all(&o.out).map_err(|e| format!("create {}: {e}", o.out.display()))?;

    let binary = match &o.binary {
        Some(b) => b.clone(),
        None if !on_macos() => return Err(
            "building the macOS app needs macOS (use --binary <path> to only assemble the bundle)"
                .into(),
        ),
        None => build(o)?,
    };

    let app = o.out.join(format!("{APP_NAME}.app"));
    assemble(&app, &binary, o)?;
    step(&format!("assembled {}", app.display()));

    if o.no_sign {
        step("skipping code signing (--no-sign)");
    } else if !on_macos() {
        step("skipping code signing (needs macOS)");
    } else {
        sign_app(&app, o)?;
    }

    if !o.dmg {
        return Ok(());
    }
    if !on_macos() {
        step("skipping .dmg (needs macOS)");
        return Ok(());
    }
    let dmg = o
        .out
        .join(format!("{APP_NAME}-{}-macos-arm64.dmg", version()));
    make_dmg(&app, &dmg, o)?;
    if o.notarize {
        notarize(&dmg)?;
    }
    step(&format!("done: {}", dmg.display()));
    Ok(())
}

fn step(msg: &str) {
    eprintln!("==> {msg}");
}

fn run(cmd: &mut Command) -> Result<(), String> {
    eprintln!("    $ {cmd:?}");
    let status = cmd
        .status()
        .map_err(|e| format!("could not run {:?}: {e}", cmd.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed ({status})", cmd.get_program()))
    }
}

/// Apple Silicon only (Intel Macs aren't supported: there is no prebuilt ONNX Runtime for them).
const TARGET: &str = "aarch64-apple-darwin";

/// `cargo build --release` for Apple Silicon (also from an Intel Mac); returns the binary.
fn build(o: &Options) -> Result<PathBuf, String> {
    let root = workspace_root();
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    step(&format!("building {CARGO_BIN} ({TARGET})"));
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(&root)
        .args(["build", "--release", "-p", CARGO_BIN, "--target", TARGET]);
    if o.no_default_features {
        cmd.arg("--no-default-features");
    }
    if !o.features.is_empty() {
        cmd.args(["--features", &o.features.join(",")]);
    }
    // Must match LSMinimumSystemVersion; also what the prebuilt ONNX Runtime needs.
    cmd.env("MACOSX_DEPLOYMENT_TARGET", &o.min_macos);
    run(&mut cmd)?;
    Ok(root.join("target").join(TARGET).join("release").join(CARGO_BIN))
}

/// Lay out `dj.app` (works on any OS).
fn assemble(app: &Path, binary: &Path, o: &Options) -> Result<(), String> {
    let io = |what: &str, e: std::io::Error| format!("{what}: {e}");
    if app.exists() {
        fs::remove_dir_all(app).map_err(|e| io("remove old bundle", e))?;
    }
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    fs::create_dir_all(&macos).map_err(|e| io("create bundle", e))?;
    fs::create_dir_all(&resources).map_err(|e| io("create bundle", e))?;

    let exe = macos.join(EXECUTABLE);
    fs::copy(binary, &exe).map_err(|e| io(&format!("copy {}", binary.display()), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).map_err(|e| io("chmod", e))?;
    }

    let has_icon = match &o.icon {
        Some(icon) => {
            install_icon(icon, &resources.join("AppIcon.icns"))?;
            true
        }
        None => false,
    };

    let plist = info_plist(&o.bundle_id, version(), &o.min_macos, has_icon);
    fs::write(contents.join("Info.plist"), plist).map_err(|e| io("write Info.plist", e))?;
    fs::write(contents.join("PkgInfo"), "APPL????").map_err(|e| io("write PkgInfo", e))?;
    Ok(())
}

fn install_icon(icon: &Path, dest: &Path) -> Result<(), String> {
    let ext = icon
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "icns" => fs::copy(icon, dest)
            .map(|_| ())
            .map_err(|e| format!("copy icon: {e}")),
        "png" if on_macos() => {
            // Render the standard iconset sizes with sips, then pack them with iconutil.
            let set = dest.with_extension("iconset");
            let _ = fs::remove_dir_all(&set);
            fs::create_dir_all(&set).map_err(|e| format!("iconset: {e}"))?;
            for size in [16u32, 32, 128, 256, 512] {
                for (scale, suffix) in [(1, ""), (2, "@2x")] {
                    let px = (size * scale).to_string();
                    let name = set.join(format!("icon_{size}x{size}{suffix}.png"));
                    run(Command::new("sips")
                        .args(["-z", &px, &px])
                        .arg(icon)
                        .arg("--out")
                        .arg(&name))?;
                }
            }
            run(Command::new("iconutil")
                .args(["-c", "icns"])
                .arg(&set)
                .arg("-o")
                .arg(dest))?;
            let _ = fs::remove_dir_all(&set);
            Ok(())
        }
        "png" => {
            Err("converting a .png icon needs macOS (sips/iconutil); pass an .icns instead".into())
        }
        _ => Err(format!("unsupported icon format: {}", icon.display())),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn info_plist(bundle_id: &str, version: &str, min_macos: &str, has_icon: bool) -> String {
    let mut entries: Vec<(&str, String)> = vec![
        ("CFBundleDevelopmentRegion", "<string>en</string>".into()),
        (
            "CFBundleDisplayName",
            format!("<string>{APP_NAME}</string>"),
        ),
        (
            "CFBundleExecutable",
            format!("<string>{EXECUTABLE}</string>"),
        ),
        (
            "CFBundleIdentifier",
            format!("<string>{}</string>", xml_escape(bundle_id)),
        ),
        (
            "CFBundleInfoDictionaryVersion",
            "<string>6.0</string>".into(),
        ),
        ("CFBundleName", format!("<string>{APP_NAME}</string>")),
        ("CFBundlePackageType", "<string>APPL</string>".into()),
        (
            "CFBundleShortVersionString",
            format!("<string>{}</string>", xml_escape(version)),
        ),
        (
            "CFBundleVersion",
            format!("<string>{}</string>", xml_escape(version)),
        ),
        (
            "LSApplicationCategoryType",
            "<string>public.app-category.music</string>".into(),
        ),
        (
            "LSMinimumSystemVersion",
            format!("<string>{}</string>", xml_escape(min_macos)),
        ),
        ("NSHighResolutionCapable", "<true/>".into()),
        ("NSSupportsAutomaticGraphicsSwitching", "<true/>".into()),
    ];
    if has_icon {
        entries.push(("CFBundleIconFile", "<string>AppIcon</string>".into()));
    }
    entries.sort_by_key(|e| e.0);
    let body: String = entries
        .iter()
        .map(|(k, v)| format!("    <key>{k}</key>\n    {v}\n"))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n{body}</dict>\n</plist>\n"
    )
}

fn sign_app(app: &Path, o: &Options) -> Result<(), String> {
    let id = o.identity();
    let entitlements = workspace_root().join("packaging/macos/entitlements.plist");
    step(if id == "-" {
        "ad-hoc code signing"
    } else {
        "code signing (Developer ID, hardened runtime)"
    });
    let mut cmd = Command::new("codesign");
    cmd.args(["--force", "--sign", id])
        .arg("--entitlements")
        .arg(&entitlements);
    if id != "-" {
        cmd.args(["--options", "runtime", "--timestamp"]);
    }
    run(cmd.arg(app))?;
    run(Command::new("codesign")
        .args(["--verify", "--strict", "--verbose=2"])
        .arg(app))
}

fn make_dmg(app: &Path, dmg: &Path, o: &Options) -> Result<(), String> {
    step("building .dmg");
    let staging = o.out.join("dmg-staging");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("staging: {e}"))?;
    // ditto preserves the code signature and extended attributes.
    run(Command::new("ditto")
        .arg(app)
        .arg(staging.join(app.file_name().unwrap())))?;
    #[cfg(unix)]
    std::os::unix::fs::symlink("/Applications", staging.join("Applications"))
        .map_err(|e| format!("symlink: {e}"))?;
    let _ = fs::remove_file(dmg);
    // hdiutil's own size estimate for -srcfolder is sometimes too small ("No space left on
    // device" inside the image, seen on CI), so give it an explicit size with headroom.
    let size_mb = dmg_size_mb(dir_size(&staging));
    let create = |attempt: u32| {
        if attempt > 1 {
            step(&format!("retrying hdiutil (attempt {attempt})"));
            let _ = fs::remove_file(dmg);
        }
        run(Command::new("hdiutil")
            .args([
                "create",
                "-volname",
                &format!("{APP_NAME} {}", version()),
                "-srcfolder",
            ])
            .arg(&staging)
            .args([
                "-fs",
                "HFS+",
                "-size",
                &format!("{size_mb}m"),
                "-ov",
                "-format",
                "UDZO",
            ])
            .arg(dmg))
    };
    // hdiutil also fails transiently on CI runners ("Resource busy"); one retry covers it.
    create(1).or_else(|_| create(2))?;
    let _ = fs::remove_dir_all(&staging);
    if !o.no_sign && o.identity() != "-" {
        run(Command::new("codesign")
            .args(["--force", "--sign", o.identity(), "--timestamp"])
            .arg(dmg))?;
    }
    Ok(())
}

/// Total size of the regular files under `dir` (symlinks are not followed).
fn dir_size(dir: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Uncompressed image size for `bytes` of content: +30 % and 32 MB for filesystem overhead.
fn dmg_size_mb(bytes: u64) -> u64 {
    (bytes as f64 * 1.3 / 1_048_576.0).ceil() as u64 + 32
}

fn notarize(dmg: &Path) -> Result<(), String> {
    step("notarizing (this can take a few minutes)");
    let mut cmd = Command::new("xcrun");
    cmd.args(["notarytool", "submit"]).arg(dmg).arg("--wait");
    match env::var("NOTARY_KEYCHAIN_PROFILE")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(profile) => {
            cmd.args(["--keychain-profile", &profile]);
        }
        None => {
            let var = |k: &str| {
                env::var(k)
                    .ok()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| format!("${k} is not set"))
            };
            cmd.args([
                "--apple-id",
                &var("APPLE_ID")?,
                "--team-id",
                &var("APPLE_TEAM_ID")?,
                "--password",
                &var("APPLE_APP_PASSWORD")?,
            ]);
        }
    }
    run(&mut cmd)?;
    run(Command::new("xcrun").args(["stapler", "staple"]).arg(dmg))?;
    run(Command::new("spctl")
        .args([
            "--assess",
            "--type",
            "open",
            "--context",
            "context:primary-signature",
            "--verbose",
        ])
        .arg(dmg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dmg_size_has_headroom() {
        assert_eq!(dmg_size_mb(0), 32);
        let mb = 1_048_576;
        assert!(dmg_size_mb(100 * mb) >= 130 + 32);
        let dir = env::temp_dir().join(format!("xtask-size-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("x"), vec![0u8; 1000]).unwrap();
        fs::write(dir.join("a/b/y"), vec![0u8; 500]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/", dir.join("root-link")).unwrap(); // must not be followed
        assert_eq!(dir_size(&dir), 1500);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plist_has_the_required_keys() {
        let p = info_plist("com.example.app", "1.2.3", "13.4", true);
        for key in [
            "CFBundleExecutable",
            "CFBundleIdentifier",
            "CFBundlePackageType",
            "CFBundleShortVersionString",
            "LSMinimumSystemVersion",
            "NSHighResolutionCapable",
            "CFBundleIconFile",
        ] {
            assert!(p.contains(&format!("<key>{key}</key>")), "{key}");
        }
        assert!(p.contains("<string>com.example.app</string>"));
        assert!(p.contains("<string>13.4</string>"));
        assert!(!info_plist("a&b", "1", "13.4", false).contains("CFBundleIconFile"));
        assert!(info_plist("a&b", "1", "13.4", false).contains("a&amp;b"));
    }

    #[test]
    fn options_parse_and_validate() {
        let o = Options::parse(&args(&[
            "--no-dmg",
            "--features",
            "a, b",
            "--min-macos",
            "14.0",
        ]))
        .unwrap();
        assert!(!o.dmg);
        assert!(Options::parse(&args(&["--universal"])).is_err(), "no Intel builds");
        assert_eq!(o.features, vec!["a", "b"]);
        assert_eq!(o.min_macos, "14.0");
        assert!(Options::parse(&args(&["--bogus"])).is_err());
        assert!(Options::parse(&args(&["--icon", "/nonexistent.icns"])).is_err());
        assert!(Options::parse(&args(&["--notarize", "--no-sign"])).is_err());
    }

    #[test]
    fn assembles_a_bundle_on_any_os() {
        let dir = env::temp_dir().join(format!("xtask-bundle-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("bin");
        fs::write(&bin, b"\x7fELF pretend").unwrap();
        let icon = dir.join("i.icns");
        fs::write(&icon, b"icns").unwrap();

        let o = Options::parse(&args(&[
            "--binary",
            bin.to_str().unwrap(),
            "--icon",
            icon.to_str().unwrap(),
            "--out",
            dir.join("out").to_str().unwrap(),
            "--no-sign",
            "--no-dmg",
        ]))
        .unwrap();
        package_macos(&o).unwrap();

        let c = dir.join("out/dj.app/Contents");
        assert_eq!(
            fs::read(c.join("MacOS/dj")).unwrap(),
            b"\x7fELF pretend"
        );
        assert!(c.join("Resources/AppIcon.icns").is_file());
        assert_eq!(fs::read_to_string(c.join("PkgInfo")).unwrap(), "APPL????");
        assert!(fs::read_to_string(c.join("Info.plist"))
            .unwrap()
            .contains("<string>dj</string>"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(c.join("MacOS/dj"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
        // Re-running replaces the bundle cleanly.
        package_macos(&o).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
