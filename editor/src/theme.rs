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

/// One `tokenColors` entry: selectors (each a list of scope parts, outermost first) and settings.
struct TokenRule {
	selectors: Vec<Vec<String>>,
	foreground: Option<Hsla>,
	font_style: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TokenStyle {
	pub foreground: Option<Hsla>,
	pub bold: bool,
	pub italic: bool,
	pub underline: bool,
}

pub struct Theme {
	pub colors: HashMap<String, Hsla>,
	token_rules: Vec<TokenRule>,
}

/// `part` matches `scope` if it is equal or a dot-separated prefix (`string` matches `string.quoted`).
fn scope_matches(part: &str, scope: &str) -> bool {
	scope == part || (scope.starts_with(part) && scope.as_bytes().get(part.len()) == Some(&b'.'))
}

/// Score of a selector against a scope stack (innermost last), or None. Higher wins:
/// deeper matched scope, then more specific last part, then more parts (parent selectors).
fn selector_score(parts: &[String], scopes: &[String]) -> Option<(usize, usize, usize)> {
	let last = parts.last()?;
	let k = scopes.iter().rposition(|s| scope_matches(last, s))?;
	let mut upto = k;
	for p in parts[..parts.len() - 1].iter().rev() {
		upto = scopes[..upto].iter().rposition(|s| scope_matches(p, s))?;
	}
	Some((k + 1, last.split('.').count(), parts.len()))
}

impl Theme {
	pub fn load(path: &Path) -> Theme {
		let mut colors = HashMap::new();
		let mut token_rules = Vec::new();
		load_into(path, &mut colors, &mut token_rules, 0);
		Theme { colors, token_rules }
	}

	/// Resolves `tokenColors` for a scope stack like the workbench's theme matcher.
	pub fn token_style(&self, scopes: &[String]) -> TokenStyle {
		let mut fg: Option<((usize, usize, usize), Hsla)> = None;
		let mut fs: Option<((usize, usize, usize), &str)> = None;
		for rule in &self.token_rules {
			let Some(score) = rule.selectors.iter().filter_map(|sel| selector_score(sel, scopes)).max() else { continue };
			if let Some(c) = rule.foreground {
				if fg.is_none_or(|(s, _)| score >= s) {
					fg = Some((score, c));
				}
			}
			if let Some(f) = &rule.font_style {
				if fs.is_none_or(|(s, _)| score >= s) {
					fs = Some((score, f.as_str()));
				}
			}
		}
		let style = fs.map_or("", |(_, f)| f);
		TokenStyle { foreground: fg.map(|(_, c)| c), bold: style.contains("bold"), italic: style.contains("italic"), underline: style.contains("underline") }
	}

	/// Theme color, or the workbench default when the theme leaves it unset.
	pub fn c(&self, key: &str, fallback: u32) -> Hsla {
		self.colors.get(key).copied().unwrap_or_else(|| rgba(fallback).into())
	}
}

fn load_into(path: &Path, colors: &mut HashMap<String, Hsla>, token_rules: &mut Vec<TokenRule>, depth: u32) {
	let Ok(text) = std::fs::read_to_string(path) else {
		eprintln!("code-native: theme {} not found", path.display());
		return;
	};
	let Ok(json) = serde_json::from_str::<Value>(&strip_jsonc(&text)) else {
		eprintln!("code-native: theme {} is not valid JSON", path.display());
		return;
	};
	if let (Some(inc), true) = (json["include"].as_str(), depth < 8) {
		load_into(&path.parent().unwrap_or(Path::new(".")).join(inc), colors, token_rules, depth + 1);
	}
	for rule in json["tokenColors"].as_array().into_iter().flatten() {
		let scopes: Vec<String> = match &rule["scope"] {
			Value::String(s) => s.split(',').map(|x| x.trim().to_string()).collect(),
			Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(|x| x.trim().to_string())).collect(),
			_ => vec![String::new()], // no scope: the default foreground
		};
		token_rules.push(TokenRule {
			// shortcut: exclusion selectors (`a - b`) are skipped rather than evaluated.
			selectors: scopes.iter().filter(|s| !s.contains(" -")).map(|s| s.split_whitespace().map(String::from).collect()).collect(),
			foreground: rule["settings"]["foreground"].as_str().and_then(parse_color),
			font_style: rule["settings"]["fontStyle"].as_str().map(String::from),
		});
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

	#[test]
	fn token_colors() {
		let t = Theme::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/theme-defaults/themes/2026-dark.json"));
		let s = |v: &[&str]| t.token_style(&v.iter().map(|x| x.to_string()).collect::<Vec<_>>());
		let string = s(&["source.rust", "string.quoted.double.rust"]).foreground;
		let keyword = s(&["source.rust", "keyword.other.fn.rust"]).foreground;
		assert!(string.is_some() && keyword.is_some() && string != keyword);
		assert!(s(&["source.rust", "comment.line.double-slash.rust"]).foreground.is_some());
		assert_eq!(selector_score(&["string".into()], &["source.x".into(), "string.quoted".into()]), Some((2, 1, 1)));
		assert_eq!(selector_score(&["meta.a".into(), "string".into()], &["string.x".into(), "meta.a".into()]), None);
	}
}
