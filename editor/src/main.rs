/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! Native VS Code front end drawn with gpui (no Electron, no webview). Extensions run unmodified
//! in VS Code's Node extension host, driven through `code-exthost`. The layout, sizes, fonts,
//! colors (default theme "Dark 2026") and icons (codicons, vs-seti) follow the real workbench.
//!
//! Usage: `code-native [folder] [files...]` (like `code`: the first folder is the workspace, files open in tabs)
//! - `CODE_NATIVE_EXTENSIONS`: extra `:`-separated extension folders (or folders of extensions);
//!   default `~/.vscode-oss/extensions`. Built-in extensions come from `<root>/extensions`.
//! - `VSCODE_NODE`: node binary (default `node`). `VSCODE_ROOT`: VS Code checkout (default: this repo).

mod buffer;
mod icons;
mod quickinput;
mod textmate;
mod theme;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use buffer::Buffer;
use code_exthost::{describe_extension, ExtHost, Options};
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt;
use gpui::{
	div, img, prelude::*, px, size, uniform_list, FontStyle, HighlightStyle, StyledText, UnderlineStyle, App, Application, Bounds, Context, Div, FocusHandle, FontWeight, Hsla, KeyDownEvent,
	MouseButton, MouseDownEvent, ScrollStrategy, SharedString, UniformListScrollHandle, Window, WindowBounds, WindowDecorations,
	WindowOptions,
};
use icons::FileIcons;
use serde_json::{json, Value};
use theme::{Theme, TokenStyle};

// Workbench metrics, measured from the web workbench of this repo (1200x800, Dark 2026).
const TITLE_H: f32 = 35.;
const GAP: f32 = 4.;
const ACTIVITY_W: f32 = 44.;
const SIDEBAR_W: f32 = 300.;
const STATUS_H: f32 = 28.;
const TABS_H: f32 = 32.;
const BREADCRUMBS_H: f32 = 22.;
const ROW_H: f32 = 22.;
const UI_SIZE: f32 = 13.;
// Editor defaults on Linux: fontSize 14, lineHeight = round(14 * 1.35).
const FONT_SIZE: f32 = 14.;
const LINE_H: f32 = 19.;
const GUTTER: f32 = 70.;
const LINE_NUMBER_RIGHT: f32 = 44.;
const MINIMAP_W: f32 = 88.;
const SCROLLBAR_W: f32 = 14.;
const MINIMAP_LINES: usize = 400;
/// files.exclude defaults.
const EXCLUDED: &[&str] = &[".git", ".svn", ".hg", ".DS_Store", "Thumbs.db", "CVS"];

enum HostEvent {
	Ready(Arc<ExtHost>),
	Failed(String),
	CompletionProvider { handle: Value, selector: Value, triggers: Vec<String> },
	/// `MainThreadDiagnostics.$changeMany`: (file path, markers) per resource.
	Diagnostics { owner: String, entries: Vec<(String, Vec<Marker>)> },
	ClearDiagnostics(String),
	/// `MainThreadMessageService.$showMessage` (severity: 1 info, 2 warning, 3 error)
	Message(u64, String),
}

/// IMarkerData: 1-based lines, 1-based UTF-16 columns; severity 1 hint, 2 info, 4 warning, 8 error.
#[derive(Clone, Debug, PartialEq)]
struct Marker {
	severity: u64,
	message: String,
	source: Option<String>,
	start: (usize, usize),
	end: (usize, usize),
}

fn parse_marker(m: &Value) -> Option<Marker> {
	let n = |k: &str| m[k].as_u64().map(|v| v as usize);
	Some(Marker {
		severity: m["severity"].as_u64()?,
		message: m["message"].as_str()?.to_string(),
		source: m["source"].as_str().map(String::from),
		start: (n("startLineNumber")?, n("startColumn")?),
		end: (n("endLineNumber")?, n("endColumn")?),
	})
}

/// Byte offset of a 1-based UTF-16 column in `line` (clamped).
fn utf16_col_to_byte(line: &str, col: usize) -> usize {
	let mut units = 0;
	for (b, c) in line.char_indices() {
		if units + 1 >= col {
			return b;
		}
		units += c.len_utf16();
	}
	line.len()
}

/// Byte ranges of `line` (0-based index) covered by markers, with their severity.
fn marker_ranges(markers: &[&Marker], ix: usize, line: &str) -> Vec<(Range<usize>, u64)> {
	markers
		.iter()
		.filter(|m| m.start.0 <= ix + 1 && ix + 1 <= m.end.0 && m.severity >= 2)
		.map(|m| {
			let s = if m.start.0 == ix + 1 { utf16_col_to_byte(line, m.start.1) } else { 0 };
			let e = if m.end.0 == ix + 1 { utf16_col_to_byte(line, m.end.1) } else { line.len() };
			// Empty ranges still get a squiggle under the next character, like the editor.
			let e = if e <= s { (s + line[s..].chars().next().map_or(0, char::len_utf8)).min(line.len()) } else { e };
			(s..e, m.severity)
		})
		.filter(|(r, _)| r.start < r.end)
		.collect()
}

struct Provider {
	handle: Value,
	selector: Value,
	triggers: Vec<String>,
}

struct Popup {
	/// (label, text to insert, sort key)
	items: Vec<(String, String, String)>,
	selected: usize,
}

struct Doc {
	path: PathBuf,
	uri: Value,
	lang: String,
	buf: Buffer,
	crlf: bool,
	version: u64,
	dirty: bool,
	/// Document as the extension host last saw it: (full range, UTF-16 length).
	synced: ((usize, usize, usize, usize), usize),
	scroll: UniformListScrollHandle,
	/// TextMate scope name of the language's grammar.
	grammar: Option<String>,
	/// Per line: styled byte ranges, tokenizer state and bracket depth at the end of the line.
	tokens: Vec<LineTokens>,
}

struct LineTokens {
	runs: Vec<(Range<usize>, TokenStyle)>,
	stack: textmate::Stack,
	bracket_depth: usize,
}

/// One `contributes.languages` entry.
struct Language {
	id: String,
	name: String,
	extensions: Vec<String>,
	filenames: Vec<String>,
}

struct Fonts {
	ui: SharedString,
	mono: SharedString,
}

struct Workbench {
	focus: FocusHandle,
	grammars: textmate::Registry,
	/// language id -> grammar scope name
	language_grammars: HashMap<String, String>,
	token_styles: HashMap<String, TokenStyle>,
	theme: Theme,
	file_icons: FileIcons,
	fonts: Fonts,
	languages: Vec<Language>,
	root: Option<PathBuf>,
	dir_cache: HashMap<PathBuf, Vec<(String, bool)>>,
	expanded: HashSet<PathBuf>,
	docs: Vec<Doc>,
	active: Option<usize>,
	host: Option<Arc<ExtHost>>,
	providers: Vec<Provider>,
	popup: Option<Popup>,
	char_w: f32,
	quick: Option<Quick>,
	/// owner -> file path -> markers
	markers: HashMap<String, HashMap<String, Vec<Marker>>>,
	/// (id, severity, message)
	notifications: Vec<(u64, u64, String)>,
	next_notification: u64,
	/// Hovered diagnostic: (line, x in px within the text area, text).
	hover: Option<(usize, f32, String)>,
	/// Id of the text editor the extension host knows as visible/active (one editor group).
	editor_id: Option<String>,
	ext_commands: Vec<quickinput::ExtensionCommand>,
	sidebar_visible: bool,
}

/// Quick Open / Command Palette state. A query starting with '>' lists commands.
struct Quick {
	query: String,
	selected: usize,
	files: Vec<PathBuf>,
}

#[derive(Clone, Copy, PartialEq)]
enum Builtin {
	Save,
	CloseEditor,
	Undo,
	Redo,
	SelectAll,
	TriggerSuggest,
	QuickOpen,
	ToggleSidebar,
}

/// Workbench commands implemented natively: (label, keybinding, command).
const BUILTINS: &[(&str, &[&str], Builtin)] = &[
	("File: Save", &["Ctrl", "S"], Builtin::Save),
	("View: Close Editor", &["Ctrl", "W"], Builtin::CloseEditor),
	("Edit: Undo", &["Ctrl", "Z"], Builtin::Undo),
	("Edit: Redo", &["Ctrl", "Y"], Builtin::Redo),
	("Selection: Select All", &["Ctrl", "A"], Builtin::SelectAll),
	("Trigger Suggest", &["Ctrl", "Space"], Builtin::TriggerSuggest),
	("Go to File...", &["Ctrl", "P"], Builtin::QuickOpen),
	("View: Toggle Primary Side Bar Visibility", &["Ctrl", "B"], Builtin::ToggleSidebar),
];

#[derive(Clone)]
enum QuickAction {
	Open(PathBuf),
	Builtin(Builtin),
	Extension(String),
}

struct QuickItem {
	label: String,
	description: String,
	file: bool,
	keys: &'static [&'static str],
	hits: Vec<usize>,
	action: QuickAction,
}

fn file_uri(path: &Path) -> Value {
	json!({ "$mid": 1, "scheme": "file", "authority": "", "path": path.to_string_lossy(), "query": "", "fragment": "" })
}

/// Sub-folders of `root` that contain a package.json.
fn extension_folders(root: &Path) -> Vec<PathBuf> {
	let mut found: Vec<PathBuf> = std::fs::read_dir(root)
		.map(|it| it.flatten().map(|e| e.path()).filter(|p| p.join("package.json").is_file()).collect())
		.unwrap_or_default();
	found.sort();
	found
}

/// Built-in extensions (`<root>/extensions/*`), then user extensions.
fn extension_dirs(app_root: &Path) -> Vec<PathBuf> {
	let mut dirs = extension_folders(&app_root.join("extensions"));
	let user: Vec<PathBuf> = match std::env::var_os("CODE_NATIVE_EXTENSIONS") {
		Some(v) => std::env::split_paths(&v).collect(),
		None => std::env::var_os("HOME").map(|h| Path::new(&h).join(".vscode-oss/extensions")).into_iter().collect(),
	};
	for root in user {
		if root.join("package.json").is_file() {
			dirs.push(root);
		} else {
			dirs.extend(extension_folders(&root));
		}
	}
	dirs
}

fn languages_of(packages: &[Value]) -> Vec<Language> {
	let strings = |v: &Value| v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_lowercase)).collect::<Vec<_>>();
	packages
		.iter()
		.flat_map(|p| p["contributes"]["languages"].as_array().cloned().unwrap_or_default())
		.filter_map(|l| {
			let id = l["id"].as_str()?.to_string();
			let name = l["aliases"][0].as_str().unwrap_or(&id).to_string();
			Some(Language { name, extensions: strings(&l["extensions"]), filenames: strings(&l["filenames"]), id })
		})
		.collect()
}

/// `contributes.grammars`: (language id -> scope name, scope name -> grammar file).
fn grammars_of(dirs: &[PathBuf], packages: &[Value]) -> (HashMap<String, String>, HashMap<String, PathBuf>) {
	let (mut by_lang, mut files) = (HashMap::new(), HashMap::new());
	for (dir, p) in dirs.iter().zip(packages) {
		for g in p["contributes"]["grammars"].as_array().into_iter().flatten() {
			let (Some(scope), Some(path)) = (g["scopeName"].as_str(), g["path"].as_str()) else { continue };
			files.insert(scope.to_string(), dir.join(path));
			if let Some(lang) = g["language"].as_str() {
				by_lang.insert(lang.to_string(), scope.to_string());
			}
		}
	}
	(by_lang, files)
}

/// Tabs shown as 4 spaces: the display text and a map from source byte to display byte.
// shortcut: tabs expand to a fixed 4 columns, not to the next tab stop.
fn expand_tabs(line: &str) -> (String, Vec<usize>) {
	let mut out = String::with_capacity(line.len());
	let mut map = Vec::with_capacity(line.len() + 1);
	for c in line.chars() {
		map.extend(std::iter::repeat_n(out.len(), c.len_utf8()));
		if c == '\t' {
			out.push_str("    ");
		} else {
			out.push(c);
		}
	}
	map.push(out.len());
	(out, map)
}

/// Language id for a file, like the workbench's language service (file names, then the longest matching extension).
fn language_for(path: &Path, languages: &[Language]) -> String {
	let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
	let ext_len = |l: &Language| l.extensions.iter().filter(|e| name.ends_with(e.as_str())).map(|e| e.len()).max();
	languages
		.iter()
		.find(|l| l.filenames.contains(&name))
		.or_else(|| languages.iter().filter(|l| ext_len(l).is_some()).max_by_key(|l| ext_len(l)))
		.map_or("plaintext".into(), |l| l.id.clone())
}

/// Does a DocumentSelector (as sent by `$register*Provider`) match this document?
fn selector_matches(selector: &Value, lang: &str, scheme: &str) -> bool {
	selector.as_array().into_iter().flatten().any(|f| {
		let lang_ok = f["language"].as_str().is_none_or(|l| l == "*" || l == lang);
		let scheme_ok = f["scheme"].as_str().is_none_or(|s| s == "*" || s == scheme);
		// shortcut: `pattern` filters are treated as matching; evaluate globs when an extension needs it.
		lang_ok && scheme_ok
	})
}

/// Converts one `ISuggestDataDto` into (label, insert text, sort key).
fn completion_item(item: &Value) -> Option<(String, String, String)> {
	let label = item["a"].as_str().or_else(|| item["a"]["label"].as_str())?.to_string();
	let mut insert = item["h"].as_str().unwrap_or(&label).to_string();
	if item["i"].as_u64().unwrap_or(0) & 4 != 0 {
		insert = strip_snippet(&insert);
	}
	let sort = item["e"].as_str().unwrap_or(&label).to_string();
	Some((label, insert, sort))
}

// shortcut: snippets are inserted as plain text (placeholders keep their default text, no tab stops).
fn strip_snippet(s: &str) -> String {
	let mut out = String::new();
	let mut open = 0; // unclosed `${` placeholders
	let mut chars = s.chars().peekable();
	while let Some(c) = chars.next() {
		match c {
			'\\' => out.extend(chars.next()),
			'$' if chars.peek() == Some(&'{') => {
				chars.next();
				open += 1;
				while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
					chars.next();
				}
				if chars.peek() == Some(&':') {
					chars.next();
				}
			}
			'$' if chars.peek().is_some_and(|c| c.is_ascii_digit()) => {
				while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
					chars.next();
				}
			}
			'}' if open > 0 => open -= 1,
			c => out.push(c),
		}
	}
	out
}

/// `editor.detectIndentation`, simplified: tabs, or the smallest leading-space run.
fn indentation_label(lines: &[String]) -> String {
	if lines.iter().filter(|l| l.starts_with('\t')).count() > lines.iter().filter(|l| l.starts_with(' ')).count() {
		return "Tab Size: 4".into();
	}
	let n = lines.iter().map(|l| l.len() - l.trim_start_matches(' ').len()).filter(|n| *n > 0 && *n <= 8).min().unwrap_or(4);
	format!("Spaces: {n}")
}

/// Same sort as the explorer's default: folders first, then case-insensitive names.
fn read_dir_sorted(dir: &Path) -> Vec<(String, bool)> {
	let mut entries: Vec<(String, bool)> = std::fs::read_dir(dir)
		.map(|it| {
			it.flatten()
				.map(|e| (e.file_name().to_string_lossy().into_owned(), e.path().is_dir()))
				.filter(|(n, _)| !EXCLUDED.contains(&n.as_str()))
				.collect()
		})
		.unwrap_or_default();
	entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
	entries
}

/// Explorer rows below `dir`, depth-first: (depth, path, is_dir).
fn flatten(cache: &HashMap<PathBuf, Vec<(String, bool)>>, expanded: &HashSet<PathBuf>, dir: &Path, depth: usize) -> Vec<(usize, PathBuf, bool)> {
	let mut out = Vec::new();
	for (name, is_dir) in cache.get(dir).cloned().unwrap_or_else(|| read_dir_sorted(dir)) {
		let p = dir.join(name);
		out.push((depth, p.clone(), is_dir));
		if is_dir && expanded.contains(&p) {
			out.extend(flatten(cache, expanded, &p, depth + 1));
		}
	}
	out
}

fn system_fonts(cx: &App) -> Fonts {
	#[cfg(target_os = "linux")]
	{
		// The workbench asks for `system-ui` / `'Droid Sans Mono', monospace`; resolve them like the browser does.
		let fc = |pattern: &str, fallback: &str| -> SharedString {
			std::process::Command::new("fc-match")
				.args(["-f", "%{family[0]}", pattern])
				.output()
				.ok()
				.and_then(|o| String::from_utf8(o.stdout).ok())
				.filter(|s| !s.is_empty())
				.unwrap_or_else(|| fallback.into())
				.into()
		};
		let mono = if cx.text_system().all_font_names().iter().any(|f| f == "Droid Sans Mono") { "Droid Sans Mono".into() } else { fc("monospace", "DejaVu Sans Mono") };
		Fonts { ui: fc("sans-serif", "DejaVu Sans"), mono }
	}
	#[cfg(target_os = "macos")]
	{
		let _ = cx;
		Fonts { ui: ".SystemUIFont".into(), mono: "Menlo".into() }
	}
	#[cfg(windows)]
	{
		let _ = cx;
		Fonts { ui: "Segoe UI".into(), mono: "Consolas".into() }
	}
}

fn icon(ch: char, size: f32, color: Hsla) -> Div {
	div()
		.flex()
		.flex_none()
		.items_center()
		.justify_center()
		.font_family(icons::CODICON_FONT)
		.text_size(px(size))
		.line_height(px(size))
		.text_color(color)
		.child(ch.to_string())
}

/// A 22x22 toolbar button with a 16px codicon, as in title and editor toolbars.
fn action(ch: char, color: Hsla) -> Div {
	icon(ch, 16., color).size(px(22.)).rounded(px(4.))
}

fn send_open(host: &ExtHost, doc: &Doc) {
	let _ = host.notify("ExtHostDocumentsAndEditors", "$acceptDocumentsAndEditorsDelta", vec![json!({
		"addedDocuments": [{
			"uri": doc.uri, "versionId": doc.version, "lines": doc.buf.lines, "EOL": if doc.crlf { "\r\n" } else { "\n" },
			"languageId": doc.lang, "isDirty": doc.dirty, "encoding": "utf8",
		}]
	})]);
}

fn start_host(app_root: PathBuf, extensions: Vec<PathBuf>, configuration_defaults: (Value, Vec<String>), tx: UnboundedSender<HostEvent>) {
	std::thread::spawn(move || {
		let events = tx.clone();
		let handler = Box::new(move |proxy: &str, method: &str, args: &[Value]| {
			if (proxy, method) == ("MainThreadLanguageFeatures", "$registerCompletionsProvider") {
				let triggers = args[2].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)).collect();
				let _ = events.unbounded_send(HostEvent::CompletionProvider { handle: args[0].clone(), selector: args[1].clone(), triggers });
			}
			match (proxy, method) {
				("MainThreadDiagnostics", "$changeMany") => {
					let entries = args[1]
						.as_array()
						.into_iter()
						.flatten()
						.filter_map(|e| {
							let path = e[0]["path"].as_str()?.to_string();
							Some((path, e[1].as_array().into_iter().flatten().filter_map(parse_marker).collect()))
						})
						.collect();
					let _ = events.unbounded_send(HostEvent::Diagnostics { owner: args[0].as_str().unwrap_or_default().to_string(), entries });
				}
				("MainThreadDiagnostics", "$clear") => {
					let _ = events.unbounded_send(HostEvent::ClearDiagnostics(args[0].as_str().unwrap_or_default().to_string()));
				}
				("MainThreadMessageService", "$showMessage") => {
					let _ = events.unbounded_send(HostEvent::Message(args[0].as_u64().unwrap_or(1), args[1].as_str().unwrap_or_default().to_string()));
				}
				_ => {}
			}
			Ok(None)
		});
		let opts = Options {
			node: std::env::var_os("VSCODE_NODE").unwrap_or("node".into()).into(),
			app_root,
			extensions,
			data_dir: std::env::temp_dir().join("code-native"),
			configuration_defaults,
		};
		let _ = tx.unbounded_send(match ExtHost::start(opts, handler) {
			Ok(h) => HostEvent::Ready(Arc::new(h)),
			Err(e) => HostEvent::Failed(e.to_string()),
		});
	});
}

impl Workbench {
	fn new(args: Vec<PathBuf>, cx: &mut Context<Self>) -> Self {
		let app_root = std::env::var_os("VSCODE_ROOT").map(PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join(".."));
		let app_root = app_root.canonicalize().unwrap_or(app_root);
		let ext_dirs = extension_dirs(&app_root);
		let (ext_dirs, packages): (Vec<PathBuf>, Vec<Value>) = ext_dirs.into_iter().filter_map(|d| describe_extension(&d).ok().map(|(_, v)| (d, v))).unzip();
		let (language_grammars, grammar_files) = grammars_of(&ext_dirs, &packages);
		let mut ext_commands: Vec<quickinput::ExtensionCommand> = ext_dirs.iter().zip(&packages).flat_map(|(d, p)| quickinput::extension_commands(d, p)).collect();
		ext_commands.sort_by(|a, b| a.label.cmp(&b.label));

		let args: Vec<PathBuf> = args.into_iter().map(|a| std::path::absolute(&a).unwrap_or(a)).collect();
		let files: Vec<PathBuf> = args.iter().filter(|a| !a.is_dir()).cloned().collect();
		let root = args.iter().find(|a| a.is_dir()).cloned().or_else(|| files.first().and_then(|f| f.parent()).map(Path::to_path_buf));

		let (tx, mut rx) = unbounded();
		start_host(app_root.clone(), ext_dirs, code_exthost::configuration_defaults(&packages), tx);
		cx.spawn(async move |this, cx| {
			while let Some(ev) = rx.next().await {
				if this.update(cx, |wb, cx| wb.on_host_event(ev, cx)).is_err() {
					break;
				}
			}
		})
		.detach();

		let mut wb = Workbench {
			focus: cx.focus_handle(),
			grammars: textmate::Registry::new(grammar_files),
			language_grammars,
			token_styles: HashMap::new(),
			theme: Theme::load(&app_root.join("extensions/theme-defaults/themes/2026-dark.json")),
			file_icons: FileIcons::load(&app_root.join("extensions/theme-seti/icons/vs-seti-icon-theme.json")),
			fonts: system_fonts(cx),
			languages: languages_of(&packages),
			root: root.clone(),
			dir_cache: HashMap::new(),
			expanded: root.into_iter().collect(),
			docs: Vec::new(),
			active: None,
			host: None,
			providers: Vec::new(),
			popup: None,
			char_w: FONT_SIZE * 0.6,
			quick: None,
			markers: HashMap::new(),
			notifications: Vec::new(),
			next_notification: 0,
			hover: None,
			editor_id: None,
			ext_commands,
			sidebar_visible: true,
		};
		for f in files {
			wb.open(f, cx);
		}
		wb
	}

	/// Tells the extension host which document is shown in the (single) visible editor, so
	/// `window.activeTextEditor` / `visibleTextEditors` and pull diagnostics work.
	fn sync_editor(&mut self) {
		let Some(host) = self.host.clone() else { return };
		let want = self.doc().map(|d| (format!("editor:{}", d.path.display()), d.uri.clone(), d.buf.cursor, d.buf.lines.len(), indentation_label(&d.buf.lines)));
		if want.as_ref().map(|w| &w.0) == self.editor_id.as_ref() {
			return;
		}
		let mut delta = serde_json::Map::new();
		if let Some(old) = self.editor_id.take() {
			delta.insert("removedEditors".into(), json!([old]));
		}
		if let Some((id, uri, (l, c), n, indent)) = want {
			let tab_size = indent.rsplit(' ').next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(4);
			delta.insert("addedEditors".into(), json!([{
				"id": id, "documentUri": uri,
				"options": { "tabSize": tab_size, "indentSize": tab_size, "originalIndentSize": tab_size, "insertSpaces": !indent.starts_with("Tab"), "cursorStyle": 1, "lineNumbers": 1 },
				"selections": [{ "selectionStartLineNumber": l + 1, "selectionStartColumn": c + 1, "positionLineNumber": l + 1, "positionColumn": c + 1 }],
				"visibleRanges": [{ "startLineNumber": 1, "startColumn": 1, "endLineNumber": n, "endColumn": 1 }],
				"editorPosition": 0,
			}]));
			delta.insert("newActiveEditor".into(), json!(id));
			self.editor_id = Some(id);
		} else {
			delta.insert("newActiveEditor".into(), Value::Null);
		}
		let _ = host.notify("ExtHostDocumentsAndEditors", "$acceptDocumentsAndEditorsDelta", vec![Value::Object(delta)]);
	}

	fn doc(&self) -> Option<&Doc> {
		self.active.and_then(|i| self.docs.get(i))
	}

	fn doc_mut(&mut self) -> Option<&mut Doc> {
		self.active.and_then(|i| self.docs.get_mut(i))
	}

	fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
		if let Some(i) = self.docs.iter().position(|d| d.path == path) {
			self.active = Some(i);
		} else {
			let raw = std::fs::read_to_string(&path).unwrap_or_default();
			let buf = Buffer::new(&raw);
			let text = buf.text();
			let doc = Doc {
				uri: file_uri(&path),
				lang: language_for(&path, &self.languages),
				crlf: raw.contains("\r\n"),
				synced: (buf.full_range(), text.encode_utf16().count()),
				buf,
				path,
				version: 1,
				dirty: false,
				scroll: UniformListScrollHandle::new(),
				grammar: None,
				tokens: Vec::new(),
			};
			let mut doc = doc;
			doc.grammar = self.language_grammars.get(&doc.lang).cloned();
			if let Some(host) = &self.host {
				send_open(host, &doc);
				let _ = host.notify("ExtHostExtensionService", "$activateByEvent", vec![json!(format!("onLanguage:{}", doc.lang)), json!(0)]);
			}
			self.docs.push(doc);
			self.active = Some(self.docs.len() - 1);
		}
		// Reveal the file in the explorer.
		if let (Some(root), Some(doc)) = (&self.root, self.doc()) {
			let mut dirs: Vec<PathBuf> = doc.path.ancestors().skip(1).take_while(|a| a.starts_with(root)).map(Path::to_path_buf).collect();
			dirs.reverse();
			self.expanded.extend(dirs);
		}
		self.popup = None;
		self.sync_editor();
		cx.notify();
	}

	fn close(&mut self, i: usize, cx: &mut Context<Self>) {
		if Some(i) == self.active {
			self.active = None;
			self.sync_editor();
			self.active = Some(i);
		}
		let doc = self.docs.remove(i);
		if let Some(host) = &self.host {
			let _ = host.notify("ExtHostDocumentsAndEditors", "$acceptDocumentsAndEditorsDelta", vec![json!({ "removedDocuments": [doc.uri] })]);
		}
		self.active = match self.active {
			_ if self.docs.is_empty() => None,
			Some(a) if a > i || a == self.docs.len() => Some(a - 1),
			a => a,
		};
		self.popup = None;
		self.sync_editor();
		cx.notify();
	}

	fn on_host_event(&mut self, ev: HostEvent, cx: &mut Context<Self>) {
		match ev {
			HostEvent::Ready(host) => {
				for doc in &self.docs {
					send_open(&host, doc);
				}
				let mut events = vec!["*".to_string(), "onStartupFinished".into()];
				events.extend(self.docs.iter().map(|d| format!("onLanguage:{}", d.lang)));
				let h = host.clone();
				cx.background_spawn(async move {
					for e in events {
						if let Err(err) = h.call("ExtHostExtensionService", "$activateByEvent", vec![json!(e), json!(0)]) {
							eprintln!("code-native: activating {e}: {err}");
						}
					}
				})
				.detach();
				self.host = Some(host);
				self.editor_id = None;
				self.sync_editor();
			}
			HostEvent::Failed(e) => eprintln!("code-native: extension host failed: {e}"),
			HostEvent::CompletionProvider { handle, selector, triggers } => self.providers.push(Provider { handle, selector, triggers }),
			HostEvent::Diagnostics { owner, entries } => {
				let by_file = self.markers.entry(owner).or_default();
				for (path, markers) in entries {
					by_file.insert(path, markers);
				}
			}
			HostEvent::ClearDiagnostics(owner) => {
				self.markers.remove(&owner);
			}
			HostEvent::Message(severity, text) => {
				let id = self.next_notification;
				self.next_notification += 1;
				self.notifications.push((id, severity, text));
				// Info and warning toasts hide after a while; errors stay until closed.
				if severity < 3 {
					cx.spawn(async move |this, cx| {
						cx.background_executor().timer(std::time::Duration::from_secs(10)).await;
						let _ = this.update(cx, |wb, cx| {
							wb.notifications.retain(|n| n.0 != id);
							cx.notify();
						});
					})
					.detach();
				}
			}
		}
		cx.notify();
	}

	/// Sends the new text to the extension host as a full-document replace.
	// shortcut: full-document sync per edit; send incremental ranges when large files get slow.
	fn edited(&mut self, cx: &mut Context<Self>) {
		let host = self.host.clone();
		let Some(doc) = self.doc_mut() else { return };
		doc.dirty = true;
		doc.version += 1;
		let text = doc.buf.text();
		let ((sl, sc, el, ec), old_len) = doc.synced;
		if let Some(host) = host {
			let _ = host.notify("ExtHostDocuments", "$acceptModelChanged", vec![
				doc.uri.clone(),
				json!({
					"changes": [{
						"range": { "startLineNumber": sl, "startColumn": sc, "endLineNumber": el, "endColumn": ec },
						"rangeOffset": 0, "rangeLength": old_len, "text": text,
					}],
					"eol": "\n", "versionId": doc.version, "isUndoing": false, "isRedoing": false,
				}),
				json!(true),
			]);
		}
		doc.synced = (doc.buf.full_range(), text.encode_utf16().count());
		self.reveal_cursor();
		cx.notify();
	}

	/// Tokenizes the active document up to `upto` lines (cached; edits truncate the cache).
	fn ensure_tokens(&mut self, upto: usize) {
		let Some(i) = self.active else { return };
		let Some(scope) = self.docs[i].grammar.clone() else { return };
		let upto = upto.min(self.docs[i].buf.lines.len());
		while self.docs[i].tokens.len() < upto {
			let n = self.docs[i].tokens.len();
			let (mut stack, mut depth) = match self.docs[i].tokens.last() {
				Some(t) => (t.stack.clone(), t.bracket_depth),
				None => match self.grammars.initial_stack(&scope) {
					Some(s) => (s, 0),
					None => return,
				},
			};
			let line = self.docs[i].buf.lines[n].clone();
			let spans = self.grammars.tokenize_line(&line, &mut stack);
			// Paint spans in order (inner captures after their match), then merge equal neighbours.
			let mut per_byte = vec![TokenStyle::default(); line.len()];
			let mut plain = vec![true; line.len()]; // not inside a string / comment / regex token
			for (r, scopes) in spans {
				let key = scopes.join(" ");
				let style = *self.token_styles.entry(key).or_insert_with(|| self.theme.token_style(&scopes));
				let skip = scopes.iter().any(|s| s.starts_with("comment") || s.starts_with("string") || s.starts_with("constant.regexp"));
				plain[r.clone()].fill(!skip);
				per_byte[r].fill(style);
			}
			// editor.bracketPairColorization: one color pool cycling by nesting depth.
			let pool = [
				self.theme.c("editorBracketHighlight.foreground1", 0xffd700ff),
				self.theme.c("editorBracketHighlight.foreground2", 0xda70d6ff),
				self.theme.c("editorBracketHighlight.foreground3", 0x179fffff),
			];
			for (b, ch) in line.char_indices() {
				if !plain.get(b).copied().unwrap_or(true) {
					continue;
				}
				let color = match ch {
					'(' | '[' | '{' => {
						depth += 1;
						pool[(depth - 1) % 3]
					}
					')' | ']' | '}' if depth > 0 => {
						depth -= 1;
						pool[depth % 3]
					}
					')' | ']' | '}' => self.theme.c("editorBracketHighlight.unexpectedBracket.foreground", 0xff1212cc),
					_ => continue,
				};
				per_byte[b].foreground = Some(color);
			}
			let mut runs: Vec<(Range<usize>, TokenStyle)> = Vec::new();
			for (b, st) in per_byte.into_iter().enumerate() {
				match runs.last_mut() {
					Some((r, s)) if *s == st && r.end == b => r.end = b + 1,
					_ => runs.push((b..b + 1, st)),
				}
			}
			self.docs[i].tokens.push(LineTokens { runs, stack, bracket_depth: depth });
		}
	}

	fn reveal_cursor(&self) {
		let Some(doc) = self.doc() else { return };
		let state = doc.scroll.0.borrow();
		let (top, height) = (-f32::from(state.base_handle.offset().y), f32::from(state.base_handle.bounds().size.height));
		drop(state);
		let y = doc.buf.cursor.0 as f32 * LINE_H;
		if y < top {
			doc.scroll.scroll_to_item(doc.buf.cursor.0, ScrollStrategy::Top);
		} else if y + LINE_H > top + height {
			doc.scroll.scroll_to_item(doc.buf.cursor.0, ScrollStrategy::Bottom);
		}
	}

	fn request_completions(&mut self, trigger: Option<String>, cx: &mut Context<Self>) {
		let (Some(host), Some(doc)) = (self.host.clone(), self.doc()) else { return };
		let (line, col) = doc.buf.position_utf16();
		let context = match &trigger {
			Some(ch) => json!({ "triggerKind": 1, "triggerCharacter": ch }),
			None => json!({ "triggerKind": 0 }),
		};
		let handles: Vec<Value> = self
			.providers
			.iter()
			.filter(|p| selector_matches(&p.selector, &doc.lang, "file") && trigger.as_ref().is_none_or(|t| p.triggers.contains(t)))
			.map(|p| p.handle.clone())
			.collect();
		let (uri, version) = (doc.uri.clone(), doc.version);
		cx.spawn(async move |this, cx| {
			let items = cx
				.background_spawn(async move {
					let mut items = Vec::new();
					for handle in handles {
						let args = vec![handle.clone(), uri.clone(), json!({ "lineNumber": line, "column": col }), context.clone()];
						if let Ok(Some(result)) = host.call_cancellable("ExtHostLanguageFeatures", "$provideCompletionItems", args) {
							items.extend(result["b"].as_array().into_iter().flatten().filter_map(completion_item));
							if let Some(id) = result.get("x") {
								let _ = host.notify("ExtHostLanguageFeatures", "$releaseCompletionItems", vec![handle, id.clone()]);
							}
						}
					}
					items
				})
				.await;
			let _ = this.update(cx, |wb, cx| {
				if wb.doc().is_some_and(|d| d.version == version) && !items.is_empty() {
					wb.popup = Some(Popup { items, selected: 0 });
					cx.notify();
				}
			});
		})
		.detach();
	}

	/// Items matching the word before the cursor: prefix matches first, then the extension's sortText.
	fn visible_items(&self) -> Vec<(String, String)> {
		let Some(doc) = self.doc() else { return Vec::new() };
		let prefix = doc.buf.word_prefix().to_lowercase();
		let mut items: Vec<_> = self.popup.iter().flat_map(|p| &p.items).filter(|(label, ..)| label.to_lowercase().contains(&prefix)).collect();
		items.sort_by_key(|(label, _, sort)| (!label.to_lowercase().starts_with(&prefix), sort.as_str()));
		items.into_iter().take(12).map(|(label, insert, _)| (label.clone(), insert.clone())).collect()
	}

	fn save(&mut self) {
		let host = self.host.clone();
		let Some(doc) = self.doc_mut() else { return };
		let text = if doc.crlf { doc.buf.text().replace('\n', "\r\n") } else { doc.buf.text() };
		match std::fs::write(&doc.path, text) {
			Ok(()) => {
				doc.dirty = false;
				if let Some(host) = host {
					let _ = host.notify("ExtHostDocuments", "$acceptModelSaved", vec![doc.uri.clone()]);
				}
			}
			Err(e) => eprintln!("code-native: save failed: {e}"),
		}
	}

	fn open_quick(&mut self, query: String, cx: &mut Context<Self>) {
		let files = self.root.as_ref().map(|r| quickinput::workspace_files(r, EXCLUDED, 20_000)).unwrap_or_default();
		self.quick = Some(Quick { query, selected: 0, files });
		self.popup = None;
		cx.notify();
	}

	/// Rows for the current query: files (most recently opened first when empty) or commands.
	fn quick_items(&self) -> Vec<QuickItem> {
		let Some(q) = &self.quick else { return Vec::new() };
		if let Some(cmd_query) = q.query.strip_prefix('>') {
			let cmd_query = cmd_query.trim();
			let builtins = BUILTINS.iter().map(|(label, keys, b)| (label.to_string(), *keys, QuickAction::Builtin(*b)));
			let ext = self.ext_commands.iter().map(|c| (c.label.clone(), &[][..], QuickAction::Extension(c.id.clone())));
			let mut items: Vec<QuickItem> = builtins
				.chain(ext)
				.filter_map(|(label, keys, action)| {
					let (_, hits) = quickinput::fuzzy(cmd_query, &label)?;
					Some(QuickItem { label, description: String::new(), file: false, keys, hits, action })
				})
				.collect();
			items.sort_by(|a, b| a.label.cmp(&b.label));
			return items;
		}
		let rel = |p: &Path| -> String {
			let parent = p.parent().unwrap_or(Path::new(""));
			self.root.as_ref().and_then(|r| parent.strip_prefix(r).ok()).unwrap_or(parent).to_string_lossy().to_string()
		};
		let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
		if q.query.is_empty() {
			// "recently opened": editors, most recent first
			return self.docs.iter().rev().map(|d| QuickItem { label: name(&d.path), description: rel(&d.path), file: true, keys: &[], hits: Vec::new(), action: QuickAction::Open(d.path.clone()) }).collect();
		}
		let mut scored: Vec<(i32, QuickItem)> = q
			.files
			.iter()
			.filter_map(|p| {
				let label = name(p);
				let (score, hits) = quickinput::fuzzy(&q.query, &label).or_else(|| quickinput::fuzzy(&q.query, &format!("{}/{}", rel(p), label)).map(|(s, _)| (s - 10, Vec::new())))?;
				Some((score, QuickItem { label, description: rel(p), file: true, keys: &[], hits, action: QuickAction::Open(p.clone()) }))
			})
			.collect();
		scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.len().cmp(&b.1.label.len())));
		scored.into_iter().take(500).map(|(_, i)| i).collect()
	}

	fn run_quick(&mut self, action: QuickAction, window: &mut Window, cx: &mut Context<Self>) {
		self.quick = None;
		match action {
			QuickAction::Open(p) => self.open(p, cx),
			QuickAction::Builtin(b) => self.run_builtin(b, cx),
			QuickAction::Extension(id) => {
				if let Some(host) = self.host.clone() {
					cx.background_spawn(async move {
						let _ = host.call("ExtHostExtensionService", "$activateByEvent", vec![json!(format!("onCommand:{id}")), json!(0)]);
						if let Err(e) = host.call("ExtHostCommands", "$executeContributedCommand", vec![json!(id)]) {
							eprintln!("code-native: command {id} failed: {e}");
						}
					})
					.detach();
				}
			}
		}
		window.focus(&self.focus);
		cx.notify();
	}

	fn run_builtin(&mut self, b: Builtin, cx: &mut Context<Self>) {
		let edit = |wb: &mut Self, f: fn(&mut Buffer) -> bool, cx: &mut Context<Self>| {
			if let Some(doc) = wb.doc_mut() {
				if f(&mut doc.buf) {
					doc.tokens.clear();
					wb.edited(cx);
				}
			}
		};
		match b {
			Builtin::Save => self.save(),
			Builtin::CloseEditor => {
				if let Some(i) = self.active {
					self.close(i, cx);
				}
			}
			Builtin::Undo => edit(self, Buffer::undo, cx),
			Builtin::Redo => edit(self, Buffer::redo, cx),
			Builtin::SelectAll => {
				if let Some(doc) = self.doc_mut() {
					doc.buf.select_all();
				}
			}
			Builtin::TriggerSuggest => self.request_completions(None, cx),
			Builtin::QuickOpen => self.open_quick(String::new(), cx),
			Builtin::ToggleSidebar => self.sidebar_visible = !self.sidebar_visible,
		}
		cx.notify();
	}

	fn quick_key(&mut self, k: &gpui::Keystroke, window: &mut Window, cx: &mut Context<Self>) {
		let n = self.quick_items().len();
		let Some(q) = self.quick.as_mut() else { return };
		match k.key.as_str() {
			"escape" => {
				self.quick = None;
				window.focus(&self.focus);
			}
			"up" => q.selected = q.selected.saturating_sub(1),
			"down" => q.selected = (q.selected + 1).min(n.saturating_sub(1)),
			"enter" => {
				let sel = q.selected;
				if let Some(item) = self.quick_items().into_iter().nth(sel) {
					return self.run_quick(item.action, window, cx);
				}
			}
			"backspace" => {
				q.query.pop();
				q.selected = 0;
			}
			_ if !(k.modifiers.control || k.modifiers.platform || k.modifiers.alt) => {
				if let Some(ch) = &k.key_char {
					q.query.push_str(ch);
					q.selected = 0;
				}
			}
			_ => {}
		}
		cx.notify();
	}

	fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
		let k = &ev.keystroke;
		let m = k.modifiers;
		let cmd = m.control || m.platform;
		match k.key.as_str() {
			"p" if cmd => return self.open_quick(if m.shift { ">".into() } else { String::new() }, cx),
			"b" if cmd => return self.run_builtin(Builtin::ToggleSidebar, cx),
			_ => {}
		}
		if self.quick.is_some() {
			return self.quick_key(k, window, cx);
		}
		if k.key == "w" && cmd {
			if let Some(i) = self.active {
				self.close(i, cx);
			}
			return;
		}
		if self.doc().is_none() {
			return;
		}

		if self.popup.is_some() {
			let items = self.visible_items();
			let popup = self.popup.as_mut().unwrap();
			match k.key.as_str() {
				"up" => return { popup.selected = popup.selected.saturating_sub(1); cx.notify() },
				"down" => return { popup.selected = (popup.selected + 1).min(items.len().saturating_sub(1)); cx.notify() },
				"enter" | "tab" if !items.is_empty() => {
					let insert = items[popup.selected.min(items.len() - 1)].1.clone();
					self.popup = None;
					let buf = &mut self.doc_mut().unwrap().buf;
					let prefix = buf.word_prefix().chars().count();
					buf.replace_prefix(prefix, &insert);
					return self.edited(cx);
				}
				"escape" => return { self.popup = None; cx.notify() },
				_ => {}
			}
		}

		// Re-tokenize from the line above the first touched line (selection start or cursor).
		let (line_before, version_before) = self.doc().map(|d| (d.buf.anchor.map_or(d.buf.cursor.0, |a| a.0.min(d.buf.cursor.0)), d.version)).unwrap();
		self.key_down_edit(k, cx);
		if let Some(doc) = self.doc_mut() {
			if doc.version != version_before {
				doc.tokens.truncate(line_before.min(doc.buf.cursor.0).saturating_sub(1));
			}
		}
		if self.popup.is_some() && self.visible_items().is_empty() {
			self.popup = None;
		}
		self.reveal_cursor();
		cx.notify();
	}

	fn key_down_edit(&mut self, k: &gpui::Keystroke, cx: &mut Context<Self>) {
		let m = k.modifiers;
		let cmd = m.control || m.platform;
		let shift = m.shift;
		let page = 30;
		let buf = &mut self.doc_mut().unwrap().buf;
		match k.key.as_str() {
			"s" if cmd => self.save(),
			"space" if cmd => self.request_completions(None, cx),
			"a" if cmd => buf.select_all(),
			"c" | "x" if cmd => {
				// editor.emptySelectionClipboard: without a selection, the whole line.
				let text = buf.selected_text().unwrap_or_else(|| format!("{}\n", buf.lines[buf.cursor.0]));
				cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
				if k.key == "x" {
					if !buf.delete_selection() {
						let l = buf.cursor.0;
						buf.set_cursor((l, 0), false);
						buf.set_cursor((l + 1, 0), true);
						if buf.selection().is_none() || l + 1 >= buf.lines.len() {
							// last line: take it up to its end (and the newline before it, if any)
							buf.set_cursor((l, 0), false);
							buf.end(true);
						}
						buf.delete_selection();
					}
					self.edited(cx);
				}
			}
			"v" if cmd => {
				if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
					buf.insert(&text.replace("\r\n", "\n"));
					self.edited(cx);
				}
			}
			"z" if cmd && !shift => {
				if buf.undo() {
					self.doc_mut().unwrap().tokens.clear();
					self.edited(cx);
				}
			}
			"y" | "z" if cmd => {
				if buf.redo() {
					self.doc_mut().unwrap().tokens.clear();
					self.edited(cx);
				}
			}
			"left" | "right" if cmd => buf.move_word(k.key == "right", shift),
			"home" if cmd => buf.set_cursor((0, 0), shift),
			"end" if cmd => buf.set_cursor((usize::MAX, usize::MAX), shift),
			"left" => buf.move_by(0, -1, shift),
			"right" => buf.move_by(0, 1, shift),
			"up" => buf.move_by(-1, 0, shift),
			"down" => buf.move_by(1, 0, shift),
			"pageup" => buf.move_by(-page, 0, shift),
			"pagedown" => buf.move_by(page, 0, shift),
			"home" => buf.home(shift),
			"end" => buf.end(shift),
			"backspace" => {
				buf.backspace();
				self.edited(cx);
			}
			"delete" => {
				buf.delete();
				self.edited(cx);
			}
			"enter" => {
				buf.insert("\n");
				self.edited(cx);
			}
			"tab" => {
				buf.insert("    ");
				self.edited(cx);
			}
			"escape" => buf.set_cursor(buf.cursor, false),
			// shortcut: text arrives via key_char, so IME composition is not supported yet (needs gpui's InputHandler).
			_ if !cmd && !m.alt => {
				if let Some(ch) = k.key_char.clone() {
					buf.insert(&ch);
					self.edited(cx);
					let lang = self.doc().map(|d| d.lang.clone()).unwrap_or_default();
					let trigger = self.providers.iter().any(|p| p.triggers.contains(&ch) && selector_matches(&p.selector, &lang, "file"));
					if trigger {
						self.request_completions(Some(ch), cx);
					} else if ch.chars().all(|c| c.is_alphanumeric() || c == '_') {
						if self.popup.is_none() {
							self.request_completions(None, cx);
						}
					} else {
						self.popup = None;
					}
				}
			}
			_ => {}
		}
	}

	/// Editor position under a window x coordinate on `line`.
	fn column_at(&self, line: usize, x: f32) -> (usize, usize) {
		let text_left = GAP + ACTIVITY_W + if self.sidebar_visible { SIDEBAR_W } else { 0. } + GAP + 1. + GUTTER;
		let display_col = ((x - text_left) / self.char_w).round().max(0.) as usize;
		// map display columns (tabs = 4) back to characters
		let mut col = 0;
		let mut acc = 0;
		if let Some(doc) = self.doc() {
			for c in doc.buf.lines[line].chars() {
				let w = if c == '\t' { 4 } else { 1 };
				if acc + w / 2 >= display_col && acc + w > display_col {
					break;
				}
				acc += w;
				col += 1;
			}
		}
		(line, col)
	}

	fn click_line(&mut self, line: usize, ev: &MouseDownEvent, cx: &mut Context<Self>) {
		let pos = self.column_at(line, f32::from(ev.position.x));
		if let Some(doc) = self.doc_mut() {
			if ev.click_count >= 2 {
				doc.buf.select_word(pos);
			} else {
				doc.buf.set_cursor(pos, ev.modifiers.shift);
			}
		}
		self.popup = None;
		cx.notify();
	}

	/// Markers of the active document (all owners).
	fn doc_markers(&self) -> Vec<&Marker> {
		let Some(doc) = self.doc() else { return Vec::new() };
		let path = doc.path.to_string_lossy();
		self.markers.values().filter_map(|m| m.get(path.as_ref())).flatten().collect()
	}

	fn drag_line(&mut self, line: usize, ev: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
		if ev.pressed_button != Some(MouseButton::Left) {
			// Hover: show the diagnostic message under the mouse.
			let (l, col) = self.column_at(line, f32::from(ev.position.x));
			let hover = self.doc().and_then(|doc| {
				let text = &doc.buf.lines[l];
				let byte = text.char_indices().nth(col).map_or(text.len(), |(b, _)| b);
				let markers = self.doc_markers();
				let hit = marker_ranges(&markers, l, text).iter().position(|(r, _)| r.contains(&byte))?;
				let m = markers.iter().filter(|m| m.start.0 <= l + 1 && l + 1 <= m.end.0 && m.severity >= 2).nth(hit)?;
				let x = text[..byte.min(text.len())].chars().count() as f32 * self.char_w;
				Some((l, x, format!("{}{}", m.message, m.source.as_ref().map(|s| format!("  {s}")).unwrap_or_default())))
			});
			if hover != self.hover {
				self.hover = hover;
				cx.notify();
			}
			return;
		}
		let pos = self.column_at(line, f32::from(ev.position.x));
		if let Some(doc) = self.doc_mut() {
			if doc.buf.cursor != pos {
				doc.buf.set_cursor(pos, true);
				cx.notify();
			}
		}
	}

	fn file_icon(&self, name: &str, lang: &str) -> Div {
		let fg = self.theme.c("foreground", 0xbfbfbfff);
		match self.file_icons.for_file(name, lang) {
			// Seti glyphs are drawn at 150% of the 16px icon box, centered.
			Some((ch, color)) => div()
				.flex()
				.flex_none()
				.items_center()
				.justify_center()
				.w(px(16.))
				.h(px(ROW_H))
				.font_family(icons::SETI_FONT)
				.text_size(px(24.))
				.text_color(color.unwrap_or(fg))
				.child(ch.to_string()),
			None => div().w(px(16.)),
		}
	}

	fn render_titlebar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
		let t = &self.theme;
		let fg = t.c("titleBar.activeForeground", 0x8c8c8cff);
		let title = self.root.as_ref().and_then(|r| r.file_name()).map_or("Code - OSS".into(), |n| n.to_string_lossy().to_string());
		let maximized = window.is_maximized();

		// Title bar flex layout from titlebarpart.css (has-center): left 20% grow 2, center fit-content
		// with 10px margins, right 20% grow 2 (min-content). The menu bar overflows into "…".
		let width = f32::from(window.viewport_size().width);
		let search_w = (width * 0.38).min(600.);
		let center_w = 20. + 20. + 6. + search_w + 2. + 6.;
		let right_min = 4. * 22. + 3. * 4. + 4. + 3. * 46.;
		let (base_l, base_r) = (width * 0.2, (width * 0.2).max(right_min));
		let left_w = base_l + ((width - base_l - base_r - center_w - 20.) / 2.).max(0.);
		let ts = window.text_system();
		let ui_font = ts.resolve_font(&gpui::font(self.fonts.ui.clone()));
		let text_w = |s: &str| s.chars().filter_map(|c| ts.advance(ui_font, px(UI_SIZE), c).ok()).map(|a| f32::from(a.width)).sum::<f32>();
		let all_menus = ["File", "Edit", "Selection", "View", "Go", "Run", "Terminal", "Help"];
		let mut used = 35.;
		let mut menus = Vec::new();
		for (i, m) in all_menus.iter().enumerate() {
			let w = text_w(m) + 16.;
			let reserve = if i + 1 < all_menus.len() { 30. } else { 0. };
			if used + w + reserve > left_w {
				break;
			}
			used += w;
			menus.push(*m);
		}
		let overflow = menus.len() < all_menus.len();

		div()
			.id("titlebar")
			.h(px(TITLE_H))
			.flex_none()
			.flex()
			.items_center()
			.relative()
			.bg(t.c("titleBar.activeBackground", 0x191a1bff))
			.text_color(fg)
			.on_mouse_down(MouseButton::Left, |ev, window, _| {
				if ev.click_count == 2 {
					window.zoom_window();
				} else {
					window.start_window_move();
				}
			})
			// app icon + menu bar (window.titleBarStyle "custom" on Linux/Windows)
			.child(
				div()
					.w(px(left_w))
					.h_full()
					.flex()
					.flex_none()
					.overflow_hidden()
					.child(div().w(px(35.)).h_full().flex().items_center().justify_center().child(img(Path::new(env!("CARGO_MANIFEST_DIR")).join("../resources/linux/code.png")).size(px(16.))))
					.children(menus.iter().map(|m| div().px(px(8.)).h_full().flex().items_center().child(*m)))
					.when(overflow, |d| d.child(icon(icons::ELLIPSIS, 16., fg).w(px(30.)).h_full())),
			)
			// command center: back/forward + search box
			.child(
				div()
					.flex()
					.flex_none()
					.items_center()
					.mx(px(10.))
					.child(icon(icons::ARROW_LEFT, 16., fg.opacity(0.5)).w(px(20.)).h(px(22.)))
					.child(icon(icons::ARROW_RIGHT, 16., fg.opacity(0.5)).w(px(20.)).h(px(22.)))
					.child(
						div()
							.w(px(search_w))
							.h(px(24.))
							.mx(px(6.))
							.pl(px(8.))
							.flex()
							.items_center()
							.rounded(px(6.))
							.border_1()
							.border_color(t.c("commandCenter.border", 0x2e3031ff))
							.bg(t.c("commandCenter.background", 0x191a1bff))
							.child(title)
							.id("command-center")
							.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
							.on_click(cx.listener(|wb, _, _, cx| wb.open_quick(String::new(), cx))),
					),
			)
			.child(div().flex_1())
			// layout controls + window controls
			.child(
				div()
					.flex()
					.items_center()
					.gap(px(4.))
					.mr(px(4.))
					.child(action(icons::LAYOUT, fg))
					.child(action(icons::LAYOUT_SIDEBAR_LEFT, fg))
					.child(action(icons::LAYOUT_PANEL_OFF, fg))
					.child(action(icons::LAYOUT_SIDEBAR_RIGHT_OFF, fg)),
			)
			.child(
				div()
					.flex()
					.h_full()
					.child(icon(icons::CHROME_MINIMIZE, 16., fg).w(px(46.)).h_full().id("min").on_click(|_, window, _| window.minimize_window()))
					.child(
						icon(if maximized { '\u{eabb}' } else { icons::CHROME_MAXIMIZE }, 16., fg)
							.w(px(46.))
							.h_full()
							.id("max")
							.on_click(|_, window, _| window.zoom_window()),
					)
					.child(
						icon(icons::CHROME_CLOSE, 16., fg)
							.w(px(46.))
							.h_full()
							.id("close")
							.hover(|s| s.bg(gpui::rgb(0xe81123)).text_color(gpui::white()))
							.on_click(cx.listener(|_, _, _, cx| cx.quit())),
					),
			)
	}

	fn render_sidebar(&self, cx: &mut Context<Self>) -> Div {
		let t = &self.theme;
		let fg = t.c("sideBar.foreground", 0xbfbfbfff);
		let active_fg = t.c("activityBar.foreground", 0xbfbfbfff);
		let inactive_fg = t.c("activityBar.inactiveForeground", 0x8c8c8cff);
		let border = t.c("sideBar.border", 0x2a2b2cff);
		let activity = |ch: char, active: bool| {
			icon(ch, 24., if active { active_fg } else { inactive_fg })
				.size(px(36.))
				.rounded(px(4.))
				.when(active, |d| d.bg(t.c("activityBar.activeBackground", 0xffffff1a)))
		};

		let rows = self.root.as_ref().map(|r| flatten(&self.dir_cache, &self.expanded, r, 0)).unwrap_or_default();
		let active_path = self.doc().map(|d| d.path.clone());
		let guide = t.c("tree.indentGuidesStroke", 0x585858ff);
		let explorer_rows = rows.into_iter().map(|(depth, path, is_dir)| {
			let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
			let selected = active_path.as_ref() == Some(&path);
			let lead = if is_dir {
				icon(if self.expanded.contains(&path) { icons::CHEVRON_DOWN } else { icons::CHEVRON_RIGHT }, 16., fg).w(px(16.)).h(px(ROW_H))
			} else {
				self.file_icon(&name, &language_for(&path, &self.languages))
			};
			let p = path.clone();
			div()
				.id(SharedString::from(path.to_string_lossy().to_string()))
				.relative()
				.h(px(ROW_H))
				.mx(px(4.))
				.pl(px(11. + depth as f32 * 8.))
				.flex()
				.items_center()
				.gap(px(3.))
				.rounded(px(4.))
				.when(selected, |d| d.bg(t.c("list.inactiveSelectionBackground", 0x2c2d2eff)).text_color(t.c("list.activeSelectionForeground", 0xedededff)))
				.hover(|s| s.bg(t.c("list.hoverBackground", 0xffffff14)))
				.children((1..=depth).map(|d| div().absolute().top_0().bottom_0().left(px(16. + (d - 1) as f32 * 8.)).w(px(1.)).bg(guide)))
				.child(lead)
				.child(name)
				.on_click(cx.listener(move |wb, _, window, cx| {
					if is_dir {
						if !wb.expanded.remove(&p) {
							wb.dir_cache.entry(p.clone()).or_insert_with(|| read_dir_sorted(&p));
							wb.expanded.insert(p.clone());
						}
						cx.notify();
					} else {
						wb.open(p.clone(), cx);
						window.focus(&wb.focus);
					}
				}))
		});

		let section = |title: &str, open: bool| {
			div()
				.h(px(28.))
				.flex()
				.items_center()
				.gap(px(3.))
				.pl(px(5.))
				.rounded(px(4.))
				.text_size(px(12.))
				.font_weight(FontWeight::SEMIBOLD)
				.text_color(t.c("sideBarSectionHeader.foreground", 0xbfbfbfff))
				.child(icon(if open { icons::CHEVRON_DOWN } else { icons::CHEVRON_RIGHT }, 16., fg))
				.child(title.to_string())
		};
		let root_name = self.root.as_ref().and_then(|r| r.file_name()).map(|n| n.to_string_lossy().to_string());

		// floatingPanels.css: the activity bar is its own card (border on all four sides); the side bar
		// card gives up its left border at the seam.
		let surface_border = t.colors.get("surface.border").copied().unwrap_or(border);
		div()
			.flex()
			.flex_none()
			.text_color(fg)
			.child(
				// activity bar
				div()
					.w(px(ACTIVITY_W))
					.flex()
					.flex_col()
					.items_center()
					.py(px(4.))
					.gap(px(8.))
					.bg(t.c("activityBar.background", 0x191a1bff))
					.border_1()
					.border_color(t.colors.get("modernActivityBar.border").copied().unwrap_or(surface_border))
					.rounded_l(px(8.))
					.child(activity(icons::FILES, true))
					.child(activity(icons::SEARCH, false))
					.child(activity(icons::SOURCE_CONTROL, false))
					.child(activity(icons::DEBUG_ALT, false))
					.child(activity(icons::EXTENSIONS, false))
					.child(div().flex_1())
					.child(activity(icons::ACCOUNT, false))
					.child(activity(icons::SETTINGS_GEAR, false)),
			)
			.when(self.sidebar_visible, |d| d.child(
				// side bar: Explorer
				div()
					.w(px(SIDEBAR_W))
					.flex()
					.flex_col()
					.bg(t.c("sideBar.background", 0x191a1bff))
					.border_t_1()
					.border_r_1()
					.border_b_1()
					.border_color(surface_border)
					.rounded_r(px(8.))
					.child(
						div()
							.h(px(32.))
							.pl(px(12.))
							.pr(px(8.))
							.flex()
							.items_center()
							.justify_between()
							.text_size(px(12.))
							.font_weight(FontWeight::SEMIBOLD)
							.text_color(t.c("sideBarTitle.foreground", 0xbfbfbfff))
							.child("Explorer")
							.child(action(icons::ELLIPSIS, fg)),
					)
					.children(root_name.map(|n| div().mx(px(4.)).child(section(&n, true))))
					.child(div().id("explorer").flex_1().flex().flex_col().overflow_y_scroll().children(explorer_rows))
					.child(div().border_t_1().border_color(border).mx(px(8.)).child(section("Outline", false).h(px(27.))))
					.child(div().border_t_1().border_color(border).mx(px(8.)).mb(px(5.)).child(section("Timeline", false).h(px(27.)))),
			))
	}

	fn render_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
		self.ensure_tokens(MINIMAP_LINES);
		let ts = window.text_system();
		if let Ok(adv) = ts.advance(ts.resolve_font(&gpui::font(self.fonts.mono.clone())), px(FONT_SIZE), 'm') {
			self.char_w = f32::from(adv.width);
		}
		let t = &self.theme;
		let icon_fg = t.c("icon.foreground", 0x8c8c8cff);
		let card = div()
			.flex_1()
			.flex()
			.flex_col()
			.overflow_hidden()
			.rounded(px(8.))
			.border_1()
			.border_color(t.colors.get("editor.border").or(t.colors.get("surface.border")).copied().unwrap_or_else(|| t.c("widget.border", 0x2a2b2cff)))
			.bg(t.c("editor.background", 0x121314ff));

		let tabs = self.docs.iter().enumerate().map(|(i, d)| {
			let active = Some(i) == self.active;
			let name = d.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
			// modernUI tabs: 6px left inset, 22px icon box, 24px actions box, 8px between tabs.
			let tab_fg = if active { t.c("list.activeSelectionForeground", 0xedededff) } else { t.c("tab.inactiveForeground", 0x8c8c8cff) };
			div()
				.id(("tab", i))
				.h(px(TABS_H - 1.))
				.pl(px(6.))
				.pr(px(5.))
				.mr(px(8.))
				.flex()
				.items_center()
				.rounded(px(4.))
				.when(active, |d| d.bg(t.c("tab.activeBackground", 0x121314ff)))
				.text_color(tab_fg)
				.child(self.file_icon(&name, &d.lang).pr(px(6.)).w(px(22.)))
				.child(name)
				.child(
					div().ml(px(2.)).w(px(24.)).flex().justify_center().child(
						icon(if d.dirty { icons::CIRCLE_FILLED } else { icons::CLOSE }, 16., tab_fg)
							.size(px(20.))
							.rounded(px(4.))
							.id(("close", i))
							.on_click(cx.listener(move |wb, _, _, cx| wb.close(i, cx))),
					),
				)
				.on_click(cx.listener(move |wb, _, window, cx| {
					wb.active = Some(i);
					wb.sync_editor();
					wb.popup = None;
					window.focus(&wb.focus);
					cx.notify();
				}))
		});
		let header = div()
			.h(px(TABS_H))
			.flex_none()
			.flex()
			.items_start()
			// modernUI connected tabs: strip color over the editor background (connectedEditorTabs.ts)
			.bg(t.c("editor.background", 0x121314ff).blend(t.colors.get("editorGroupHeader.connectedTabsBackground").copied().unwrap_or_else(|| t.c("editorGroupHeader.tabsBackground", 0x191a1bff))))
			.border_b_1()
			.border_color(t.c("editorGroupHeader.tabsBorder", 0x2a2b2cff))
			.child(div().flex_1().flex().h_full().children(tabs))
			.when(self.doc().is_some(), |d| {
				d.child(div().h_full().flex().items_center().gap(px(4.)).px(px(4.)).child(action(icons::SPLIT_HORIZONTAL, icon_fg)).child(action(icons::ELLIPSIS, icon_fg)))
			});

		let Some(doc) = self.doc() else {
			return card.child(header);
		};

		// breadcrumbs: folders relative to the workspace, then the file
		let rel = self.root.as_ref().and_then(|r| doc.path.strip_prefix(r).ok()).unwrap_or(&doc.path).to_path_buf();
		let parts: Vec<String> = rel.iter().map(|p| p.to_string_lossy().to_string()).collect();
		let crumb_fg = t.c("breadcrumb.foreground", 0x8c8c8cff);
		let crumbs = parts.iter().enumerate().flat_map(|(i, p)| {
			let last = i + 1 == parts.len();
			let mut v = Vec::new();
			if i > 0 {
				v.push(icon(icons::CHEVRON_RIGHT, 16., crumb_fg));
			}
			v.push(div().flex().items_center().gap(px(3.)).when(last, |d| d.child(self.file_icon(p, &doc.lang).h(px(BREADCRUMBS_H)))).child(p.clone()));
			v
		});

		let (cl, cc) = doc.buf.cursor;
		let items = self.visible_items();
		let selected = self.popup.as_ref().map_or(0, |p| p.selected.min(items.len().saturating_sub(1)));
		let scroll_y = f32::from(doc.scroll.0.borrow().base_handle.offset().y);
		let line_fg = t.c("editorLineNumber.foreground", 0x858889ff);
		let line_active = t.c("editorLineNumber.activeForeground", 0xbbbebfff);
		let line_hl = t.c("editor.lineHighlightBackground", 0x242526ff);
		let line_dimmed = t.colors.get("editorLineNumber.dimmedForeground").copied().unwrap_or(line_fg.opacity(0.4));
		let indent_guide = t.c("editorIndentGuide.background1", 0x404040ff);
		let selection_bg = t.c("editor.selectionBackground", 0x264f78ff);
		let squiggle_error = t.c("editorError.foreground", 0xf14c4cff);
		let squiggle_warning = t.c("editorWarning.foreground", 0xcca700ff);
		let squiggle_info = t.c("editorInfo.foreground", 0x3794ffff);
		let cursor_c = t.c("editorCursor.foreground", 0xbbbebfff);
		let editor_fg = t.c("editor.foreground", 0xbbbebfff);
		// Minimap (scale 1): each character is a 1x2px block in its token color.
		// shortcut: only the first MINIMAP_LINES lines, and the minimap does not scroll with the editor.
		let mut minimap: Vec<(f32, f32, f32, Hsla)> = Vec::new();
		for (i, line) in doc.buf.lines.iter().take(MINIMAP_LINES).enumerate() {
			let runs: Vec<(Range<usize>, Option<Hsla>)> = match doc.tokens.get(i) {
				Some(t) => t.runs.iter().map(|(r, st)| (r.clone(), st.foreground)).collect(),
				None => vec![(0..line.len(), None)],
			};
			for (r, color) in runs {
				let col = line[..r.start].chars().count() as f32;
				let piece = &line[r.clone()];
				let lead = piece.chars().take_while(|c| c.is_whitespace()).count() as f32;
				let w = piece.trim_end().chars().count() as f32 - lead;
				if w > 0. {
					minimap.push((i as f32 * 2., col + lead, w, color.unwrap_or(editor_fg).opacity(0.6)));
				}
			}
		}

		card.child(header)
			.child(div().h(px(BREADCRUMBS_H)).flex_none().pl(px(16.)).flex().items_center().text_color(crumb_fg).children(crumbs))
			.child(
				div()
					.flex_1()
					.relative()
					.overflow_hidden()
					.font_family(self.fonts.mono.clone())
					.text_size(px(FONT_SIZE))
					.line_height(px(LINE_H))
					.text_color(t.c("editor.foreground", 0xbbbebfff))
					.child(
						uniform_list(
							"lines",
							doc.buf.lines.len(),
							cx.processor(move |wb, range: std::ops::Range<usize>, _window, cx| {
								wb.ensure_tokens(range.end);
								let char_w = wb.char_w;
								let Some(doc) = wb.doc() else { return Vec::new() };
								let lines = &doc.buf.lines;
								// Indentation in levels of 4 columns; blank lines take the smaller of their neighbours (like the guides renderer).
								let indent_of = |l: &str| l.chars().take_while(|c| c.is_whitespace()).map(|c| if c == '\t' { 4 } else { 1 }).sum::<usize>() / 4;
								let blank = |l: &String| l.trim().is_empty();
								let level = |ix: usize| -> usize {
									if !blank(&lines[ix]) {
										return indent_of(&lines[ix]);
									}
									let prev = lines[..ix].iter().rev().take(200).find(|l| !blank(l)).map_or(0, |l| indent_of(l));
									let next = lines[ix + 1..].iter().take(200).find(|l| !blank(l)).map_or(0, |l| indent_of(l));
									prev.min(next)
								};
								let selection = doc.buf.selection();
								let markers = wb.doc_markers();
								let display_col = |line: &str, chars: usize| -> f32 { line.chars().take(chars).map(|c| if c == '\t' { 4. } else { 1. }).sum::<f32>() * char_w };
								// editor.renderFinalNewline "dimmed": the empty last line's number at 40%.
								let final_empty = lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty());
								range
									.map(|ix| {
										let line = &doc.buf.lines[ix];
										let (text, map) = expand_tabs(line);
										// Token styles per byte, then diagnostics squiggles on top; merged into runs.
										let mut per_byte: Vec<(TokenStyle, Option<Hsla>)> = vec![(TokenStyle::default(), None); line.len()];
										for (r, st) in doc.tokens.get(ix).into_iter().flat_map(|t| &t.runs) {
											per_byte[r.clone()].iter_mut().for_each(|b| b.0 = *st);
										}
										for (r, sev) in marker_ranges(&markers, ix, line) {
											let c = match sev {
												8 => squiggle_error,
												4 => squiggle_warning,
												_ => squiggle_info,
											};
											per_byte[r].iter_mut().for_each(|b| b.1 = Some(c));
										}
										let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
										let mut start = 0;
										for b in 1..=per_byte.len() {
											if b == per_byte.len() || per_byte[b] != per_byte[start] {
												let (st, squiggle) = per_byte[start];
												let underline = squiggle
													.map(|color| UnderlineStyle { thickness: px(1.), color: Some(color), wavy: true })
													.or_else(|| st.underline.then(|| UnderlineStyle { thickness: px(1.), ..Default::default() }));
												highlights.push((
													map[start]..map[b],
													HighlightStyle {
														color: st.foreground,
														font_weight: st.bold.then_some(FontWeight::BOLD),
														font_style: st.italic.then_some(FontStyle::Italic),
														underline,
														..Default::default()
													},
												));
												start = b;
											}
										}
										let styled = StyledText::new(text).with_highlights(highlights);
										// shortcut: indent guides are all drawn inactive (no active-block highlight).
										let guides = (0..level(ix)).map(|k| div().absolute().top_0().bottom_0().left(px((k * 4) as f32 * char_w)).w(px(1.)).bg(indent_guide));
										// Selection: from the start column (or 0) to the end column (or past the line end, for the newline).
										let sel = selection.filter(|((sl, _), (el, _))| (*sl..=*el).contains(&ix)).map(|((sl, sc), (el, ec))| {
											let x0 = if ix == sl { display_col(line, sc) } else { 0. };
											let x1 = if ix == el { display_col(line, ec) } else { display_col(line, usize::MAX) + char_w };
											div().absolute().top_0().bottom_0().left(px(x0)).w(px((x1 - x0).max(0.))).bg(selection_bg)
										});
										let content = div().relative().flex_1().h_full().children(sel).children(guides).child(styled);
										let content = if ix == cl {
											// shortcut: the caret x assumes a monospace grid (wide CJK glyphs are off).
											let col: usize = line.chars().take(cc).map(|c| if c == '\t' { 4 } else { 1 }).sum();
											content
												.when(selection.is_none(), |d| d.bg(line_hl))
												.child(div().absolute().top_0().left(px(col as f32 * char_w)).w(px(2.)).h_full().bg(cursor_c))
										} else {
											content
										};
										div()
											.id(ix)
											.flex()
											.w_full()
											.h(px(LINE_H))
											.whitespace_nowrap()
											.on_mouse_down(
												MouseButton::Left,
												cx.listener(move |wb, ev, window, cx| {
													window.focus(&wb.focus);
													wb.click_line(ix, ev, cx)
												}),
											)
											.on_mouse_move(cx.listener(move |wb, ev, _, cx| wb.drag_line(ix, ev, cx)))
											.child(
												div()
													.w(px(GUTTER))
													.flex_none()
													.pr(px(GUTTER - LINE_NUMBER_RIGHT))
													.flex()
													.justify_end()
													.text_color(if ix == cl {
														line_active
													} else if final_empty && ix + 1 == lines.len() {
														line_dimmed
													} else {
														line_fg
													})
													.child((ix + 1).to_string()),
											)
											.child(content)
											.child(div().w(px(MINIMAP_W + SCROLLBAR_W)).flex_none())
									})
									.collect()
							}),
						)
						.track_scroll(doc.scroll.clone())
						.size_full(),
					)
					.child(div().absolute().top_0().bottom_0().right(px(SCROLLBAR_W - 1.)).w(px(1.)).bg(t.c("editorOverviewRuler.border", 0x2a2b2cff).opacity(0.5)))
					// minimap: 1px per character, 2px per line (scale 1)
					.child(
						div().absolute().top_0().right(px(SCROLLBAR_W)).w(px(MINIMAP_W)).h_full().children(
							minimap
								.into_iter()
								.enumerate()
								.filter(|(_, (_, x, _, _))| *x < MINIMAP_W)
								.map(|(_, (y, x, w, c))| div().absolute().top(px(y)).left(px(x)).w(px(w.min(MINIMAP_W - x))).h(px(2.)).bg(c)),
						),
					)
					.children(self.hover.as_ref().map(|(line, x, text)| {
						div()
							.absolute()
							.top(px((*line + 1) as f32 * LINE_H + scroll_y))
							.left(px(GUTTER + x))
							.max_w(px(500.))
							.px(px(8.))
							.py(px(4.))
							.rounded(px(4.))
							.font_family(self.fonts.ui.clone())
							.text_size(px(UI_SIZE))
							.bg(t.c("editorHoverWidget.background", 0x202122ff))
							.border_1()
							.border_color(t.c("editorHoverWidget.border", 0x2a2b2cff))
							.text_color(t.c("editorHoverWidget.foreground", 0xbfbfbfff))
							.child(text.clone())
					}))
					.when(!items.is_empty(), |d| {
						d.child(
							div()
								.absolute()
								.top(px((cl + 1) as f32 * LINE_H + scroll_y))
								.left(px(GUTTER + cc as f32 * self.char_w))
								.min_w(px(240.))
								.rounded(px(4.))
								.bg(t.c("editorSuggestWidget.background", 0x202122ff))
								.border_1()
								.border_color(t.c("editorSuggestWidget.border", 0x2a2b2cff))
								.text_color(t.c("editorSuggestWidget.foreground", 0xbfbfbfff))
								.children(items.iter().enumerate().map(|(i, (label, _))| {
									div()
										.px(px(6.))
										.h(px(LINE_H))
										.when(i == selected, |d| d.bg(t.c("editorSuggestWidget.selectedBackground", 0xffffff26)))
										.child(label.clone())
								})),
						)
					}),
			)
	}

	fn render_statusbar(&self) -> Div {
		let t = &self.theme;
		let fg = t.c("statusBar.foreground", 0x8c8c8cff);
		let item = || div().h(px(24.)).px(px(5.)).mx(px(3.)).flex().items_center().gap(px(4.)).rounded(px(4.));
		let mut right = Vec::new();
		if let Some(doc) = self.doc() {
			let (l, c) = doc.buf.cursor;
			let lang = self.languages.iter().find(|x| x.id == doc.lang).map_or("Plain Text".to_string(), |x| x.name.clone());
			right.push(item().child(format!("Ln {}, Col {}", l + 1, c + 1)));
			right.push(item().child(indentation_label(&doc.buf.lines)));
			right.push(item().child("UTF-8"));
			right.push(item().child(if doc.crlf { "CRLF" } else { "LF" }));
			right.push(item().child(icon(icons::BRACKET, 14., fg)).child(lang));
		}
		right.push(item().child(icon(icons::BELL, 16., fg)));
		div()
			.h(px(STATUS_H))
			.flex_none()
			.flex()
			.items_center()
			.px(px(9.))
			.bg(t.c("statusBar.background", 0x191a1bff))
			.text_color(fg)
			.text_size(px(12.))
			.child(item().w(px(28.)).justify_center().child(icon(icons::REMOTE, 16., fg)))
			.child({
				let count = |sev: u64| self.markers.values().flat_map(|f| f.values()).flatten().filter(|m| m.severity == sev).count().to_string();
				item().child(icon(icons::ERROR, 14., fg)).child(count(8)).child(icon(icons::WARNING, 14., fg)).child(count(4))
			})
			.child(div().flex_1())
			.children(right)
	}
}

impl Workbench {
	/// Quick input widget, measured from the workbench: 602px wide, 6px from the top, 26px input,
	/// 22px rows, at most 40% of the window tall.
	fn render_quick(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement> {
		let q = self.quick.as_ref()?;
		let t = &self.theme;
		let items = self.quick_items();
		let width = f32::from(window.viewport_size().width);
		let max_rows = ((f32::from(window.viewport_size().height) * 0.4) / ROW_H).floor().max(1.) as usize;
		let first = q.selected.saturating_sub(max_rows - 1);
		let fg = t.c("quickInput.foreground", 0xbfbfbfff);
		let focus_bg = t.c("quickInputList.focusBackground", 0x297aa0ff);
		let focus_fg = t.c("quickInputList.focusForeground", 0xffffffff);
		let desc_fg = t.c("descriptionForeground", 0x8c8c8cff);
		let hl = t.colors.get("list.highlightForeground").copied();
		let commands = q.query.starts_with('>');
		let keycap = |k: &str| {
			div()
				.px(px(5.))
				.h(px(18.))
				.flex()
				.items_center()
				.rounded(px(3.))
				.text_size(px(11.))
				.bg(t.c("keybindingLabel.background", 0x8080802b))
				.text_color(t.c("keybindingLabel.foreground", 0xccccccff))
				.border_b_1()
				.border_color(t.c("keybindingLabel.bottomBorder", 0x0000005c))
				.child(k.to_string())
		};
		let rows = items.iter().enumerate().skip(first).take(max_rows).map(|(i, item)| {
			let focused = i == q.selected;
			let row_fg = if focused { focus_fg } else { fg };
			let highlights: Vec<(Range<usize>, HighlightStyle)> = item
				.hits
				.iter()
				.filter_map(|&c| {
					let (b, ch) = item.label.char_indices().nth(c)?;
					Some((b..b + ch.len_utf8(), HighlightStyle { font_weight: Some(FontWeight::BOLD), color: if focused { None } else { hl }, ..Default::default() }))
				})
				.collect();
			let action = item.action.clone();
			div()
				.id(("quick", i))
				.h(px(ROW_H))
				.mx(px(6.))
				.pl(px(9.))
				.pr(px(4.))
				.flex()
				.items_center()
				.gap(px(6.))
				.rounded(px(4.))
				.text_color(row_fg)
				.when(focused, |d| d.bg(focus_bg))
				.when(item.file, |d| d.child(self.file_icon(&item.label, &language_for(Path::new(&item.label), &self.languages))))
				.child(StyledText::new(item.label.clone()).with_highlights(highlights))
				.when(!item.description.is_empty(), |d| d.child(div().text_size(px(UI_SIZE * 0.9)).text_color(if focused { focus_fg } else { desc_fg }).child(item.description.clone())))
				.child(div().flex_1())
				.when(i == 0 && item.file && q.query.is_empty(), |d| d.child(div().text_size(px(11.)).text_color(t.c("pickerGroup.foreground", 0xbfbfbfff)).child("recently opened")))
				.when(!item.keys.is_empty(), |d| {
					d.child(div().flex().items_center().gap(px(4.)).children(item.keys.iter().enumerate().flat_map(|(n, k)| {
						let mut v = Vec::new();
						if n > 0 {
							v.push(div().text_size(px(11.)).child("+"));
						}
						v.push(keycap(k));
						v
					})))
				})
				.on_click(cx.listener(move |wb, _, window, cx| wb.run_quick(action.clone(), window, cx)))
		});
		let list_h = (items.len().min(max_rows) as f32 * ROW_H + 7.).min(max_rows as f32 * ROW_H + 1.);
		let placeholder = if commands { "" } else { "Search files by name (append : to go to line or @ to go to symbol)" };
		Some(
			div()
				.absolute()
				.size_full()
				.child(div().id("quick-backdrop").absolute().size_full().on_mouse_down(MouseButton::Left, cx.listener(|wb, _, window, cx| {
					wb.quick = None;
					window.focus(&wb.focus);
					cx.notify();
				})))
				.child(
					div()
						.id("quick-input")
						.occlude()
						.absolute()
						.top(px(6.))
						.left(px(((width - 602.) / 2.).floor()))
						.w(px(602.))
						.flex()
						.flex_col()
						.bg(t.c("quickInput.background", 0x202122ff))
						.text_color(fg)
						.border_1()
						.border_color(t.c("widget.border", 0x2a2b2cff))
						.rounded(px(8.))
						.shadow(vec![gpui::BoxShadow { color: gpui::rgba(0x00000026).into(), offset: gpui::point(px(0.), px(0.)), blur_radius: px(20.), spread_radius: px(0.) }])
						.child(
							div().pt(px(6.)).px(px(6.)).pb(px(4.)).child(
								div()
									.h(px(26.))
									.px(px(6.))
									.flex()
									.items_center()
									.rounded(px(6.))
									.bg(t.c("input.background", 0x191a1bff))
									.border_1()
									.border_color(t.c("focusBorder", 0x3994bcb3))
									.child(q.query.clone())
									.child(div().w(px(1.)).h(px(17.)).bg(fg))
									.when(q.query.is_empty(), |d| d.child(div().ml(px(-1.)).text_color(t.c("input.placeholderForeground", 0x8c8c8cff)).child(placeholder))),
							),
						)
						.when(!items.is_empty(), |d| d.child(div().mt(px(2.)).h(px(list_h)).overflow_hidden().children(rows))),
				),
		)
	}
}

impl Render for Workbench {
	fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
		let titlebar = self.render_titlebar(window, cx);
		let sidebar = self.render_sidebar(cx);
		let editor = self.render_editor(window, cx);
		let status = self.render_statusbar();
		let quick = self.render_quick(window, cx);
		let t = &self.theme;
		// Notification toasts: bottom right, above the status bar.
		let toasts = (!self.notifications.is_empty()).then(|| {
			div().absolute().right(px(12.)).bottom(px(STATUS_H + 8.)).w(px(450.)).flex().flex_col().gap(px(8.)).children(self.notifications.iter().map(|(id, sev, text)| {
				let (glyph, color) = match sev {
					3 => (icons::ERROR, t.c("notificationsErrorIcon.foreground", 0xf14c4cff)),
					2 => (icons::WARNING, t.c("notificationsWarningIcon.foreground", 0xcca700ff)),
					_ => (icons::INFO, t.c("notificationsInfoIcon.foreground", 0x3794ffff)),
				};
				let id = *id;
				div()
					.id(("toast", id as usize))
					.occlude()
					.flex()
					.items_center()
					.gap(px(8.))
					.px(px(12.))
					.py(px(10.))
					.rounded(px(8.))
					.bg(t.c("notifications.background", 0x202122ff))
					.border_1()
					.border_color(t.c("notificationToast.border", 0x2a2b2cff))
					.text_color(t.c("notifications.foreground", 0xbfbfbfff))
					.shadow(vec![gpui::BoxShadow { color: gpui::rgba(0x00000026).into(), offset: gpui::point(px(0.), px(0.)), blur_radius: px(20.), spread_radius: px(0.) }])
					.child(icon(glyph, 16., color))
					.child(div().flex_1().child(text.clone()))
					.child(action(icons::CLOSE, t.c("icon.foreground", 0x8c8c8cff)).id(("toast-close", id as usize)).on_click(cx.listener(move |wb, _, _, cx| {
						wb.notifications.retain(|n| n.0 != id);
						cx.notify();
					})))
			}))
		});
		div()
			.relative()
			.key_context("Workbench")
			.track_focus(&self.focus)
			.on_key_down(cx.listener(Self::key_down))
			.size_full()
			.flex()
			.flex_col()
			.bg(self.theme.c("titleBar.activeBackground", 0x191a1bff))
			.text_color(self.theme.c("foreground", 0xbfbfbfff))
			.font_family(self.fonts.ui.clone())
			.text_size(px(UI_SIZE))
			.child(titlebar)
			.child(div().flex_1().flex().gap(px(GAP)).px(px(GAP)).pb(px(GAP)).overflow_hidden().child(sidebar).child(editor))
			.child(status)
			.children(toasts)
			.children(quick)
	}
}

fn main() {
	let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
	Application::new().run(move |cx: &mut App| {
		cx.text_system().add_fonts(vec![Cow::Borrowed(icons::CODICON_TTF), Cow::Borrowed(icons::SETI_TTF)]).expect("load icon fonts");
		let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
		cx.open_window(
			WindowOptions {
				window_bounds: Some(WindowBounds::Windowed(bounds)),
				titlebar: None,
				window_decorations: Some(if std::env::var_os("CODE_NATIVE_SERVER_DECORATIONS").is_some() { WindowDecorations::Server } else { WindowDecorations::Client }),
				app_id: Some("code-oss".into()),
				..Default::default()
			},
			|window, cx| {
				let wb = cx.new(|cx| Workbench::new(args.clone(), cx));
				window.focus(&wb.read(cx).focus.clone());
				wb
			},
		)
		.expect("open window");
		cx.on_window_closed(|cx| cx.quit()).detach();
		cx.activate(true);
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn snippets_and_selectors() {
		assert_eq!(strip_snippet("fn ${1:name}($2) {$0}"), "fn name() {}");
		assert_eq!(strip_snippet("a\\$b"), "a$b");
		assert_eq!(strip_snippet("if ${1:x} { ${2:y} }"), "if x { y }");
		assert!(selector_matches(&json!([{ "language": "rust", "scheme": "file" }]), "rust", "file"));
		assert!(!selector_matches(&json!([{ "language": "css" }]), "rust", "file"));
		assert!(selector_matches(&json!([{ "scheme": "*" }]), "rust", "untitled"));
	}

	#[test]
	fn language_from_contributions() {
		let langs = languages_of(&[json!({ "contributes": { "languages": [
			{ "id": "rust", "aliases": ["Rust"], "extensions": [".rs"] },
			{ "id": "dockerfile", "filenames": ["Dockerfile"] },
			{ "id": "typescript", "extensions": [".ts"] }, { "id": "dts", "extensions": [".d.ts"] },
		] } })]);
		assert_eq!(language_for(Path::new("/x/main.RS"), &langs), "rust");
		assert_eq!(language_for(Path::new("Dockerfile"), &langs), "dockerfile");
		assert_eq!(language_for(Path::new("a.d.ts"), &langs), "dts");
		assert_eq!(language_for(Path::new("a.txt"), &langs), "plaintext");
		assert_eq!(langs[0].name, "Rust");
	}

	#[test]
	fn completion_item_dto() {
		assert_eq!(completion_item(&json!({ "a": "foo" })), Some(("foo".into(), "foo".into(), "foo".into())));
		assert_eq!(completion_item(&json!({ "a": { "label": "bar" }, "h": "bar(${1})", "i": 4, "e": "1" })), Some(("bar".into(), "bar()".into(), "1".into())));
	}

	#[test]
	fn diagnostic_ranges() {
		assert_eq!(utf16_col_to_byte("a😀b", 1), 0);
		assert_eq!(utf16_col_to_byte("a😀b", 4), 5); // after the surrogate pair
		assert_eq!(utf16_col_to_byte("ab", 9), 2);
		let m = Marker { severity: 4, message: "x".into(), source: None, start: (1, 3), end: (2, 2) };
		let hint = Marker { severity: 1, ..m.clone() };
		let ms = [&m, &hint];
		assert_eq!(marker_ranges(&ms, 0, "abcdef"), [(2..6, 4)]);
		assert_eq!(marker_ranges(&ms, 1, "xyz"), [(0..1, 4)]);
		assert!(marker_ranges(&ms, 2, "zzz").is_empty());
		let empty = Marker { start: (1, 2), end: (1, 2), ..m.clone() };
		assert_eq!(marker_ranges(&[&empty], 0, "abc"), [(1..2, 4)]);
	}

	#[test]
	fn tabs_map_bytes() {
		let (text, map) = expand_tabs("\tä=1");
		assert_eq!(text, "    ä=1");
		assert_eq!(map, [0, 4, 4, 6, 7, 8]);
	}

	#[test]
	fn indentation_and_sorting() {
		assert_eq!(indentation_label(&["fn x() {".into(), "    a".into(), "        b".into()]), "Spaces: 4");
		assert_eq!(indentation_label(&["\ta".into()]), "Tab Size: 4");
		let dir = std::env::temp_dir().join(format!("code-native-sort-{}", std::process::id()));
		std::fs::create_dir_all(dir.join("src")).unwrap();
		std::fs::create_dir_all(dir.join(".git")).unwrap();
		std::fs::write(dir.join("B.toml"), "").unwrap();
		std::fs::write(dir.join("a.rs"), "").unwrap();
		assert_eq!(read_dir_sorted(&dir), [("src".into(), true), ("a.rs".into(), false), ("B.toml".into(), false)]);
		std::fs::remove_dir_all(dir).unwrap();
	}
}
