// Proxy ids are assigned 1..N in the order extHost.protocol.ts calls createProxyIdentifier
// (it is the only file that does), so generate the table from that file.
fn main() {
	let src = "../src/vs/workbench/api/common/extHost.protocol.ts";
	println!("cargo:rerun-if-changed={src}");
	let text = std::fs::read_to_string(src).expect("read extHost.protocol.ts");
	let names: Vec<&str> = text
		.split("createProxyIdentifier<")
		.skip(1)
		.map(|s| s.split("('").nth(1).and_then(|r| r.split('\'').next()).expect("proxy id name"))
		.collect();
	assert!(names.len() > 100, "unexpected proxy id count {}", names.len());
	let body: String = names.iter().map(|n| format!("\t\"{n}\",\n")).collect();
	let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("proxy_ids.rs");
	std::fs::write(out, format!("/// Index + 1 is the rpc id.\npub const PROXY_IDS: &[&str] = &[\n{body}];\n")).unwrap();
}
