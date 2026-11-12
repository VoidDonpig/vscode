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
	let completion_handle = Arc::new(Mutex::new(None));
	let (seen, seen_handle) = (registered.clone(), completion_handle.clone());
	let host = ExtHost::start(
		Options {
			node: std::env::var_os("VSCODE_NODE").unwrap_or("node".into()).into(),
			app_root: root.canonicalize().unwrap(),
			extensions: vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/hello-ext")],
			data_dir: std::env::temp_dir().join("vscode-rs-exthost-test"),
			configuration_defaults: (json!({}), Vec::new()),
		},
		Box::new(move |proxy, method, args| {
			match (proxy, method) {
				("MainThreadCommands", "$registerCommand") => seen.lock().unwrap().push(args[0].as_str().unwrap_or_default().to_string()),
				("MainThreadLanguageFeatures", "$registerCompletionsProvider") => *seen_handle.lock().unwrap() = Some(args[0].clone()),
				_ => {}
			}
			Ok(None)
		}),
	)
	.expect("start extension host");

	host.call("ExtHostExtensionService", "$activateByEvent", vec![json!("onCommand:hello.greet"), json!(0)]).unwrap();
	assert!(registered.lock().unwrap().contains(&"hello.greet".to_string()));

	let result = host.call("ExtHostCommands", "$executeContributedCommand", vec![json!("hello.greet"), json!("Rust")]).unwrap();
	assert_eq!(result, Some(json!("Hello Rust from Code - OSS")));

	// Completions: open a document, then ask the provider the extension registered.
	let handle = completion_handle.lock().unwrap().clone().expect("completion provider registered");
	let doc = json!({ "$mid": 1, "scheme": "untitled", "authority": "", "path": "Untitled-1", "query": "", "fragment": "" });
	host.notify("ExtHostDocumentsAndEditors", "$acceptDocumentsAndEditorsDelta", vec![json!({
		"addedDocuments": [{ "uri": doc, "versionId": 1, "lines": ["fn main() {}"], "EOL": "\n", "languageId": "plaintext", "isDirty": false, "encoding": "utf8" }]
	})]).unwrap();
	let result = host
		.call_cancellable("ExtHostLanguageFeatures", "$provideCompletionItems", vec![handle, doc, json!({ "lineNumber": 1, "column": 1 }), json!({ "triggerKind": 0 })])
		.unwrap()
		.expect("completion result");
	let labels: Vec<&str> = result["b"].as_array().unwrap().iter().map(|i| i["a"].as_str().unwrap()).collect();
	assert_eq!(labels, ["rust", "line:fn main() {}"]);
}
