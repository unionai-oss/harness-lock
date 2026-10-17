"""`hlock init`: scan the project and the target harness, then write harness.toml."""

import json
import os
import stat
from pathlib import Path

import pytest
from click.testing import CliRunner

from harness_lock import Manifest
from harness_lock.cli import main

REPO = Path(__file__).resolve().parent.parent
GH_TOKEN = "ghp_0123456789abcdefABCDEF0123456789"


def invoke(*args, input=None):
    return CliRunner().invoke(main, list(args), input=input)


def executable(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


@pytest.fixture
def setup(tmp_path, monkeypatch):
    """A Claude Code user with an MCP server, a skill and Ollama, in a project with
    a skill, AGENTS.md, a task pack and a pydantic-ai dependency."""
    for key in list(os.environ):
        if key.startswith(("CLAUDE", "CODEX_", "PI_", "OPENCODE", "HERMES_", "CURSOR_", "HLOCK_", "XDG_", "OLLAMA_")) \
                or key.endswith("_API_KEY"):
            monkeypatch.delenv(key)
    home, project, bindir = tmp_path / "home", tmp_path / "project", tmp_path / "bin"
    for d in (home, project, bindir):
        d.mkdir()
    executable(bindir / "claude", '#!/bin/sh\necho "2.1.0 (Claude Code)"\n')
    executable(bindir / "ollama", "#!/bin/sh\necho 'ollama version is 0.12.0'\n")
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("PATH", f"{bindir}{os.pathsep}/usr/bin{os.pathsep}/bin")
    monkeypatch.setenv("NO_COLOR", "1")
    monkeypatch.setenv("CLAUDECODE", "1")
    monkeypatch.setenv("ANTHROPIC_API_KEY", "sk-ant-test-0123456789")
    monkeypatch.chdir(project)

    (home / ".claude/skills/mine").mkdir(parents=True)
    (home / ".claude/skills/mine/SKILL.md").write_text("---\nname: mine\ndescription: d\n---\n")
    (home / ".claude/settings.json").write_text(json.dumps({"model": "opus"}))
    (home / ".claude.json").write_text(json.dumps({"mcpServers": {"github": {
        "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": GH_TOKEN}}}}))
    m = home / ".ollama/models/manifests/registry.ollama.ai/library/qwen3/8b"
    m.parent.mkdir(parents=True)
    m.write_text(json.dumps({"config": {"size": 1}, "layers": [{"size": 5_000_000_000}]}))

    (project / "skills/release-notes").mkdir(parents=True)
    (project / "skills/release-notes/SKILL.md").write_text("---\nname: release-notes\ndescription: d\n---\n")
    (project / "AGENTS.md").write_text("# rules\n")
    (project / "tasks").mkdir()
    (project / "tasks/pack.toml").write_text((REPO / "tasks/pack.toml").read_text())
    (project / "pyproject.toml").write_text('[project]\nname = "triage"\ndependencies = ["pydantic-ai-slim>=1"]\n')
    return project


def test_init_scans_and_writes(setup):
    r = invoke("init", "--yes", "--memory-gb", "16")
    assert r.exit_code == 0, r.output
    text = (setup / "harness.toml").read_text()
    assert 'harness = "claude-code"' in text
    assert '"anthropic:opus",  # Claude Code default model' in text
    assert 'github = { mcp = "npm:@modelcontextprotocol/server-github", env = ["GITHUB_TOKEN"] }' in text
    assert 'release-notes = { path = "./skills/release-notes", editable = true }' in text
    assert 'mine = { path = "~/.claude/skills/mine" }' in text
    assert '"AGENTS.md"' in text and 'packs = ["./tasks"]' in text
    assert GH_TOKEN not in text and "sk-ant-test" not in text
    assert "ollama:qwen3:8b" not in text  # offered, not pre-selected
    assert "pydantic-ai" in r.output  # noted, not added without asking
    m = Manifest.load(str(setup / "harness.toml"))
    assert m.target_harness == "claude-code" and m.task_packs() == ["./tasks"]
    assert invoke("resolve").exit_code == 0  # the generated manifest resolves


def test_init_offers_what_it_found(setup):
    r = invoke("init", "--json", "--memory-gb", "16")
    d = json.loads(r.output)
    sections = {s["key"]: {i["id"]: i for i in s["items"]} for s in d["sections"]}
    assert sections["llm"]["anthropic:opus"]["selected"]
    assert "ollama:qwen3:8b" in sections["llm"] and "anthropic:claude-sonnet-5-5" in sections["llm"]
    assert "llm:ollama:qwen3:1.7b" in sections["decision"]
    assert "pydantic-ai" in sections["code"]
    assert not (setup / "harness.toml").exists()


def test_init_interactive_choices(setup):
    discovery = json.loads(invoke("init", "--json", "--memory-gb", "16").output)
    order = [s["key"] for s in discovery["sections"]]
    llm = next(s for s in discovery["sections"] if s["key"] == "llm")["items"]
    ollama = next(k for k, i in enumerate(llm, 1) if i["id"] == "ollama:qwen3:8b")
    answers = {
        "target": "",                  # keep Claude Code
        "llm": f"1 {ollama}",          # drop opus, add the local model
        "tools": "n",                  # no MCP servers
        "code": "1",                   # add the pydantic-ai stub
    }
    lines = []
    for key in order:
        if answers.get(key):
            lines.append(answers[key])
        lines.append("")               # Enter finishes each section
    r = invoke("init", "--interactive", "--memory-gb", "16", input="\n".join(lines) + "\n")
    assert r.exit_code == 0, r.output
    text = (setup / "harness.toml").read_text()
    assert '"ollama:qwen3:8b"' in text and '"anthropic:opus"' not in text
    assert "\n[tools]" not in text
    assert '# [harnesses.triage]\n# framework = "pydantic-ai"' in text


def test_init_options(setup, tmp_path):
    r = invoke("init", "--dry-run", "--yes")
    assert r.exit_code == 0 and "[candidates.llm.planner]" in r.output
    assert not (setup / "harness.toml").exists()

    assert invoke("init", "--yes").exit_code == 0
    r = invoke("init", "--yes")
    assert r.exit_code == 1 and "--force" in r.output

    r = invoke("init", "--template", "--force", "--pack", "./tasks")
    assert r.exit_code == 0 and "starter template" in r.output
    assert "reference:qwen-7b" in (setup / "harness.toml").read_text()

    other = tmp_path / "other"
    (other / "sub").mkdir(parents=True)
    assert invoke("init", "--yes", "--dir", str(other)).exit_code == 0
    assert (other / "harness.toml").is_file()
    assert invoke("-m", str(other / "agent.toml"), "init", "--yes").exit_code == 0
    assert (other / "agent.toml").is_file()

    os.chdir(other / "sub")  # inside a project: init writes here, not over the parent's
    assert invoke("init", "--yes").exit_code == 0
    assert (other / "sub" / "harness.toml").is_file()
