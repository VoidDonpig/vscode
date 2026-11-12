/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! Text buffer with one cursor, an optional selection and undo/redo. Columns are char
//! indices; VS Code positions are 1-based UTF-16, see `position_utf16`.

pub type Pos = (usize, usize);

#[derive(Debug, Clone, PartialEq)]
struct Snapshot {
	lines: Vec<String>,
	cursor: Pos,
}

#[derive(Debug, Clone, PartialEq, Copy)]
enum EditKind {
	Typing,
	Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Buffer {
	pub lines: Vec<String>,
	/// (line, column in chars)
	pub cursor: Pos,
	/// Other end of the selection, if any.
	pub anchor: Option<Pos>,
	undo: Vec<Snapshot>,
	redo: Vec<Snapshot>,
	last_edit: Option<EditKind>,
}

const UNDO_LIMIT: usize = 1000;

fn byte_at(s: &str, col: usize) -> usize {
	s.char_indices().nth(col).map_or(s.len(), |(i, _)| i)
}

fn is_word(c: char) -> bool {
	c.is_alphanumeric() || c == '_'
}

impl Buffer {
	pub fn new(text: &str) -> Self {
		Buffer {
			lines: text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect(),
			cursor: (0, 0),
			anchor: None,
			undo: Vec::new(),
			redo: Vec::new(),
			last_edit: None,
		}
	}

	pub fn text(&self) -> String {
		self.lines.join("\n")
	}

	fn line_len(&self, line: usize) -> usize {
		self.lines[line].chars().count()
	}

	// --- undo ---

	/// Records the state before an edit. Consecutive typing is one undo step, like the editor's
	/// default; anything else starts a new step.
	fn checkpoint(&mut self, kind: EditKind) {
		if kind == EditKind::Typing && self.last_edit == Some(EditKind::Typing) {
			return;
		}
		self.undo.push(Snapshot { lines: self.lines.clone(), cursor: self.cursor });
		if self.undo.len() > UNDO_LIMIT {
			self.undo.remove(0);
		}
		self.redo.clear();
		self.last_edit = Some(kind);
	}

	pub fn undo(&mut self) -> bool {
		let Some(s) = self.undo.pop() else { return false };
		self.redo.push(Snapshot { lines: std::mem::replace(&mut self.lines, s.lines), cursor: self.cursor });
		self.cursor = s.cursor;
		self.anchor = None;
		self.last_edit = None;
		true
	}

	pub fn redo(&mut self) -> bool {
		let Some(s) = self.redo.pop() else { return false };
		self.undo.push(Snapshot { lines: std::mem::replace(&mut self.lines, s.lines), cursor: self.cursor });
		self.cursor = s.cursor;
		self.anchor = None;
		self.last_edit = None;
		true
	}

	// --- selection ---

	/// Selection as (start, end), start <= end; None when empty.
	pub fn selection(&self) -> Option<(Pos, Pos)> {
		let a = self.anchor?;
		match a.cmp(&self.cursor) {
			std::cmp::Ordering::Less => Some((a, self.cursor)),
			std::cmp::Ordering::Greater => Some((self.cursor, a)),
			std::cmp::Ordering::Equal => None,
		}
	}

	pub fn selected_text(&self) -> Option<String> {
		let ((sl, sc), (el, ec)) = self.selection()?;
		if sl == el {
			return Some(self.lines[sl].chars().skip(sc).take(ec - sc).collect());
		}
		let mut out: String = self.lines[sl].chars().skip(sc).collect();
		for l in &self.lines[sl + 1..el] {
			out.push('\n');
			out.push_str(l);
		}
		out.push('\n');
		out.extend(self.lines[el].chars().take(ec));
		Some(out)
	}

	pub fn select_all(&mut self) {
		self.anchor = Some((0, 0));
		let last = self.lines.len() - 1;
		self.cursor = (last, self.line_len(last));
	}

	/// Moves the cursor; `extend` keeps/starts a selection (Shift), otherwise it is cleared.
	pub fn set_cursor(&mut self, pos: Pos, extend: bool) {
		if extend {
			self.anchor.get_or_insert(self.cursor);
		} else {
			self.anchor = None;
		}
		let l = pos.0.min(self.lines.len() - 1);
		self.cursor = (l, pos.1.min(self.line_len(l)));
		self.last_edit = None;
	}

	fn delete_selection_raw(&mut self) -> bool {
		let Some(((sl, sc), (el, ec))) = self.selection() else {
			self.anchor = None;
			return false;
		};
		let tail: String = self.lines[el].chars().skip(ec).collect();
		let start = byte_at(&self.lines[sl], sc);
		self.lines[sl].truncate(start);
		self.lines[sl].push_str(&tail);
		self.lines.drain(sl + 1..=el);
		self.cursor = (sl, sc);
		self.anchor = None;
		true
	}

	pub fn delete_selection(&mut self) -> bool {
		if self.selection().is_none() {
			return false;
		}
		self.checkpoint(EditKind::Other);
		self.delete_selection_raw()
	}

	// --- edits ---

	fn insert_raw(&mut self, text: &str) {
		self.delete_selection_raw();
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

	pub fn insert(&mut self, text: &str) {
		let typing = !text.contains('\n') && text.chars().count() == 1 && self.selection().is_none();
		self.checkpoint(if typing { EditKind::Typing } else { EditKind::Other });
		self.insert_raw(text);
	}

	pub fn backspace(&mut self) {
		if self.delete_selection() {
			return;
		}
		let (l, c) = self.cursor;
		if c == 0 && l == 0 {
			return;
		}
		self.checkpoint(EditKind::Other);
		if c > 0 {
			let start = byte_at(&self.lines[l], c - 1);
			let end = byte_at(&self.lines[l], c);
			self.lines[l].replace_range(start..end, "");
			self.cursor = (l, c - 1);
		} else {
			let line = self.lines.remove(l);
			let col = self.line_len(l - 1);
			self.lines[l - 1].push_str(&line);
			self.cursor = (l - 1, col);
		}
	}

	pub fn delete(&mut self) {
		if self.delete_selection() {
			return;
		}
		let (l, c) = self.cursor;
		if c < self.line_len(l) {
			self.checkpoint(EditKind::Other);
			let start = byte_at(&self.lines[l], c);
			let end = byte_at(&self.lines[l], c + 1);
			self.lines[l].replace_range(start..end, "");
		} else if l + 1 < self.lines.len() {
			self.checkpoint(EditKind::Other);
			let next = self.lines.remove(l + 1);
			self.lines[l].push_str(&next);
		}
	}

	// --- movement ---

	pub fn move_by(&mut self, dl: isize, dc: isize, extend: bool) {
		// Without Shift, Left/Right collapse a selection to its start/end like the editor does.
		if !extend && dl == 0 {
			if let Some((s, e)) = self.selection() {
				return self.set_cursor(if dc < 0 { s } else { e }, false);
			}
		}
		let (l, c) = self.cursor;
		let target = if dc < 0 && c == 0 && l > 0 {
			(l - 1, self.line_len(l - 1))
		} else if dc > 0 && c == self.line_len(l) && l + 1 < self.lines.len() {
			(l + 1, 0)
		} else {
			let l = (l as isize + dl).clamp(0, self.lines.len() as isize - 1) as usize;
			(l, (c as isize + dc).clamp(0, self.line_len(l) as isize) as usize)
		};
		self.set_cursor(target, extend);
	}

	/// Ctrl+Left / Ctrl+Right: to the previous word start / next word end.
	pub fn move_word(&mut self, forward: bool, extend: bool) {
		let (l, c) = self.cursor;
		let chars: Vec<char> = self.lines[l].chars().collect();
		let target = if forward {
			if c >= chars.len() {
				if l + 1 < self.lines.len() { (l + 1, 0) } else { (l, c) }
			} else {
				let mut i = c;
				while i < chars.len() && !is_word(chars[i]) {
					i += 1;
				}
				while i < chars.len() && is_word(chars[i]) {
					i += 1;
				}
				(l, i)
			}
		} else if c == 0 {
			if l > 0 { (l - 1, self.line_len(l - 1)) } else { (0, 0) }
		} else {
			let mut i = c;
			while i > 0 && !is_word(chars[i - 1]) {
				i -= 1;
			}
			while i > 0 && is_word(chars[i - 1]) {
				i -= 1;
			}
			(l, i)
		};
		self.set_cursor(target, extend);
	}

	/// Home: to the first non-whitespace character, or column 0 when already there.
	pub fn home(&mut self, extend: bool) {
		let (l, c) = self.cursor;
		let first = self.lines[l].chars().take_while(|c| c.is_whitespace()).count();
		self.set_cursor((l, if c == first { 0 } else { first }), extend);
	}

	pub fn end(&mut self, extend: bool) {
		let l = self.cursor.0;
		self.set_cursor((l, self.line_len(l)), extend);
	}

	/// Selects the word at a position (double click).
	pub fn select_word(&mut self, pos: Pos) {
		let chars: Vec<char> = self.lines[pos.0].chars().collect();
		let (mut s, mut e) = (pos.1.min(chars.len()), pos.1.min(chars.len()));
		while s > 0 && is_word(chars[s - 1]) {
			s -= 1;
		}
		while e < chars.len() && is_word(chars[e]) {
			e += 1;
		}
		self.anchor = Some((pos.0, s));
		self.cursor = (pos.0, e);
	}

	// --- completion helpers ---

	/// Identifier characters right before the cursor (what a completion replaces).
	pub fn word_prefix(&self) -> String {
		let (l, c) = self.cursor;
		let before: Vec<char> = self.lines[l].chars().take(c).collect();
		let start = before.iter().rposition(|ch| !(is_word(*ch) || *ch == '-')).map_or(0, |i| i + 1);
		before[start..].iter().collect()
	}

	pub fn replace_prefix(&mut self, prefix_chars: usize, text: &str) {
		self.checkpoint(EditKind::Other);
		self.anchor = None;
		let (l, c) = self.cursor;
		self.anchor = Some((l, c - prefix_chars.min(c)));
		self.insert_raw(text);
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
		b.end(false);
		b.delete();
		assert_eq!(b.text(), "aXbcd");
		b.move_by(0, 1, false);
		b.move_by(0, 1, false);
		assert_eq!(b.cursor, (0, 5));
	}

	#[test]
	fn selection_editing() {
		let mut b = Buffer::new("hello world\nsecond line");
		b.set_cursor((0, 6), false);
		b.move_by(1, 0, true);
		assert_eq!(b.selected_text().as_deref(), Some("world\nsecond"));
		b.insert("X");
		assert_eq!(b.text(), "hello X line");
		b.select_all();
		assert_eq!(b.selected_text().as_deref(), Some("hello X line"));
		b.move_by(0, -1, false);
		assert_eq!((b.cursor, b.anchor), ((0, 0), None));
		b.select_word((0, 2));
		assert_eq!(b.selected_text().as_deref(), Some("hello"));
		b.backspace();
		assert_eq!(b.text(), " X line");
	}

	#[test]
	fn undo_groups_typing() {
		let mut b = Buffer::new("");
		for c in "abc".chars() {
			b.insert(&c.to_string());
		}
		b.insert("\n");
		b.insert("d");
		assert_eq!(b.text(), "abc\nd");
		assert!(b.undo());
		assert_eq!(b.text(), "abc\n");
		assert!(b.undo());
		assert_eq!(b.text(), "abc");
		assert!(b.undo());
		assert_eq!(b.text(), "");
		assert!(!b.undo());
		assert!(b.redo() && b.redo());
		assert_eq!(b.text(), "abc\n");
		b.insert("z"); // a new edit drops the redo stack
		assert!(!b.redo());
	}

	#[test]
	fn words_and_home() {
		let mut b = Buffer::new("    let foo_bar = 1;");
		b.end(false);
		b.move_word(false, false);
		assert_eq!(b.cursor, (0, 18));
		b.move_word(false, false);
		assert_eq!(b.cursor, (0, 8));
		b.move_word(true, true);
		assert_eq!(b.selected_text().as_deref(), Some("foo_bar"));
		b.home(false);
		assert_eq!(b.cursor, (0, 4));
		b.home(false);
		assert_eq!(b.cursor, (0, 0));
	}

	#[test]
	fn completion_helpers() {
		let mut b = Buffer::new("let ñame_x = foo.ba");
		b.end(false);
		assert_eq!(b.word_prefix(), "ba");
		b.replace_prefix(2, "bar()");
		assert_eq!(b.text(), "let ñame_x = foo.bar()");
		assert!(b.undo());
		assert_eq!(b.text(), "let ñame_x = foo.ba");
		b.cursor = (0, 9);
		assert_eq!(b.position_utf16(), (1, 10));
		assert_eq!(Buffer::new("a\n😀").full_range(), (1, 1, 2, 3));
	}
}
