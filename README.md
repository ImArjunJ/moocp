# moocp

Moodle for your AI CLI. Courses, files, deadlines, feedback, and student actions.
Written in Rust; your CLI supplies the model.

```sh
cargo install --path . --locked
moocp setup https://your-moodle.example
```

Builds need Rust 1.88+. Browser sign-in needs Chrome, Chromium, or Edge.
Passwords and MFA stay in the browser. Publishing and submitting work require
student review.
