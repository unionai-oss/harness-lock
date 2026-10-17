"""resolve locks declared peripherals, previews with --dry-run/--json, and offers init."""

import json
import os
import shutil
import stat
from pathlib import Path

import pytest
from click.testing import CliRunner

from harness_lock import Lockfile
from harness_lock.cli import MANUAL_EXIT, main

REPO = Path(__file__).resolve().parent.parent
GH_TOKEN = "ghp_0123456789abcdefABCDEF0123456789"


def invoke(*args):
    return CliRunner().invoke(main, list(args))


def executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


@pytest.fixture
def project(tmp_path, monkeypatch):
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_")):
            monkeypatch.delenv(key)
    home, proj, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, proj, bindir):
        d.mkdir()
    executable(bindir / "claude", '#!/bin/sh\necho "2.1.0 (Claude Code)"\n')
    executable(bindir / "npx", "#!/bin/sh\necho 10\n")
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.chdir(proj)
    shutil.copytree(REPO / "tasks", proj / "tasks")
    (proj / "skills/notes").mkdir(parents=True)
    (proj / "skills/notes/SKILL.md").write_text("---\nname: notes\ndescription: d\n---\n")
    (home / ".claude/skills/mine").mkdir(parents=True)
    (home / ".claude/skills/mine/SKILL.md").write_text("---\nname: mine\ndescription: d\n---\n")
    (proj / "AGENTS.md").write_text("rules\n")
    return proj


MANIFEST = """
[project]
name = "demo"

[target]
harness = "claude-code"

[tasks]
packs = ["./tasks"]
seeds = 2

[candidates.llm.planner]
any_of = ["reference:qwen-7b", "reference:qwen-32b"]

[candidates]
decision = ["none", "rules:default"]
harness = ["claude-code"]

[tools]
github = { mcp = "npm:@modelcontextprotocol/server-github@2025.9.1", env = ["GITHUB_TOKEN"] }
fs = { mcp = "npm:mcp-fs" }

[skills]
notes = { path = "./skills/notes", editable = true }
mine = { path = "~/.claude/skills/mine" }

[instructions]
files = ["AGENTS.md"]

[objective]
select = "min-cost"
"""


def test_resolve_locks_declared_peripherals(project):
    (project / "harness.toml").write_text(MANIFEST)
    r = invoke("resolve", "--offline")
    assert r.exit_code == 0, r.output
    assert "locked 2 MCP servers, 2 skills, 1 instruction file" in r.output
    assert "mcp fs: npm:mcp-fs has no version" in r.output
    lock = json.loads(Lockfile.load("harness.lock").to_json())
    gh = next(t for t in lock["tool"] if t["name"] == "github")
    assert (gh["command"], gh["args"]) == ("npx", ["-y", "@modelcontextprotocol/server-github@2025.9.1"])
    assert gh["env"]["GITHUB_TOKEN"] == "${HLOCK_SECRET_GITHUB_GITHUB_TOKEN}"
    assert {s["name"] for s in lock["secret"]} == {"HLOCK_SECRET_GITHUB_GITHUB_TOKEN"}
    skills = {s["name"]: s for s in lock["skill"]}
    assert skills["notes"]["origin"] == "first-party" and skills["notes"]["digest"].startswith("sha256:")
    assert skills["mine"]["vendored"] == ".hlock/vendor/claude-code/skills/mine"
    assert (project / ".hlock/vendor/claude-code/skills/mine/SKILL.md").is_file()
    assert lock["instructions"][0]["digest"].startswith("sha256:")
    assert invoke("check").exit_code == 0

    # a missing declared skill is a manifest error, not a silent skip
    (project / "harness.toml").write_text(MANIFEST.replace("./skills/notes", "./skills/gone"))
    r = invoke("resolve", "--offline")
    assert r.exit_code == 1 and "no SKILL.md" in r.output


def test_a_resolved_lock_syncs_onto_a_new_machine(project, tmp_path, monkeypatch):
    (project / "harness.toml").write_text(MANIFEST)
    assert invoke("resolve", "--offline").exit_code == 0
    new_home = tmp_path / "new-home"
    new_home.mkdir()
    monkeypatch.setenv("HOME", str(new_home))

    r = invoke("sync", "--yes")
    assert r.exit_code == MANUAL_EXIT, r.output
    assert "export HLOCK_SECRET_GITHUB_GITHUB_TOKEN" in r.output
    state = json.loads((new_home / ".claude.json").read_text())
    assert state["mcpServers"]["github"]["args"][-1] == "@modelcontextprotocol/server-github@2025.9.1"
    assert (new_home / ".claude/skills/mine/SKILL.md").is_file()

    monkeypatch.setenv("HLOCK_SECRET_GITHUB_GITHUB_TOKEN", GH_TOKEN)
    assert invoke("sync", "--yes").exit_code == 0
    state = json.loads((new_home / ".claude.json").read_text())
    assert state["mcpServers"]["github"]["env"]["GITHUB_TOKEN"] == GH_TOKEN


def test_resolve_dry_run_and_json(project):
    (project / "harness.toml").write_text(MANIFEST)
    r = invoke("resolve", "--dry-run")
    assert r.exit_code == 0, r.output
    assert "4 stacks × 1 task × 2 seeds = 8 trials" in r.output
    assert "reference adapter: a stand-in" in r.output
    assert "2 MCP servers, 2 skills, 1 instruction file" in r.output
    assert not (project / "harness.lock").exists()

    plan = json.loads(invoke("resolve", "--json").output)
    assert (plan["stacks"], plan["tasks"], plan["seeds"], plan["trials"]) == (4, 1, 2, 8)
    assert plan["adapters"] == {"claude-code": "reference"}
    assert plan["peripherals"]["secrets"] == ["HLOCK_SECRET_GITHUB_GITHUB_TOKEN"]
    assert not (project / "harness.lock").exists()


def test_resolve_offers_init_without_a_manifest(project):
    r = invoke("resolve")
    assert r.exit_code == 1 and "hlock resolve --init" in r.output

    r = invoke("resolve", "--init", "--offline")
    assert r.exit_code == 0, r.output
    assert (project / "harness.toml").is_file() and (project / "harness.lock").is_file()
    text = (project / "harness.toml").read_text()
    assert 'packs = ["./tasks"]' in text and "notes" in text
