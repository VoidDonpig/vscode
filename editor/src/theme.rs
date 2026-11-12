/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! Loads VS Code color themes (JSONC with `include` chains) such as
//! `extensions/theme-defaults/themes/2026-dark.json`, the default dark theme.

use std::collections::HashMap;
use std::path::Path;

use gpui::{rgba, Hsla};
use serde_json::Value;

/// Removes `//` and `/* */` comments and trailing commas outside strings.
pub fn strip_jsonc(s: &str) -> String {
	// Second pass catches commas that were only trailing once a comment after them was removed.
	scan(&scan(s))
}

fn scan(s: &str) -> String {
	let b = s.as_bytes();
	let mut out = String::with_capacity(s.len());
	let (mut i, mut in_str) = (0, false);
	while i < b.len() {
		let c = b[i];
		if in_str {
			if c == b'\\' && i + 1 < b.len() {
				out.push_str(&s[i..i + 2]);
				i += 2;
				continue;
			}
			in_str = c != b'"';
		} else if c == b'"' {
			in_str = true;
		} else if c == b'/' && b.get(i + 1) == Some(&b'/') {
			while i < b.len() && b[i] != b'\n' {
				i += 1;
			}
			continue;
		} else if c == b'/' && b.get(i + 1) == Some(&b'*') {
			i = s[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
			continue;
		} else if c == b',' {
			let rest = s[i + 1..].trim_start();
			if rest.starts_with('}') || rest.starts_with(']') {
				i += 1;
				continue;
			}
		}
		let ch = s[i..].chars().next().unwrap();
		out.push(ch);
		i += ch.len_utf8();
	}
	out
}

/// `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`.
pub fn parse_color(s: &str) -> Option<Hsla> {
	let h = s.strip_prefix('#')?;
	let expand = |h: &str| h.chars().flat_map(|c| [c, c]).collect::<String>();
	let full = match h.len() {
		3 | 4 => expand(h),
		6 | 8 => h.to_string(),
		_ => return None,
	};
	let v = u32::from_str_radix(&full, 16).ok()?;
	Some(rgba(if full.len() == 6 { (v << 8) | 0xff } else { v }).into())
}

pub struct Theme {
	pub colors: HashMap<String, Hsla>,
}

impl Theme {
	pub fn load(path: &Path) -> Theme {
		let mut colors = HashMap::new();
		load_into(path, &mut colors, 0);
		Theme { colors }
	}

	/// Theme color, or the workbench default when the theme leaves it unset.
	pub fn c(&self, key: &str, fallback: u32) -> Hsla {
		self.colors.get(key).copied().unwrap_or_else(|| rgba(fallback).into())
	}
}

fn load_into(path: &Path, colors: &mut HashMap<String, Hsla>, depth: u32) {
	let Ok(text) = std::fs::read_to_string(path) else {
		eprintln!("code-native: theme {} not found", path.display());
		return;
	};
	let Ok(json) = serde_json::from_str::<Value>(&strip_jsonc(&text)) else {
		eprintln!("code-native: theme {} is not valid JSON", path.display());
		return;
	};
	if let (Some(inc), true) = (json["include"].as_str(), depth < 8) {
		load_into(&path.parent().unwrap_or(Path::new(".")).join(inc), colors, depth + 1);
	}
	for (k, v) in json["colors"].as_object().into_iter().flatten() {
		if let Some(c) = v.as_str().and_then(parse_color) {
			colors.insert(k.clone(), c);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn jsonc() {
		let s = "{ // c\n \"a\": \"//x\", /* b */ \"b\": [1, 2, // x\n], }";
		let v: Value = serde_json::from_str(&strip_jsonc(s)).unwrap();
		assert_eq!(v, serde_json::json!({ "a": "//x", "b": [1, 2] }));
	}

	#[test]
	fn colors() {
		assert_eq!(parse_color("#ffffff"), Some(rgba(0xffffffff).into()));
		assert_eq!(parse_color("#FFFFFF22"), Some(rgba(0xffffff22).into()));
		assert_eq!(parse_color("#fff"), Some(rgba(0xffffffff).into()));
		assert_eq!(parse_color("red"), None);
	}

	#[test]
	fn default_theme_resolves_includes() {
		let t = Theme::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/theme-defaults/themes/2026-dark.json"));
		assert_eq!(t.colors.get("editor.background"), parse_color("#121314").as_ref());
		// Only set by an included theme (dark_vs / dark_plus), not by 2026-dark itself.
		assert!(t.colors.len() > 298);
	}
}
