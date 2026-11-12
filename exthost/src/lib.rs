/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! Starts VS Code's real Node extension host (`out/vs/workbench/api/node/extensionHostProcess`)
//! and plays the "main thread" side of its RPC protocol from Rust, so a native UI can run
//! unmodified VS Code extensions.
//!
//! Wire format mirrors `ipc.net.ts` (PersistentProtocol framing) and `rpcProtocol.ts` (RPC messages).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

include!(concat!(env!("OUT_DIR"), "/proxy_ids.rs"));

pub fn rpc_id(proxy: &str) -> Option<u8> {
	PROXY_IDS.iter().position(|p| *p == proxy).map(|i| (i + 1) as u8)
}

pub fn proxy_name(rpc_id: u8) -> &'static str {
	PROXY_IDS.get((rpc_id as usize).wrapping_sub(1)).copied().unwrap_or("?")
}

// --- PersistentProtocol framing (ipc.net.ts) ---

const FRAME_REGULAR: u8 = 1;
const FRAME_ACK: u8 = 3;
const HEADER_LEN: usize = 13;

fn write_frame(w: &mut impl Write, kind: u8, id: u32, ack: u32, data: &[u8]) -> io::Result<()> {
	let mut buf = Vec::with_capacity(HEADER_LEN + data.len());
	buf.push(kind);
	buf.extend_from_slice(&id.to_be_bytes());
	buf.extend_from_slice(&ack.to_be_bytes());
	buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
	buf.extend_from_slice(data);
	w.write_all(&buf)
}

fn read_frame(r: &mut impl Read) -> io::Result<(u8, u32, Vec<u8>)> {
	let mut h = [0u8; HEADER_LEN];
	r.read_exact(&mut h)?;
	let id = u32::from_be_bytes(h[1..5].try_into().unwrap());
	let len = u32::from_be_bytes(h[9..13].try_into().unwrap()) as usize;
	let mut data = vec![0; len];
	r.read_exact(&mut data)?;
	Ok((h[0], id, data))
}

// --- RPC messages (rpcProtocol.ts MessageIO) ---

#[derive(Debug, PartialEq)]
pub enum Message {
	Request { req: u32, rpc_id: u8, method: String, args: Vec<Value> },
	Reply { req: u32, result: Result<Option<Value>, String> },
	/// Acknowledged / Cancel: nothing to do for us.
	Other,
}

struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
	fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
		if self.0.len() < n {
			return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated rpc message"));
		}
		let (a, b) = self.0.split_at(n);
		self.0 = b;
		Ok(a)
	}
	fn u8(&mut self) -> io::Result<u8> {
		Ok(self.take(1)?[0])
	}
	fn u32(&mut self) -> io::Result<u32> {
		Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
	}
	fn short_str(&mut self) -> io::Result<String> {
		let n = self.u8()? as usize;
		Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
	}
	fn long_bytes(&mut self) -> io::Result<&'a [u8]> {
		let n = self.u32()? as usize;
		self.take(n)
	}
	fn json(&mut self) -> io::Result<Value> {
		serde_json::from_slice(self.long_bytes()?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
	}
}

pub fn decode(buf: &[u8]) -> io::Result<Message> {
	let mut c = Cursor(buf);
	let kind = c.u8()?;
	let req = c.u32()?;
	Ok(match kind {
		1 | 2 => {
			let rpc_id = c.u8()?;
			let method = c.short_str()?;
			let args = c.json()?.as_array().cloned().unwrap_or_default();
			Message::Request { req, rpc_id, method, args }
		}
		3 | 4 => {
			let rpc_id = c.u8()?;
			let method = c.short_str()?;
			let n = c.u8()?;
			let mut args = Vec::with_capacity(n as usize);
			for _ in 0..n {
				args.push(match c.u8()? {
					1 => c.json()?,
					// shortcut: binary args (VSBuffer) arrive as null; decode them when a feature needs bytes (e.g. file system providers).
					2 => {
						c.long_bytes()?;
						Value::Null
					}
					3 => {
						let count = c.u32()?;
						let v = c.json()?;
						for _ in 0..count {
							c.long_bytes()?;
						}
						v
					}
					_ => Value::Null,
				});
			}
			Message::Request { req, rpc_id, method, args }
		}
		7 => Message::Reply { req, result: Ok(None) },
		8 => {
			c.long_bytes()?;
			Message::Reply { req, result: Ok(Some(Value::Null)) }
		}
		9 => Message::Reply { req, result: Ok(Some(c.json()?)) },
		10 => {
			let count = c.u32()?;
			let v = c.json()?;
			for _ in 0..count {
				c.long_bytes()?;
			}
			Message::Reply { req, result: Ok(Some(v)) }
		}
		11 => Message::Reply { req, result: Err(c.json()?.to_string()) },
		12 => Message::Reply { req, result: Err("error".into()) },
		_ => Message::Other,
	})
}

fn header(kind: u8, req: u32) -> Vec<u8> {
	let mut b = vec![kind];
	b.extend_from_slice(&req.to_be_bytes());
	b
}

fn push_long(b: &mut Vec<u8>, s: &[u8]) {
	b.extend_from_slice(&(s.len() as u32).to_be_bytes());
	b.extend_from_slice(s);
}

/// `cancellable`: the callee's last parameter is a CancellationToken, which the receiver appends.
pub fn encode_request(req: u32, rpc_id: u8, method: &str, args: &[Value], cancellable: bool) -> Vec<u8> {
	let mut b = header(if cancellable { 2 } else { 1 }, req);
	b.push(rpc_id);
	b.push(method.len() as u8);
	b.extend_from_slice(method.as_bytes());
	push_long(&mut b, Value::from(args.to_vec()).to_string().as_bytes());
	b
}

pub fn encode_reply(req: u32, result: &Result<Option<Value>, String>) -> Vec<u8> {
	match result {
		Ok(None) => header(7, req),
		Ok(Some(v)) => {
			let mut b = header(9, req);
			push_long(&mut b, v.to_string().as_bytes());
			b
		}
		Err(msg) => {
			let mut b = header(11, req);
			push_long(&mut b, json!({ "$isError": true, "name": "Error", "message": msg }).to_string().as_bytes());
			b
		}
	}
}

// --- Host ---

/// Answers a request from the extension host: (proxy, method, args) -> reply (`None` = undefined).
pub type Handler = Box<dyn Fn(&str, &str, &[Value]) -> Result<Option<Value>, String> + Send>;

pub struct Options {
	/// Node binary.
	pub node: PathBuf,
	/// VS Code root containing `out/bootstrap-fork.js`.
	pub app_root: PathBuf,
	/// Extension folders (each with a package.json).
	pub extensions: Vec<PathBuf>,
	/// Where global/workspace storage and logs go.
	pub data_dir: PathBuf,
}

struct Writer {
	stream: UnixStream,
	out_id: u32,
}

pub struct ExtHost {
	child: Child,
	writer: Arc<Mutex<Writer>>,
	incoming_id: Arc<AtomicU32>,
	pending: Arc<Mutex<HashMap<u32, Sender<Result<Option<Value>, String>>>>>,
	next_req: AtomicU32,
}

fn uri(path: &Path) -> Value {
	json!({ "$mid": 1, "scheme": "file", "authority": "", "path": path.to_string_lossy(), "query": "", "fragment": "" })
}

fn ext_id(id: &str) -> Value {
	json!({ "value": id, "_lower": id.to_lowercase() })
}

/// Turns an extension folder into the IExtensionDescription the extension host expects.
pub fn describe_extension(dir: &Path) -> io::Result<(String, Value)> {
	let pkg: Value = serde_json::from_slice(&std::fs::read(dir.join("package.json"))?)
		.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
	let id = format!("{}.{}", pkg["publisher"].as_str().unwrap_or("undefined_publisher"), pkg["name"].as_str().unwrap_or(""));
	let mut desc = pkg.clone();
	let o = desc.as_object_mut().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "package.json is not an object"))?;
	o.insert("identifier".into(), ext_id(&id));
	o.insert("extensionLocation".into(), uri(&dir.canonicalize()?));
	o.insert("isBuiltin".into(), json!(false));
	o.insert("isUserBuiltin".into(), json!(false));
	o.insert("isUnderDevelopment".into(), json!(false));
	o.insert("targetPlatform".into(), json!("undefined"));
	o.insert("preRelease".into(), json!(false));
	o.entry("activationEvents").or_insert(json!([]));
	Ok((id, desc))
}

fn init_data(opts: &Options) -> io::Result<Value> {
	let mut all = Vec::new();
	let mut events = serde_json::Map::new();
	let mut mine = Vec::new();
	for dir in &opts.extensions {
		let (id, desc) = describe_extension(dir)?;
		events.insert(id.clone(), desc["activationEvents"].clone());
		mine.push(ext_id(&id));
		all.push(desc);
	}
	let product: Value = std::fs::read(opts.app_root.join("product.json"))
		.ok()
		.and_then(|b| serde_json::from_slice(&b).ok())
		.unwrap_or(json!({}));
	let version = std::fs::read(opts.app_root.join("package.json"))
		.ok()
		.and_then(|b| serde_json::from_slice::<Value>(&b).ok())
		.and_then(|p| p["version"].as_str().map(String::from))
		.unwrap_or_else(|| "1.0.0".into());
	let d = &opts.data_dir;
	Ok(json!({
		"version": version,
		"quality": product["quality"],
		"commit": product["commit"],
		"parentPid": std::process::id(),
		"environment": {
			"isExtensionDevelopmentDebug": false,
			"appName": product["nameLong"].as_str().unwrap_or("Code - OSS"),
			"appHost": "desktop",
			"appRoot": uri(&opts.app_root),
			"appLanguage": "en",
			"isExtensionTelemetryLoggingOnly": false,
			"appUriScheme": product["urlProtocol"].as_str().unwrap_or("code-oss"),
			"globalStorageHome": uri(&d.join("globalStorage")),
			"workspaceStorageHome": uri(&d.join("workspaceStorage")),
		},
		"workspace": null,
		"extensions": { "versionId": 0, "allExtensions": all, "activationEvents": events, "myExtensions": mine },
		"telemetryInfo": { "sessionId": "rust", "machineId": "rust", "sqmId": "", "devDeviceId": "", "firstSessionDate": "" },
		"logLevel": 3,
		"loggers": [],
		"logsLocation": uri(&d.join("logs")),
		"autoStart": true,
		"remote": { "isRemote": false, "authority": null, "connectionData": null },
		"consoleForward": { "includeStack": false, "logNative": false },
		"uiKind": 1,
	}))
}

fn empty_config_model() -> Value {
	json!({ "contents": {}, "keys": [], "overrides": [] })
}

impl ExtHost {
	// shortcut: unix domain sockets only; Windows needs a named pipe here.
	pub fn start(opts: Options, handler: Handler) -> io::Result<ExtHost> {
		std::fs::create_dir_all(&opts.data_dir)?;
		let sock = std::env::temp_dir().join(format!("vscode-rs-exthost-{}.sock", std::process::id()));
		let _ = std::fs::remove_file(&sock);
		let listener = UnixListener::bind(&sock)?;

		let mut cmd = Command::new(&opts.node);
		cmd.arg(opts.app_root.join("out").join("bootstrap-fork.js"))
			.arg("--type=extensionHost")
			.env("VSCODE_ESM_ENTRYPOINT", "vs/workbench/api/node/extensionHostProcess")
			.env("VSCODE_HANDLES_UNCAUGHT_ERRORS", "true")
			.env("VSCODE_EXTHOST_IPC_HOOK", &sock);
		if opts.app_root.join("src").is_dir() {
			cmd.env("VSCODE_DEV", "1");
		}
		let child = cmd.spawn()?;
		let (stream, _) = listener.accept()?;
		let _ = std::fs::remove_file(&sock);

		let host = ExtHost {
			child,
			writer: Arc::new(Mutex::new(Writer { stream: stream.try_clone()?, out_id: 0 })),
			incoming_id: Arc::new(AtomicU32::new(0)),
			pending: Arc::default(),
			next_req: AtomicU32::new(1),
		};

		let (handshake_tx, handshake_rx) = channel();
		host.spawn_reader(stream, handler, handshake_tx);
		let wait = |rx: &Receiver<u8>, want: u8| match rx.recv_timeout(Duration::from_secs(60)) {
			Ok(b) if b == want => Ok(()),
			other => Err(io::Error::other(format!("extension host handshake failed: expected {want}, got {other:?}"))),
		};
		wait(&handshake_rx, 2)?; // Ready
		host.send_raw(init_data(&opts)?.to_string().as_bytes())?;
		wait(&handshake_rx, 1)?; // Initialized

		// What the workbench's MainThread* services send right after connecting; activation waits on these.
		let m = empty_config_model();
		host.notify("ExtHostConfiguration", "$initializeConfiguration", vec![json!({
			"defaults": m, "policy": m, "application": m, "userLocal": m, "userRemote": m, "workspace": m,
			"folders": [], "configurationScopes": [],
		})])?;
		host.notify("ExtHostWorkspace", "$initializeWorkspace", vec![Value::Null, json!(true)])?;
		host.notify("ExtHostTelemetry", "$initializeTelemetryLevel", vec![json!(0), json!(false)])?;
		Ok(host)
	}

	fn spawn_reader(&self, mut stream: UnixStream, handler: Handler, handshake: Sender<u8>) {
		let (writer, pending, incoming) = (self.writer.clone(), self.pending.clone(), self.incoming_id.clone());
		std::thread::spawn(move || {
			while let Ok((kind, id, data)) = read_frame(&mut stream) {
				if kind != FRAME_REGULAR {
					continue; // ack / keepalive / resume
				}
				incoming.store(id, Ordering::SeqCst);
				{
					let mut w = writer.lock().unwrap();
					let _ = write_frame(&mut w.stream, FRAME_ACK, 0, id, &[]);
				}
				if data.len() == 1 {
					let _ = handshake.send(data[0]);
					continue;
				}
				match decode(&data) {
					Ok(Message::Request { req, rpc_id, method, args }) => {
						let reply = handler(proxy_name(rpc_id), &method, &args);
						let body = encode_reply(req, &reply);
						let mut w = writer.lock().unwrap();
						w.out_id += 1;
						let id = w.out_id;
						let _ = write_frame(&mut w.stream, FRAME_REGULAR, id, incoming.load(Ordering::SeqCst), &body);
					}
					Ok(Message::Reply { req, result }) => {
						if let Some(tx) = pending.lock().unwrap().remove(&req) {
							let _ = tx.send(result);
						}
					}
					Ok(Message::Other) => {}
					Err(e) => eprintln!("exthost: bad rpc message: {e}"),
				}
			}
			// Connection gone: fail everyone still waiting.
			for (_, tx) in pending.lock().unwrap().drain() {
				let _ = tx.send(Err("extension host disconnected".into()));
			}
		});
	}

	fn send_raw(&self, body: &[u8]) -> io::Result<()> {
		let mut w = self.writer.lock().unwrap();
		w.out_id += 1;
		let id = w.out_id;
		write_frame(&mut w.stream, FRAME_REGULAR, id, self.incoming_id.load(Ordering::SeqCst), body)
	}

	fn send_request(&self, req: u32, proxy: &str, method: &str, args: Vec<Value>, cancellable: bool) -> io::Result<()> {
		let rpc = rpc_id(proxy).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("unknown proxy {proxy}")))?;
		self.send_raw(&encode_request(req, rpc, method, &args, cancellable))
	}

	/// Fire-and-forget call (for `void` methods).
	pub fn notify(&self, proxy: &str, method: &str, args: Vec<Value>) -> io::Result<()> {
		self.send_request(self.next_req.fetch_add(1, Ordering::SeqCst), proxy, method, args, false)
	}

	/// Calls `proxy.method(...args)` in the extension host and blocks for the reply.
	pub fn call(&self, proxy: &str, method: &str, args: Vec<Value>) -> Result<Option<Value>, String> {
		self.request(proxy, method, args, false)
	}

	/// Like `call`, for methods whose last parameter is a CancellationToken (e.g. `$provideCompletionItems`).
	pub fn call_cancellable(&self, proxy: &str, method: &str, args: Vec<Value>) -> Result<Option<Value>, String> {
		self.request(proxy, method, args, true)
	}

	fn request(&self, proxy: &str, method: &str, args: Vec<Value>, cancellable: bool) -> Result<Option<Value>, String> {
		let (tx, rx) = channel();
		let req = self.next_req.fetch_add(1, Ordering::SeqCst);
		self.pending.lock().unwrap().insert(req, tx);
		if let Err(e) = self.send_request(req, proxy, method, args, cancellable) {
			self.pending.lock().unwrap().remove(&req);
			return Err(e.to_string());
		}
		rx.recv().map_err(|e| e.to_string())?
	}
}

impl Drop for ExtHost {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn proxy_ids_match_protocol_order() {
		assert_eq!(rpc_id("MainThreadAuthentication"), Some(1));
		assert_eq!(proxy_name(rpc_id("ExtHostCommands").unwrap()), "ExtHostCommands");
		assert!(PROXY_IDS.len() < 256, "rpc ids are u8 on the wire");
	}

	#[test]
	fn request_and_reply_roundtrip() {
		let buf = encode_request(7, 10, "$registerCommand", &[json!("hello.greet")], false);
		assert_eq!(decode(&buf).unwrap(), Message::Request { req: 7, rpc_id: 10, method: "$registerCommand".into(), args: vec![json!("hello.greet")] });
		assert_eq!(decode(&encode_reply(3, &Ok(Some(json!({"a": 1}))))).unwrap(), Message::Reply { req: 3, result: Ok(Some(json!({"a": 1}))) });
		assert_eq!(decode(&encode_reply(4, &Ok(None))).unwrap(), Message::Reply { req: 4, result: Ok(None) });
		assert!(matches!(decode(&encode_reply(5, &Err("x".into()))).unwrap(), Message::Reply { req: 5, result: Err(_) }));
	}

	#[test]
	fn mixed_args_request() {
		// RequestMixedArgs: [String json, Undefined]
		let mut b = header(3, 9);
		b.push(10);
		b.push(4);
		b.extend_from_slice(b"$foo");
		b.push(2);
		b.push(1);
		push_long(&mut b, b"\"x\"");
		b.push(4);
		assert_eq!(decode(&b).unwrap(), Message::Request { req: 9, rpc_id: 10, method: "$foo".into(), args: vec![json!("x"), Value::Null] });
	}

	#[test]
	fn frame_roundtrip() {
		let mut out = Vec::new();
		write_frame(&mut out, FRAME_REGULAR, 5, 2, b"hi").unwrap();
		assert_eq!(out.len(), HEADER_LEN + 2);
		assert_eq!(read_frame(&mut out.as_slice()).unwrap(), (FRAME_REGULAR, 5, b"hi".to_vec()));
	}
}
