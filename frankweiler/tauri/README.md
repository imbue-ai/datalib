# Frankweiler Tauri shell

v0 stub. The actual Tauri v2 project will live here once we are ready to bundle.
The Tauri bundler is **not** owned by Bazel; it is driven directly via
`pnpm tauri` / `cargo tauri`, and depends on:

- `frankweiler/ui/` (built as `dist/` by Vite)
- `frankweiler/backend/tauri-backend` (Rust lib crate exposing commands)

When we wire this up:

```
frankweiler/tauri/
  Cargo.toml          # bin crate, depends on frankweiler-tauri-backend
  tauri.conf.json     # bundle identifier "com.imbue.frankweiler"
  src/main.rs         # tauri::Builder::default().run()
  icons/              # generated icons
```

Deep-link handler will register `frankweiler://` via `tauri-plugin-deep-link`
and forward to `frankweiler/ui/src/router/deeplink.ts`.
