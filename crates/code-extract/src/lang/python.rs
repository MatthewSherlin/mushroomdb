//! Python: definitions, imports, and module-path resolution.

use super::{field, field_children, first_doc_line, named_children, signature, text};
use super::{Definition, Spec};
use crate::{first_known, join, parent_dir};
use std::sync::OnceLock;
use tree_sitter::{Language, Node, Query};

const QUERY: &str = r"
(function_definition) @def
(class_definition) @def
(module (expression_statement (assignment) @def))
(import_statement) @import
(import_from_statement) @import
(call) @call
";

pub(crate) struct Python;

impl Spec for Python {
    fn language(&self) -> Language {
        Language::new(tree_sitter_python::LANGUAGE)
    }

    fn query_source(&self) -> &'static str {
        QUERY
    }

    fn cache(&self) -> &'static OnceLock<Option<Query>> {
        static CACHE: OnceLock<Option<Query>> = OnceLock::new();
        &CACHE
    }

    fn definition(&self, node: Node, src: &str) -> Option<Definition> {
        let (name, kind) = match node.kind() {
            "function_definition" => {
                let name = field(node, "name", src)?.to_string();
                let kind = if enclosing_class(node, src).is_some() {
                    "method"
                } else {
                    "function"
                };
                (name, kind)
            }
            "class_definition" => (field(node, "name", src)?.to_string(), "class"),
            // Module-level `NAME = …` in caps is the closest thing Python has
            // to a declared constant; anything else is ordinary state.
            "assignment" => {
                let name = field(node, "left", src)?.trim().to_string();
                if !is_constant_name(&name) {
                    return None;
                }
                (name, "const")
            }
            _ => return None,
        };
        let qualified = match enclosing_class(node, src) {
            Some(class) => format!("{class}.{name}"),
            None => name,
        };
        Some(Definition {
            name: qualified,
            kind,
            signature: signature(node, src),
            doc: docstring(node, src),
        })
    }

    fn imports(&self, node: Node, src: &str) -> Vec<String> {
        match node.kind() {
            "import_statement" => field_children(node, "name")
                .into_iter()
                .filter_map(|child| module_of(child, src))
                .collect(),
            "import_from_statement" => {
                let Some(module) = node.child_by_field_name("module_name") else {
                    return Vec::new();
                };
                let module = text(module, src).trim().to_string();
                if module.is_empty() {
                    return Vec::new();
                }
                // `from . import sibling` names no module of its own, so the
                // imported name is what identifies the file.
                if module.chars().all(|c| c == '.') {
                    return field_children(node, "name")
                        .into_iter()
                        .filter_map(|child| module_of(child, src))
                        .map(|name| format!("{module}{name}"))
                        .collect();
                }
                vec![module]
            }
            _ => Vec::new(),
        }
    }

    fn callee(&self, node: Node, src: &str) -> Option<String> {
        let function = node.child_by_field_name("function")?;
        let callee = text(function, src).trim();
        (!callee.is_empty() && !callee.contains('\n')).then(|| callee.to_string())
    }
}

/// `pkg.mod` from a `dotted_name`, or the original name of an alias.
fn module_of(node: Node, src: &str) -> Option<String> {
    let node = if node.kind() == "aliased_import" {
        node.child_by_field_name("name")
            .or_else(|| named_children(node).into_iter().next())?
    } else {
        node
    };
    let name = text(node, src).trim().to_string();
    (!name.is_empty() && name != "*").then_some(name)
}

fn is_constant_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && name.chars().any(|c| c.is_ascii_uppercase())
}

/// The name of the innermost enclosing class, if any.
fn enclosing_class(node: Node, src: &str) -> Option<String> {
    let mut cursor = node.parent();
    while let Some(parent) = cursor {
        if parent.kind() == "class_definition" {
            return field(parent, "name", src).map(str::to_string);
        }
        cursor = parent.parent();
    }
    None
}

/// The first line of the leading string literal in the definition's body.
fn docstring(node: Node, src: &str) -> String {
    let Some(body) = node.child_by_field_name("body") else {
        return String::new();
    };
    let Some(first) = named_children(body).into_iter().next() else {
        return String::new();
    };
    let statement = if first.kind() == "expression_statement" {
        named_children(first).into_iter().next()
    } else {
        Some(first)
    };
    let Some(literal) = statement.filter(|n| n.kind() == "string") else {
        return String::new();
    };
    let raw = text(literal, src);
    let trimmed = raw
        .trim()
        .trim_start_matches(['r', 'b', 'u', 'f', 'R', 'B', 'U', 'F'])
        .trim_matches('"')
        .trim_matches('\'');
    first_doc_line(trimmed)
}

// ── resolution ──────────────────────────────────────────────────────────────

/// Resolve a Python module path. See [`crate::resolve_import`].
pub(crate) fn resolve_import(from: &str, raw: &str, known: &dyn Fn(&str) -> bool) -> Vec<String> {
    let dots = raw.chars().take_while(|c| *c == '.').count();
    let rest: Vec<&str> = raw
        .get(dots..)
        .unwrap_or("")
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect();

    let bases = if dots == 0 {
        if rest.first().is_some_and(|head| is_stdlib(head)) {
            return Vec::new();
        }
        vec![parent_dir(from).to_string(), String::new()]
    } else {
        let mut base = parent_dir(from).to_string();
        for _ in 1..dots {
            base = parent_dir(&base).to_string();
        }
        vec![base]
    };

    let mut candidates = Vec::new();
    for base in &bases {
        if rest.is_empty() {
            candidates.push(join(base, "__init__.py"));
            continue;
        }
        for take in (1..=rest.len()).rev() {
            let path = join(base, &rest[..take].join("/"));
            candidates.push(format!("{path}.py"));
            candidates.push(format!("{path}/__init__.py"));
        }
    }
    first_known(&candidates, known)
}

/// True when `name` is a top-level standard-library module.
fn is_stdlib(name: &str) -> bool {
    STDLIB.binary_search(&name).is_ok()
}

/// Snapshot of `sys.stdlib_module_names` (CPython 3.14), private names
/// removed. Embedded rather than probed so resolution never depends on which
/// interpreter happens to be installed.
const STDLIB: &[&str] = &[
    "abc",
    "annotationlib",
    "antigravity",
    "argparse",
    "array",
    "ast",
    "asyncio",
    "atexit",
    "base64",
    "bdb",
    "binascii",
    "bisect",
    "builtins",
    "bz2",
    "cProfile",
    "calendar",
    "cmath",
    "cmd",
    "code",
    "codecs",
    "codeop",
    "collections",
    "colorsys",
    "compileall",
    "compression",
    "concurrent",
    "configparser",
    "contextlib",
    "contextvars",
    "copy",
    "copyreg",
    "csv",
    "ctypes",
    "curses",
    "dataclasses",
    "datetime",
    "dbm",
    "decimal",
    "difflib",
    "dis",
    "doctest",
    "email",
    "encodings",
    "ensurepip",
    "enum",
    "errno",
    "faulthandler",
    "fcntl",
    "filecmp",
    "fileinput",
    "fnmatch",
    "fractions",
    "ftplib",
    "functools",
    "gc",
    "genericpath",
    "getopt",
    "getpass",
    "gettext",
    "glob",
    "graphlib",
    "grp",
    "gzip",
    "hashlib",
    "heapq",
    "hmac",
    "html",
    "http",
    "idlelib",
    "imaplib",
    "importlib",
    "inspect",
    "io",
    "ipaddress",
    "itertools",
    "json",
    "keyword",
    "linecache",
    "locale",
    "logging",
    "lzma",
    "mailbox",
    "marshal",
    "math",
    "mimetypes",
    "mmap",
    "modulefinder",
    "msvcrt",
    "multiprocessing",
    "netrc",
    "nt",
    "ntpath",
    "nturl2path",
    "numbers",
    "opcode",
    "operator",
    "optparse",
    "os",
    "pathlib",
    "pdb",
    "pickle",
    "pickletools",
    "pkgutil",
    "platform",
    "plistlib",
    "poplib",
    "posix",
    "posixpath",
    "pprint",
    "profile",
    "pstats",
    "pty",
    "pwd",
    "py_compile",
    "pyclbr",
    "pydoc",
    "pydoc_data",
    "pyexpat",
    "queue",
    "quopri",
    "random",
    "re",
    "readline",
    "reprlib",
    "resource",
    "rlcompleter",
    "runpy",
    "sched",
    "secrets",
    "select",
    "selectors",
    "shelve",
    "shlex",
    "shutil",
    "signal",
    "site",
    "smtplib",
    "socket",
    "socketserver",
    "sqlite3",
    "sre_compile",
    "sre_constants",
    "sre_parse",
    "ssl",
    "stat",
    "statistics",
    "string",
    "stringprep",
    "struct",
    "subprocess",
    "symtable",
    "sys",
    "sysconfig",
    "syslog",
    "tabnanny",
    "tarfile",
    "tempfile",
    "termios",
    "textwrap",
    "this",
    "threading",
    "time",
    "timeit",
    "tkinter",
    "token",
    "tokenize",
    "tomllib",
    "trace",
    "traceback",
    "tracemalloc",
    "tty",
    "turtle",
    "turtledemo",
    "types",
    "typing",
    "unicodedata",
    "unittest",
    "urllib",
    "uuid",
    "venv",
    "warnings",
    "wave",
    "weakref",
    "webbrowser",
    "winreg",
    "winsound",
    "wsgiref",
    "xml",
    "xmlrpc",
    "zipapp",
    "zipfile",
    "zipimport",
    "zlib",
    "zoneinfo",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stdlib_snapshot_is_sorted_for_binary_search() {
        assert!(STDLIB.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(is_stdlib("os"));
        assert!(is_stdlib("zoneinfo"));
        assert!(!is_stdlib("pkg"));
    }
}
