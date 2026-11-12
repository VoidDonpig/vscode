# code-native

Zed-style native front end for VS Code: the UI is drawn with
[gpui](https://crates.io/crates/gpui) (GPU, no Electron, no webview), and
extensions run unmodified in VS Code's Node extension host via `../exthost`.

```bash
npm run compile                          # builds the extension host JS in out/
cd editor
CODE_NATIVE_EXTENSIONS=../exthost/fixtures/hello-ext cargo run -- notes.txt
```

By default extensions are loaded from `~/.vscode-oss/extensions`. Language ids come from
the extensions' `contributes.languages`.

Linux build deps: `libxkbcommon-x11-dev libxcb1-dev libx11-xcb-dev libwayland-dev libvulkan-dev libfontconfig-dev libfreetype-dev`
(and a Vulkan driver, e.g. `mesa-vulkan-drivers`).

Verified with the built-in CSS language features extension (real language server completions).

Works today: open/edit/save one file, keyboard navigation, paste, mouse cursor placement,
extension completions (Ctrl+Space, trigger characters, typing), document sync to extensions.
Not yet: selections/undo, multiple files/tabs, explorer, syntax highlighting, hover,
diagnostics, command palette, settings, themes, IME.
