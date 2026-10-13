# moocp

Moodle for your AI CLI. Courses, files, deadlines, feedback, and student actions.
Written in Rust. Uses your CLI's model.

## Paste this into your agent

Works with Codex, Claude Code, and Gemini CLI:

```text
Set up https://github.com/ImArjunJ/moocp for this CLI.
Read and follow docs/SETUP.md from that repo. Ask me for my Moodle URL
if you don't already have it. Handle installation and MCP configuration;
I'll complete the browser sign-in when it opens.
```

Your agent handles the setup. You sign in through your university as usual.
Restart the CLI session if it asks you to.

## Or use the terminal

macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/ImArjunJ/moocp/main/install.sh | sh
~/.local/bin/moocp setup https://your-moodle.example --client codex
```

Swap `codex` for `claude` or `gemini`. [Windows and source installs](docs/SETUP.md#2-install).
Chrome, Chromium, or Edge is needed for sign-in.

Then ask something like “what coursework is due this week?” or “find the lecture
that explains this and cite it.” Publishing and submitting work get your review.

[Usage](docs/GUIDE.md) · [Coverage](docs/COMPATIBILITY.md) · [Development](docs/DEVELOPMENT.md)
