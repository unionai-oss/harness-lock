//! `hlock init`'s scan: find what a project and its target harness already use,
//! offer it as choices, and render the chosen ones into a commented `harness.toml`.
//!
//! [`discover`] is side-effect free. It reuses `hlock pin`'s capture (models, MCP
//! servers, skills, instruction files), the models configured under the harness's
//! providers, installed local runtimes with what fits in memory, API keys present
//! in the environment (by name only), other installed harnesses, and the project
//! directory itself (skills, instruction files, task packs, agent frameworks).
//! [`render`] turns a [`Discovery`] plus the chosen item ids into TOML.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::capture::{capture, configured_provider_models, host_port};
use crate::error::Result;
use crate::localize::{local_candidates, machine_memory, DEFAULT_RUNTIMES};
use crate::profiles::{probe_harness, probe_runtime, Ctx, Registry};

/// One thing a user can pick.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Item {
    pub id: String,
    pub label: String,
    /// Where it was found or why it's offered.
    pub detail: String,
    /// Pre-selected (in use today).
    pub selected: bool,
    /// What [`render`] writes for it.
    pub value: Value,
}

/// A group of choices that becomes one manifest section.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Section {
    pub key: String,
    pub title: String,
    pub help: String,
    /// Pick exactly one (the target harness) instead of any number.
    pub single: bool,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Discovery {
    pub dir: PathBuf,
    pub project: String,
    /// The target harness and why it was chosen.
    pub target: Option<Value>,
    pub sections: Vec<Section>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DiscoverOptions {
    pub harness: Option<String>,
    pub memory_gb: Option<f64>,
    /// Task packs given on the command line (skips scanning for them).
    pub packs: Vec<String>,
}

/// API providers hlock recognizes by their key's environment variable. Anthropic
/// also gets current model presets; the others only add a hint.
const API_KEYS: &[(&str, &str)] = &[
    ("ANTHROPIC_API_KEY", "anthropic"),
    ("OPENAI_API_KEY", "openai"),
    ("GEMINI_API_KEY", "google"),
    ("GOOGLE_API_KEY", "google"),
    ("OPENROUTER_API_KEY", "openrouter"),
    ("MISTRAL_API_KEY", "mistral"),
    ("GROQ_API_KEY", "groq"),
    ("DEEPSEEK_API_KEY", "deepseek"),
    ("XAI_API_KEY", "xai"),
    ("TOGETHER_API_KEY", "together"),
    ("FIREWORKS_API_KEY", "fireworks"),
];
const ANTHROPIC_PRESETS: &[&str] = &["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-4-5-20251001"];

/// Python packages that identify an agent framework profile.
const FRAMEWORK_PACKAGES: &[(&str, &str)] = &[
    ("pydantic-ai", "pydantic-ai"),
    ("pydantic-ai-slim", "pydantic-ai"),
    ("deepagents", "deepagents"),
    ("langgraph", "langgraph"),
    ("openai-agents", "openai-agents"),
    ("claude-agent-sdk", "claude-agent-sdk"),
    ("google-adk", "google-adk"),
    ("smolagents", "smolagents"),
    ("crewai", "crewai"),
];

/// Project directories that hold skills, for any harness.
const PROJECT_SKILL_DIRS: &[&str] =
    &["skills", ".agents/skills", ".claude/skills", ".opencode/skills", ".pi/skills", ".cursor/skills", ".gemini/skills", ".codex/skills", ".github/skills"];
const PROJECT_INSTRUCTIONS: &[&str] = &["AGENTS.md", "CLAUDE.md", "GEMINI.md", ".github/copilot-instructions.md", ".cursorrules", ".goosehints"];
const SKIP_DIRS: &[&str] = &["node_modules", "target", ".git", ".venv", "venv", "__pycache__", ".hlock", ".harnesslock", "dist", "build"];

fn rel(p: &Path, dir: &Path) -> String {
    match p.strip_prefix(dir) {
        Ok(r) => format!("./{}", r.to_string_lossy()),
        Err(_) => p.to_string_lossy().into_owned(),
    }
}

/// Directories (up to `depth` deep) under `dir` that contain `file`.
fn find_dirs_with(dir: &Path, file: &str, depth: usize) -> Vec<PathBuf> {
    fn rec(d: &Path, file: &str, depth: usize, out: &mut Vec<PathBuf>) {
        if d.join(file).is_file() {
            out.push(d.to_path_buf());
            return;
        }
        if depth == 0 {
            return;
        }
        let mut subs: Vec<PathBuf> = std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                e.path().is_dir() && !SKIP_DIRS.contains(&n.as_str()) && !(n.starts_with('.') && n != ".agents" && n != ".claude")
            })
            .map(|e| e.path())
            .collect();
        subs.sort();
        for s in subs {
            rec(&s, file, depth - 1, out);
        }
    }
    let mut out = Vec::new();
    rec(dir, file, depth, &mut out);
    out
}

/// Framework profiles whose packages the project depends on.
fn project_frameworks(dir: &Path) -> Vec<(String, String)> {
    let mut text = String::new();
    for f in ["pyproject.toml", "requirements.txt", "requirements.in", "setup.cfg", "uv.lock"] {
        if let Ok(t) = std::fs::read_to_string(dir.join(f)) {
            text.push_str(&t.to_ascii_lowercase());
            text.push('\n');
        }
    }
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    // a package name at the start of a requirement: after a quote or line start,
    // and not followed by more name characters (so `pydantic-ai` ≠ `pydantic-ai-slim`)
    let mentions = |pkg: &str| {
        text.match_indices(pkg).any(|(i, _)| {
            let before = text[..i].chars().last();
            let after = text[i + pkg.len()..].chars().next();
            matches!(before, None | Some('"' | '\'' | '\n' | ' ')) && !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
    };
    for (pkg, fw) in FRAMEWORK_PACKAGES {
        if mentions(pkg) {
            found.entry(fw.to_string()).or_insert_with(|| pkg.to_string());
        }
    }
    found.into_iter().collect()
}

fn push_unique(items: &mut Vec<Item>, item: Item) {
    if !items.iter().any(|i| i.id == item.id) {
        items.push(item);
    }
}

/// Scan `ctx.cwd` (the project directory) and the target harness.
pub fn discover(reg: &Registry, ctx: &Ctx, opts: &DiscoverOptions) -> Result<Discovery> {
    let dir = ctx.cwd.clone();
    // the project's own name (pyproject.toml, package.json), else the directory's
    let declared = std::fs::read_to_string(dir.join("pyproject.toml"))
        .ok()
        .and_then(|t| toml::from_str::<Value>(&t).ok())
        .and_then(|v| v.pointer("/project/name").and_then(Value::as_str).map(str::to_string))
        .or_else(|| {
            std::fs::read_to_string(dir.join("package.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v.get("name").and_then(Value::as_str).map(str::to_string))
        });
    let project = declared.unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "agent".into()));
    let mut notes = Vec::new();

    // ---- target harness ----
    let selection = reg.select(opts.harness.as_deref(), None, ctx).ok();
    let target = selection.as_ref().and_then(|s| reg.harness(&s.id));
    // installed agent CLIs, plus the target even if it isn't on PATH (named with --harness,
    // or detected from the environment)
    let installed: Vec<_> = reg
        .harnesses
        .iter()
        .filter(|h| h.kind == "cli")
        .filter(|h| target.is_some_and(|t| t.id == h.id) || probe_harness(h, ctx).installed)
        .collect();
    let mut target_items = Vec::new();
    for h in &installed {
        let p = probe_harness(h, ctx);
        let is_target = target.is_some_and(|t| t.id == h.id);
        target_items.push(Item {
            id: h.id.clone(),
            label: h.name.clone(),
            detail: format!(
                "{}{}",
                p.version.unwrap_or_else(|| if p.installed { String::new() } else { "not installed".into() }),
                if is_target { format!(" · {}", selection.as_ref().map(|s| s.reason.as_str()).unwrap_or("")) } else { String::new() }
            ),
            selected: is_target,
            value: json!(h.id),
        });
    }
    if target.is_none() {
        notes.push(match &selection {
            None if installed.len() > 1 => "several harnesses are installed; pick the target below (or pass --harness)".into(),
            None => "no agent harness found; the manifest will use hlock's reference harness".into(),
            Some(_) => String::new(),
        });
    }

    // ---- what the target harness uses today ----
    let cap = match target {
        Some(t) if t.kind == "cli" => Some(capture(t, reg, ctx, Some(&project))?),
        _ => None,
    };
    let tname = target.map(|t| t.name.clone()).unwrap_or_default();

    // ---- LLM candidates ----
    let mut llm: Vec<Item> = Vec::new();
    if let Some(c) = &cap {
        for l in c.lock.llm.iter().filter(|l| l.model != "harness-default") {
            let id = format!("{}:{}", l.provider, l.model);
            push_unique(&mut llm, Item { id: id.clone(), label: id.clone(), detail: format!("{tname} {} model", l.role), selected: true, value: json!(id) });
        }
        if c.lock.llm.iter().all(|l| l.model == "harness-default") {
            notes.push(format!("{tname} uses its built-in default model; hlock couldn't read which one"));
        }
    }
    if let Some(t) = target {
        for (provider, model, base) in configured_provider_models(t, ctx) {
            let id = format!("{provider}:{model}");
            let host = base.as_deref().and_then(host_port).map(|(h, _)| format!(" @ {h}")).unwrap_or_default();
            push_unique(&mut llm, Item { id: id.clone(), label: id.clone(), detail: format!("configured in {tname}{host}"), selected: false, value: json!(id) });
        }
    }
    let mem = machine_memory(ctx);
    let usable = opts.memory_gb.or(mem.as_ref().map(|m| m.1));
    let mut small_local: Option<String> = None;
    for rt in DEFAULT_RUNTIMES.iter().filter_map(|r| reg.runtime(r)) {
        if !probe_runtime(rt, ctx).installed {
            continue;
        }
        let mut cands = local_candidates(rt, usable, ctx);
        cands.sort_by(|a, b| b.present.cmp(&a.present).then(b.tier.cmp(&a.tier)));
        for c in cands.iter().take(8) {
            let id = format!("{}:{}", rt.id, c.id);
            let state = if c.present { "on disk" } else { "not downloaded" };
            let fit = usable.map(|u| format!(", fits {u} GB")).unwrap_or_default();
            push_unique(&mut llm, Item { id: id.clone(), label: id.clone(), detail: format!("{} · {state}{fit}", rt.name), selected: false, value: json!(id) });
        }
        if small_local.is_none() {
            // the smallest model of known size (unknown sizes sort last)
            let size = |c: &&crate::localize::Candidate| c.params_b.unwrap_or(f64::MAX);
            small_local = cands
                .iter()
                .filter(|c| c.tier == 1 && c.tools && c.params_b.is_some())
                .min_by(|a, b| size(a).partial_cmp(&size(b)).unwrap_or(std::cmp::Ordering::Equal))
                .map(|c| format!("{}:{}", rt.id, c.id));
        }
    }
    let mut keyed = BTreeSet::new();
    for (var, provider) in API_KEYS {
        if ctx.var(var).is_some() && keyed.insert(*provider) {
            if *provider == "anthropic" {
                for m in ANTHROPIC_PRESETS {
                    let id = format!("anthropic:{m}");
                    push_unique(&mut llm, Item { id: id.clone(), label: id.clone(), detail: format!("{var} is set"), selected: false, value: json!(id) });
                }
            } else if !llm.iter().any(|i| i.id.starts_with(&format!("{provider}:"))) {
                notes.push(format!("{var} is set; add {provider}:<model> to [candidates.llm.planner] to try {provider} models"));
            }
        }
    }

    // ---- harness candidates ----
    let harness_items: Vec<Item> = installed
        .iter()
        .map(|h| Item {
            id: h.id.clone(),
            label: h.name.clone(),
            detail: if target.is_some_and(|t| t.id == h.id) { "the target".into() } else { "installed".into() },
            selected: target.is_some_and(|t| t.id == h.id),
            value: json!(h.id),
        })
        .collect();

    // ---- decision models ----
    let mut decision = vec![
        Item { id: "none".into(), label: "none".into(), detail: "no decision model".into(), selected: true, value: json!("none") },
        Item { id: "rules:default".into(), label: "rules:default".into(), detail: "rule-based retries and stops".into(), selected: true, value: json!("rules:default") },
    ];
    if let Some(s) = &small_local {
        let id = format!("llm:{s}");
        decision.push(Item { id: id.clone(), label: id.clone(), detail: "a small local model decides".into(), selected: false, value: json!(id) });
    }

    // ---- MCP servers ----
    let mut tools = Vec::new();
    if let Some(c) = &cap {
        for t in c.lock.tool.iter().filter(|t| t.kind == "mcp") {
            let mut v = serde_json::Map::new();
            match (&t.source, &t.url) {
                (Some(src), _) if src != "remote" => {
                    v.insert("mcp".into(), json!(src));
                }
                (_, Some(url)) => {
                    v.insert("url".into(), json!(url));
                    v.insert("transport".into(), json!(t.transport.clone().unwrap_or_else(|| "http".into())));
                }
                _ => {
                    v.insert("command".into(), json!(t.command));
                    v.insert("args".into(), json!(t.args));
                }
            }
            // secrets by name only: environment variables, and headers for remote servers
            if !t.env.is_empty() {
                v.insert("env".into(), json!(t.env.keys().collect::<Vec<_>>()));
            }
            if !t.headers.is_empty() {
                v.insert("headers".into(), json!(t.headers.keys().collect::<Vec<_>>()));
            }
            let scope = t.scope.clone().unwrap_or_default();
            let off = if t.enabled == Some(false) { ", disabled" } else { "" };
            tools.push(Item {
                id: t.name.clone(),
                label: t.name.clone(),
                detail: format!("{tname} ({scope}{off}) · {}", t.source.clone().or(t.url.clone()).or(t.command.clone()).unwrap_or_default()),
                selected: t.enabled != Some(false),
                value: json!({ "name": t.name, "spec": Value::Object(v) }),
            });
        }
    }

    // ---- skills ----
    let mut skills = Vec::new();
    let mut seen_paths = BTreeSet::new();
    for sd in PROJECT_SKILL_DIRS {
        for d in find_dirs_with(&dir.join(sd), "SKILL.md", 2) {
            let name = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let path = rel(&d, &dir);
            if seen_paths.insert(path.clone()) {
                skills.push(Item {
                    id: format!("project:{name}"),
                    label: name.clone(),
                    detail: format!("project · {path}"),
                    selected: true,
                    value: json!({ "name": name, "spec": { "path": path, "editable": true } }),
                });
            }
        }
    }
    if let Some(c) = &cap {
        for s in c.lock.skill.iter().filter(|s| !matches!(s.origin.as_str(), "harness-builtin" | "bundled" | "harness-managed")) {
            if s.scope.as_deref() == Some("project") {
                continue; // found above
            }
            let spec = match s.source.as_deref().and_then(|x| x.strip_prefix("git+")) {
                Some(src) => {
                    let (url_rev, sub) = src.split_once("#path=").unwrap_or((src, ""));
                    let (url, rev) = url_rev.rsplit_once('@').unwrap_or((url_rev, ""));
                    json!({ "git": url, "rev": rev, "path": sub })
                }
                None => {
                    let root = target.map(|t| t.config_root(ctx));
                    let p = s.path.clone().unwrap_or_default().replace("{root}", &root.map(|r| r.to_string_lossy().into_owned()).unwrap_or_default());
                    let p = match p.strip_prefix(&ctx.home.to_string_lossy().into_owned()) {
                        Some(rest) => format!("~{rest}"),
                        None => p,
                    };
                    json!({ "path": p })
                }
            };
            let key = format!("{}:{}", s.origin, s.name);
            if skills.iter().any(|i: &Item| i.label == s.name) {
                continue;
            }
            skills.push(Item { id: key, label: s.name.clone(), detail: format!("{tname} · {}", s.origin), selected: true, value: json!({ "name": s.name, "spec": spec }) });
        }
        let managed = c.lock.skill.iter().filter(|s| matches!(s.origin.as_str(), "harness-builtin" | "bundled" | "harness-managed")).count();
        if managed > 0 {
            notes.push(format!("{managed} skills that {tname} provides itself (built-in, plugin or synced) aren't listed"));
        }
    }

    // ---- instruction files ----
    let mut instructions = Vec::new();
    for f in PROJECT_INSTRUCTIONS {
        if dir.join(f).is_file() {
            instructions.push(Item { id: format!("project:{f}"), label: f.to_string(), detail: "project".into(), selected: true, value: json!(f) });
        }
    }
    if let Some(c) = &cap {
        for i in c.lock.instructions.iter().filter(|i| i.scope == "user") {
            let root = target.map(|t| t.config_root(ctx).to_string_lossy().into_owned()).unwrap_or_default();
            let p = i.name.replace("{root}", &root);
            let p = match p.strip_prefix(&ctx.home.to_string_lossy().into_owned()) {
                Some(rest) => format!("~{rest}"),
                None => p,
            };
            instructions.push(Item { id: format!("user:{}", i.name), label: p.clone(), detail: format!("{tname} (user)"), selected: true, value: json!(p) });
        }
    }

    // ---- task packs ----
    let mut tasks = Vec::new();
    if opts.packs.is_empty() {
        for d in find_dirs_with(&dir, "pack.toml", 4) {
            let path = rel(&d, &dir);
            tasks.push(Item { id: path.clone(), label: path.clone(), detail: "task pack".into(), selected: true, value: json!(path) });
        }
        if tasks.is_empty() {
            notes.push("no task pack (pack.toml) found; add one before `hlock resolve` (see the task packs guide)".into());
        }
    } else {
        for p in &opts.packs {
            tasks.push(Item { id: p.clone(), label: p.clone(), detail: "--pack".into(), selected: true, value: json!(p) });
        }
    }

    // ---- code harnesses ----
    let mut code = Vec::new();
    for (fw, pkg) in project_frameworks(&dir) {
        code.push(Item {
            id: fw.clone(),
            label: format!("{project} ({fw})"),
            detail: format!("depends on {pkg}"),
            selected: false,
            value: json!({ "name": project, "framework": fw }),
        });
    }

    if !code.is_empty() {
        let fws: Vec<&str> = code.iter().map(|i| i.id.as_str()).collect();
        notes.push(format!("the project uses {}; choose \"Your own agent\" to add a [harnesses] stub for it", fws.join(", ")));
    }

    let mut sections = vec![
        Section { key: "target".into(), title: "Target harness".into(), help: "the harness this lock is for".into(), single: true, items: target_items },
        Section { key: "llm".into(), title: "LLM candidates".into(), help: "models resolve may try for the planner role".into(), single: false, items: llm },
        Section { key: "harness".into(), title: "Harness candidates".into(), help: "harnesses resolve may try".into(), single: false, items: harness_items },
        Section { key: "decision".into(), title: "Decision models".into(), help: "how retries, escalation and stopping are decided".into(), single: false, items: decision },
        Section { key: "tools".into(), title: "MCP servers".into(), help: "recorded in [tools]".into(), single: false, items: tools },
        Section { key: "skills".into(), title: "Skills".into(), help: "recorded in [skills]".into(), single: false, items: skills },
        Section { key: "instructions".into(), title: "Instruction files".into(), help: "recorded in [instructions]".into(), single: false, items: instructions },
        Section { key: "tasks".into(), title: "Task packs".into(), help: "what resolve measures against".into(), single: false, items: tasks },
        Section { key: "code".into(), title: "Your own agent".into(), help: "adds a [harnesses.<id>] stub to fill in".into(), single: false, items: code },
    ];
    sections.retain(|s| !s.items.is_empty());
    notes.retain(|n| !n.is_empty());
    Ok(Discovery {
        dir,
        project,
        target: selection.map(|s| json!({ "id": s.id, "reason": s.reason })),
        sections,
        notes,
    })
}

// --------------------------------------------------------------------------- //
// Rendering
// --------------------------------------------------------------------------- //

fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn key(k: &str) -> String {
    if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        k.to_string()
    } else {
        q(k)
    }
}

/// A JSON value as an inline TOML value.
fn inline(v: &Value) -> String {
    match v {
        Value::String(s) => q(s),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => format!("[{}]", a.iter().map(inline).collect::<Vec<_>>().join(", ")),
        Value::Object(m) => {
            // the identifying key first (`path`, `git`, `mcp`, …), then the rest
            const ORDER: &[&str] = &["mcp", "url", "transport", "command", "args", "git", "rev", "path", "editable", "env", "headers"];
            let mut keys: Vec<&String> = m.keys().filter(|k| !m[*k].is_null()).collect();
            keys.sort_by_key(|k| ORDER.iter().position(|o| o == k).unwrap_or(ORDER.len()));
            format!("{{ {} }}", keys.iter().map(|k| format!("{} = {}", key(k), inline(&m[*k]))).collect::<Vec<_>>().join(", "))
        }
        Value::Null => "\"\"".into(),
    }
}

/// `items` chosen in `section` (by id), in discovery order.
fn chosen<'a>(d: &'a Discovery, section: &str, picks: &BTreeMap<String, Vec<String>>) -> Vec<&'a Item> {
    let Some(s) = d.sections.iter().find(|s| s.key == section) else { return vec![] };
    match picks.get(section) {
        Some(ids) => s.items.iter().filter(|i| ids.contains(&i.id)).collect(),
        None => s.items.iter().filter(|i| i.selected).collect(),
    }
}

fn list_block(items: &[&Item]) -> String {
    let mut out = String::from("[\n");
    for i in items {
        out.push_str(&format!("  {},  # {}\n", inline(&i.value), i.detail));
    }
    out.push(']');
    out
}

/// Render a manifest from a discovery and the ids chosen per section (sections
/// missing from `picks` use their pre-selected items).
pub fn render(d: &Discovery, picks: &BTreeMap<String, Vec<String>>, name: Option<&str>) -> String {
    let name = name.unwrap_or(&d.project);
    let mut out = String::new();
    let target_item = chosen(d, "target", picks).first().copied();
    let target = target_item.map(|i| (i.id.clone(), i.detail.clone()));
    out.push_str(&format!(
        "# hlock manifest, generated by `hlock init`{}.\n",
        target_item.map(|i| format!(" from your {} setup", i.label)).unwrap_or_default()
    ));
    out.push_str("# `hlock resolve` searches [candidates]. [tools], [skills] and [instructions] are\n# recorded in the lock but not searched yet. Comments say where each entry came from.\n\n");
    out.push_str(&format!("[project]\nname = {}\n\n", q(name)));

    if let Some((t, detail)) = &target {
        out.push_str(&format!("[target]\nharness = {}  # {}\n\n", q(t), detail));
    } else {
        out.push_str("# [target]\n# harness = \"claude-code\"      # the harness this lock is for (see: hlock harness list)\n\n");
    }

    let tasks = chosen(d, "tasks", picks);
    out.push_str("[tasks]\n");
    if tasks.is_empty() {
        out.push_str("packs = [\"./tasks\"]  # TODO: no task pack found; point this at a directory with a pack.toml\n");
    } else {
        out.push_str(&format!("packs = {}\n", inline(&json!(tasks.iter().map(|i| i.value.clone()).collect::<Vec<_>>()))));
    }
    out.push_str("seeds = 2\n\n");

    let llm = chosen(d, "llm", picks);
    out.push_str("[candidates.llm.planner]\n");
    if llm.is_empty() {
        out.push_str("any_of = [\"reference:qwen-7b\", \"reference:qwen-32b\"]  # hlock's offline reference models\n\n");
    } else {
        out.push_str(&format!("any_of = {}\n\n", list_block(&llm)));
    }

    out.push_str("[candidates]\n");
    let decision = chosen(d, "decision", picks);
    let decisions: Vec<Value> = if decision.is_empty() { vec![json!("none")] } else { decision.iter().map(|i| i.value.clone()).collect() };
    out.push_str(&format!("decision = {}\n", inline(&json!(decisions))));
    let mut harness: Vec<Value> = chosen(d, "harness", picks).iter().map(|i| i.value.clone()).collect();
    if let Some((t, _)) = &target {
        if !harness.contains(&json!(t)) {
            harness.insert(0, json!(t));
        }
    }
    if harness.is_empty() {
        harness.push(json!("reference"));
    }
    out.push_str(&format!("harness = {}\n\n", inline(&json!(harness))));

    let tools = chosen(d, "tools", picks);
    if !tools.is_empty() {
        out.push_str("[tools]  # MCP servers; secrets are listed by name, never stored here\n");
        for i in tools {
            let n = i.value["name"].as_str().unwrap_or(&i.id);
            out.push_str(&format!("{} = {}  # {}\n", key(n), inline(&i.value["spec"]), i.detail));
        }
        out.push('\n');
    }
    let skills = chosen(d, "skills", picks);
    if !skills.is_empty() {
        out.push_str("[skills]\n");
        for i in skills {
            let n = i.value["name"].as_str().unwrap_or(&i.label);
            out.push_str(&format!("{} = {}  # {}\n", key(n), inline(&i.value["spec"]), i.detail));
        }
        out.push('\n');
    }
    let ins = chosen(d, "instructions", picks);
    if !ins.is_empty() {
        out.push_str(&format!("[instructions]\nfiles = {}\n\n", list_block(&ins)));
    }

    out.push_str("[objective]\nprimary = \"success-rate\"\nselect = \"min-cost\"\n\n[objective.constraints]\nsuccess-rate = \">=0.5\"\n\n");
    out.push_str("[budget]\nstrategy = \"grid\"\nmax_trials = 200\n\n[backend]\ntarget = \"local\"\n");

    for i in chosen(d, "code", picks) {
        let n = i.value["name"].as_str().unwrap_or("my-agent");
        let fw = i.value["framework"].as_str().unwrap_or("");
        out.push_str(&format!(
            "\n# Your own agent ({}). Fill in the entrypoint, then target it above.\n# [harnesses.{}]\n# framework = {}\n# entrypoint = \"your_module:run\"   # called as run(prompt, ctx)\n",
            i.detail,
            key(n),
            q(fw)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, selected: bool, value: Value) -> Item {
        Item { id: id.into(), label: id.into(), detail: "found".into(), selected, value }
    }

    #[test]
    fn renders_a_parseable_manifest() {
        let d = Discovery {
            dir: "/p".into(),
            project: "demo".into(),
            target: None,
            notes: vec![],
            sections: vec![
                Section { key: "target".into(), single: true, items: vec![item("claude-code", true, json!("claude-code"))], ..Default::default() },
                Section { key: "llm".into(), items: vec![item("anthropic:opus", true, json!("anthropic:opus")), item("ollama:qwen3:8b", false, json!("ollama:qwen3:8b"))], ..Default::default() },
                Section { key: "tools".into(), items: vec![item("gh", true, json!({"name": "github", "spec": {"mcp": "npm:@x/gh", "env": ["GITHUB_TOKEN"]}}))], ..Default::default() },
                Section { key: "skills".into(), items: vec![item("s", true, json!({"name": "my skill", "spec": {"path": "./skills/s", "editable": true}}))], ..Default::default() },
                Section { key: "tasks".into(), items: vec![item("./tasks", true, json!("./tasks"))], ..Default::default() },
                Section { key: "code".into(), items: vec![item("pydantic-ai", true, json!({"name": "demo", "framework": "pydantic-ai"}))], ..Default::default() },
            ],
        };
        let text = render(&d, &BTreeMap::new(), None);
        let m = crate::Manifest::from_str(&text).unwrap();
        assert_eq!(m.target_harness().as_deref(), Some("claude-code"));
        assert_eq!(m.candidates.llm["planner"].any_of, ["anthropic:opus"]);
        assert_eq!(m.candidates.harness[0].name, "claude-code");
        assert_eq!(m.tools["github"]["mcp"], "npm:@x/gh");
        assert_eq!(m.skills["my skill"]["editable"], true);
        assert_eq!(m.task_packs(), ["./tasks"]);
        assert!(m.harnesses.is_empty() && text.contains("# [harnesses.demo]"));

        // explicit picks override the defaults
        let picks = BTreeMap::from([("llm".to_string(), vec!["ollama:qwen3:8b".to_string()]), ("tools".to_string(), vec![])]);
        let m = crate::Manifest::from_str(&render(&d, &picks, Some("x"))).unwrap();
        assert_eq!(m.candidates.llm["planner"].any_of, ["ollama:qwen3:8b"]);
        assert!(m.tools.is_empty());
        assert_eq!(m.name(), "x");
    }

    #[test]
    fn empty_discovery_falls_back_to_reference() {
        let d = Discovery { project: "p".into(), ..Default::default() };
        let m = crate::Manifest::from_str(&render(&d, &BTreeMap::new(), None)).unwrap();
        assert_eq!(m.candidates.harness[0].name, "reference");
        assert!(m.candidates.llm["planner"].any_of[0].starts_with("reference:"));
    }

    #[test]
    fn frameworks_from_pyproject() {
        let dir = std::env::temp_dir().join(format!("hlock-fw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "[project]\ndependencies = [\"pydantic-ai-slim[openai]>=1\", \"httpx\"]\n").unwrap();
        assert_eq!(project_frameworks(&dir), [("pydantic-ai".to_string(), "pydantic-ai-slim".to_string())]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
