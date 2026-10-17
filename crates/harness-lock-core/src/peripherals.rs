//! Lock the peripherals a manifest declares (`[tools]`, `[skills]`,
//! `[instructions]`) the way `hlock pin` locks a live setup: MCP servers with an
//! exact launch command and pinned package version, skills with a tree digest
//! (vendored when they live outside the project), git skills at a commit, and
//! instruction files with a digest. A resolved lock can then be synced and
//! launched like a pinned one.
//!
//! Network lookups (an npm package's current version, a git ref's commit) are
//! skipped when `offline` is set or the tool isn't installed; the entry is then
//! recorded as declared, with a warning.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::capture::{is_project_path, secret_var, tree_digest, vendor_tree, VendorFile, VENDOR_DIR};
use crate::error::{Error, Result};
use crate::hash::sha256_bytes;
use crate::lockfile::{Instructions, SecretRef, Skill, Tool};
use crate::manifest::Manifest;
use crate::profiles::{run_capture, Ctx, HarnessProfile};

/// What [`lock_peripherals`] produced.
#[derive(Debug, Clone, Default)]
pub struct Locked {
    pub tools: Vec<Tool>,
    pub skills: Vec<Skill>,
    pub instructions: Vec<Instructions>,
    pub secrets: Vec<SecretRef>,
    pub vendor: Vec<VendorFile>,
    pub warnings: Vec<String>,
}

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// `npm:@scope/pkg@1.2` → (`@scope/pkg`, Some("1.2")).
fn split_npm(spec: &str) -> (String, Option<String>) {
    let (scope, rest) = spec.strip_prefix('@').map(|r| ("@", r)).unwrap_or(("", spec));
    match rest.split_once('@') {
        Some((n, v)) if !v.is_empty() => (format!("{scope}{n}"), Some(v.to_string())),
        _ => (format!("{scope}{rest}"), None),
    }
}

/// The current version of an npm package, from the registry.
fn npm_version(pkg: &str, ctx: &Ctx) -> Option<String> {
    let npm = ctx.which("npm")?;
    let out = run_capture(&npm, &["view".into(), pkg.into(), "version".into()], ctx, LOOKUP_TIMEOUT)?;
    out.lines().map(str::trim).find(|l| !l.is_empty() && l.chars().next().is_some_and(|c| c.is_ascii_digit())).map(str::to_string)
}

/// The commit a git ref points at, from the remote.
fn git_commit(url: &str, rev: &str, ctx: &Ctx) -> Option<String> {
    let git = ctx.which("git")?;
    let out = run_capture(&git, &["ls-remote".into(), url.into(), rev.into()], ctx, LOOKUP_TIMEOUT)?;
    out.lines().filter_map(|l| l.split_whitespace().next()).find(|h| h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit())).map(str::to_string)
}

fn is_sha(s: &str) -> bool {
    s.len() >= 7 && s.len() <= 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn expand(p: &str, ctx: &Ctx) -> PathBuf {
    ctx.expand(p, None)
}

fn lock_tool(name: &str, spec: &Value, ctx: &Ctx, offline: bool, out: &mut Locked) {
    let obj = spec.as_object().cloned().unwrap_or_default();
    let mut t = Tool { name: name.to_string(), kind: "mcp".into(), permissions: str_list(obj.get("permissions")), ..Default::default() };
    let secret = |out: &mut Locked, section: Option<&str>, key: &str| -> String {
        let mut parts = vec![name];
        if let Some(s) = section {
            parts.push(s);
        }
        parts.push(key);
        let var = secret_var(&parts);
        if !out.secrets.iter().any(|s| s.name == var) {
            out.secrets.push(SecretRef { name: var.clone(), used_by: format!("mcp {name} {key}") });
        }
        format!("${{{var}}}")
    };

    if let Some(src) = obj.get("mcp").and_then(Value::as_str) {
        let (kind, rest) = src.split_once(':').unwrap_or(("", src));
        match kind {
            "npm" => {
                let (pkg, mut ver) = split_npm(rest);
                if ver.is_none() && !offline {
                    ver = npm_version(&pkg, ctx);
                }
                if ver.is_none() {
                    out.warnings.push(format!("mcp {name}: {src} has no version; pin one (npm:{pkg}@x.y.z) or resolve online"));
                }
                let spec_str = ver.as_ref().map(|v| format!("{pkg}@{v}")).unwrap_or_else(|| pkg.clone());
                t.source = Some(format!("npm:{spec_str}"));
                t.command = Some("npx".into());
                t.args = vec!["-y".into(), spec_str];
            }
            "pypi" => {
                if !rest.contains("==") {
                    out.warnings.push(format!("mcp {name}: {src} has no version; pin one (pypi:{rest}==x.y.z)"));
                }
                t.source = Some(format!("pypi:{rest}"));
                t.command = Some("uvx".into());
                t.args = vec![rest.to_string()];
            }
            "oci" | "docker" => {
                if !rest.contains('@') && !rest.rsplit('/').next().unwrap_or(rest).contains(':') {
                    out.warnings.push(format!("mcp {name}: {src} has no tag or digest"));
                }
                t.source = Some(format!("oci:{rest}"));
                t.command = Some("docker".into());
                t.args = vec!["run".into(), "-i".into(), "--rm".into(), rest.to_string()];
            }
            _ => {
                out.warnings.push(format!("mcp {name}: hlock can't install {src:?}; record a command instead"));
                t.source = Some(src.to_string());
            }
        }
        t.transport = Some("stdio".into());
    } else if let Some(url) = obj.get("url").and_then(Value::as_str) {
        t.transport = Some(obj.get("transport").and_then(Value::as_str).unwrap_or("http").to_string());
        t.url = Some(url.to_string());
        t.source = Some("remote".into());
    } else if let Some(cmd) = obj.get("command").and_then(Value::as_str) {
        t.transport = Some("stdio".into());
        t.command = Some(cmd.to_string());
        t.args = str_list(obj.get("args"));
    } else {
        // `python = {}`: a harness built-in
        t.kind = "builtin".into();
        out.tools.push(t);
        return;
    }
    for k in str_list(obj.get("env")) {
        let v = secret(out, None, &k);
        t.env.insert(k, v);
    }
    for k in str_list(obj.get("headers")) {
        let v = secret(out, Some("header"), &k);
        t.headers.insert(k, v);
    }
    t.scope = Some("project".into());
    out.tools.push(t);
}

fn lock_skill(name: &str, spec: &Value, ctx: &Ctx, vroot: &str, offline: bool, out: &mut Locked) -> Result<()> {
    let obj = spec.as_object().cloned().unwrap_or_default();
    let install = format!("{{root}}/skills/{name}");
    if let Some(url) = obj.get("git").and_then(Value::as_str) {
        let sub = obj.get("path").and_then(Value::as_str).unwrap_or("");
        let rev = ["commit", "rev", "tag", "branch"].iter().find_map(|k| obj.get(*k).and_then(Value::as_str)).unwrap_or("HEAD");
        let commit = if rev.len() == 40 && is_sha(rev) {
            Some(rev.to_string())
        } else if offline {
            None
        } else {
            git_commit(url, rev, ctx)
        };
        if commit.is_none() {
            out.warnings.push(format!("skill {name}: couldn't resolve {url}@{rev} to a commit"));
        }
        out.skills.push(Skill {
            name: name.to_string(),
            origin: "third-party".into(),
            source: Some(format!("git+{url}@{}#path={sub}", commit.as_deref().unwrap_or(rev))),
            scope: Some("user".into()),
            path: Some(install),
            enabled: true,
            ..Default::default()
        });
        return Ok(());
    }
    // a local directory (a git skill's `path` is inside the repo, handled above)
    if let Some(path) = obj.get("path").and_then(Value::as_str) {
        let dir = expand(path, ctx);
        if !dir.join("SKILL.md").is_file() {
            return Err(Error::Manifest(format!("[skills] {name}: no SKILL.md in {}", dir.display())));
        }
        let digest = tree_digest(&dir);
        let editable = obj.get("editable").and_then(Value::as_bool).unwrap_or(false);
        let mut s = Skill { name: name.to_string(), digest: Some(digest.clone()), enabled: true, editable, ..Default::default() };
        if is_project_path(&dir, ctx) {
            let rel = dir.strip_prefix(&ctx.cwd).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string());
            s.origin = "first-party".into();
            s.source = Some(format!("path:{rel}"));
            s.scope = Some("project".into());
            s.path = Some(rel);
            if editable {
                s.evidence_digest = Some(digest);
            }
        } else {
            s.origin = "local".into();
            s.source = Some(format!("path:{path}"));
            s.scope = Some("user".into());
            s.path = Some(install);
            let dest = format!("{vroot}/skills/{name}");
            match vendor_tree(&dir, &dest, &mut out.vendor) {
                Ok(()) => s.vendored = Some(dest),
                Err(e) => out.warnings.push(format!("skill {name}: {e}")),
            }
        }
        out.skills.push(s);
        return Ok(());
    }
    // registry / gh / oci / plugin sources: recorded, not fetched yet
    let source = ["registry", "gh", "oci", "plugin"]
        .iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str).map(|v| format!("{k}:{v}")))
        .unwrap_or_else(|| spec.to_string());
    out.warnings.push(format!("skill {name}: {source} isn't fetched by hlock yet; recorded as declared"));
    out.skills.push(Skill { name: name.to_string(), origin: "third-party".into(), source: Some(source), path: Some(install), enabled: true, ..Default::default() });
    Ok(())
}

/// Lock the manifest's `[tools]`, `[skills]` and `[instructions]`. `ctx` must be
/// rooted at the project (the manifest's directory).
pub fn lock_peripherals(m: &Manifest, target: Option<&HarnessProfile>, ctx: &Ctx, offline: bool) -> Result<Locked> {
    let mut out = Locked::default();
    let vroot = format!("{VENDOR_DIR}/{}", target.map(|t| t.id.as_str()).unwrap_or("shared"));
    for (name, spec) in &m.tools {
        lock_tool(name, spec, ctx, offline, &mut out);
    }
    for (name, spec) in &m.skills {
        // `[skills.builtin]` / `[skills.trust]` are settings tables, not skills
        if matches!(name.as_str(), "builtin" | "trust") {
            continue;
        }
        lock_skill(name, spec, ctx, &vroot, offline, &mut out)?;
    }
    let files = str_list(m.instructions.get("files"));
    for f in files {
        let p = expand(&f, ctx);
        let bytes = std::fs::read(&p).map_err(|_| Error::Manifest(format!("[instructions] file not found: {}", p.display())))?;
        let project = is_project_path(&p, ctx);
        let mut ins = Instructions { name: f.clone(), digest: Some(sha256_bytes(&bytes)), scope: if project { "project" } else { "user" }.into(), vendored: None };
        if !project {
            let rel = format!("{vroot}/instructions/{}", p.file_name().unwrap_or_default().to_string_lossy());
            out.vendor.push(VendorFile { rel: rel.clone(), bytes, executable: false });
            ins.vendored = Some(rel);
        }
        out.instructions.push(ins);
    }
    Ok(out)
}

/// Counts and warnings for a dry run, without network lookups.
pub fn summarize(m: &Manifest, target: Option<&HarnessProfile>, ctx: &Ctx) -> Value {
    match lock_peripherals(m, target, ctx, true) {
        Ok(l) => {
            let mcp = l.tools.iter().filter(|t| t.kind == "mcp").count();
            serde_json::json!({
                "mcp-servers": mcp,
                "skills": l.skills.len(),
                "instructions": l.instructions.len(),
                "secrets": l.secrets.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
                "warnings": l.warnings,
            })
        }
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn ctx(dir: &Path) -> Ctx {
        Ctx { home: dir.join("home"), cwd: dir.join("project"), env: BTreeMap::new(), path: vec![] }
    }

    #[test]
    fn locks_declared_peripherals() {
        let dir = std::env::temp_dir().join(format!("hlock-periph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project/skills/notes")).unwrap();
        std::fs::create_dir_all(dir.join("home/.claude/skills/mine")).unwrap();
        std::fs::write(dir.join("project/skills/notes/SKILL.md"), "---\nname: notes\n---\n").unwrap();
        std::fs::write(dir.join("home/.claude/skills/mine/SKILL.md"), "---\nname: mine\n---\n").unwrap();
        std::fs::write(dir.join("project/AGENTS.md"), "rules\n").unwrap();
        let m = Manifest::from_str(
            r#"
[tools]
python = {}
github = { mcp = "npm:@modelcontextprotocol/server-github@2025.9.1", env = ["GITHUB_TOKEN"] }
fs = { mcp = "npm:mcp-fs" }
docs = { url = "https://docs.example.com/mcp", headers = ["Authorization"] }
local = { command = "./bin/srv", args = ["--stdio"] }

[skills]
notes = { path = "./skills/notes", editable = true }
mine = { path = "~/.claude/skills/mine" }
pdf = { git = "https://github.com/anthropics/skills", rev = "9b1e2f0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", path = "skills/pdf" }
odd = { registry = "skills.sh", id = "x/y" }

[instructions]
files = ["AGENTS.md"]
"#,
        )
        .unwrap();
        let l = lock_peripherals(&m, None, &ctx(&dir), true).unwrap();
        let t: BTreeMap<&str, &Tool> = l.tools.iter().map(|t| (t.name.as_str(), t)).collect();
        assert_eq!(t["python"].kind, "builtin");
        assert_eq!(t["github"].args, ["-y", "@modelcontextprotocol/server-github@2025.9.1"]);
        assert_eq!(t["github"].env["GITHUB_TOKEN"], "${HLOCK_SECRET_GITHUB_GITHUB_TOKEN}");
        assert_eq!(t["docs"].headers["Authorization"], "${HLOCK_SECRET_DOCS_HEADER_AUTHORIZATION}");
        assert_eq!(t["local"].command.as_deref(), Some("./bin/srv"));
        assert!(l.warnings.iter().any(|w| w.contains("mcp fs") && w.contains("no version")));
        assert!(l.secrets.iter().any(|s| s.name == "HLOCK_SECRET_GITHUB_GITHUB_TOKEN"));

        let s: BTreeMap<&str, &Skill> = l.skills.iter().map(|s| (s.name.as_str(), s)).collect();
        assert_eq!((s["notes"].origin.as_str(), s["notes"].path.as_deref()), ("first-party", Some("skills/notes")));
        assert!(s["notes"].editable && s["notes"].evidence_digest == s["notes"].digest);
        assert_eq!(s["mine"].path.as_deref(), Some("{root}/skills/mine"));
        assert_eq!(s["mine"].vendored.as_deref(), Some(".hlock/vendor/shared/skills/mine"));
        assert!(s["pdf"].source.as_deref().unwrap().contains("@9b1e2f0aaaaaaaa"));
        assert!(l.warnings.iter().any(|w| w.contains("skill odd")));
        assert_eq!(l.instructions[0].scope, "project");
        assert!(l.vendor.iter().any(|f| f.rel.ends_with("skills/mine/SKILL.md")));

        let bad = Manifest::from_str("[skills]\nnope = { path = \"./missing\" }\n").unwrap();
        assert!(lock_peripherals(&bad, None, &ctx(&dir), true).unwrap_err().to_string().contains("no SKILL.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
