# code-desktop

An Electron-free desktop shell written in Rust. It starts the VS Code server
(`out/server-main.js`, which runs the Node extension host, so extensions keep
working) on `127.0.0.1` with a random connection token, and shows the web
workbench in the OS webview (WebKitGTK on Linux, WebView2 on Windows, WKWebView on macOS).

```bash
npm run compile
cd desktop && cargo run -- /path/to/folder
```

Linux build deps: `libwebkit2gtk-4.1-dev libgtk-3-dev libxdo-dev`.

- `VSCODE_SERVER_ROOT`: a server build or checkout containing `out/server-main.js` (default: this repo).
- `VSCODE_NODE`: node binary (default: `<root>/node`, then `node` on PATH).
