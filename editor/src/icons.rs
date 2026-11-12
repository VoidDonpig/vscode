/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! Product icons (codicons, codepoints from `src/vs/base/common/codiconsLibrary.ts`) and
//! file icons from the default `vs-seti` icon theme.

use std::collections::HashMap;
use std::path::Path;

use gpui::Hsla;
use serde_json::Value;

use crate::theme::parse_color;

pub const CODICON_FONT: &str = "codicon";
pub const SETI_FONT: &str = "seti";
pub static CODICON_TTF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/codicon.ttf"));
pub static SETI_TTF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/seti.ttf"));

pub const FILES: char = '\u{eaf0}';
pub const SEARCH: char = '\u{ea6d}';
pub const SOURCE_CONTROL: char = '\u{ea68}';
pub const DEBUG_ALT: char = '\u{eb91}';
pub const EXTENSIONS: char = '\u{eae6}';
pub const ACCOUNT: char = '\u{eb99}';
pub const SETTINGS_GEAR: char = '\u{eb51}';
pub const ELLIPSIS: char = '\u{ea7c}';
pub const CHEVRON_DOWN: char = '\u{eab4}';
pub const CHEVRON_RIGHT: char = '\u{eab6}';
pub const CLOSE: char = '\u{ea76}';
pub const CIRCLE_FILLED: char = '\u{ea71}';
pub const SPLIT_HORIZONTAL: char = '\u{eb56}';
pub const ARROW_LEFT: char = '\u{ea9b}';
pub const ARROW_RIGHT: char = '\u{ea9c}';
pub const LAYOUT: char = '\u{ebeb}';
pub const LAYOUT_SIDEBAR_LEFT: char = '\u{ebf3}';
pub const LAYOUT_PANEL_OFF: char = '\u{ec01}';
pub const LAYOUT_SIDEBAR_RIGHT_OFF: char = '\u{ec00}';
pub const CHROME_MINIMIZE: char = '\u{eaba}';
pub const CHROME_MAXIMIZE: char = '\u{eab9}';
pub const CHROME_CLOSE: char = '\u{eab8}';
pub const REMOTE: char = '\u{eb3a}';
pub const ERROR: char = '\u{ea87}';
pub const WARNING: char = '\u{ea6c}';
pub const BELL: char = '\u{eaa2}';
pub const BRACKET: char = '\u{eb0f}';

/// The `vs-seti` file icon theme: font character + color per file.
pub struct FileIcons {
	defs: HashMap<String, (char, Option<Hsla>)>,
	file_names: HashMap<String, String>,
	file_extensions: HashMap<String, String>,
	language_ids: HashMap<String, String>,
	default: String,
}

fn map(v: &Value) -> HashMap<String, String> {
	v.as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.to_lowercase(), v.as_str()?.to_string()))).collect()
}

impl FileIcons {
	pub fn load(path: &Path) -> FileIcons {
		let json: Value = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
		let defs = json["iconDefinitions"]
			.as_object()
			.into_iter()
			.flatten()
			.filter_map(|(id, d)| {
				// "\\E082" -> U+E082
				let code = u32::from_str_radix(d["fontCharacter"].as_str()?.trim_start_matches('\\'), 16).ok()?;
				Some((id.clone(), (char::from_u32(code)?, d["fontColor"].as_str().and_then(parse_color))))
			})
			.collect();
		FileIcons {
			defs,
			file_names: map(&json["fileNames"]),
			file_extensions: map(&json["fileExtensions"]),
			language_ids: map(&json["languageIds"]),
			default: json["file"].as_str().unwrap_or("_default").to_string(),
		}
	}

	/// Same precedence as the workbench: file name, then longest extension (`d.ts` before `ts`), then language id.
	pub fn for_file(&self, name: &str, language: &str) -> Option<(char, Option<Hsla>)> {
		let name = name.to_lowercase();
		let by_ext = || {
			name.char_indices().filter(|(_, c)| *c == '.').find_map(|(i, _)| self.file_extensions.get(&name[i + 1..]))
		};
		let id = self.file_names.get(&name).or_else(by_ext).or_else(|| self.language_ids.get(language)).unwrap_or(&self.default);
		self.defs.get(id).copied()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn seti_lookup() {
		let icons = FileIcons::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/theme-seti/icons/vs-seti-icon-theme.json"));
		assert_eq!(icons.for_file("main.rs", "rust").map(|i| i.0), Some('\u{e082}'));
		assert_eq!(icons.for_file("Cargo.toml", "toml").map(|i| i.0), Some('\u{e019}'));
		assert!(icons.for_file("unknown.zzz", "plaintext").is_some());
	}
}
