// Embeds VS Code's icon fonts: codicons (product icons) and Seti (default file icon theme).
// gpui loads TrueType only, so Seti's WOFF is unpacked to TTF here.
use std::io::Read;

fn be16(b: &[u8], at: usize) -> u16 {
	u16::from_be_bytes([b[at], b[at + 1]])
}
fn be32(b: &[u8], at: usize) -> u32 {
	u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

type Tables = Vec<([u8; 4], Vec<u8>)>;

/// WOFF 1.0 -> sfnt tables (https://www.w3.org/TR/WOFF/).
fn woff_tables(w: &[u8]) -> Tables {
	assert_eq!(&w[0..4], b"wOFF", "not a WOFF file");
	let num = be16(w, 12) as usize;
	let mut tables = Vec::new();
	for i in 0..num {
		let e = 44 + i * 20;
		let (off, comp, orig) = (be32(w, e + 4) as usize, be32(w, e + 8) as usize, be32(w, e + 12) as usize);
		let raw = &w[off..off + comp];
		let data = if comp < orig {
			let mut out = Vec::with_capacity(orig);
			flate2::read::ZlibDecoder::new(raw).read_to_end(&mut out).expect("inflate WOFF table");
			out
		} else {
			raw.to_vec()
		};
		tables.push((w[e..e + 4].try_into().unwrap(), data));
	}
	tables
}

fn sfnt_tables(f: &[u8]) -> Tables {
	(0..be16(f, 4) as usize)
		.map(|i| {
			let e = 12 + i * 16;
			let (off, len) = (be32(f, e + 8) as usize, be32(f, e + 12) as usize);
			(f[e..e + 4].try_into().unwrap(), f[off..off + len].to_vec())
		})
		.collect()
}

/// gpui (0.2, Linux) drops any font without a glyph for 'm' unless its PostScript name is
/// "SegoeFluentIcons", so icon fonts get that PostScript name; the family name is kept.
fn icon_font_name_table(family: &str) -> Vec<u8> {
	let names = [(1u16, family), (2, "Regular"), (4, family), (6, "SegoeFluentIcons")];
	let mut storage = Vec::new();
	let mut out = Vec::new();
	for v in [0u16, names.len() as u16, 6 + 12 * names.len() as u16] {
		out.extend_from_slice(&v.to_be_bytes());
	}
	for (id, s) in names {
		let utf16: Vec<u8> = s.encode_utf16().flat_map(u16::to_be_bytes).collect();
		for v in [3u16, 1, 0x409, id, utf16.len() as u16, storage.len() as u16] {
			out.extend_from_slice(&v.to_be_bytes());
		}
		storage.extend(utf16);
	}
	out.extend(storage);
	out
}

fn write_sfnt(flavor: [u8; 4], tables: &Tables) -> Vec<u8> {
	let num = tables.len();
	let entry_selector = (num as f64).log2().floor() as u16;
	let search_range = 16u16 << entry_selector;
	let mut out = flavor.to_vec();
	for v in [num as u16, search_range, entry_selector, num as u16 * 16 - search_range] {
		out.extend_from_slice(&v.to_be_bytes());
	}
	let mut offset = 12 + 16 * num;
	for (tag, data) in tables {
		let checksum = data.chunks(4).fold(0u32, |acc, c| {
			let mut b = [0u8; 4];
			b[..c.len()].copy_from_slice(c);
			acc.wrapping_add(u32::from_be_bytes(b))
		});
		out.extend_from_slice(tag);
		out.extend_from_slice(&checksum.to_be_bytes());
		out.extend_from_slice(&(offset as u32).to_be_bytes());
		out.extend_from_slice(&(data.len() as u32).to_be_bytes());
		offset += (data.len() + 3) & !3;
	}
	for (_, data) in tables {
		out.extend_from_slice(data);
		out.resize((out.len() + 3) & !3, 0);
	}
	out
}

fn main() {
	let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
	let seti = "../extensions/theme-seti/icons/seti.woff";
	let codicon = "../node_modules/@vscode/codicons/dist/codicon.ttf";
	println!("cargo:rerun-if-changed={seti}");
	println!("cargo:rerun-if-changed={codicon}");
	let fix = |family: &str, tables: Tables| -> Vec<u8> {
		let tables: Tables = tables.into_iter().map(|(tag, data)| if &tag == b"name" { (tag, icon_font_name_table(family)) } else { (tag, data) }).collect();
		write_sfnt([0, 1, 0, 0], &tables)
	};
	let woff = std::fs::read(seti).expect("read seti.woff");
	assert_eq!(&woff[4..8], &[0, 1, 0, 0], "seti should be TrueType");
	std::fs::write(out.join("seti.ttf"), fix("seti", woff_tables(&woff))).unwrap();
	let ttf = std::fs::read(codicon).expect("codicon.ttf missing: run `npm install` at the repo root");
	std::fs::write(out.join("codicon.ttf"), fix("codicon", sfnt_tables(&ttf))).unwrap();
}
