<p align="center">
  <img src="https://raw.githubusercontent.com/unionai-oss/hlock/main/docs/assets/logo.svg" alt="hlock logo" width="96">
</p>

# harness-lock

**`harness-lock` is an alias for [`hlock`](https://pypi.org/project/hlock/).**

hlock makes your agent harness pinnable, portable, and tunable: tune your agent stack
against your own tasks, then pin the winner in a `harness.lock`.

This package has no code of its own. Each `harness-lock` release pins the identical
`hlock` release, so both names install the same thing. New projects should depend on
the primary name:

```bash
pip install hlock        # or: uv add hlock
```

Either install gives you the `hlock` and `harness-lock` commands, and `import hlock`
and `import harness_lock` are the same API.

- 📖 **Docs:** <https://docs.hlock.ai/>
- 💻 **Source:** <https://github.com/unionai-oss/hlock>
- 🦀 **Rust:** `cargo add hlock`
