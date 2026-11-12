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

The workbench follows VS Code's layout pixel-for-pixel where measured (1200x800, default
"Dark 2026" theme, modern UI): sizes come from the workbench CSS, colors from the theme file
(with `include`s), icons from the real codicon and vs-seti fonts, fonts resolved like the
browser does (`system-ui`, `'Droid Sans Mono', monospace`). Syntax highlighting runs the
extensions' TextMate grammars on Oniguruma (as `vscode-textmate` does) with the theme's
`tokenColors`, plus bracket pair colorization, indent guides and the dimmed final line number.

Verified with the built-in CSS language features extension (real language server completions).

Works today: folder + explorer, tabs, Quick Open (Ctrl+P, command center) and Command Palette
(Ctrl+Shift+P: extension commands and native ones), selections, clipboard, undo/redo, edit/save, keyboard navigation, paste, mouse cursor placement,
extension completions (Ctrl+Space, trigger characters, typing), document sync to extensions.
Not yet: menus and other views (search, SCM, ...), most workbench commands, settings UI,
hover, diagnostics, grammar injections, semantic highlighting, IME, suggest widget kind icons.
