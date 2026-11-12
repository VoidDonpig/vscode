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

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

const READY_PREFIX: &str = "Web UI available at ";

fn ready_url(line: &str) -> Option<&str> {
	line.find(READY_PREFIX).map(|i| line[i + READY_PREFIX.len()..].trim())
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

fn main() {
	let (mut server, url) = match start_server(std::env::args_os().nth(1).map(PathBuf::from)) {
		Ok(v) => v,
		Err(e) => {
			eprintln!("code-desktop: {e}");
			std::process::exit(1);
		}
	};

	let event_loop = EventLoop::new();
	let window = WindowBuilder::new()
		.with_title("Visual Studio Code")
		.with_inner_size(tao::dpi::LogicalSize::new(1280.0, 800.0))
		.build(&event_loop)
		.expect("failed to create window");

	let builder = WebViewBuilder::new().with_url(&url);
	#[cfg(not(target_os = "linux"))]
	let webview = builder.build(&window);
	#[cfg(target_os = "linux")]
	let webview = {
		use tao::platform::unix::WindowExtUnix;
		use wry::WebViewBuilderExtUnix;
		builder.build_gtk(window.default_vbox().unwrap())
	};
	let _webview = webview.expect("failed to create webview");

	event_loop.run(move |event, _, control_flow| {
		*control_flow = ControlFlow::Wait;
		// shortcut: server is only killed on window close, not on SIGTERM/crash; add a signal handler if orphans show up.
		if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
			let _ = server.kill();
			let _ = server.wait();
			*control_flow = ControlFlow::Exit;
		}
	});
}

#[cfg(test)]
mod tests {
	use super::ready_url;

	#[test]
	fn parses_ready_line() {
		assert_eq!(ready_url("Web UI available at http://localhost:4242?tkn=abc\n"), Some("http://localhost:4242?tkn=abc"));
		assert_eq!(ready_url("Extension host agent listening on 4242"), None);
	}
}
