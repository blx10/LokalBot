fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    embed_origin_build_meta();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources();
    }
}

fn git_stdout(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn embed_origin_build_meta() {
    println!("cargo:rerun-if-env-changed=LOKALBOT_BUILD_ORIGIN");
    if let Some(head_path) = git_stdout(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head_path}");
    }
    if let Some(sym_ref) = git_stdout(&["rev-parse", "--symbolic-full-name", "HEAD"])
        && let Some(ref_path) = git_stdout(&["rev-parse", "--git-path", &sym_ref])
    {
        println!("cargo:rerun-if-changed={ref_path}");
    }

    let custom = std::env::var("LOKALBOT_BUILD_ORIGIN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let sha = git_stdout(&["rev-parse", "--short", "HEAD"]);
    let branch = git_stdout(&["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD");

    let badge = custom.clone().unwrap_or_else(|| match &sha {
        Some(sha) => format!("origin · {sha}"),
        None => "origin".to_string(),
    });
    let detail = custom.unwrap_or_else(|| match (branch.as_deref(), sha.as_deref()) {
        (Some(branch), Some(sha)) => format!("origin · {branch}@{sha}"),
        _ => badge.clone(),
    });

    println!("cargo:rustc-env=LOKALBOT_BUILD_ORIGIN={badge}");
    println!("cargo:rustc-env=LOKALBOT_BUILD_ORIGIN_DETAIL={detail}");
}

#[cfg(windows)]
fn windows_resources() {
    let packaging = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging");
    let icon = packaging.join("icons/lokalbot.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(&icon.to_string_lossy())
        .set("ProductName", "LokalBot")
        .set("FileDescription", "LokalBot")
        .set("CompanyName", "LokalBot")
        .set("LegalCopyright", "GPL-3.0-only");
    if let Err(err) = resource.compile() {
        panic!("embedding Windows resources failed: {err}");
    }
}

#[cfg(not(windows))]
fn windows_resources() {
    println!("cargo:warning=Windows resources can only be embedded when building on Windows");
}
