# Getting started

[Documentation](README.md) · [Project home](../README.md)

Choose the terminal shell for interactive work, or the Python CLI for scripted pipelines.
Both use the same repository. Keep the checkout available after installation: it contains
the prompts, plans and Python runtime the installed binary uses.

## Install the terminal shell

You need git, a current stable [Rust toolchain](https://www.rust-lang.org/tools/install),
and at least one supported agent CLI. Install Python 3.11+ too for research, Python task
checks, and the Python fallback runtime. Rust 2024 edition support is required by `cli/Cargo.toml`.

```sh
git clone https://github.com/SkylerLK510/Ringo.git
cd Ringo
cargo install --locked --path cli
```

Cargo installs `ringo` into `~/.cargo/bin` (or the corresponding Cargo bin folder on
Windows). Make sure that folder is on your `PATH`.

Open a terminal in the project you want to work on and type:

```sh
ringo
```

First-run setup helps you select installed agents and sign in. Setup/status checks
make no model calls; sending a task uses the selected account's allowance.
Use `ringo setup` to add an agent later.

Try `explain how this project is organized`, then `/usage`. To choose an agent, put
its name first: `@codex explain the test suite` or `@claude review the changes`.
The [user guide](USER-GUIDE.md) covers permissions, input editing, images, undo,
agent handoffs and restarting a session.

The binary finds its runtime at the checkout it was built from. If you move the
checkout, set `RINGO_HOME` to its new absolute path.

### Platform notes

- **macOS and Linux:** the shell and native pipeline runtime are exercised in CI.
- **Windows:** the shell builds in CI; pipelines use the Python runtime. Native
  Windows provider calls still need live verification. WSL 2 is another option.
- **Build failure:** update to stable Rust, check that Cargo and git are on your
  `PATH`, and include the compiler error when [reporting a problem](https://github.com/SkylerLK510/Ringo/issues/new/choose).

## Use the Python CLI

The Python runtime uses only the standard library. You need:

- Python 3.11 or newer
- git, or a ZIP download
- at least one model CLI, to make model calls (the offline tests need none)

### macOS

```sh
brew install python git                    # Homebrew; or install Python from python.org
git clone https://github.com/SkylerLK510/Ringo.git
cd Ringo
python3 --version                          # must print 3.11 or newer
python3 -m unittest discover -s tests -q   # offline check: makes no model calls
python3 ringo.py --help
```

The `python3` that comes with Apple's command-line tools may be older than 3.11. If it is, use the Homebrew or
python.org Python.

### Linux

```sh
sudo apt update && sudo apt install -y python3 git   # Debian, Ubuntu
# sudo dnf install -y python3 git                    # Fedora, RHEL
# sudo pacman -S python git                          # Arch
git clone https://github.com/SkylerLK510/Ringo.git
cd Ringo
python3 --version                          # must print 3.11 or newer
python3 -m unittest discover -s tests -q
python3 ringo.py --help
```

Ubuntu 24.04 and later ship Python 3.12. On an older release, install Python 3.11 or newer first.

### Windows

Ringo runs natively with Python 3.11 or newer, and the offline test suite passes on Windows in CI. Real model calls
have so far been run only on macOS.

- Native Windows refuses `.cmd` and `.bat` backend and check programs. Call the underlying executable directly, for
  example `node.exe` with the CLI's JavaScript entry point.
- Private folders are locked to your Windows account. [Portability](EXPERIMENTS.md#9-portability) explains
  what that covers and what it doesn't.
- `/copy` keeps Unicode text intact: text with a non-ASCII letter goes to PowerShell's `Set-Clipboard`, which takes
  it, rather than `clip`, which reads the console codepage and garbles it.
- Agents the shell starts run in a process group of their own, so Ctrl-C in the shell stops the shell alone, and
  `/kill` force-stops the process tree (`taskkill /T /F`) off the UI thread. Windows does not wait for a grace
  period: a parent that exits during that wait can leave descendants beyond `taskkill`'s reach.

To run Ringo as on Linux instead, use WSL 2 (the Windows Subsystem for Linux):

1. Open PowerShell as Administrator, run `wsl --install`, then restart Windows. This installs WSL 2 with Ubuntu.
2. Open **Ubuntu** from the Start menu and follow the Linux steps above inside it.
3. Install the model CLIs inside Ubuntu too, using the Linux commands below, so Ringo can find them. Under WSL 2,
   `/copy` also tries Windows' own `clip.exe` (WSL puts it on your PATH) where the Linux tools are missing.

### Without git

On the repository page, choose **Code**, then **Download ZIP**, or download an archive from
[Releases](https://github.com/SkylerLK510/Ringo/releases). Unzip it, open a terminal in the folder, and run the
checks above.

### Model CLIs

Install and sign in to at least one supported CLI using its official instructions:

| CLI | Setup | Pipeline backend |
| --- | --- | --- |
| Codex | [Official setup](https://github.com/openai/codex#quickstart) | `codex-chatgpt` |
| Claude Code | [Official setup](https://code.claude.com/docs/en/setup) | `claude` |
| Ollama | [Official download](https://ollama.com/download) | `ollama:MODEL` |

The shell also supports Antigravity, Grok and custom agents; see the
[shell reference](SHELL.md#first-run). Ollama is a pipeline backend.
The shell uses subscription sign-ins. For pipeline authentication and custom commands,
see [backends](CLI.md#backends).

### First run

From the checkout, choose one of these examples. Each sends model calls using your
existing sign-in. Use a new output directory for every run.

```sh
python3 ringo.py work --spec examples/v3-codex.json --out ~/ringo-runs/first-run
# or, with Claude Code:
python3 ringo.py work --spec examples/v3-claude.json --out ~/ringo-runs/first-run-claude
```

The example asks for a small `slugify` function with explicit rules. Ringo:

1. writes `slug.py`
2. runs the example's check against it
3. reviews the result
4. puts the version that's ready for your review in `~/ringo-runs/first-run/final/`

`report.json` lists the rounds, checks and token usage. Our first run of the Codex example took one round, two model
calls, about 2,800 tokens and 14 seconds.

Run folders must be outside the Ringo checkout, which keeps private material out of the repository. Ringo creates
them with owner-only permissions.
