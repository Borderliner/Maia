//! Small syntax highlighter for the inspector's text preview: keywords, types, strings,
//! comments and numbers for common languages. Line based, with block comments and
//! multi-line strings carried from one line to the next. Not a parser: it only has to
//! make code easier to read at a glance.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Keyword,
    Type,
    Str,
    Comment,
    Number,
}

pub struct Lang {
    line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    /// String delimiters; `"""` / `'''` style triple quotes are handled when listed.
    quotes: &'static [&'static str],
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    /// Markup: `<tag` names and `#`/heading lines count as keywords.
    markup: bool,
}

const C_KW: &[&str] = &["auto", "break", "case", "const", "continue", "default", "do", "else", "enum", "extern", "for", "goto", "if", "inline", "register", "return", "sizeof", "static", "struct", "switch", "typedef", "union", "volatile", "while", "class", "namespace", "template", "typename", "public", "private", "protected", "virtual", "override", "new", "delete", "using", "try", "catch", "throw", "this", "nullptr", "true", "false", "constexpr", "noexcept", "#include", "#define", "#if", "#ifdef", "#ifndef", "#endif", "#else", "#pragma"];
const C_TY: &[&str] = &["int", "char", "void", "float", "double", "long", "short", "unsigned", "signed", "bool", "size_t", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "int8_t", "int16_t", "int32_t", "int64_t", "std", "string", "vector"];

const LANGS: &[(&[&str], Lang)] = &[
    (&["rs"], Lang { line: &["//"], block: Some(("/*", "*/")), quotes: &["\""], keywords: &["as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "macro_rules"], types: &["bool", "char", "str", "String", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32", "f64", "Vec", "Option", "Result", "Box", "Some", "None", "Ok", "Err"], markup: false }),
    (&["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "ino"], Lang { line: &["//"], block: Some(("/*", "*/")), quotes: &["\"", "'"], keywords: C_KW, types: C_TY, markup: false }),
    (&["js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "cts"], Lang { line: &["//"], block: Some(("/*", "*/")), quotes: &["\"", "'", "`"], keywords: &["async", "await", "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do", "else", "export", "extends", "false", "finally", "for", "from", "function", "if", "import", "in", "instanceof", "interface", "let", "new", "null", "of", "return", "static", "super", "switch", "this", "throw", "true", "try", "type", "typeof", "undefined", "var", "void", "while", "yield", "enum", "implements", "private", "public", "readonly"], types: &["string", "number", "boolean", "any", "unknown", "never", "object", "Array", "Promise", "Map", "Set", "Record"], markup: false }),
    (&["py", "pyw", "pyi"], Lang { line: &["#"], block: None, quotes: &["\"\"\"", "'''", "\"", "'"], keywords: &["and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while", "with", "yield", "match", "case", "self"], types: &["int", "str", "float", "bool", "list", "dict", "set", "tuple", "bytes", "object"], markup: false }),
    (&["sh", "bash", "zsh", "fish", "ksh"], Lang { line: &["#"], block: None, quotes: &["\"", "'"], keywords: &["if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "function", "return", "local", "export", "set", "unset", "echo", "exit", "source", "alias", "end", "and", "or", "not"], types: &[], markup: false }),
    (&["go"], Lang { line: &["//"], block: Some(("/*", "*/")), quotes: &["\"", "`", "'"], keywords: &["break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "for", "func", "go", "goto", "if", "import", "interface", "map", "package", "range", "return", "select", "struct", "switch", "type", "var", "true", "false", "nil"], types: &["int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16", "uint32", "uint64", "float32", "float64", "string", "bool", "byte", "rune", "error"], markup: false }),
    (&["java", "kt", "kts", "scala", "cs", "swift", "dart"], Lang { line: &["//"], block: Some(("/*", "*/")), quotes: &["\"", "'"], keywords: &["abstract", "break", "case", "catch", "class", "const", "continue", "default", "do", "else", "enum", "extends", "final", "finally", "for", "if", "implements", "import", "interface", "new", "null", "package", "private", "protected", "public", "return", "static", "super", "switch", "this", "throw", "throws", "try", "void", "while", "fun", "val", "var", "func", "let", "true", "false", "override", "namespace", "using", "async", "await", "struct", "protocol", "guard", "when", "object"], types: &["int", "long", "short", "byte", "char", "float", "double", "boolean", "bool", "String", "string", "Int", "Long", "Double", "Float", "Boolean", "List", "Map"], markup: false }),
    (&["lua"], Lang { line: &["--"], block: Some(("--[[", "]]")), quotes: &["\"", "'"], keywords: &["and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while"], types: &[], markup: false }),
    (&["rb"], Lang { line: &["#"], block: None, quotes: &["\"", "'"], keywords: &["def", "end", "if", "elsif", "else", "unless", "while", "until", "for", "in", "do", "return", "class", "module", "self", "nil", "true", "false", "and", "or", "not", "yield", "begin", "rescue", "ensure", "require", "case", "when"], types: &[], markup: false }),
    (&["php"], Lang { line: &["//", "#"], block: Some(("/*", "*/")), quotes: &["\"", "'"], keywords: &["function", "return", "if", "else", "elseif", "foreach", "for", "while", "class", "public", "private", "protected", "static", "new", "echo", "use", "namespace", "true", "false", "null", "array", "fn"], types: &["int", "string", "bool", "float", "array"], markup: false }),
    (&["sql"], Lang { line: &["--"], block: Some(("/*", "*/")), quotes: &["'", "\""], keywords: &["select", "from", "where", "insert", "into", "values", "update", "set", "delete", "create", "table", "drop", "alter", "join", "left", "right", "inner", "outer", "on", "and", "or", "not", "null", "as", "order", "by", "group", "having", "limit", "primary", "key", "index", "SELECT", "FROM", "WHERE", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "TABLE", "DROP", "JOIN", "ON", "AND", "OR", "NOT", "NULL", "AS", "ORDER", "BY", "GROUP", "LIMIT"], types: &["int", "integer", "text", "varchar", "real", "INTEGER", "TEXT", "VARCHAR", "REAL"], markup: false }),
    (&["zig"], Lang { line: &["//"], block: None, quotes: &["\"", "'"], keywords: &["const", "var", "fn", "pub", "return", "if", "else", "while", "for", "switch", "struct", "enum", "union", "try", "catch", "defer", "errdefer", "comptime", "true", "false", "null", "undefined"], types: &["u8", "u16", "u32", "u64", "usize", "i8", "i16", "i32", "i64", "isize", "f32", "f64", "bool", "void"], markup: false }),
    (&["json", "jsonc", "json5"], Lang { line: &["//"], block: Some(("/*", "*/")), quotes: &["\""], keywords: &["true", "false", "null"], types: &[], markup: false }),
    (&["toml", "ini", "conf", "cfg", "desktop", "service", "properties"], Lang { line: &["#", ";"], block: None, quotes: &["\"\"\"", "\"", "'"], keywords: &["true", "false"], types: &[], markup: false }),
    (&["yaml", "yml"], Lang { line: &["#"], block: None, quotes: &["\"", "'"], keywords: &["true", "false", "null", "yes", "no"], types: &[], markup: false }),
    (&["css", "scss", "sass", "less"], Lang { line: &["//"], block: Some(("/*", "*/")), quotes: &["\"", "'"], keywords: &["@media", "@import", "@keyframes", "!important", "@font-face", "@supports"], types: &[], markup: false }),
    (&["html", "htm", "xml", "svg", "xhtml", "vue", "plist", "xsd"], Lang { line: &[], block: Some(("<!--", "-->")), quotes: &["\"", "'"], keywords: &[], types: &[], markup: true }),
    (&["md", "markdown"], Lang { line: &[], block: None, quotes: &["`"], keywords: &[], types: &[], markup: true }),
    (&["mk"], Lang { line: &["#"], block: None, quotes: &["\"", "'"], keywords: &["ifeq", "ifneq", "ifdef", "ifndef", "else", "endif", "include", "define", "endef", "export"], types: &[], markup: false }),
];

/// Language for a file name, if it is code or markup we know.
pub fn lang_for(name: &str) -> Option<&'static Lang> {
    let lower = name.to_ascii_lowercase();
    let ext = match lower.as_str() {
        "makefile" | "gnumakefile" => "mk",
        "dockerfile" | "pkgbuild" | ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" => "sh",
        "cargo.lock" => "toml",
        _ => lower.rsplit_once('.').map(|(_, e)| e).unwrap_or(""),
    };
    LANGS.iter().find(|(exts, _)| exts.contains(&ext)).map(|(_, l)| l)
}

/// Spans (byte ranges and kinds) for each line, carrying block comments and multi-line
/// strings across lines. Lines beyond `max_lines` get no spans (plain).
pub fn highlight(lang: &Lang, lines: &[String], max_lines: usize) -> Vec<Vec<(usize, usize, Kind)>> {
    let mut out = Vec::with_capacity(lines.len().min(max_lines));
    // Open block comment, or open multi-line string (its delimiter), from the previous line.
    let mut in_block = false;
    let mut in_str: Option<&'static str> = None;
    for line in lines.iter().take(max_lines) {
        let b = line.as_bytes();
        let mut spans: Vec<(usize, usize, Kind)> = vec![];
        let mut i = 0;
        let push = |spans: &mut Vec<(usize, usize, Kind)>, s: usize, e: usize, k: Kind| {
            if e > s && k != Kind::Plain {
                spans.push((s, e, k));
            }
        };
        if lang.markup && lang.quotes == ["`"] {
            // Markdown: headings, fences and inline code.
            let t = line.trim_start();
            if t.starts_with('#') || t.starts_with("```") || t.starts_with("~~~") {
                push(&mut spans, 0, b.len(), Kind::Keyword);
                out.push(spans);
                continue;
            }
        }
        while i < b.len() {
            if in_block {
                let (_, close) = lang.block.unwrap_or(("", ""));
                let found = line[i..].find(close);
                let end = found.map(|p| i + p + close.len()).unwrap_or(b.len());
                push(&mut spans, i, end, Kind::Comment);
                if found.is_some() {
                    in_block = false;
                }
                i = end;
                continue;
            }
            if let Some(q) = in_str {
                let found = find_close(line, i, q);
                let end = found.unwrap_or(b.len()).min(b.len());
                push(&mut spans, i, end, Kind::Str);
                if found.is_some() {
                    in_str = None;
                }
                i = end;
                continue;
            }
            let rest = &line[i..];
            if let Some(lc) = lang.line.iter().find(|lc| rest.starts_with(**lc)) {
                // "#" in shells/Python counts only at a word boundary ("a#b" isn't a comment).
                if !(*lc == "#" && i > 0 && !b[i - 1].is_ascii_whitespace() && b[i - 1] != b';') {
                    push(&mut spans, i, b.len(), Kind::Comment);
                    break;
                }
            }
            if let Some((open, close)) = lang.block {
                if rest.starts_with(open) {
                    let from = i + open.len();
                    match line[from..].find(close) {
                        Some(p) => {
                            push(&mut spans, i, from + p + close.len(), Kind::Comment);
                            i = from + p + close.len();
                        }
                        None => {
                            push(&mut spans, i, b.len(), Kind::Comment);
                            in_block = true;
                            i = b.len();
                        }
                    }
                    continue;
                }
            }
            if let Some(q) = lang.quotes.iter().find(|q| rest.starts_with(**q)) {
                let from = i + q.len();
                match find_close(line, from, q) {
                    Some(end) => {
                        let end = end.min(b.len());
                        push(&mut spans, i, end, Kind::Str);
                        i = end;
                    }
                    None => {
                        push(&mut spans, i, b.len(), Kind::Str);
                        // Only triple quotes and backticks span lines; a lone quote ends here.
                        if q.len() == 3 || *q == "`" {
                            in_str = Some(q);
                        }
                        i = b.len();
                    }
                }
                continue;
            }
            let c = b[i];
            if lang.markup && c == b'<' {
                let s = i;
                let mut j = i + 1;
                if j < b.len() && (b[j] == b'/' || b[j] == b'!' || b[j] == b'?') {
                    j += 1;
                }
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b"-:_".contains(&b[j])) {
                    j += 1;
                }
                push(&mut spans, s, j, Kind::Keyword);
                i = j.max(i + 1);
                continue;
            }
            if c.is_ascii_digit() && (i == 0 || !is_word(b[i - 1])) {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                    i += 1;
                }
                push(&mut spans, s, i, Kind::Number);
                continue;
            }
            if is_word(c) || c == b'#' || c == b'@' || c == b'!' {
                let s = i;
                i += 1;
                while i < b.len() && is_word(b[i]) {
                    i += 1;
                }
                let w = &line[s..i];
                let k = if lang.keywords.contains(&w) {
                    Kind::Keyword
                } else if lang.types.contains(&w) {
                    Kind::Type
                } else {
                    Kind::Plain
                };
                push(&mut spans, s, i, k);
                continue;
            }
            i += line[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
        out.push(spans);
    }
    out
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// End (exclusive) of a string starting at `from` (after its opening quote), honouring
/// backslash escapes for single-character quotes.
fn find_close(line: &str, from: usize, q: &str) -> Option<usize> {
    let b = line.as_bytes();
    let mut i = from;
    while i < b.len() {
        if q.len() == 1 && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i..].starts_with(q.as_bytes()) {
            return Some(i + q.len());
        }
        i += 1;
    }
    None
}
