/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! Native VS Code front end drawn with gpui (no Electron, no webview). Extensions run unmodified
//! in VS Code's Node extension host, driven through `code-exthost`.
//!
//! Usage: `code-native [file]`
//! - `CODE_NATIVE_EXTENSIONS`: `:`-separated extension folders, or folders of extensions
//!   (default: `~/.vscode-oss/extensions`).
//! - `VSCODE_NODE`: node binary (default `node`).

mod buffer;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use buffer::Buffer;
use code_exthost::{describe_extension, ExtHost, Options};
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt;
use gpui::{
	div, font, prelude::*, px, rgb, size, uniform_list, App, Application, Bounds, Context, FocusHandle, KeyDownEvent, MouseButton,
	MouseDownEvent, ScrollStrategy, SharedString, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};
use serde_json::{json, Value};

#[cfg(target_os = "macos")]
const MONO: &str = "Menlo";
#[cfg(windows)]
const MONO: &str = "Consolas";
#[cfg(not(any(target_os = "macos", windows)))]
const MONO: &str = "DejaVu Sans Mono";
const FONT_SIZE: f32 = 14.;
const LINE_H: f32 = 20.;
const GUTTER: f32 = 56.;

enum HostEvent {
	Ready(Arc<ExtHost>),
	Failed(String),
	CompletionProvider { handle: Value, selector: Value, triggers: Vec<String> },
}

struct Provider {
	handle: Value,
	triggers: Vec<String>,
}

struct Popup {
	/// (label, text to insert, sort key)
	items: Vec<(String, String, String)>,
	selected: usize,
}

struct Editor {
	focus: FocusHandle,
	buf: Buffer,
	path: Option<PathBuf>,
	uri: Value,
	lang: String,
	host: Option<Arc<ExtHost>>,
	providers: Vec<Provider>,
	/// Document as the extension host last saw it: (full range, UTF-16 length).
	synced: ((usize, usize, usize, usize), usize),
	version: u64,
	dirty: bool,
	popup: Option<Popup>,
	scroll: UniformListScrollHandle,
	char_w: f32,
	status: SharedString,
}

fn file_uri(path: &Path) -> Value {
	json!({ "$mid": 1, "scheme": "file", "authority": "", "path": path.to_string_lossy(), "query": "", "fragment": "" })
}

fn extension_dirs() -> Vec<PathBuf> {
	let roots: Vec<PathBuf> = match std::env::var_os("CODE_NATIVE_EXTENSIONS") {
		Some(v) => std::env::split_paths(&v).collect(),
		None => std::env::var_os("HOME").map(|h| vec![Path::new(&h).join(".vscode-oss/extensions")]).unwrap_or_default(),
	};
	let mut dirs = Vec::new();
	for root in roots {
		if root.join("package.json").is_file() {
			dirs.push(root);
		} else if let Ok(entries) = std::fs::read_dir(&root) {
			dirs.extend(entries.flatten().map(|e| e.path()).filter(|p| p.join("package.json").is_file()));
		}
	}
	dirs
}

/// Language id from the extensions' `contributes.languages`, like VS Code's language service.
fn language_for(path: Option<&Path>, extensions: &[Value]) -> String {
	let Some(name) = path.and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_lowercase()) else {
		return "plaintext".into();
	};
	for ext in extensions {
		for lang in ext["contributes"]["languages"].as_array().into_iter().flatten() {
			let hit = lang["extensions"].as_array().into_iter().flatten().filter_map(Value::as_str).any(|e| name.ends_with(&e.to_lowercase()))
				|| lang["filenames"].as_array().into_iter().flatten().filter_map(Value::as_str).any(|f| f.eq_ignore_ascii_case(&name));
			if let (true, Some(id)) = (hit, lang["id"].as_str()) {
				return id.into();
			}
		}
	}
	"plaintext".into()
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

impl Editor {
	fn new(path: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
		let text = path.as_deref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
		let ext_dirs = extension_dirs();
		let packages: Vec<Value> = ext_dirs.iter().filter_map(|d| describe_extension(d).ok().map(|(_, v)| v)).collect();
		let lang = language_for(path.as_deref(), &packages);
		let uri = match &path {
			Some(p) => file_uri(&std::path::absolute(p).unwrap_or(p.clone())),
			None => json!({ "$mid": 1, "scheme": "untitled", "authority": "", "path": "Untitled-1", "query": "", "fragment": "" }),
		};
		let buf = Buffer::new(&text);
		let synced = (buf.full_range(), text.encode_utf16().count());

		let (tx, mut rx) = unbounded();
		start_host(ext_dirs.clone(), tx);
		cx.spawn(async move |this, cx| {
			while let Some(ev) = rx.next().await {
				if this.update(cx, |ed, cx| ed.on_host_event(ev, cx)).is_err() {
					break;
				}
			}
		})
		.detach();

		Editor {
			focus: cx.focus_handle(),
			buf,
			path,
			uri,
			status: format!("{lang} · starting extension host ({} extensions)…", ext_dirs.len()).into(),
			lang,
			host: None,
			providers: Vec::new(),
			synced,
			version: 1,
			dirty: false,
			popup: None,
			scroll: UniformListScrollHandle::new(),
			char_w: FONT_SIZE * 0.6,
		}
	}

	fn on_host_event(&mut self, ev: HostEvent, cx: &mut Context<Self>) {
		match ev {
			HostEvent::Ready(host) => {
				let _ = host.notify("ExtHostDocumentsAndEditors", "$acceptDocumentsAndEditorsDelta", vec![json!({
					"addedDocuments": [{
						"uri": self.uri, "versionId": self.version, "lines": self.buf.lines, "EOL": "\n",
						"languageId": self.lang, "isDirty": self.dirty, "encoding": "utf8",
					}]
				})]);
				let events = ["*".to_string(), "onStartupFinished".into(), format!("onLanguage:{}", self.lang)];
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
				self.status = format!("{} · extension host ready", self.lang).into();
			}
			HostEvent::Failed(e) => self.status = format!("extension host failed: {e}").into(),
			HostEvent::CompletionProvider { handle, selector, triggers } => {
				let scheme = self.uri["scheme"].as_str().unwrap_or("file");
				if selector_matches(&selector, &self.lang, scheme) {
					self.providers.push(Provider { handle, triggers });
					self.status = format!("{} · {} completion provider(s)", self.lang, self.providers.len()).into();
				}
			}
		}
		cx.notify();
	}

	/// Sends the new text to the extension host as a full-document replace.
	// shortcut: full-document sync per edit; send incremental ranges when large files get slow.
	fn edited(&mut self, cx: &mut Context<Self>) {
		self.dirty = true;
		self.version += 1;
		let text = self.buf.text();
		let ((sl, sc, el, ec), old_len) = self.synced;
		if let Some(host) = &self.host {
			let _ = host.notify("ExtHostDocuments", "$acceptModelChanged", vec![
				self.uri.clone(),
				json!({
					"changes": [{
						"range": { "startLineNumber": sl, "startColumn": sc, "endLineNumber": el, "endColumn": ec },
						"rangeOffset": 0, "rangeLength": old_len, "text": text,
					}],
					"eol": "\n", "versionId": self.version, "isUndoing": false, "isRedoing": false,
				}),
				json!(true),
			]);
		}
		self.synced = (self.buf.full_range(), text.encode_utf16().count());
		self.reveal_cursor();
		cx.notify();
	}

	fn reveal_cursor(&self) {
		let state = self.scroll.0.borrow();
		let (top, height) = (-f32::from(state.base_handle.offset().y), f32::from(state.base_handle.bounds().size.height));
		drop(state);
		let y = self.buf.cursor.0 as f32 * LINE_H;
		if y < top {
			self.scroll.scroll_to_item(self.buf.cursor.0, ScrollStrategy::Top);
		} else if y + LINE_H > top + height {
			self.scroll.scroll_to_item(self.buf.cursor.0, ScrollStrategy::Bottom);
		}
	}

	fn request_completions(&mut self, trigger: Option<String>, cx: &mut Context<Self>) {
		let Some(host) = self.host.clone() else { return };
		let (line, col) = self.buf.position_utf16();
		let context = match &trigger {
			Some(ch) => json!({ "triggerKind": 1, "triggerCharacter": ch }),
			None => json!({ "triggerKind": 0 }),
		};
		let handles: Vec<Value> = self
			.providers
			.iter()
			.filter(|p| trigger.as_ref().is_none_or(|t| p.triggers.contains(t)))
			.map(|p| p.handle.clone())
			.collect();
		let (uri, version) = (self.uri.clone(), self.version);
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
			let _ = this.update(cx, |ed, cx| {
				if ed.version == version && !items.is_empty() {
					ed.popup = Some(Popup { items, selected: 0 });
					cx.notify();
				}
			});
		})
		.detach();
	}

	/// Items matching the word before the cursor: prefix matches first, then the extension's sortText.
	fn visible_items(&self) -> Vec<(String, String)> {
		let prefix = self.buf.word_prefix().to_lowercase();
		let mut items: Vec<_> = self.popup.iter().flat_map(|p| &p.items).filter(|(label, ..)| label.to_lowercase().contains(&prefix)).collect();
		items.sort_by_key(|(label, _, sort)| (!label.to_lowercase().starts_with(&prefix), sort.as_str()));
		items.into_iter().take(12).map(|(label, insert, _)| (label.clone(), insert.clone())).collect()
	}

	fn save(&mut self) {
		let Some(path) = &self.path else {
			self.status = "untitled: pass a file path to save".into();
			return;
		};
		match std::fs::write(path, self.buf.text()) {
			Ok(()) => {
				self.dirty = false;
				self.status = format!("saved {}", path.display()).into();
				if let Some(host) = &self.host {
					let _ = host.notify("ExtHostDocuments", "$acceptModelSaved", vec![self.uri.clone()]);
				}
			}
			Err(e) => self.status = format!("save failed: {e}").into(),
		}
	}

	fn key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
		let k = &ev.keystroke;
		let m = k.modifiers;
		let cmd = m.control || m.platform;

		if self.popup.is_some() {
			let items = self.visible_items();
			let popup = self.popup.as_mut().unwrap();
			match k.key.as_str() {
				"up" => return { popup.selected = popup.selected.saturating_sub(1); cx.notify() },
				"down" => return { popup.selected = (popup.selected + 1).min(items.len().saturating_sub(1)); cx.notify() },
				"enter" | "tab" if !items.is_empty() => {
					let insert = items[popup.selected.min(items.len() - 1)].1.clone();
					self.popup = None;
					let prefix = self.buf.word_prefix().chars().count();
					self.buf.replace_prefix(prefix, &insert);
					return self.edited(cx);
				}
				"escape" => return { self.popup = None; cx.notify() },
				_ => {}
			}
		}

		match k.key.as_str() {
			"s" if cmd => self.save(),
			"space" if cmd => self.request_completions(None, cx),
			"v" if cmd => {
				if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
					self.buf.insert(&text.replace("\r\n", "\n"));
					self.edited(cx);
				}
			}
			"left" => self.buf.move_by(0, -1),
			"right" => self.buf.move_by(0, 1),
			"up" => self.buf.move_by(-1, 0),
			"down" => self.buf.move_by(1, 0),
			"home" => self.buf.home(),
			"end" => self.buf.end(),
			"backspace" => {
				self.buf.backspace();
				self.edited(cx);
			}
			"delete" => {
				self.buf.delete();
				self.edited(cx);
			}
			"enter" => {
				self.buf.insert("\n");
				self.edited(cx);
			}
			"tab" => {
				self.buf.insert("    ");
				self.edited(cx);
			}
			"escape" => {}
			// shortcut: text arrives via key_char, so IME composition is not supported yet (needs gpui's InputHandler).
			_ if !cmd && !m.alt => {
				if let Some(ch) = k.key_char.clone() {
					self.buf.insert(&ch);
					self.edited(cx);
					let trigger = self.providers.iter().any(|p| p.triggers.contains(&ch));
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
		if self.popup.as_ref().is_some_and(|_| self.visible_items().is_empty()) {
			self.popup = None;
		}
		self.reveal_cursor();
		cx.notify();
	}

	fn click_line(&mut self, line: usize, ev: &MouseDownEvent, cx: &mut Context<Self>) {
		let col = ((f32::from(ev.position.x) - GUTTER) / self.char_w).round().max(0.) as usize;
		self.buf.cursor = (line, col.min(self.buf.lines[line].chars().count()));
		self.popup = None;
		cx.notify();
	}
}

fn start_host(extensions: Vec<PathBuf>, tx: UnboundedSender<HostEvent>) {
	std::thread::spawn(move || {
		let app_root = std::env::var_os("VSCODE_ROOT")
			.map(PathBuf::from)
			.unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join(".."));
		let events = tx.clone();
		let handler = Box::new(move |proxy: &str, method: &str, args: &[Value]| {
			if (proxy, method) == ("MainThreadLanguageFeatures", "$registerCompletionsProvider") {
				let triggers = args[2].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)).collect();
				let _ = events.unbounded_send(HostEvent::CompletionProvider { handle: args[0].clone(), selector: args[1].clone(), triggers });
			}
			Ok(None)
		});
		let opts = Options {
			node: std::env::var_os("VSCODE_NODE").unwrap_or("node".into()).into(),
			app_root: app_root.canonicalize().unwrap_or(app_root),
			extensions,
			data_dir: std::env::temp_dir().join("code-native"),
		};
		let _ = tx.unbounded_send(match ExtHost::start(opts, handler) {
			Ok(h) => HostEvent::Ready(Arc::new(h)),
			Err(e) => HostEvent::Failed(e.to_string()),
		});
	});
}

impl Render for Editor {
	fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
		let ts = window.text_system();
		if let Ok(adv) = ts.advance(ts.resolve_font(&font(MONO)), px(FONT_SIZE), 'm') {
			self.char_w = f32::from(adv.width);
		}
		let (cl, cc) = self.buf.cursor;
		let items = self.visible_items();
		let selected = self.popup.as_ref().map_or(0, |p| p.selected.min(items.len().saturating_sub(1)));
		let scroll_y = f32::from(self.scroll.0.borrow().base_handle.offset().y);
		let title = self.path.as_ref().map_or("Untitled-1".into(), |p| p.display().to_string());

		div()
			.key_context("Editor")
			.track_focus(&self.focus)
			.on_key_down(cx.listener(Self::key_down))
			.size_full()
			.flex()
			.flex_col()
			.bg(rgb(0x1e1e1e))
			.text_color(rgb(0xd4d4d4))
			.font_family(MONO)
			.text_size(px(FONT_SIZE))
			.child(
				div()
					.h(px(30.))
					.px_3()
					.flex()
					.items_center()
					.bg(rgb(0x252526))
					.text_color(rgb(0xcccccc))
					.child(format!("{title}{}", if self.dirty { " ●" } else { "" })),
			)
			.child(
				div()
					.flex_1()
					.relative()
					.overflow_hidden()
					.child(
						uniform_list(
							"lines",
							self.buf.lines.len(),
							cx.processor(move |ed, range: std::ops::Range<usize>, _window, cx| {
								range
									.map(|ix| {
										let text = ed.buf.lines[ix].replace('\t', "    ");
										let content = if ix == cl {
											let at = text.char_indices().nth(cc).map_or(text.len(), |(i, _)| i);
											div()
												.flex()
												.child(text[..at].to_string())
												.child(div().w(px(2.)).h(px(LINE_H)).bg(rgb(0xaeafad)))
												.child(text[at..].to_string())
										} else {
											div().child(text)
										};
										div()
											.id(ix)
											.flex()
											.h(px(LINE_H))
											.whitespace_nowrap()
											.when(ix == cl, |d| d.bg(rgb(0x2a2d2e)))
											.on_mouse_down(MouseButton::Left, cx.listener(move |ed, ev, _, cx| ed.click_line(ix, ev, cx)))
											.child(div().w(px(GUTTER - 12.)).mr(px(12.)).flex().justify_end().text_color(rgb(0x858585)).child((ix + 1).to_string()))
											.child(content)
									})
									.collect()
							}),
						)
						.track_scroll(self.scroll.clone())
						.size_full(),
					)
					.when(!items.is_empty(), |d| {
						d.child(
							div()
								.absolute()
								.top(px((cl + 1) as f32 * LINE_H + scroll_y))
								.left(px(GUTTER + cc as f32 * self.char_w))
								.min_w(px(240.))
								.bg(rgb(0x252526))
								.border_1()
								.border_color(rgb(0x454545))
								.children(items.iter().enumerate().map(|(i, (label, _))| {
									div().px_2().h(px(LINE_H)).when(i == selected, |d| d.bg(rgb(0x04395e))).child(label.clone())
								})),
						)
					}),
			)
			.child(
				div()
					.h(px(22.))
					.px_3()
					.flex()
					.items_center()
					.justify_between()
					.bg(rgb(0x007acc))
					.text_color(rgb(0xffffff))
					.text_size(px(12.))
					.child(self.status.clone())
					.child(format!("Ln {}, Col {}", cl + 1, cc + 1)),
			)
	}
}

fn main() {
	let path = std::env::args_os().nth(1).map(PathBuf::from);
	Application::new().run(move |cx: &mut App| {
		let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
		let title = path.as_ref().and_then(|p| p.file_name()).map_or("Untitled-1".into(), |n| n.to_string_lossy().to_string());
		let window = cx
			.open_window(
				WindowOptions {
					window_bounds: Some(WindowBounds::Windowed(bounds)),
					titlebar: Some(gpui::TitlebarOptions { title: Some(title.into()), ..Default::default() }),
					..Default::default()
				},
				|window, cx| {
					let editor = cx.new(|cx| Editor::new(path.clone(), cx));
					window.focus(&editor.read(cx).focus.clone());
					editor
				},
			)
			.expect("open window");
		let _ = window;
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
		let exts = [json!({ "contributes": { "languages": [{ "id": "rust", "extensions": [".rs"] }, { "id": "dockerfile", "filenames": ["Dockerfile"] }] } })];
		assert_eq!(language_for(Some(Path::new("/x/main.RS")), &exts), "rust");
		assert_eq!(language_for(Some(Path::new("Dockerfile")), &exts), "dockerfile");
		assert_eq!(language_for(None, &exts), "plaintext");
	}

	#[test]
	fn completion_item_dto() {
		assert_eq!(completion_item(&json!({ "a": "foo" })), Some(("foo".into(), "foo".into(), "foo".into())));
		assert_eq!(completion_item(&json!({ "a": { "label": "bar" }, "h": "bar(${1})", "i": 4, "e": "1" })), Some(("bar".into(), "bar()".into(), "1".into())));
	}
}
