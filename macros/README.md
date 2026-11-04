# Macros

Each folder here is one macro for the psconnector-view bar. A folder needs a `macro.json`:

```json
{
  "name": "Shown in the bar (defaults to the folder name)",
  "cmd": "python3 -u main.py",
  "env": {"KEY": "value"},
  "limit_min": 0,
  "after": "none"
}
```

| Field | Meaning |
|---|---|
| `cmd` | Command line to run (required). It runs inside the macro's folder, so relative paths work |
| `name` | Name in the bar. Defaults to the folder name |
| `cwd` | Run somewhere else instead (relative to the macro's folder) |
| `env` | Extra environment variables |
| `limit_min`, `after` | Default run time limit (0 = none) and what to do afterwards (`none` / `standby`) |

psconnector-view reads the `macros` folder next to the program (in a development build, this folder in the
repository). Put a macro folder here, or link a whole repository:

```bash
ln -s ~/Project/codevein-macro macros/codevein
```

Your own macros here are ignored by git; only the examples are part of the repository.

Then press **Reload list** in the bar. Macros get the command port in `PSCONNECTOR_PORT`; from Python, use
`python/psconnector.py`.
