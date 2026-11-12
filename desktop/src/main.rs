/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! Electron-free desktop shell: starts the VS Code server (which runs the
//! Node extension host, so extensions keep working) and shows its web
//! workbench in a native OS webview.
//!
//! Usage: `code-desktop [folder]`
//! - `VSCODE_SERVER_ROOT`: directory containing `out/server-main.js`
//!   (defaults to this repository checkout).
//! - `VSCODE_NODE`: node binary (defaults to `<root>/node`, then `node` on PATH).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{NewWindowResponse, WebView, WebViewBuilder};

const READY_PREFIX: &str = "Web UI available at ";

enum UserEvent {
	Open(String),
	Title(WindowId, String),
}

fn ready_url(line: &str) -> Option<&str> {
	line.find(READY_PREFIX).map(|i| line[i + READY_PREFIX.len()..].trim())
}

/// `scheme://host:port` of a URL, used to tell workbench windows from external links.
fn origin(url: &str) -> &str {
	let after_scheme = url.find("://").map_or(0, |i| i + 3);
	let end = url[after_scheme..].find(['/', '?', '#']).map_or(url.len(), |i| after_scheme + i);
	&url[..end]
}

/// Opens http(s)/mailto links in the system browser; anything else (file:, custom schemes) is dropped.
fn open_external(url: &str) {
	if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:")) {
		eprintln!("code-desktop: refusing to open {url}");
		return;
	}
	#[cfg(target_os = "linux")]
	let res = Command::new("xdg-open").arg(url).spawn();
	#[cfg(target_os = "macos")]
	let res = Command::new("open").arg(url).spawn();
	#[cfg(windows)]
	let res = Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
	if let Err(e) = res {
		eprintln!("code-desktop: failed to open {url}: {e}");
	}
}

fn start_server(folder: Option<PathBuf>) -> Result<(Child, String), String> {
	let root = std::env::var_os("VSCODE_SERVER_ROOT")
		.map(PathBuf::from)
		.unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join(".."));
	let entry = root.join("out").join("server-main.js");
	if !entry.exists() {
		return Err(format!("{} not found (run `npm run compile` or set VSCODE_SERVER_ROOT)", entry.display()));
	}
	let node = std::env::var_os("VSCODE_NODE").map(PathBuf::from).unwrap_or_else(|| {
		let bundled = root.join(if cfg!(windows) { "node.exe" } else { "node" });
		if bundled.is_file() { bundled } else { PathBuf::from("node") }
	});

	let mut cmd = Command::new(node);
	cmd.arg(&entry)
		.args(["--host", "127.0.0.1", "--port", "0", "--accept-server-license-terms"])
		// Random token so other local processes cannot drive this server.
		.args(["--connection-token", &uuid::Uuid::new_v4().simple().to_string()])
		.stdout(Stdio::piped());
	if root.join("src").is_dir() {
		// Source checkout: same env as scripts/code-server.sh.
		cmd.env("VSCODE_DEV", "1").env("NODE_ENV", "development");
	}
	if let Some(folder) = folder {
		let folder = folder.canonicalize().map_err(|e| format!("{}: {e}", folder.display()))?;
		cmd.arg("--default-folder").arg(folder);
	}
	// shortcut: only Linux kills the server when this process dies abruptly (kill/crash); window close works everywhere.
	#[cfg(target_os = "linux")]
	unsafe {
		use std::os::unix::process::CommandExt;
		cmd.pre_exec(|| {
			libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
			Ok(())
		});
	}

	let mut child = cmd.spawn().map_err(|e| format!("failed to start server: {e}"))?;
	let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
	for line in lines.by_ref() {
		let line = line.map_err(|e| e.to_string())?;
		println!("{line}");
		if let Some(url) = ready_url(&line) {
			let url = url.to_string();
			// Keep draining server output so it never blocks on a full pipe.
			std::thread::spawn(move || lines.map_while(Result::ok).for_each(|l| println!("{l}")));
			return Ok((child, url));
		}
	}
	let _ = child.kill();
	Err("server exited before the web UI was ready".into())
}

fn open_window(target: &EventLoopWindowTarget<UserEvent>, proxy: &EventLoopProxy<UserEvent>, url: &str) -> (Window, WebView) {
	let window = WindowBuilder::new()
		.with_title("Visual Studio Code")
		.with_inner_size(tao::dpi::LogicalSize::new(1280.0, 800.0))
		.build(target)
		.expect("failed to create window");

	let (open_proxy, title_proxy, id) = (proxy.clone(), proxy.clone(), window.id());
	let builder = WebViewBuilder::new()
		.with_url(url)
		.with_clipboard(true)
		.with_document_title_changed_handler(move |t| {
			let _ = title_proxy.send_event(UserEvent::Title(id, t));
		})
		// "New Window" in the workbench and external links both arrive here via window.open.
		.with_new_window_req_handler(move |url, _| {
			let _ = open_proxy.send_event(UserEvent::Open(url));
			NewWindowResponse::Deny
		});
	#[cfg(not(target_os = "linux"))]
	let webview = builder.build(&window);
	#[cfg(target_os = "linux")]
	let webview = {
		use tao::platform::unix::WindowExtUnix;
		use wry::WebViewBuilderExtUnix;
		builder.build_gtk(window.default_vbox().unwrap())
	};
	(window, webview.expect("failed to create webview"))
}

fn main() {
	let (mut server, url) = match start_server(std::env::args_os().nth(1).map(PathBuf::from)) {
		Ok(v) => v,
		Err(e) => {
			eprintln!("code-desktop: {e}");
			std::process::exit(1);
		}
	};
	let server_origin = origin(&url).to_string();

	let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
	let proxy = event_loop.create_proxy();
	let mut windows: HashMap<WindowId, (Window, WebView)> = HashMap::new();
	let first = open_window(&event_loop, &proxy, &url);
	windows.insert(first.0.id(), first);

	event_loop.run(move |event, target, control_flow| {
		*control_flow = ControlFlow::Wait;
		match event {
			Event::UserEvent(UserEvent::Open(u)) if origin(&u) == server_origin => {
				let w = open_window(target, &proxy, &u);
				windows.insert(w.0.id(), w);
			}
			Event::UserEvent(UserEvent::Open(u)) => open_external(&u),
			Event::UserEvent(UserEvent::Title(id, t)) => {
				if let Some((w, _)) = windows.get(&id) {
					w.set_title(&t);
				}
			}
			Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. } => {
				windows.remove(&window_id);
				if windows.is_empty() {
					let _ = server.kill();
					let _ = server.wait();
					*control_flow = ControlFlow::Exit;
				}
			}
			_ => {}
		}
	});
}

#[cfg(test)]
mod tests {
	use super::{origin, ready_url};

	#[test]
	fn parses_ready_line() {
		assert_eq!(ready_url("Web UI available at http://localhost:4242?tkn=abc\n"), Some("http://localhost:4242?tkn=abc"));
		assert_eq!(ready_url("Extension host agent listening on 4242"), None);
	}

	#[test]
	fn origin_of_urls() {
		assert_eq!(origin("http://localhost:4242?tkn=abc"), "http://localhost:4242");
		assert_eq!(origin("http://localhost:4242/?folder=/x"), "http://localhost:4242");
		assert_eq!(origin("http://localhost:42420/"), "http://localhost:42420");
		assert_eq!(origin("https://github.com/microsoft/vscode"), "https://github.com");
	}
}
