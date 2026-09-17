//! Embeds the build identity, and the Windows resources.
//!
//! **Build identity** is the commit the binary was built from, emitted as
//! `PURPTOOF_BUILD`. A version string alone cannot tell two builds apart - see
//! [`emit_build_id`].
//!
//! **Windows resources** are the icon and the version block. Without them the
//! executable is a generic blank page in Explorer, the Start menu and
//! Add/Remove Programs, and its Properties dialog shows no version or
//! publisher. The eframe icon only covers the app's own title bar - everything
//! Windows itself draws reads the compiled-in resource.

fn main() {
    // Before the target guard: the build identity is wanted on every target,
    // and it is the half of this script that cannot be allowed to silently
    // not happen.
    emit_build_id();

    // Only meaningful on Windows, and the crate does not build elsewhere, but
    // guarding keeps the build script honest about what it assumes.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=assets/purptoof.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/purptoof.ico")
        .set(
            "FileDescription",
            "Bluetooth audio receiver that recovers itself",
        )
        .set("ProductName", "PurpToof")
        .set("CompanyName", "TheHalfrican")
        .set("LegalCopyright", "MIT licensed");

    if let Err(e) = res.compile() {
        // A missing resource compiler should not stop a developer building and
        // testing; it only costs the icon and the version block.
        println!("cargo:warning=could not embed Windows resources: {e}");
    }
}

/// Emit `PURPTOOF_BUILD`: the short commit, plus `-dirty` for an uncommitted
/// tree.
///
/// # Why this is not optional
///
/// `CARGO_PKG_VERSION` alone cannot identify a build. Every binary between two
/// releases carries the same version string, so a log line saying
/// `version="0.2.0"` matches both the build that had a bug and the build that
/// fixed it - which is exactly the question a log gets asked the morning after.
///
/// CLAUDE.md's soak matrix, `docs/progress.md` and `docs/soak.md` all say to
/// record results "with date and build hash". Until now the binary carried no
/// hash, so the release gate asked for a value that did not exist.
///
/// Best effort. A tarball with no `.git`, or no `git` on PATH, yields
/// `unknown` rather than failing the build - being unable to name the commit
/// is not a reason to be unable to compile.
fn emit_build_id() {
    // Cargo stops watching everything else once any rerun-if-changed is
    // emitted, and this script already narrows to the icon. Without these the
    // hash would be baked in at the first build and then never update.
    println!("cargo:rerun-if-changed=.git/HEAD");
    // Appended on every commit, checkout and reset, which the ref files
    // themselves are not reliably observed to be.
    println!("cargo:rerun-if-changed=.git/logs/HEAD");

    let hash = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());

    // `--porcelain` prints one line per changed path and nothing at all for a
    // clean tree, so emptiness is the test.
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());

    let id = if dirty { format!("{hash}-dirty") } else { hash };
    println!("cargo:rustc-env=PURPTOOF_BUILD={id}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
