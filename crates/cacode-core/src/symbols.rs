//! Lightweight symbol index (regex-based, no tree-sitter) for Rust, Go,
//! TypeScript/JavaScript, Python and C#. Cached per workspace and refreshed
//! incrementally by file mtime.

use crate::tools::search::text_files;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

/// Bump when extraction changes so stale caches are rebuilt (2 = tree-sitter).
pub const INDEX_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: usize,
    pub public: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FileEntry {
    mtime: u64,
    symbols: Vec<Symbol>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SymbolIndex {
    #[serde(default)]
    version: u32,
    root: PathBuf,
    files: HashMap<String, FileEntry>,
}

struct LangRules {
    rules: Vec<(Regex, usize, usize, Option<usize>)>, // (regex, name group, kind group, pub group)
}

fn rules_for(ext: &str) -> Option<&'static LangRules> {
    static RS: OnceLock<LangRules> = OnceLock::new();
    static GO: OnceLock<LangRules> = OnceLock::new();
    static TS: OnceLock<LangRules> = OnceLock::new();
    static PY: OnceLock<LangRules> = OnceLock::new();
    static CS: OnceLock<LangRules> = OnceLock::new();
    let r = |s: &str| Regex::new(s).expect("static regex");
    match ext {
        "rs" => Some(RS.get_or_init(|| LangRules {
            rules: vec![
                (r(r"^\s*(pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:const\s+)?(?:unsafe\s+)?(fn|struct|enum|trait|type|mod|const|static)\s+([A-Za-z_][A-Za-z0-9_]*)"), 3, 2, Some(1)),
                (r(r"^\s*macro_rules!\s+([A-Za-z_][A-Za-z0-9_]*)"), 1, 0, None),
                (r(r"^\s*impl(?:<[^>]*>)?\s+(?:[\w:<>, ]+\s+for\s+)?([A-Za-z_][A-Za-z0-9_]*)"), 1, 0, None),
            ],
        })),
        "go" => Some(GO.get_or_init(|| LangRules {
            rules: vec![
                (r(r"^func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)"), 1, 0, None),
                (r(r"^type\s+([A-Za-z_]\w*)\s+(struct|interface)?"), 1, 2, None),
            ],
        })),
        "ts" | "tsx" | "js" | "jsx" | "mjs" => Some(TS.get_or_init(|| LangRules {
            rules: vec![(r(r"^\s*(export\s+)?(?:default\s+)?(?:async\s+)?(function|class|interface|type|enum|const)\s+([A-Za-z_$][\w$]*)"), 3, 2, Some(1))],
        })),
        "py" => Some(PY.get_or_init(|| LangRules { rules: vec![(r(r"^(\s*)(?:async\s+)?(def|class)\s+([A-Za-z_]\w*)"), 3, 2, None)] })),
        "cs" => Some(CS.get_or_init(|| LangRules {
            rules: vec![
                (r(r"^\s*(public\s+)?(?:(?:private|internal|protected|static|sealed|abstract|partial|readonly)\s+)*(class|interface|struct|enum|record)\s+([A-Za-z_]\w*)"), 3, 2, Some(1)),
                (r(r"^\s*(public\s+)?(?:(?:private|internal|protected|static|virtual|override|async|abstract)\s+)*[\w<>\[\],?]+\s+([A-Z]\w*)\s*\([^;]*$"), 2, 0, Some(1)),
            ],
        })),
        _ => None,
    }
}

fn default_kind(ext: &str, rule_idx: usize) -> &'static str {
    match (ext, rule_idx) {
        ("rs", 1) => "macro",
        ("rs", 2) => "impl",
        ("go", 0) => "func",
        ("cs", 1) => "method",
        _ => "type",
    }
}

fn ts_language(ext: &str) -> Option<tree_sitter::Language> {
    Some(match ext {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "js" | "jsx" | "mjs" => tree_sitter_javascript::LANGUAGE.into(),
        "py" => tree_sitter_python::LANGUAGE.into(),
        "cs" => tree_sitter_c_sharp::LANGUAGE.into(),
        _ => return None,
    })
}

/// Map a tree-sitter node to (kind label, name node) for definition nodes.
fn ts_def<'a>(ext: &str, node: tree_sitter::Node<'a>) -> Option<(&'static str, tree_sitter::Node<'a>)> {
    let name = |f: &str| node.child_by_field_name(f);
    let k = node.kind();
    let kind: &'static str = match (ext, k) {
        ("rs", "function_item") | ("rs", "function_signature_item") => "fn",
        ("rs", "struct_item") => "struct",
        ("rs", "enum_item") => "enum",
        ("rs", "trait_item") => "trait",
        ("rs", "type_item") => "type",
        ("rs", "mod_item") => "mod",
        ("rs", "const_item") => "const",
        ("rs", "static_item") => "static",
        ("rs", "macro_definition") => "macro",
        ("rs", "impl_item") => return name("type").map(|n| ("impl", n)),
        ("go", "function_declaration") => "func",
        ("go", "method_declaration") => "method",
        ("go", "type_spec") => match name("type").map(|t| t.kind()) {
            Some("struct_type") => "struct",
            Some("interface_type") => "interface",
            _ => "type",
        },
        ("ts" | "tsx" | "js" | "jsx" | "mjs", "function_declaration") | ("ts" | "tsx" | "js" | "jsx" | "mjs", "generator_function_declaration") => "function",
        ("ts" | "tsx" | "js" | "jsx" | "mjs", "class_declaration") | ("ts" | "tsx", "abstract_class_declaration") => "class",
        ("ts" | "tsx", "interface_declaration") => "interface",
        ("ts" | "tsx", "type_alias_declaration") => "type",
        ("ts" | "tsx", "enum_declaration") => "enum",
        ("ts" | "tsx" | "js" | "jsx" | "mjs", "method_definition") => "method",
        ("ts" | "tsx" | "js" | "jsx" | "mjs", "variable_declarator") => match name("value").map(|v| v.kind()) {
            Some("arrow_function") | Some("function_expression") | Some("function") => "function",
            _ => return None,
        },
        ("py", "function_definition") => "def",
        ("py", "class_definition") => "class",
        ("cs", "class_declaration") => "class",
        ("cs", "interface_declaration") => "interface",
        ("cs", "struct_declaration") => "struct",
        ("cs", "enum_declaration") => "enum",
        ("cs", "record_declaration") => "record",
        ("cs", "method_declaration") => "method",
        _ => return None,
    };
    name("name").map(|n| (kind, n))
}

fn ts_public(ext: &str, node: tree_sitter::Node, name: &str, src: &[u8]) -> bool {
    match ext {
        "rs" => {
            let mut c = node.walk();
            let vis = node.children(&mut c).any(|ch| ch.kind() == "visibility_modifier");
            vis || node.kind() == "impl_item"
        }
        "go" => name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false),
        "py" => !name.starts_with('_'),
        "cs" => {
            let mut c = node.walk();
            let public = node.children(&mut c).any(|ch| ch.kind() == "modifier" && ch.utf8_text(src).map(|t| t == "public").unwrap_or(false));
            public
        }
        _ => {
            // TS/JS: exported (directly or via lexical declaration) or class members
            let mut p = node.parent();
            while let Some(n) = p {
                match n.kind() {
                    "export_statement" => return true,
                    "class_body" => return true,
                    "program" => return false,
                    _ => p = n.parent(),
                }
            }
            false
        }
    }
}

/// tree-sitter extraction → (symbols, tree_has_errors); `None` when unsupported.
pub fn extract_symbols_ts(rel: &str, ext: &str, text: &str) -> Option<(Vec<Symbol>, bool)> {
    let lang = ts_language(ext)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(text, None)?;
    let src = text.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if let Some((kind, name_node)) = ts_def(ext, node) {
            if let Ok(name) = name_node.utf8_text(src) {
                // `Wrapper<T>` / `fmt::Display` → `Wrapper` / `Display`
                let base = name.split('<').next().unwrap_or(name);
                let name = base.rsplit("::").next().unwrap_or(base).trim().to_string();
                if !name.is_empty() {
                    let public = ts_public(ext, node, &name, src);
                    out.push(Symbol { name, kind: kind.to_string(), path: rel.to_string(), line: node.start_position().row + 1, public });
                }
            }
        }
        let mut c = node.walk();
        let children: Vec<_> = node.children(&mut c).collect();
        for ch in children.into_iter().rev() {
            stack.push(ch);
        }
    }
    out.sort_by_key(|s| s.line);
    Some((out, tree.root_node().has_error()))
}

/// Symbols of one file: tree-sitter first, regex fallback.
pub fn extract_symbols(rel: &str, ext: &str, text: &str) -> Vec<Symbol> {
    match extract_symbols_ts(rel, ext, text) {
        Some((v, false)) => v,
        // syntax errors (e.g. a file mid-edit): merge in regex hits so nothing is lost
        Some((mut v, true)) => {
            for r in extract_symbols_regex(rel, ext, text) {
                if !v.iter().any(|s| s.name == r.name && s.line == r.line) {
                    v.push(r);
                }
            }
            v.sort_by_key(|s| s.line);
            v
        }
        None => extract_symbols_regex(rel, ext, text),
    }
}

/// Regex fallback (used when tree-sitter cannot parse).
pub fn extract_symbols_regex(rel: &str, ext: &str, text: &str) -> Vec<Symbol> {
    let Some(lang) = rules_for(ext) else { return Vec::new() };
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for (ri, (re, name_g, kind_g, pub_g)) in lang.rules.iter().enumerate() {
            if let Some(c) = re.captures(line) {
                let Some(name) = c.get(*name_g) else { continue };
                let kind = if *kind_g > 0 { c.get(*kind_g).map(|m| m.as_str().to_string()).unwrap_or_else(|| default_kind(ext, ri).into()) } else { default_kind(ext, ri).into() };
                let public = match (ext, pub_g) {
                    ("go", _) => name.as_str().chars().next().map(|c| c.is_uppercase()).unwrap_or(false),
                    ("py", _) => !name.as_str().starts_with('_') && c.get(1).map(|m| m.as_str().is_empty()).unwrap_or(true),
                    (_, Some(g)) => c.get(*g).is_some(),
                    _ => false,
                };
                out.push(Symbol { name: name.as_str().to_string(), kind, path: rel.to_string(), line: i + 1, public });
                break;
            }
        }
    }
    out
}

fn mtime(p: &Path) -> u64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

fn cache_path(root: &Path) -> Option<PathBuf> {
    let h = crate::tools::fs::sha256_hex(root.to_string_lossy().as_bytes());
    dirs::home_dir().map(|d| d.join(".cacode").join("index").join(format!("{}.json", &h[..16])))
}

impl SymbolIndex {
    /// Load the cached index (if any) and refresh changed / new / deleted files.
    pub fn load_or_build(root: &Path) -> Self {
        let mut idx: SymbolIndex = cache_path(root)
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .filter(|i: &SymbolIndex| i.root == root && i.version == INDEX_VERSION)
            .unwrap_or_else(|| SymbolIndex { version: INDEX_VERSION, root: root.to_path_buf(), files: HashMap::new() });
        idx.refresh();
        idx
    }

    pub fn refresh(&mut self) -> usize {
        let mut seen = std::collections::HashSet::new();
        let mut changed = 0;
        for f in text_files(&self.root) {
            let ext = f.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            if rules_for(&ext).is_none() {
                continue;
            }
            let rel = f.strip_prefix(&self.root).unwrap_or(&f).to_string_lossy().to_string();
            seen.insert(rel.clone());
            let mt = mtime(&f);
            if self.files.get(&rel).map(|e| e.mtime == mt).unwrap_or(false) {
                continue;
            }
            let text = std::fs::read_to_string(&f).unwrap_or_default();
            self.files.insert(rel.clone(), FileEntry { mtime: mt, symbols: extract_symbols(&rel, &ext, &text) });
            changed += 1;
        }
        let before = self.files.len();
        self.files.retain(|k, _| seen.contains(k));
        changed += before - self.files.len();
        if changed > 0 {
            self.save();
        }
        changed
    }

    fn save(&self) {
        if let Some(p) = cache_path(&self.root) {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(b) = serde_json::to_vec(self) {
                let _ = std::fs::write(p, b);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.files.values().map(|f| f.symbols.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Exact matches first, then case-insensitive prefix, then substring.
    pub fn find(&self, name: &str, kind: Option<&str>, limit: usize) -> Vec<Symbol> {
        let lname = name.to_lowercase();
        let mut scored: Vec<(u8, &Symbol)> = self
            .files
            .values()
            .flat_map(|f| f.symbols.iter())
            .filter(|s| kind.map(|k| s.kind.eq_ignore_ascii_case(k)).unwrap_or(true))
            .filter_map(|s| {
                let ls = s.name.to_lowercase();
                if s.name == name { Some((0, s)) } else if ls == lname { Some((1, s)) } else if ls.starts_with(&lname) { Some((2, s)) } else if ls.contains(&lname) { Some((3, s)) } else { None }
            })
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.path.cmp(&b.1.path)).then(a.1.line.cmp(&b.1.line)));
        scored.into_iter().take(limit).map(|(_, s)| s.clone()).collect()
    }

    /// Compact repository map: per file, its public (or top-level) symbols,
    /// most symbol-dense files first, within `budget` bytes.
    pub fn repo_map(&self, budget: usize) -> String {
        let mut files: Vec<(&String, Vec<&Symbol>)> = self
            .files
            .iter()
            .map(|(p, e)| {
                let pubs: Vec<&Symbol> = e.symbols.iter().filter(|s| s.public && s.kind != "impl").collect();
                (p, if pubs.is_empty() { e.symbols.iter().filter(|s| s.kind != "impl").take(8).collect() } else { pubs })
            })
            .filter(|(_, s)| !s.is_empty())
            .collect();
        files.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
        let mut out = String::new();
        for (path, syms) in files {
            let names: Vec<String> = syms.iter().take(20).map(|s| format!("{} {}", s.kind, s.name)).collect();
            let line = format!("{}: {}\n", path, names.join(", "));
            if out.len() + line.len() > budget {
                out.push_str("…\n");
                break;
            }
            out.push_str(&line);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_sitter_handles_what_regex_misses() {
        // multi-line signature + nested module + generic impl: regex misses / mislabels these
        let rs = "pub mod outer {\n    pub(crate) fn\n    split_sig(a: i32) {}\n    impl<T: Clone> Wrapper<T> {\n        pub fn get(&self) {}\n    }\n}\n// fn not_a_fn() in a comment\nconst S: &str = \"fn fake() {}\";\n";
        let ts = extract_symbols_ts("a.rs", "rs", rs).unwrap().0;
        let names: Vec<(&str, &str, usize)> = ts.iter().map(|s| (s.kind.as_str(), s.name.as_str(), s.line)).collect();
        assert!(names.contains(&("mod", "outer", 1)));
        assert!(names.contains(&("fn", "split_sig", 2)), "{names:?}");
        assert!(names.contains(&("impl", "Wrapper", 4)));
        assert!(names.contains(&("fn", "get", 5)));
        assert!(!names.iter().any(|n| n.1 == "not_a_fn" || n.1 == "fake"), "comments/strings are not symbols: {names:?}");
        let tsx = "export const Button = (p: Props) => <b/>;\nfunction local() {}\nexport class Store { save() {} }\n";
        let t = extract_symbols_ts("a.tsx", "tsx", tsx).unwrap().0;
        assert!(t.iter().any(|s| s.name == "Button" && s.kind == "function" && s.public));
        assert!(t.iter().any(|s| s.name == "local" && !s.public));
        assert!(t.iter().any(|s| s.name == "save" && s.kind == "method"));
        let cs = "namespace X {\n  public sealed partial class MainWindow : Window {\n    public void OnLoad(object s) {}\n    private int Calc() { return 1; }\n  }\n}\n";
        let c = extract_symbols_ts("a.cs", "cs", cs).unwrap().0;
        assert!(c.iter().any(|s| s.name == "MainWindow" && s.public));
        assert!(c.iter().any(|s| s.name == "OnLoad" && s.public && s.kind == "method"));
        assert!(c.iter().any(|s| s.name == "Calc" && !s.public));
    }

    #[test]
    fn extracts_rust_go_ts_py_cs() {
        let rs = "pub struct ReActAgent {\n}\nimpl ReActAgent {\n    pub async fn run_turn(&mut self) {}\n    fn private() {}\n}\n";
        let s = extract_symbols("a.rs", "rs", rs);
        assert!(s.iter().any(|x| x.name == "ReActAgent" && x.kind == "struct" && x.public && x.line == 1));
        assert!(s.iter().any(|x| x.name == "run_turn" && x.kind == "fn" && x.public));
        assert!(s.iter().any(|x| x.name == "private" && !x.public));
        let go = "func (s *Server) Handle(w http.ResponseWriter) {}\ntype Config struct {\n";
        let g = extract_symbols("a.go", "go", go);
        assert!(g.iter().any(|x| x.name == "Handle" && x.public));
        assert!(g.iter().any(|x| x.name == "Config" && x.kind == "struct"));
        assert!(extract_symbols("a.ts", "ts", "export async function loadUser() {}").iter().any(|x| x.name == "loadUser" && x.public));
        assert!(extract_symbols("a.py", "py", "class Foo:\n    def bar(self):\n").len() == 2);
        assert!(extract_symbols("a.cs", "cs", "public sealed partial class MainWindow : Window\n").iter().any(|x| x.name == "MainWindow"));
    }
}
