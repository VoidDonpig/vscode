//! End-to-end: real extension host + fixtures/hello-ext. Needs a compiled checkout (`npm run compile`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use code_exthost::{ExtHost, Options};
use serde_json::json;

#[test]
fn runs_a_real_extension_command() {
	let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
	if !root.join("out/bootstrap-fork.js").exists() {
		eprintln!("skipping: out/ not compiled");
		return;
	}
	let registered = Arc::new(Mutex::new(Vec::<String>::new()));
	let seen = registered.clone();
	let host = ExtHost::start(
		Options {
			node: std::env::var_os("VSCODE_NODE").unwrap_or("node".into()).into(),
			app_root: root.canonicalize().unwrap(),
			extensions: vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/hello-ext")],
			data_dir: std::env::temp_dir().join("vscode-rs-exthost-test"),
		},
		Box::new(move |proxy, method, args| {
			if (proxy, method) == ("MainThreadCommands", "$registerCommand") {
				seen.lock().unwrap().push(args[0].as_str().unwrap_or_default().to_string());
			}
			Ok(None)
		}),
	)
	.expect("start extension host");

	host.call("ExtHostExtensionService", "$activateByEvent", vec![json!("onCommand:hello.greet"), json!(0)]).unwrap();
	assert!(registered.lock().unwrap().contains(&"hello.greet".to_string()));

	let result = host.call("ExtHostCommands", "$executeContributedCommand", vec![json!("hello.greet"), json!("Rust")]).unwrap();
	assert_eq!(result, Some(json!("Hello Rust from Code - OSS")));
}
