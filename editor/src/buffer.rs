/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! Text buffer with a single cursor. Columns are char indices; VS Code positions are
//! 1-based UTF-16, see `position_utf16`.

#[derive(Debug, Clone, PartialEq)]
pub struct Buffer {
	pub lines: Vec<String>,
	/// (line, column in chars)
	pub cursor: (usize, usize),
}

fn byte_at(s: &str, col: usize) -> usize {
	s.char_indices().nth(col).map_or(s.len(), |(i, _)| i)
}

impl Buffer {
	pub fn new(text: &str) -> Self {
		Buffer { lines: text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect(), cursor: (0, 0) }
	}

	pub fn text(&self) -> String {
		self.lines.join("\n")
	}

	fn line_len(&self, line: usize) -> usize {
		self.lines[line].chars().count()
	}

	pub fn insert(&mut self, text: &str) {
		let (l, c) = self.cursor;
		let at = byte_at(&self.lines[l], c);
		let tail = self.lines[l].split_off(at);
		let mut parts = text.split('\n');
		self.lines[l].push_str(parts.next().unwrap_or(""));
		let mut line = l;
		for part in parts {
			line += 1;
			self.lines.insert(line, part.to_string());
		}
		let col = self.line_len(line);
		self.lines[line].push_str(&tail);
		self.cursor = (line, col);
	}

	pub fn backspace(&mut self) {
		let (l, c) = self.cursor;
		if c > 0 {
			let start = byte_at(&self.lines[l], c - 1);
			let end = byte_at(&self.lines[l], c);
			self.lines[l].replace_range(start..end, "");
			self.cursor = (l, c - 1);
		} else if l > 0 {
			let line = self.lines.remove(l);
			let col = self.line_len(l - 1);
			self.lines[l - 1].push_str(&line);
			self.cursor = (l - 1, col);
		}
	}

	pub fn delete(&mut self) {
		let (l, c) = self.cursor;
		if c < self.line_len(l) {
			self.cursor = (l, c + 1);
			self.backspace();
		} else if l + 1 < self.lines.len() {
			let next = self.lines.remove(l + 1);
			self.lines[l].push_str(&next);
		}
	}

	pub fn move_by(&mut self, dl: isize, dc: isize) {
		let (l, c) = self.cursor;
		if dc < 0 && c == 0 && l > 0 {
			self.cursor = (l - 1, self.line_len(l - 1));
		} else if dc > 0 && c == self.line_len(l) && l + 1 < self.lines.len() {
			self.cursor = (l + 1, 0);
		} else {
			let l = (l as isize + dl).clamp(0, self.lines.len() as isize - 1) as usize;
			let c = (c as isize + dc).clamp(0, self.line_len(l) as isize) as usize;
			self.cursor = (l, c);
		}
	}

	pub fn home(&mut self) {
		self.cursor.1 = 0;
	}

	pub fn end(&mut self) {
		self.cursor.1 = self.line_len(self.cursor.0);
	}

	/// Identifier characters right before the cursor (what a completion replaces).
	pub fn word_prefix(&self) -> String {
		let (l, c) = self.cursor;
		let before: Vec<char> = self.lines[l].chars().take(c).collect();
		let start = before.iter().rposition(|ch| !(ch.is_alphanumeric() || *ch == '_' || *ch == '-')).map_or(0, |i| i + 1);
		before[start..].iter().collect()
	}

	pub fn replace_prefix(&mut self, prefix_chars: usize, text: &str) {
		for _ in 0..prefix_chars {
			self.backspace();
		}
		self.insert(text);
	}

	/// Cursor as a VS Code `IPosition` (1-based, UTF-16 column).
	pub fn position_utf16(&self) -> (usize, usize) {
		let (l, c) = self.cursor;
		(l + 1, self.lines[l].chars().take(c).map(char::len_utf16).sum::<usize>() + 1)
	}

	/// The range covering the whole document, as VS Code expects for a full replace.
	pub fn full_range(&self) -> (usize, usize, usize, usize) {
		let last = self.lines.len() - 1;
		(1, 1, last + 1, self.lines[last].encode_utf16().count() + 1)
	}
}

#[cfg(test)]
mod tests {
	use super::Buffer;

	#[test]
	fn edit_and_move() {
		let mut b = Buffer::new("ab\r\ncd");
		assert_eq!(b.lines, ["ab", "cd"]);
		b.cursor = (0, 1);
		b.insert("X\nY");
		assert_eq!(b.text(), "aX\nYb\ncd");
		assert_eq!(b.cursor, (1, 1));
		b.backspace();
		b.backspace();
		assert_eq!(b.text(), "aXb\ncd");
		assert_eq!(b.cursor, (0, 2));
		b.end();
		b.delete();
		assert_eq!(b.text(), "aXbcd");
		b.move_by(0, 1);
		b.move_by(0, 1);
		assert_eq!(b.cursor, (0, 5));
	}

	#[test]
	fn completion_helpers() {
		let mut b = Buffer::new("let ñame_x = foo.ba");
		b.end();
		assert_eq!(b.word_prefix(), "ba");
		b.replace_prefix(2, "bar()");
		assert_eq!(b.text(), "let ñame_x = foo.bar()");
		b.cursor = (0, 9);
		assert_eq!(b.position_utf16(), (1, 10));
		assert_eq!(Buffer::new("a\n😀").full_range(), (1, 1, 2, 3));
	}
}
