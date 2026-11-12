/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! TextMate tokenizer (the model of `vscode-textmate`) on top of Oniguruma, the regex engine
//! VS Code uses, so extension grammars (`contributes.grammars`) tokenize the same way.
//! Supports match, begin/end (with end back-references), begin/while, captures with nested
//! patterns, applyEndPatternLast, repositories and includes (`#name`, `$self`, `$base`,
//! other grammars by scope name).
// shortcut: grammar injections (`injectTo` / `injectionSelector`) are not applied yet.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use onig::{Regex, RegexOptions, Region, SearchOptions, Syntax};
use serde_json::Value;

use crate::theme::strip_jsonc;

#[derive(Default, Clone)]
struct Capture {
	name: Option<String>,
	/// Container rule with the capture's own patterns.
	rule: Option<usize>,
}

#[derive(Clone)]
enum Include {
	Rule(usize),
	/// Root patterns of a grammar (by scope name); `$self` / `$base` resolve to their grammar.
	Grammar(String),
	/// `source.x#name`
	GrammarRepo(String, String),
}

#[derive(Default, Clone)]
struct Rule {
	name: Option<String>,
	content_name: Option<String>,
	match_: Option<String>,
	begin: Option<String>,
	end: Option<String>,
	while_: Option<String>,
	captures: Vec<(usize, Capture)>,
	begin_captures: Vec<(usize, Capture)>,
	end_captures: Vec<(usize, Capture)>,
	while_captures: Vec<(usize, Capture)>,
	patterns: Vec<usize>,
	include: Option<Include>,
	apply_end_pattern_last: bool,
}

/// One entry of the rule stack carried from line to line.
#[derive(Clone, PartialEq)]
pub struct Frame {
	rule: usize,
	/// `end` / `while` with back-references already substituted.
	end: Option<String>,
	while_: Option<String>,
	name_scopes: Rc<Vec<String>>,
	content_scopes: Rc<Vec<String>>,
}

pub type Stack = Vec<Frame>;

pub struct Registry {
	/// scope name -> grammar file
	paths: HashMap<String, PathBuf>,
	/// scope name -> (root rule, top-level repository)
	loaded: HashMap<String, Option<(usize, HashMap<String, usize>)>>,
	rules: Vec<Rule>,
	regexes: HashMap<String, Option<Rc<Regex>>>,
	flat: HashMap<usize, Rc<Vec<usize>>>,
}

fn caps(v: &Value) -> impl Iterator<Item = (usize, &Value)> {
	v.as_object().into_iter().flatten().filter_map(|(k, c)| Some((k.parse().ok()?, c)))
}

/// Replaces `\1`..`\9` in an end/while pattern with the (escaped) text captured by begin.
fn substitute_backrefs(pattern: &str, line: &str, region: &Region) -> String {
	let mut out = String::new();
	let mut chars = pattern.chars().peekable();
	while let Some(c) = chars.next() {
		if c == '\\' {
			match chars.peek().copied() {
				Some(d @ '0'..='9') => {
					chars.next();
					let text = region.pos(d as usize - '0' as usize).map_or("", |(s, e)| &line[s..e]);
					for ch in text.chars() {
						if "\\|()[]{}^$*+?.-#".contains(ch) {
							out.push('\\');
						}
						out.push(ch);
					}
				}
				Some(n) => {
					chars.next();
					out.push('\\');
					out.push(n);
				}
				None => out.push('\\'),
			}
		} else {
			out.push(c);
		}
	}
	out
}

impl Registry {
	pub fn new(grammars: HashMap<String, PathBuf>) -> Registry {
		Registry { paths: grammars, loaded: HashMap::new(), rules: Vec::new(), regexes: HashMap::new(), flat: HashMap::new() }
	}

	fn add(&mut self, r: Rule) -> usize {
		self.rules.push(r);
		self.rules.len() - 1
	}

	fn load(&mut self, scope: &str) -> Option<(usize, HashMap<String, usize>)> {
		if let Some(g) = self.loaded.get(scope) {
			return g.clone();
		}
		self.loaded.insert(scope.to_string(), None); // guards recursive loads
		let json: Value = self.paths.get(scope).and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&strip_jsonc(&t)).ok())?;
		let mut chain = Vec::new();
		let repo = self.parse_repository(&json, &mut chain, scope);
		let root = self.parse_rule(&json, &chain, scope);
		let g = Some((root, repo));
		self.loaded.insert(scope.to_string(), g.clone());
		g
	}

	/// Allocates the repository entries first so they can reference each other, then parses them.
	fn parse_repository(&mut self, v: &Value, chain: &mut Vec<HashMap<String, usize>>, scope: &str) -> HashMap<String, usize> {
		let Some(repo) = v["repository"].as_object() else { return HashMap::new() };
		let ids: HashMap<String, usize> = repo.keys().map(|k| (k.clone(), self.add(Rule::default()))).collect();
		chain.push(ids.clone());
		for (k, body) in repo {
			let parsed = self.parse_rule(body, chain, scope);
			self.rules[ids[k]] = self.rules[parsed].clone();
		}
		ids
	}

	fn parse_caps(&mut self, v: &Value, chain: &[HashMap<String, usize>], scope: &str) -> Vec<(usize, Capture)> {
		let mut out: Vec<(usize, Capture)> = caps(v)
			.map(|(i, c)| {
				let rule = c["patterns"].is_array().then(|| self.parse_rule(&serde_json::json!({ "patterns": c["patterns"] }), chain, scope));
				(i, Capture { name: c["name"].as_str().map(String::from), rule })
			})
			.collect();
		out.sort_by_key(|(i, _)| *i);
		out
	}

	fn parse_rule(&mut self, v: &Value, chain: &[HashMap<String, usize>], scope: &str) -> usize {
		let mut chain = chain.to_vec();
		if v.get("repository").is_some() {
			self.parse_repository(v, &mut chain, scope);
		}
		let s = |k: &str| v[k].as_str().map(String::from);
		let include = s("include").map(|inc| match inc.as_str() {
			"$self" | "$base" => Include::Grammar(scope.to_string()),
			i if i.starts_with('#') => chain.iter().rev().find_map(|m| m.get(&i[1..]).copied()).map_or(Include::Grammar(String::new()), Include::Rule),
			i => match i.split_once('#') {
				Some((g, r)) => Include::GrammarRepo(g.to_string(), r.to_string()),
				None => Include::Grammar(i.to_string()),
			},
		});
		let patterns = v["patterns"].as_array().map(|ps| ps.iter().map(|p| self.parse_rule(p, &chain, scope)).collect()).unwrap_or_default();
		let rule = Rule {
			name: s("name"),
			content_name: s("contentName"),
			match_: s("match"),
			begin: s("begin"),
			end: s("end"),
			while_: s("while"),
			captures: self.parse_caps(&v["captures"], &chain, scope),
			begin_captures: self.parse_caps(&v["beginCaptures"], &chain, scope),
			end_captures: self.parse_caps(&v["endCaptures"], &chain, scope),
			while_captures: self.parse_caps(&v["whileCaptures"], &chain, scope),
			patterns,
			include,
			apply_end_pattern_last: v["applyEndPatternLast"].as_bool().unwrap_or(false) || v["applyEndPatternLast"].as_i64() == Some(1),
		};
		self.add(rule)
	}

	fn regex(&mut self, pattern: &str) -> Option<Rc<Regex>> {
		if let Some(r) = self.regexes.get(pattern) {
			return r.clone();
		}
		let r = Regex::with_options(pattern, RegexOptions::REGEX_OPTION_CAPTURE_GROUP, Syntax::default()).ok().map(Rc::new);
		self.regexes.insert(pattern.to_string(), r.clone());
		r
	}

	/// Concrete (match / begin) rules reachable from `rule`'s patterns, includes expanded.
	fn flatten(&mut self, rule: usize) -> Rc<Vec<usize>> {
		if let Some(f) = self.flat.get(&rule) {
			return f.clone();
		}
		let mut out = Vec::new();
		let mut seen = HashSet::new();
		let patterns = self.rules[rule].patterns.clone();
		for p in patterns {
			self.expand(p, &mut out, &mut seen);
		}
		let out = Rc::new(out);
		self.flat.insert(rule, out.clone());
		out
	}

	fn expand(&mut self, id: usize, out: &mut Vec<usize>, seen: &mut HashSet<usize>) {
		if !seen.insert(id) {
			return;
		}
		let r = self.rules[id].clone();
		if let Some(inc) = r.include {
			let target = match inc {
				Include::Rule(t) => Some(t),
				Include::Grammar(g) => self.load(&g).map(|(root, _)| root),
				Include::GrammarRepo(g, name) => self.load(&g).and_then(|(_, repo)| repo.get(&name).copied()),
			};
			if let Some(t) = target {
				self.expand(t, out, seen);
			}
		} else if r.match_.is_some() || r.begin.is_some() {
			out.push(id);
		} else {
			for p in r.patterns {
				self.expand(p, out, seen);
			}
		}
	}

	pub fn initial_stack(&mut self, scope: &str) -> Option<Stack> {
		let (root, _) = self.load(scope)?;
		let scopes = Rc::new(vec![scope.to_string()]);
		Some(vec![Frame { rule: root, end: None, while_: None, name_scopes: scopes.clone(), content_scopes: scopes }])
	}

	/// Tokenizes one line (without its newline). Returns (byte range, scope stack) spans; later
	/// spans paint over earlier ones.
	pub fn tokenize_line(&mut self, line: &str, stack: &mut Stack) -> Vec<(Range<usize>, Rc<Vec<String>>)> {
		let text = format!("{line}\n");
		let mut out = Vec::new();
		// begin/while: a frame survives a new line only while its `while` still matches at the line start.
		let mut pos = 0;
		for i in 1..stack.len() {
			if let Some(w) = stack[i].while_.clone() {
				let mut region = Region::new();
				let hit = self.regex(&w).and_then(|re| re.search_with_options(&text, 0, text.len(), SearchOptions::SEARCH_OPTION_NONE, Some(&mut region)));
				if hit == Some(0) {
					let caps = self.rules[stack[i].rule].while_captures.clone();
					self.emit_captures(&text, &region, &caps, &stack[i].name_scopes, &mut out);
					pos = region.pos(0).map_or(0, |(_, e)| e);
				} else {
					stack.truncate(i);
					break;
				}
			}
		}
		self.tokenize_range(&text, pos, text.len(), stack, &mut out);
		for (r, _) in &mut out {
			r.end = r.end.min(line.len());
		}
		out.retain(|(r, _)| r.start < r.end);
		out
	}

	fn tokenize_range(&mut self, text: &str, mut pos: usize, end: usize, stack: &mut Stack, out: &mut Vec<(Range<usize>, Rc<Vec<String>>)>) {
		let mut guard = 0;
		while pos < end {
			let top = stack.last().unwrap().clone();
			let rule = self.rules[top.rule].clone();
			let candidates = self.flatten(top.rule);
			// (rule id or None for end, start, region)
			let mut best: Option<(Option<usize>, usize, Region)> = None;
			let try_re = |reg: &mut Self, pattern: &str, id: Option<usize>, best: &mut Option<(Option<usize>, usize, Region)>| {
				let Some(re) = reg.regex(pattern) else { return };
				let mut region = Region::new();
				if let Some(start) = re.search_with_options(text, pos, end, SearchOptions::SEARCH_OPTION_NONE, Some(&mut region)) {
					if best.as_ref().is_none_or(|b| start < b.1) {
						*best = Some((id, start, region));
					}
				}
			};
			if let (Some(e), false) = (&top.end, rule.apply_end_pattern_last) {
				try_re(self, e, None, &mut best);
			}
			for &c in candidates.iter() {
				let p = self.rules[c].match_.clone().or_else(|| self.rules[c].begin.clone()).unwrap_or_default();
				try_re(self, &p, Some(c), &mut best);
			}
			if let (Some(e), true) = (&top.end, rule.apply_end_pattern_last) {
				try_re(self, e, None, &mut best);
			}
			let Some((id, start, region)) = best else {
				out.push((pos..end, top.content_scopes.clone()));
				return;
			};
			let (m_start, m_end) = region.pos(0).unwrap_or((start, start));
			if m_start > pos {
				out.push((pos..m_start, top.content_scopes.clone()));
			}
			match id {
				None => {
					// end of the current begin/end rule
					out.push((m_start..m_end, top.name_scopes.clone()));
					let caps = if rule.end_captures.is_empty() { rule.captures.clone() } else { rule.end_captures.clone() };
					self.emit_captures(text, &region, &caps, &top.name_scopes, out);
					if stack.len() > 1 {
						stack.pop();
					}
				}
				Some(c) => {
					let r = self.rules[c].clone();
					let mut name_scopes = (*top.content_scopes).clone();
					name_scopes.extend(r.name.iter().flat_map(|n| n.split_whitespace().map(String::from)));
					let name_scopes = Rc::new(name_scopes);
					out.push((m_start..m_end, name_scopes.clone()));
					if r.match_.is_some() {
						self.emit_captures(text, &region, &r.captures, &name_scopes, out);
					} else {
						let caps = if r.begin_captures.is_empty() { r.captures.clone() } else { r.begin_captures.clone() };
						self.emit_captures(text, &region, &caps, &name_scopes, out);
						let mut content = (*name_scopes).clone();
						content.extend(r.content_name.iter().flat_map(|n| n.split_whitespace().map(String::from)));
						stack.push(Frame {
							rule: c,
							end: r.end.as_ref().map(|e| substitute_backrefs(e, text, &region)),
							while_: r.while_.as_ref().map(|w| substitute_backrefs(w, text, &region)),
							name_scopes,
							content_scopes: Rc::new(content),
						});
					}
				}
			}
			if m_end > pos {
				pos = m_end;
				guard = 0;
			} else {
				// Empty match: avoid looping forever on the same position (vscode-textmate does the same).
				guard += 1;
				if guard > 2 {
					if stack.len() > 1 && id.is_some() {
						stack.pop();
					}
					let next = text[pos..].chars().next().map_or(1, char::len_utf8);
					out.push((pos..pos + next, stack.last().unwrap().content_scopes.clone()));
					pos += next;
					guard = 0;
				}
			}
		}
	}

	fn emit_captures(&mut self, text: &str, region: &Region, caps: &[(usize, Capture)], base: &Rc<Vec<String>>, out: &mut Vec<(Range<usize>, Rc<Vec<String>>)>) {
		for (i, cap) in caps {
			let Some((s, e)) = region.pos(*i) else { continue };
			if s >= e {
				continue;
			}
			let mut scopes = (**base).clone();
			scopes.extend(cap.name.iter().flat_map(|n| n.split_whitespace().map(String::from)));
			let scopes = Rc::new(scopes);
			out.push((s..e, scopes.clone()));
			if let Some(rule) = cap.rule {
				let mut sub = vec![Frame { rule, end: None, while_: None, name_scopes: scopes.clone(), content_scopes: scopes }];
				self.tokenize_range(text, s, e, &mut sub, out);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::Path;

	fn registry() -> Registry {
		let ext = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions");
		Registry::new(HashMap::from([
			("source.rust".to_string(), ext.join("rust/syntaxes/rust.tmLanguage.json")),
			("text.html.markdown".to_string(), ext.join("markdown-basics/syntaxes/markdown.tmLanguage.json")),
		]))
	}

	fn scope_at(spans: &[(Range<usize>, Rc<Vec<String>>)], byte: usize) -> String {
		spans.iter().rev().find(|(r, _)| r.contains(&byte)).map(|(_, s)| s.join(" ")).unwrap_or_default()
	}

	#[test]
	fn rust_tokens() {
		let mut reg = registry();
		let mut stack = reg.initial_stack("source.rust").unwrap();
		let line = r#"fn main() { println!("hi {}", 1); }"#;
		let spans = reg.tokenize_line(line, &mut stack);
		assert!(scope_at(&spans, 0).contains("keyword"), "{}", scope_at(&spans, 0));
		assert!(scope_at(&spans, 3).contains("entity.name.function"), "{}", scope_at(&spans, 3));
		assert!(scope_at(&spans, 22).contains("string.quoted.double"), "{}", scope_at(&spans, 22));
		assert!(scope_at(&spans, 30).contains("constant.numeric"), "{}", scope_at(&spans, 30));
	}

	#[test]
	fn multiline_state_and_backrefs() {
		let mut reg = registry();
		let mut stack = reg.initial_stack("source.rust").unwrap();
		reg.tokenize_line("/* start", &mut stack);
		let spans = reg.tokenize_line("still comment */ let", &mut stack);
		assert!(scope_at(&spans, 2).contains("comment.block"));
		assert!(!scope_at(&spans, 18).contains("comment"), "{}", scope_at(&spans, 18));
		// raw string end uses a back-reference to the number of #s
		let mut stack = reg.initial_stack("source.rust").unwrap();
		let spans = reg.tokenize_line(r###"let s = r##"a"#b"##; 1"###, &mut stack);
		assert!(scope_at(&spans, 14).contains("string"), "{}", scope_at(&spans, 14));
		assert!(scope_at(&spans, 21).contains("constant.numeric"), "{}", scope_at(&spans, 21));
	}

	#[test]
	fn while_rules() {
		let mut reg = registry();
		let mut stack = reg.initial_stack("text.html.markdown").unwrap();
		let a = reg.tokenize_line("> quoted", &mut stack);
		assert!(scope_at(&a, 3).contains("markup.quote"), "{}", scope_at(&a, 3));
		reg.tokenize_line("", &mut stack);
		let c = reg.tokenize_line("plain", &mut stack);
		assert!(!scope_at(&c, 1).contains("markup.quote"), "{}", scope_at(&c, 1));
	}
}
