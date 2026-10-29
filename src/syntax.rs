//! Small syntax highlighter for the inspector's text preview, the editor and Search Code:
//! keywords, types, functions, fields and keys, constants, attributes and macros, strings
//! with their escapes, comments and numbers for common languages. Line based, with block
//! comments, multi-line strings and open markup tags carried from one line to the next.
//! Not a parser: it reads what a name is from what is around it (`fn` before it, `(`
//! after it, a `.` in front), which is what makes code easier to read at a glance.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Keyword,
    Type,
    Str,
    Comment,
    Number,
    /// A function or method, where it is defined or called.
    Function,
    /// A field, an object key, a markup attribute, a shell variable.
    Property,
    /// What is about the code: attributes, macros, decorators, lifetimes, list markers.
    Meta,
    /// `true`, `None`, an ALL_CAPS name, an escape inside a string.
    Constant,
}

pub type Spans = Vec<(usize, usize, Kind)>;

/// Where keys are, in data and config files.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keys {
    None,
    /// `"key":`
    Json,
    /// `key:` at the start of a line.
    Yaml,
    /// `[section]` and `key =`.
    Ini,
    /// `property:` inside a rule.
    Css,
}

pub struct Lang {
    line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    /// String delimiters; `"""` / `'''` style triple quotes are handled when listed.
    quotes: &'static [&'static str],
    /// Quotes whose text is taken as written: no escapes, no variables.
    raw: &'static [&'static str],
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    constants: &'static [&'static str],
    /// Keywords whose next name is a function being defined (`fn`, `def`).
    fn_decl: &'static [&'static str],
    /// Keywords whose next name is a type being defined (`struct`, `class`).
    ty_decl: &'static [&'static str],
    /// Capitalised names are types, by the language's convention.
    caps_types: bool,
    /// ALL_CAPS names are constants.
    caps_consts: bool,
    /// The name after a `.` is a field (or a method, when called).
    members: bool,
    /// Backslash escapes inside strings.
    escapes: bool,
    /// Keywords match in any case (SQL, Dockerfile).
    any_case: bool,
    /// `#[attr]` and `#![attr]`.
    attrs: bool,
    /// `name!` macros.
    macros: bool,
    /// `@decorator` and `@Annotation`.
    decorators: bool,
    /// `'a` lifetimes beside `'x'` characters.
    lifetimes: bool,
    /// `$VAR`, `${VAR}`, `$1`, also inside double quotes.
    vars: bool,
    /// `-` inside names (CSS properties, markup attributes).
    dashes: bool,
    keys: Keys,
    /// Markup: `<tag` names, attributes in tags, `&entities;`; Markdown's headings too.
    markup: bool,
}

const CODE: Lang = Lang {
    line: &["//"],
    block: Some(("/*", "*/")),
    quotes: &["\""],
    raw: &[],
    keywords: &[],
    types: &[],
    constants: &["true", "false", "null"],
    fn_decl: &[],
    ty_decl: &[],
    caps_types: false,
    caps_consts: true,
    members: true,
    escapes: true,
    any_case: false,
    attrs: false,
    macros: false,
    decorators: false,
    lifetimes: false,
    vars: false,
    dashes: false,
    keys: Keys::None,
    markup: false,
};
const SCRIPT: Lang = Lang { line: &["#"], block: None, quotes: &["\"", "'"], ..CODE };
const DATA: Lang = Lang { caps_consts: false, members: false, ..CODE };
const MARKUP: Lang = Lang { line: &[], block: Some(("<!--", "-->")), quotes: &["\"", "'"], constants: &[], caps_consts: false, members: false, escapes: false, dashes: true, markup: true, ..CODE };

const C_KW: &[&str] = &["auto", "break", "case", "const", "continue", "default", "do", "else", "enum", "extern", "for", "goto", "if", "inline", "register", "return", "sizeof", "static", "struct", "switch", "typedef", "union", "volatile", "while", "class", "namespace", "template", "typename", "public", "private", "protected", "virtual", "override", "new", "delete", "using", "try", "catch", "throw", "this", "constexpr", "noexcept", "operator", "friend", "#include", "#define", "#undef", "#if", "#ifdef", "#ifndef", "#elif", "#endif", "#else", "#pragma", "#error"];
const C_TY: &[&str] = &["int", "char", "void", "float", "double", "long", "short", "unsigned", "signed", "bool", "size_t", "ssize_t", "ptrdiff_t", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "int8_t", "int16_t", "int32_t", "int64_t", "uintptr_t", "std", "string", "vector", "auto"];
const JS_KW: &[&str] = &["as", "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do", "else", "export", "extends", "finally", "for", "from", "function", "get", "if", "implements", "import", "in", "instanceof", "interface", "keyof", "let", "new", "of", "private", "protected", "public", "readonly", "return", "satisfies", "set", "static", "super", "switch", "this", "throw", "try", "type", "typeof", "var", "void", "while", "with", "yield", "enum", "declare", "namespace", "abstract"];
const JAVA_KW: &[&str] = &["abstract", "break", "case", "catch", "class", "const", "continue", "default", "do", "else", "enum", "extends", "final", "finally", "for", "if", "implements", "import", "instanceof", "interface", "new", "package", "private", "protected", "public", "return", "static", "super", "switch", "synchronized", "this", "throw", "throws", "try", "void", "volatile", "while", "fun", "val", "var", "func", "let", "override", "namespace", "using", "async", "await", "struct", "protocol", "extension", "guard", "when", "object", "companion", "data", "sealed", "open", "internal", "init", "def", "trait", "match", "case", "is", "in", "out", "yield", "where", "lazy", "lateinit", "suspend", "inline", "operator", "get", "set", "required", "late", "mixin", "library", "part", "show", "hide", "typedef", "record", "readonly", "virtual", "sealed"];

const LANGS: &[(&[&str], Lang)] = &[
    (
        &["rs"],
        Lang {
            keywords: &["as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "type", "union", "unsafe", "use", "where", "while", "macro_rules"],
            types: &["bool", "char", "str", "String", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32", "f64"],
            constants: &["true", "false"],
            fn_decl: &["fn"],
            ty_decl: &["struct", "enum", "trait", "type", "union", "impl"],
            caps_types: true,
            attrs: true,
            macros: true,
            lifetimes: true,
            ..CODE
        },
    ),
    (&["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "ino", "m", "mm"], Lang { quotes: &["\"", "'"], keywords: C_KW, types: C_TY, constants: &["true", "false", "nullptr", "NULL"], ty_decl: &["struct", "class", "enum", "union", "typedef", "namespace"], ..CODE }),
    (
        &["js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "cts"],
        Lang {
            quotes: &["\"", "'", "`"],
            keywords: JS_KW,
            types: &["string", "number", "boolean", "bigint", "symbol", "any", "unknown", "never", "object", "Array", "Promise", "Map", "Set", "Record", "Partial", "Readonly"],
            constants: &["true", "false", "null", "undefined", "NaN", "Infinity"],
            fn_decl: &["function"],
            ty_decl: &["class", "interface", "type", "enum", "extends", "implements", "new"],
            caps_types: true,
            decorators: true,
            ..CODE
        },
    ),
    (
        &["py", "pyw", "pyi"],
        Lang {
            quotes: &["\"\"\"", "'''", "\"", "'"],
            keywords: &["and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with", "yield", "match", "case", "self", "cls"],
            types: &["int", "str", "float", "bool", "list", "dict", "set", "frozenset", "tuple", "bytes", "bytearray", "object", "type", "complex"],
            constants: &["True", "False", "None", "NotImplemented", "Ellipsis"],
            fn_decl: &["def"],
            ty_decl: &["class"],
            caps_types: true,
            decorators: true,
            ..SCRIPT
        },
    ),
    (
        &["sh", "bash", "zsh", "fish", "ksh"],
        Lang {
            keywords: &["if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "function", "return", "local", "export", "declare", "readonly", "set", "unset", "shift", "echo", "printf", "exit", "source", "alias", "end", "and", "or", "not", "eval", "exec", "trap", "test", "cd"],
            constants: &["true", "false"],
            fn_decl: &["function"],
            raw: &["'"],
            caps_consts: false,
            members: false,
            vars: true,
            ..SCRIPT
        },
    ),
    (
        &["go"],
        Lang {
            quotes: &["\"", "`", "'"],
            raw: &["`"],
            keywords: &["break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "for", "func", "go", "goto", "if", "import", "interface", "map", "package", "range", "return", "select", "struct", "switch", "type", "var"],
            types: &["int", "int8", "int16", "int32", "int64", "uint", "uint8", "uint16", "uint32", "uint64", "uintptr", "float32", "float64", "complex64", "complex128", "string", "bool", "byte", "rune", "error", "any"],
            constants: &["true", "false", "nil", "iota"],
            fn_decl: &["func"],
            ty_decl: &["type"],
            ..CODE
        },
    ),
    (
        &["java", "kt", "kts", "scala", "cs", "swift", "dart", "groovy", "gradle"],
        Lang {
            quotes: &["\"\"\"", "\"", "'"],
            keywords: JAVA_KW,
            types: &["int", "long", "short", "byte", "char", "float", "double", "boolean", "bool", "String", "string", "Int", "Long", "Double", "Float", "Boolean", "Unit", "Any", "List", "Map", "Set", "Array", "object", "dynamic", "var"],
            constants: &["true", "false", "null", "nil"],
            fn_decl: &["fun", "func", "def"],
            ty_decl: &["class", "interface", "enum", "struct", "protocol", "extension", "object", "record", "trait", "extends", "implements", "new"],
            caps_types: true,
            decorators: true,
            ..CODE
        },
    ),
    (&["lua"], Lang { line: &["--"], block: Some(("--[[", "]]")), quotes: &["\"", "'"], keywords: &["and", "break", "do", "else", "elseif", "end", "for", "function", "goto", "if", "in", "local", "not", "or", "repeat", "return", "then", "until", "while"], constants: &["true", "false", "nil"], fn_decl: &["function"], ..CODE }),
    (&["rb", "rake", "gemspec"], Lang { keywords: &["def", "end", "if", "elsif", "else", "unless", "while", "until", "for", "in", "do", "return", "class", "module", "self", "and", "or", "not", "yield", "begin", "rescue", "ensure", "require", "require_relative", "include", "extend", "case", "when", "then", "attr_accessor", "attr_reader", "attr_writer", "private", "protected", "public", "lambda", "proc"], constants: &["true", "false", "nil"], fn_decl: &["def"], ty_decl: &["class", "module"], caps_types: true, ..SCRIPT }),
    (&["php"], Lang { line: &["//", "#"], quotes: &["\"", "'"], keywords: &["function", "return", "if", "else", "elseif", "foreach", "for", "while", "as", "class", "interface", "trait", "extends", "implements", "public", "private", "protected", "static", "new", "echo", "use", "namespace", "fn", "match", "try", "catch", "throw", "abstract", "final"], types: &["int", "string", "bool", "float", "array", "mixed", "void"], constants: &["true", "false", "null", "TRUE", "FALSE", "NULL"], fn_decl: &["function"], ty_decl: &["class", "interface", "trait", "new"], caps_types: true, vars: true, raw: &["'"], ..CODE }),
    (
        &["sql"],
        Lang {
            line: &["--"],
            quotes: &["'", "\""],
            keywords: &["select", "from", "where", "insert", "into", "values", "update", "set", "delete", "create", "table", "view", "drop", "alter", "add", "column", "join", "left", "right", "inner", "outer", "full", "cross", "on", "using", "and", "or", "not", "in", "is", "like", "between", "exists", "as", "order", "by", "group", "having", "limit", "offset", "distinct", "union", "all", "case", "when", "then", "else", "end", "primary", "foreign", "references", "key", "index", "unique", "default", "begin", "commit", "rollback", "with", "returning", "if", "asc", "desc"],
            types: &["int", "integer", "bigint", "smallint", "text", "varchar", "char", "real", "float", "double", "decimal", "numeric", "boolean", "date", "time", "timestamp", "blob", "json", "jsonb", "uuid", "serial"],
            constants: &["null", "true", "false"],
            caps_consts: false,
            members: false,
            escapes: false,
            any_case: true,
            ..CODE
        },
    ),
    (&["zig"], Lang { block: None, quotes: &["\"", "'"], keywords: &["const", "var", "fn", "pub", "return", "if", "else", "while", "for", "switch", "struct", "enum", "union", "try", "catch", "defer", "errdefer", "comptime", "inline", "test", "orelse", "and", "or", "error", "usingnamespace", "export", "extern"], types: &["u8", "u16", "u32", "u64", "usize", "i8", "i16", "i32", "i64", "isize", "f32", "f64", "bool", "void", "anytype", "type", "noreturn"], constants: &["true", "false", "null", "undefined"], fn_decl: &["fn"], caps_types: true, macros: false, ..CODE }),
    (&["json", "jsonc", "json5", "geojson", "webmanifest"], Lang { keys: Keys::Json, ..DATA }),
    (&["toml", "ini", "conf", "cfg", "desktop", "service", "properties", "editorconfig"], Lang { line: &["#", ";"], block: None, quotes: &["\"\"\"", "\"", "'"], constants: &["true", "false"], keys: Keys::Ini, ..DATA }),
    (&["yaml", "yml"], Lang { line: &["#"], block: None, quotes: &["\"", "'"], constants: &["true", "false", "null", "yes", "no", "on", "off", "~"], keys: Keys::Yaml, ..DATA }),
    (&["css", "scss", "sass", "less"], Lang { quotes: &["\"", "'"], keywords: &["@media", "@import", "@keyframes", "!important", "@font-face", "@supports", "@layer", "@container", "@use", "@forward", "@mixin", "@include", "@extend", "@if", "@else", "@each", "@return", "@function"], constants: &[], caps_consts: false, members: false, dashes: true, keys: Keys::Css, vars: false, ..CODE }),
    (&["html", "htm", "xml", "svg", "xhtml", "vue", "svelte", "astro", "plist", "xsd", "xsl", "xslt", "ui", "glade", "csproj", "props"], MARKUP),
    (&["md", "markdown", "mdx"], Lang { block: None, quotes: &["`"], ..MARKUP }),
    (&["mk"], Lang { keywords: &["ifeq", "ifneq", "ifdef", "ifndef", "else", "endif", "include", "define", "endef", "export", "override", "vpath"], constants: &[], caps_consts: false, members: false, vars: true, raw: &["'"], ..SCRIPT }),
    (&["dockerfile"], Lang { keywords: &["from", "run", "cmd", "label", "expose", "env", "add", "copy", "entrypoint", "volume", "user", "workdir", "arg", "onbuild", "stopsignal", "healthcheck", "shell", "as"], constants: &[], caps_consts: false, members: false, any_case: true, vars: true, raw: &["'"], ..SCRIPT }),
    (&["nix"], Lang { line: &["#"], quotes: &["''", "\""], raw: &["''"], keywords: &["let", "in", "with", "rec", "inherit", "if", "then", "else", "assert", "import", "or"], constants: &["true", "false", "null"], ..CODE }),
    (&["hs", "lhs", "elm", "purs"], Lang { line: &["--"], block: Some(("{-", "-}")), quotes: &["\"", "'"], keywords: &["module", "where", "import", "qualified", "as", "hiding", "data", "type", "newtype", "class", "instance", "deriving", "let", "in", "case", "of", "if", "then", "else", "do", "forall", "exposing", "port"], constants: &["True", "False", "Nothing"], ty_decl: &["data", "type", "newtype", "class", "instance"], caps_types: true, caps_consts: false, ..CODE }),
    (&["ex", "exs", "heex"], Lang { quotes: &["\"\"\"", "\"", "'"], keywords: &["def", "defp", "defmodule", "defmacro", "defstruct", "defimpl", "defprotocol", "do", "end", "fn", "case", "cond", "with", "if", "else", "unless", "when", "and", "or", "not", "in", "import", "alias", "require", "use", "raise", "rescue", "try", "receive", "after"], constants: &["true", "false", "nil"], fn_decl: &["def", "defp", "defmacro"], ty_decl: &["defmodule", "defimpl", "defprotocol"], caps_types: true, decorators: true, ..SCRIPT }),
    (&["r", "rmd"], Lang { keywords: &["if", "else", "for", "while", "repeat", "function", "return", "break", "next", "in", "library", "require"], constants: &["TRUE", "FALSE", "NULL", "NA", "Inf", "NaN", "T", "F"], fn_decl: &[], ..SCRIPT }),
    (&["jl"], Lang { block: Some(("#=", "=#")), quotes: &["\"\"\"", "\"", "'"], keywords: &["function", "end", "if", "elseif", "else", "for", "while", "return", "begin", "let", "local", "global", "const", "module", "using", "import", "export", "struct", "mutable", "abstract", "type", "macro", "quote", "do", "try", "catch", "finally", "where", "in", "isa"], constants: &["true", "false", "nothing", "missing"], fn_decl: &["function", "macro"], ty_decl: &["struct", "type"], caps_types: true, decorators: true, ..SCRIPT }),
    (&["pl", "pm", "t"], Lang { keywords: &["my", "our", "local", "sub", "if", "elsif", "else", "unless", "while", "until", "for", "foreach", "return", "use", "require", "package", "last", "next", "redo", "and", "or", "not", "eq", "ne", "lt", "gt", "le", "ge", "print", "die"], constants: &[], fn_decl: &["sub"], ty_decl: &["package"], vars: true, raw: &["'"], ..SCRIPT }),
    (&["vim"], Lang { line: &["\""], block: None, quotes: &["'"], keywords: &["let", "set", "if", "elseif", "else", "endif", "for", "endfor", "while", "endwhile", "function", "endfunction", "return", "call", "map", "nnoremap", "inoremap", "vnoremap", "noremap", "autocmd", "augroup", "command", "syntax", "highlight", "source", "execute"], constants: &["v:true", "v:false"], fn_decl: &["function"], escapes: false, ..CODE }),
    (&["cmake"], Lang { keywords: &["if", "elseif", "else", "endif", "foreach", "endforeach", "while", "endwhile", "function", "endfunction", "macro", "endmacro", "return", "set", "unset", "option", "project", "add_executable", "add_library", "target_link_libraries", "target_include_directories", "find_package", "include", "install", "message", "cmake_minimum_required", "add_subdirectory", "list", "string"], constants: &["ON", "OFF", "TRUE", "FALSE"], any_case: false, vars: true, ..SCRIPT }),
    (&["proto"], Lang { keywords: &["syntax", "package", "import", "option", "message", "enum", "service", "rpc", "returns", "repeated", "optional", "required", "oneof", "map", "reserved", "extend", "stream"], types: &["double", "float", "int32", "int64", "uint32", "uint64", "sint32", "sint64", "fixed32", "fixed64", "bool", "string", "bytes"], ty_decl: &["message", "enum", "service"], caps_types: true, ..CODE }),
    (&["graphql", "gql"], Lang { line: &["#"], block: None, keywords: &["query", "mutation", "subscription", "fragment", "on", "type", "interface", "union", "enum", "input", "scalar", "schema", "extend", "implements", "directive"], types: &["Int", "Float", "String", "Boolean", "ID"], ty_decl: &["type", "interface", "union", "enum", "input", "scalar", "fragment"], caps_types: true, decorators: true, ..CODE }),
    (&["tf", "tfvars", "hcl"], Lang { line: &["#", "//"], keywords: &["resource", "data", "variable", "output", "module", "provider", "terraform", "locals", "for", "in", "if", "for_each", "count", "depends_on", "lifecycle", "dynamic"], types: &["string", "number", "bool", "list", "map", "set", "object", "tuple", "any"], ..CODE }),
    (&["ps1", "psm1", "psd1"], Lang { line: &["#"], block: Some(("<#", "#>")), quotes: &["\"", "'"], keywords: &["function", "param", "if", "elseif", "else", "foreach", "for", "while", "do", "until", "switch", "return", "break", "continue", "try", "catch", "finally", "throw", "begin", "process", "end", "in", "class", "enum", "using", "filter"], constants: &[], fn_decl: &["function", "filter"], ty_decl: &["class", "enum"], caps_consts: false, any_case: true, vars: true, ..CODE }),
    (&["ml", "mli"], Lang { line: &[], block: Some(("(*", "*)")), quotes: &["\""], keywords: &["let", "rec", "in", "and", "fun", "function", "match", "with", "when", "if", "then", "else", "type", "of", "module", "struct", "sig", "end", "open", "include", "begin", "mutable", "val", "external", "exception", "raise", "try"], types: &["int", "float", "string", "bool", "unit", "list", "array", "option", "char"], constants: &["true", "false", "None", "Some"], ty_decl: &["type", "module", "exception"], caps_types: true, caps_consts: false, ..CODE }),
];

/// Language for a file name, if it is code or markup we know.
impl Lang {
    /// What starts a comment to the end of the line, and what opens and closes one.
    pub fn comments(&self) -> (&'static [&'static str], Option<(&'static str, &'static str)>) {
        (self.line, self.block)
    }
}

pub fn lang_for(name: &str) -> Option<&'static Lang> {
    let lower = name.to_ascii_lowercase();
    let ext = match lower.as_str() {
        "makefile" | "gnumakefile" => "mk",
        "pkgbuild" | ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".bash_aliases" | ".xprofile" | "apkbuild" => "sh",
        "dockerfile" | "containerfile" => "dockerfile",
        "cmakelists.txt" => "cmake",
        "cargo.lock" | "uv.lock" | "poetry.lock" => "toml",
        "gemfile" | "rakefile" => "rb",
        ".vimrc" | ".gvimrc" => "vim",
        n if n.starts_with("dockerfile.") || n.ends_with(".dockerfile") => "dockerfile",
        _ => lower.rsplit_once('.').map(|(_, e)| e).unwrap_or(""),
    };
    LANGS.iter().find(|(exts, _)| exts.contains(&ext)).map(|(_, l)| l)
}

pub fn is_markdown(lang: &Lang) -> bool {
    lang.markup && lang.quotes == ["`"]
}

/// What a line leaves open for the next one.
#[derive(Clone, Copy, Default)]
pub struct State {
    in_block: bool,
    /// The delimiter of a string still open.
    in_str: Option<&'static str>,
    /// Inside a markup tag, among its attributes.
    in_tag: bool,
}

/// Spans (byte ranges and kinds) for each line, carrying block comments and multi-line
/// strings across lines. Lines beyond `max_lines` get no spans (plain).
pub fn highlight(lang: &Lang, lines: &[String], max_lines: usize) -> Vec<Spans> {
    let mut st = State::default();
    lines.iter().take(max_lines).map(|l| line_spans(lang, l, &mut st)).collect()
}

/// One line's spans, going on from what the line before left open in `st`. A line seen
/// alone (a search result) starts from `State::default()`.
pub fn line_spans(lang: &Lang, line: &str, st: &mut State) -> Spans {
    let b = line.as_bytes();
    let mut out: Spans = vec![];
    let mut i = 0;
    let word = |c: u8| is_word(c) || (lang.dashes && c == b'-');
    if is_markdown(lang) && !st.in_block && st.in_str.is_none() {
        let t = line.trim_start();
        let lead = line.len() - t.len();
        if t.starts_with('#') || t.starts_with("```") || t.starts_with("~~~") {
            push(&mut out, 0, b.len(), Kind::Keyword);
            return out;
        }
        if t.starts_with('>') {
            push(&mut out, lead, b.len(), Kind::Comment);
            return out;
        }
        if let Some(n) = list_marker(t) {
            push(&mut out, lead, lead + n, Kind::Meta);
            i = lead + n;
        }
    }
    if st.in_str.is_none() && !st.in_block {
        match lang.keys {
            Keys::Yaml => {
                if let Some((s, e)) = yaml_key(line) {
                    push(&mut out, s, e, Kind::Property);
                    i = e;
                }
            }
            Keys::Ini => {
                let t = line.trim_start();
                let lead = line.len() - t.len();
                if t.starts_with('[') {
                    let end = t.rfind(']').map_or(t.len(), |e| e + 1);
                    push(&mut out, lead, lead + end, Kind::Type);
                    i = lead + end;
                } else if let Some(eq) = t.find('=').filter(|&e| e > 0 && !t[..e].contains(['"', '\'', '#', ';'])) {
                    push(&mut out, lead, lead + t[..eq].trim_end().len(), Kind::Property);
                    i = lead + eq;
                }
            }
            _ => {}
        }
    }
    // The next name is one being defined: a function's or a type's.
    let mut defining: Option<Kind> = None;
    while i < b.len() {
        if st.in_block {
            let (_, close) = lang.block.unwrap_or(("", ""));
            let found = line[i..].find(close);
            let end = found.map(|p| i + p + close.len()).unwrap_or(b.len());
            push(&mut out, i, end, Kind::Comment);
            if found.is_some() {
                st.in_block = false;
            }
            i = end;
            continue;
        }
        if let Some(q) = st.in_str {
            let found = find_close(line, i, q, lang.escapes);
            let end = found.unwrap_or(b.len()).min(b.len());
            string(lang, line, i, end, q, &mut out);
            if found.is_some() {
                st.in_str = None;
            }
            i = end;
            continue;
        }
        let rest = &line[i..];
        if let Some(lc) = lang.line.iter().find(|lc| rest.starts_with(**lc)) {
            // "#" in shells/Python counts only at a word boundary ("a#b" isn't a comment).
            if !(*lc == "#" && i > 0 && !b[i - 1].is_ascii_whitespace() && b[i - 1] != b';') {
                push(&mut out, i, b.len(), Kind::Comment);
                break;
            }
        }
        if let Some((open, close)) = lang.block {
            if rest.starts_with(open) {
                let from = i + open.len();
                match line[from..].find(close) {
                    Some(p) => {
                        push(&mut out, i, from + p + close.len(), Kind::Comment);
                        i = from + p + close.len();
                    }
                    None => {
                        push(&mut out, i, b.len(), Kind::Comment);
                        st.in_block = true;
                        i = b.len();
                    }
                }
                continue;
            }
        }
        if lang.attrs && (rest.starts_with("#[") || rest.starts_with("#![")) {
            let end = bracket_end(b, i + rest.find('[').unwrap_or(1));
            push(&mut out, i, end, Kind::Meta);
            i = end;
            continue;
        }
        if lang.lifetimes && b[i] == b'\'' {
            if let Some(end) = char_literal(line, i) {
                string(lang, line, i, end, "'", &mut out);
                i = end;
                continue;
            }
            let mut j = i + 1;
            while j < b.len() && is_word(b[j]) {
                j += 1;
            }
            push(&mut out, i, j, Kind::Meta);
            i = j;
            continue;
        }
        // Quotes in markup's text are apostrophes; only an attribute's value is a string.
        let quoting = !lang.markup || is_markdown(lang) || st.in_tag;
        if let Some(q) = lang.quotes.iter().find(|q| quoting && rest.starts_with(**q)) {
            let from = i + q.len();
            match find_close(line, from, q, lang.escapes) {
                Some(end) => {
                    let end = end.min(b.len());
                    let key = lang.keys == Keys::Json && line[end..].trim_start().starts_with(':');
                    if key {
                        push(&mut out, i, end, Kind::Property);
                    } else {
                        string(lang, line, i, end, q, &mut out);
                    }
                    i = end;
                }
                None => {
                    string(lang, line, i, b.len(), q, &mut out);
                    // Only triple quotes and backticks span lines; a lone quote ends here.
                    if q.len() > 1 || *q == "`" {
                        st.in_str = Some(q);
                    }
                    i = b.len();
                }
            }
            continue;
        }
        let c = b[i];
        if c == b'<' && lang.keywords.contains(&"#include") && line.trim_start().starts_with("#include") {
            let end = line[i..].find('>').map_or(b.len(), |p| i + p + 1);
            push(&mut out, i, end, Kind::Str);
            i = end;
            continue;
        }
        if lang.markup {
            if c == b'<' {
                let s = i;
                let mut j = i + 1;
                if j < b.len() && (b[j] == b'/' || b[j] == b'!' || b[j] == b'?') {
                    j += 1;
                }
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b"-:_.".contains(&b[j])) {
                    j += 1;
                }
                push(&mut out, s, j, Kind::Keyword);
                st.in_tag = j > s + 1 && !is_markdown(lang);
                i = j.max(i + 1);
                continue;
            }
            if c == b'>' && st.in_tag {
                st.in_tag = false;
                i += 1;
                continue;
            }
            if c == b'&' {
                let n = b[i + 1..].iter().take(10).position(|&x| x == b';').filter(|&n| n > 0 && b[i + 1..i + 1 + n].iter().all(|x| x.is_ascii_alphanumeric() || *x == b'#'));
                if let Some(n) = n {
                    push(&mut out, i, i + n + 2, Kind::Constant);
                    i += n + 2;
                    continue;
                }
            }
            if is_markdown(lang) && c == b'[' {
                if let Some((text_end, url_end)) = md_link(line, i) {
                    push(&mut out, i, text_end, Kind::Property);
                    push(&mut out, text_end, url_end, Kind::Str);
                    i = url_end;
                    continue;
                }
            }
        }
        if c.is_ascii_digit() && (i == 0 || !is_word(b[i - 1])) {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            // A range's `..` is not part of the number before it.
            if let Some(p) = line[s..i].find("..") {
                i = s + p;
            }
            push(&mut out, s, i, Kind::Number);
            defining = None;
            continue;
        }
        if lang.keys == Keys::Css && c == b'#' && !line.contains('{') {
            let n = b[i + 1..].iter().take_while(|x| x.is_ascii_hexdigit()).count();
            if matches!(n, 3 | 4 | 6 | 8) {
                push(&mut out, i, i + 1 + n, Kind::Number);
                i += 1 + n;
                continue;
            }
        }
        if lang.vars && c == b'$' {
            if let Some(end) = var_end(b, i, b.len()) {
                push(&mut out, i, end, Kind::Property);
                i = end;
                continue;
            }
        }
        if lang.decorators && c == b'@' && b.get(i + 1).is_some_and(|x| x.is_ascii_alphabetic() || *x == b'_') {
            let mut j = i + 1;
            while j < b.len() && (is_word(b[j]) || b[j] == b'.') {
                j += 1;
            }
            push(&mut out, i, j, Kind::Meta);
            i = j;
            continue;
        }
        if lang.dashes && c == b'-' && b.get(i + 1).is_some_and(|x| x.is_ascii_digit() || *x == b'.') {
            // A negative number, not a name.
            i += 1;
            continue;
        }
        if word(c) || c == b'#' || c == b'@' || c == b'!' {
            let s = i;
            i += 1;
            while i < b.len() && word(b[i]) {
                i += 1;
            }
            let w = &line[s..i];
            let has = |list: &[&str]| if lang.any_case { list.iter().any(|k| k.eq_ignore_ascii_case(w)) } else { list.contains(&w) };
            if !is_word(c) {
                // `#include`, `@media`, `!important`: a sign that is not part of a name.
                if has(lang.keywords) || has(lang.constants) {
                    push(&mut out, s, i, if has(lang.keywords) { Kind::Keyword } else { Kind::Constant });
                } else {
                    i = s + 1;
                }
                continue;
            }
            let next = line[i..].trim_start();
            let k = if has(lang.keywords) {
                defining = if has(lang.fn_decl) {
                    Some(Kind::Function)
                } else if has(lang.ty_decl) {
                    Some(Kind::Type)
                } else {
                    None
                };
                push(&mut out, s, i, Kind::Keyword);
                continue;
            } else if has(lang.constants) {
                Kind::Constant
            } else if has(lang.types) {
                Kind::Type
            } else if let Some(k) = defining.take() {
                k
            } else if lang.macros && b.get(i) == Some(&b'!') && b.get(i + 1) != Some(&b'=') {
                i += 1;
                Kind::Meta
            } else if lang.caps_types && is_capitalised(w) {
                Kind::Type
            } else if b.get(i) == Some(&b'(') {
                Kind::Function
            } else if lang.members && s > 0 && b[s - 1] == b'.' && (s < 2 || b[s - 2] != b'.') {
                Kind::Property
            } else if lang.markup && st.in_tag && next.starts_with('=') {
                Kind::Property
            } else if lang.keys == Keys::Css && next.starts_with(':') && !next.starts_with("::") && !line.contains('{') {
                Kind::Property
            } else if lang.caps_consts && is_caps(w) {
                Kind::Constant
            } else {
                Kind::Plain
            };
            push(&mut out, s, i, k);
            continue;
        }
        if !c.is_ascii_whitespace() && c != b'<' && c != b'&' && c != b'*' {
            defining = None;
        }
        i += line[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
    }
    out
}

fn push(out: &mut Spans, s: usize, e: usize, k: Kind) {
    if e > s && k != Kind::Plain {
        out.push((s, e, k));
    }
}

/// A string from `s` to `e` (quotes included), its escapes and the variables a shell
/// expands in it picked out.
fn string(lang: &Lang, line: &str, s: usize, e: usize, q: &str, out: &mut Spans) {
    let b = line.as_bytes();
    // Single quotes keep everything as written in shells; Go's backticks are raw.
    let literal = lang.raw.contains(&q);
    let (mut at, mut k) = (s, s);
    while k < e {
        if lang.escapes && !literal && b[k] == b'\\' && k + 1 < e {
            let end = escape_end(line, k).min(e);
            push(out, at, k, Kind::Str);
            push(out, k, end, Kind::Constant);
            (at, k) = (end, end);
            continue;
        }
        if lang.vars && !literal && b[k] == b'$' {
            if let Some(end) = var_end(b, k, e) {
                push(out, at, k, Kind::Str);
                push(out, k, end, Kind::Property);
                (at, k) = (end, end);
                continue;
            }
        }
        k += 1;
    }
    push(out, at, e, Kind::Str);
}

/// The end of the escape at `k` (a backslash): `\n`, `\x41`, `\u{1F600}`, `é`, `\101`.
fn escape_end(line: &str, k: usize) -> usize {
    let b = line.as_bytes();
    let hex = |from: usize, n: usize| from + b[from.min(b.len())..].iter().take(n).take_while(|c| c.is_ascii_hexdigit()).count();
    match b.get(k + 1) {
        Some(b'x') => hex(k + 2, 2),
        Some(b'u') if b.get(k + 2) == Some(&b'{') => b[k + 2..].iter().position(|&c| c == b'}').map_or(k + 2, |p| k + 3 + p),
        Some(b'u') => hex(k + 2, 4),
        Some(b'U') => hex(k + 2, 8),
        Some(b'0'..=b'7') => k + 1 + b[k + 1..].iter().take(3).take_while(|c| (b'0'..=b'7').contains(c)).count(),
        Some(_) => k + 1 + line[k + 1..].chars().next().map_or(1, char::len_utf8),
        None => k + 1,
    }
}

/// The end of a shell variable at `k` (a `$`): `$NAME`, `${NAME}`, `$1`, `$@`.
fn var_end(b: &[u8], k: usize, e: usize) -> Option<usize> {
    match *b.get(k + 1)? {
        b'{' => b[k + 1..e].iter().position(|&c| c == b'}').map(|p| k + 2 + p),
        // Make's `$(NAME)`; a shell's `$(command args)` is not one name.
        b'(' => {
            let n = b[k + 2..e].iter().take_while(|&&c| c.is_ascii_alphanumeric() || c == b'_').count();
            (n > 0 && b.get(k + 2 + n) == Some(&b')')).then_some(k + 3 + n)
        }
        c if c.is_ascii_digit() || b"@#?$!*-".contains(&c) => Some(k + 2),
        c if c.is_ascii_alphabetic() || c == b'_' => Some(k + 1 + b[k + 1..e].iter().take_while(|&&c| c.is_ascii_alphanumeric() || c == b'_').count()),
        _ => None,
    }
}

/// `'x'`, `'\n'`, `'é'`: a character, not a lifetime. Its end, if so.
fn char_literal(line: &str, i: usize) -> Option<usize> {
    let b = line.as_bytes();
    if b.get(i + 1) == Some(&b'\\') {
        return b[i + 2..].iter().take(12).position(|&c| c == b'\'').map(|p| i + 3 + p);
    }
    let c = line.get(i + 1..)?.chars().next()?;
    let end = i + 1 + c.len_utf8();
    (b.get(end) == Some(&b'\'')).then_some(end + 1)
}

/// The end of `[...]` opening at `open`, nested brackets and all, or the line's end.
fn bracket_end(b: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    for (k, &c) in b.iter().enumerate().skip(open) {
        match c {
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return k + 1;
                }
            }
            _ => {}
        }
    }
    b.len()
}

/// A YAML key at the start of a line (after an indent and a `- `), up to its colon.
fn yaml_key(line: &str) -> Option<(usize, usize)> {
    let t = line.trim_start();
    let mut s = line.len() - t.len();
    let t = match t.strip_prefix("- ") {
        Some(r) => {
            s += 2 + (r.len() - r.trim_start().len());
            r.trim_start()
        }
        None => t,
    };
    if t.starts_with(['#', '"', '\'', '{', '[', '-', '|', '>']) {
        return None;
    }
    let colon = t.match_indices(':').map(|(p, _)| p).find(|&p| t[p + 1..].is_empty() || t[p + 1..].starts_with([' ', '\t']))?;
    (colon > 0).then_some((s, s + colon))
}

/// A Markdown list's marker at the start of `t`: `- `, `* `, `+ `, `1. `, `2) `.
fn list_marker(t: &str) -> Option<usize> {
    let b = t.as_bytes();
    if b.len() >= 2 && b"-*+".contains(&b[0]) && b[1] == b' ' {
        return Some(1);
    }
    let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
    (n > 0 && n < 10 && b.len() > n + 1 && (b[n] == b'.' || b[n] == b')') && b[n + 1] == b' ').then_some(n + 1)
}

/// `[text](url)` at `i`: where the text and the address end.
fn md_link(line: &str, i: usize) -> Option<(usize, usize)> {
    let close = i + line[i..].find("](")?;
    let end = close + 2 + line[close + 2..].find(')')?;
    Some((close + 1, end + 1))
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// `Name`, `HashMap`: an upper-case letter first and a lower-case one somewhere after.
fn is_capitalised(w: &str) -> bool {
    w.as_bytes().first().is_some_and(u8::is_ascii_uppercase) && w.bytes().any(|c| c.is_ascii_lowercase())
}

/// `MAX_LEN`, `EPOLLIN`: two or more characters, upper-case letters, digits and `_`.
fn is_caps(w: &str) -> bool {
    w.len() >= 2 && w.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_') && w.bytes().any(|c| c.is_ascii_uppercase())
}

/// End (exclusive) of a string starting at `from` (after its opening quote), honouring
/// backslash escapes for single-character quotes where the language has them.
fn find_close(line: &str, from: usize, q: &str, escapes: bool) -> Option<usize> {
    let b = line.as_bytes();
    let mut i = from;
    while i < b.len() {
        if escapes && q.len() == 1 && b[i] == b'\\' {
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
