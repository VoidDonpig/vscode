/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! Data side of Quick Open (Ctrl+P) and the Command Palette (Ctrl+Shift+P): fuzzy matching,
//! workspace files, and commands contributed by extensions.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Case-insensitive subsequence match. Returns (score, matched char indices); higher is better:
/// consecutive matches and matches at word starts score more, like the workbench's fuzzy scorer.
pub fn fuzzy(query: &str, target: &str) -> Option<(i32, Vec<usize>)> {
	if query.is_empty() {
		return Some((0, Vec::new()));
	}
	let t: Vec<char> = target.chars().collect();
	let mut hits = Vec::new();
	let mut score = 0;
	let mut ti = 0;
	for qc in query.chars().filter(|c| !c.is_whitespace()) {
		let ql = qc.to_lowercase().next()?;
		let found = (ti..t.len()).find(|&i| t[i].to_lowercase().next() == Some(ql))?;
		score += 1;
		if hits.last() == Some(&(found.wrapping_sub(1))) {
			score += 5;
		}
		if found == 0 || !t[found - 1].is_alphanumeric() || (t[found].is_uppercase() && t[found - 1].is_lowercase()) {
			score += 3;
		}
		hits.push(found);
		ti = found + 1;
	}
	Some((score, hits))
}

/// Files under `root` for Quick Open (files.exclude defaults plus node_modules; capped).
// shortcut: .gitignore and search.exclude are not evaluated beyond node_modules.
pub fn workspace_files(root: &Path, excluded: &[&str], limit: usize) -> Vec<PathBuf> {
	let mut out = Vec::new();
	let mut dirs = vec![root.to_path_buf()];
	while let Some(dir) = dirs.pop() {
		let Ok(entries) = std::fs::read_dir(&dir) else { continue };
		let mut entries: Vec<_> = entries.flatten().collect();
		entries.sort_by_key(|e| e.file_name());
		for e in entries {
			let name = e.file_name().to_string_lossy().into_owned();
			if excluded.contains(&name.as_str()) || name == "node_modules" {
				continue;
			}
			let p = e.path();
			if p.is_dir() {
				dirs.push(p);
			} else {
				out.push(p);
				if out.len() >= limit {
					return out;
				}
			}
		}
	}
	out
}

/// A command from `contributes.commands`, as shown in the palette.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionCommand {
	pub id: String,
	pub label: String,
}

/// Replaces `%key%` placeholders with values from the extension's package.nls.json.
fn localize(s: &str, nls: &Value) -> String {
	match s.strip_prefix('%').and_then(|r| r.strip_suffix('%')) {
		Some(key) => nls[key].as_str().or_else(|| nls[key]["message"].as_str()).unwrap_or(s).to_string(),
		None => s.to_string(),
	}
}

/// Palette entries of one extension: "Category: Title", minus commands its
/// `menus.commandPalette` hides with `"when": "false"`.
// shortcut: other `when` clauses are not evaluated (no context keys), so those commands show.
pub fn extension_commands(dir: &Path, package: &Value) -> Vec<ExtensionCommand> {
	let nls: Value = std::fs::read(dir.join("package.nls.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
	let hidden: Vec<&str> = package["contributes"]["menus"]["commandPalette"]
		.as_array()
		.into_iter()
		.flatten()
		.filter(|m| m["when"].as_str() == Some("false"))
		.filter_map(|m| m["command"].as_str())
		.collect();
	let commands = match &package["contributes"]["commands"] {
		Value::Array(a) => a.clone(),
		v @ Value::Object(_) => vec![v.clone()],
		_ => Vec::new(),
	};
	commands
		.iter()
		.filter_map(|c| {
			let id = c["command"].as_str()?;
			if hidden.contains(&id) {
				return None;
			}
			let title = c["title"].as_str().map(|t| localize(t, &nls))?;
			let label = match c["category"].as_str().map(|cat| localize(cat, &nls)) {
				Some(cat) if !cat.is_empty() => format!("{cat}: {title}"),
				_ => title,
			};
			Some(ExtensionCommand { id: id.to_string(), label })
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn fuzzy_scores() {
		assert!(fuzzy("mrs", "main.rs").is_some());
		assert!(fuzzy("xyz", "main.rs").is_none());
		let (contiguous, _) = fuzzy("main", "main.rs").unwrap();
		let (scattered, _) = fuzzy("main", "m_a_i_n.rs").unwrap();
		assert!(contiguous > scattered);
		assert_eq!(fuzzy("fs", "File: Save").unwrap().1, [0, 6]);
	}

	#[test]
	fn commands_from_package() {
		let dir = std::env::temp_dir().join(format!("qi-nls-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join("package.nls.json"), r#"{ "cat": "Git", "t1": "Commit" }"#).unwrap();
		let pkg = json!({ "contributes": {
			"commands": [
				{ "command": "git.commit", "title": "%t1%", "category": "%cat%" },
				{ "command": "git.internal", "title": "Hidden" },
				{ "command": "x.plain", "title": "Plain" },
			],
			"menus": { "commandPalette": [{ "command": "git.internal", "when": "false" }] },
		} });
		let cmds = extension_commands(&dir, &pkg);
		assert_eq!(cmds, [
			ExtensionCommand { id: "git.commit".into(), label: "Git: Commit".into() },
			ExtensionCommand { id: "x.plain".into(), label: "Plain".into() },
		]);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn files_skip_excluded() {
		let dir = std::env::temp_dir().join(format!("qi-files-{}", std::process::id()));
		std::fs::create_dir_all(dir.join("src")).unwrap();
		std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
		std::fs::create_dir_all(dir.join(".git")).unwrap();
		std::fs::write(dir.join("src/a.rs"), "").unwrap();
		std::fs::write(dir.join("node_modules/x/b.js"), "").unwrap();
		std::fs::write(dir.join(".git/HEAD"), "").unwrap();
		assert_eq!(workspace_files(&dir, &[".git"], 100), [dir.join("src/a.rs")]);
		std::fs::remove_dir_all(dir).unwrap();
	}
}
