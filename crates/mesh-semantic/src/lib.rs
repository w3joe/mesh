use mesh_proto::{
    ChangeOperation, FileSemanticDelta, ObjectId, SemanticConflict, SemanticConflictKind,
    SymbolDelta, SymbolKind, SymbolSpan,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use tree_sitter::{Node, Parser};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CstMergeOutcome {
    /// Orthogonal symbol edits were spliced verbatim without conflict.
    Clean {
        merged_source: String,
        merged_symbols: Vec<String>,
    },
    /// True semantic/structural conflict on one or more AST symbols.
    Conflict {
        conflicts: Vec<SemanticConflict>,
        algebraic_conflict_preview: String,
    },
}

/// Detects source language from file extension.
pub fn detect_language(file_path: &str) -> Option<&'static str> {
    if file_path.ends_with(".rs") {
        Some("rust")
    } else if file_path.ends_with(".tsx") {
        Some("tsx")
    } else if file_path.ends_with(".ts") {
        Some("typescript")
    } else if file_path.ends_with(".js") || file_path.ends_with(".jsx") {
        Some("javascript")
    } else if file_path.ends_with(".py") {
        Some("python")
    } else if file_path.ends_with(".go") {
        Some("go")
    } else {
        None
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(&hasher.finalize()[..16])
}

/// Parses a source file into Concrete Syntax Tree (CST) top-level declaration spans using
/// `tree-sitter` (for Rust, TypeScript/JS, Python) or a string-aware lexical scanner (for Go).
///
/// Guarantees:
/// - Rejects any file containing syntax errors (`root.has_error()`).
/// - Attaches leading doc-comments and attributes (`///`, `#[...]`) to the declaration they document.
/// - Captures `mod`, `static`, `macro_rules!`, and inner attributes (`#![...]`) so zero top-level
///   statements are ever lost during CST reconstruction.
pub fn parse_cst_spans(file_path: &str, source: &str) -> Result<Vec<SymbolSpan>, String> {
    let lang = detect_language(file_path).ok_or_else(|| "unsupported language".to_string())?;
    match lang {
        "rust" | "typescript" | "tsx" | "javascript" | "python" => {
            parse_with_tree_sitter(lang, source)
        }
        "go" => parse_go_lexical(source),
        _ => Err("unsupported language".to_string()),
    }
}

fn parse_with_tree_sitter(lang: &str, source: &str) -> Result<Vec<SymbolSpan>, String> {
    let mut parser = Parser::new();
    let ts_lang = match lang {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "typescript" | "javascript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        _ => return Err("unsupported tree-sitter language".to_string()),
    };
    parser
        .set_language(&ts_lang)
        .map_err(|e| format!("failed to load tree-sitter grammar: {e}"))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "tree-sitter failed to parse source".to_string())?;
    let root = tree.root_node();

    if root.has_error() {
        return Err("syntax error detected by tree-sitter CST parser".to_string());
    }

    let mut spans = Vec::new();
    let mut cursor = root.walk();
    let children: Vec<Node> = root.children(&mut cursor).collect();

    let mut pending_start_byte: Option<usize> = None;
    let mut pending_start_line: Option<usize> = None;

    for child in children {
        let kind_str = child.kind();

        // Attach leading comments and outer attributes (`///`, `//`, `/* */`, `#[...]`) to the next item
        if matches!(
            kind_str,
            "line_comment" | "block_comment" | "comment" | "attribute_item"
        ) {
            if pending_start_byte.is_none() {
                pending_start_byte = Some(child.start_byte());
                pending_start_line = Some(child.start_position().row + 1);
            }
            continue;
        }

        let start_byte = pending_start_byte.take().unwrap_or_else(|| child.start_byte());
        let start_line = pending_start_line
            .take()
            .unwrap_or_else(|| child.start_position().row + 1);
        let end_byte = enforce_line_end_boundary(source, child.end_byte());
        let end_line = child.end_position().row + 1;

        let raw_with_trivia = &source[start_byte..end_byte];
        let decl_slice = &source[child.start_byte()..end_byte];

        let (sym_kind, sym_name, signature) = classify_ts_node(lang, child, source, decl_slice);
        let mut referenced_symbols = BTreeSet::new();
        collect_called_identifiers_ast(child, source.as_bytes(), &sym_name, &mut referenced_symbols);

        spans.push(SymbolSpan {
            name: sym_name,
            kind: sym_kind,
            signature,
            start_byte,
            end_byte,
            start_line,
            end_line,
            body_hash: hash_bytes(raw_with_trivia.trim().as_bytes()),
            referenced_symbols: referenced_symbols.into_iter().collect(),
        });
    }

    // If there are trailing comments at the end of the file not attached to a declaration, preserve them
    if let (Some(sb), Some(sl)) = (pending_start_byte, pending_start_line) {
        let raw = &source[sb..];
        if !raw.trim().is_empty() {
            spans.push(SymbolSpan {
                name: format!("__trailing_comment_{sl}"),
                kind: SymbolKind::VerbatimBlock,
                signature: raw.trim().to_string(),
                start_byte: sb,
                end_byte: source.len(),
                start_line: sl,
                end_line: source.lines().count().max(sl),
                body_hash: hash_bytes(raw.trim().as_bytes()),
                referenced_symbols: vec![],
            });
        }
    }

    Ok(spans)
}

fn enforce_line_end_boundary(source: &str, byte_idx: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = byte_idx.min(bytes.len());
    while i < bytes.len() && (bytes[i] == b';' || bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\r') {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'\n' {
        i += 1;
    }
    i
}

fn classify_ts_node(
    lang: &str,
    node: Node,
    source: &str,
    decl_slice: &str,
) -> (SymbolKind, String, String) {
    let kind_str = node.kind();

    // Unwrap TypeScript/JS `export_statement` around a declaration
    if kind_str == "export_statement" {
        let mut cursor = node.walk();
        for sub in node.children(&mut cursor) {
            if sub.kind() != "export" && sub.kind() != "default" && sub.kind() != "decorator" {
                let (k, n, _) = classify_ts_node(lang, sub, source, decl_slice);
                if k != SymbolKind::VerbatimBlock {
                    let sig = extract_signature(lang, k, decl_slice);
                    return (k, n, sig);
                }
            }
        }
    }

    // Unwrap Python `decorated_definition`
    if kind_str == "decorated_definition" {
        if let Some(def_node) = node.child_by_field_name("definition") {
            let (k, n, _) = classify_ts_node(lang, def_node, source, decl_slice);
            let sig = extract_signature(lang, k, decl_slice);
            return (k, n, sig);
        }
    }

    let sym_kind = match kind_str {
        "use_declaration" | "import_statement" | "import_from_statement" => SymbolKind::Import,
        "function_item" | "function_declaration" | "function_definition" | "generator_function_declaration" => {
            SymbolKind::Function
        }
        "struct_item" => SymbolKind::Struct,
        "enum_item" | "enum_declaration" => SymbolKind::Enum,
        "trait_item" => SymbolKind::Trait,
        "impl_item" => SymbolKind::Impl,
        "class_declaration" | "class_definition" => SymbolKind::Class,
        "interface_declaration" => SymbolKind::Interface,
        "type_item" | "type_alias_declaration" => SymbolKind::TypeAlias,
        "const_item" => SymbolKind::Const,
        "static_item" => SymbolKind::Static,
        "mod_item" | "internal_module" => SymbolKind::Module,
        "macro_definition" | "macro_invocation" => SymbolKind::Macro,
        "lexical_declaration" | "variable_declaration" => SymbolKind::Const,
        "inner_attribute_item" => SymbolKind::VerbatimBlock,
        _ => SymbolKind::VerbatimBlock,
    };

    let sig = extract_signature(lang, sym_kind, decl_slice);

    let sym_name = match sym_kind {
        SymbolKind::Import => decl_slice
            .trim()
            .trim_end_matches(';')
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        SymbolKind::Impl => {
            // Key impl blocks by their full header before `{` (e.g. `impl Display for User` vs `impl User`)
            decl_slice
                .split('{')
                .next()
                .unwrap_or(decl_slice)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        }
        SymbolKind::VerbatimBlock => {
            decl_slice
                .trim()
                .lines()
                .next()
                .unwrap_or("block")
                .trim()
                .to_string()
        }
        _ => {
            if let Some(name_node) = node.child_by_field_name("name") {
                source[name_node.start_byte()..name_node.end_byte()].to_string()
            } else {
                extract_first_identifier(node, source)
                    .unwrap_or_else(|| sig.clone())
            }
        }
    };

    (sym_kind, sym_name, sig)
}

fn extract_first_identifier(node: Node, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "identifier" | "type_identifier" | "property_identifier"
        ) {
            return Some(source[child.start_byte()..child.end_byte()].to_string());
        }
        if matches!(child.kind(), "variable_declarator" | "assignment") {
            if let Some(id) = extract_first_identifier(child, source) {
                return Some(id);
            }
        }
    }
    None
}

fn collect_called_identifiers_ast(
    node: Node,
    source: &[u8],
    own_name: &str,
    out: &mut BTreeSet<String>,
) {
    let kind = node.kind();
    if matches!(kind, "call_expression" | "call") {
        if let Some(func_node) = node.child_by_field_name("function") {
            let ident = extract_call_target_name(func_node, source);
            if !ident.is_empty() && ident != own_name {
                out.insert(ident);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_called_identifiers_ast(child, source, own_name, out);
    }
}

fn extract_call_target_name(func_node: Node, source: &[u8]) -> String {
    match func_node.kind() {
        "identifier" | "field_identifier" | "property_identifier" => {
            String::from_utf8_lossy(&source[func_node.start_byte()..func_node.end_byte()])
                .to_string()
        }
        "scoped_identifier" | "field_expression" | "member_expression" | "attribute" => {
            if let Some(name_child) = func_node
                .child_by_field_name("name")
                .or_else(|| func_node.child_by_field_name("field"))
                .or_else(|| func_node.child_by_field_name("property"))
                .or_else(|| func_node.child_by_field_name("attribute"))
            {
                String::from_utf8_lossy(&source[name_child.start_byte()..name_child.end_byte()])
                    .to_string()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// String- and comment-aware lexical fallback for Go (`.go`) files.
fn parse_go_lexical(source: &str) -> Result<Vec<SymbolSpan>, String> {
    if !is_brace_balanced_lexically(source) {
        return Err("syntax error: unbalanced braces in Go source".to_string());
    }
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut offsets = Vec::with_capacity(lines.len() + 1);
    let mut acc = 0usize;
    for l in &lines {
        offsets.push(acc);
        acc += l.len();
    }
    offsets.push(acc);

    let mut spans = Vec::new();
    let mut idx = 0usize;
    let mut pending_start: Option<usize> = None;

    while idx < lines.len() {
        let trimmed = lines[idx].trim();
        if trimmed.is_empty() {
            pending_start = None;
            idx += 1;
            continue;
        }
        if trimmed.starts_with("//") || trimmed.starts_with("/*") {
            if pending_start.is_none() {
                pending_start = Some(idx);
            }
            idx += 1;
            continue;
        }

        let start_line = pending_start.take().unwrap_or(idx);
        let end_line = find_brace_end_line_lexically(&lines, idx);
        let start_byte = offsets[start_line];
        let end_byte = offsets[(end_line + 1).min(lines.len())];
        let decl_slice = &source[offsets[idx]..end_byte];
        let raw_slice = &source[start_byte..end_byte];

        let (kind, name) = if trimmed.starts_with("import ") {
            (SymbolKind::Import, trimmed.to_string())
        } else if let Some(rest) = trimmed.strip_prefix("func ") {
            let ident = rest
                .trim_start_matches(|c: char| c == '(' || c != ')')
                .trim_start_matches(')')
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>();
            (
                SymbolKind::Function,
                if ident.is_empty() {
                    trimmed.to_string()
                } else {
                    ident
                },
            )
        } else if let Some(rest) = trimmed.strip_prefix("type ") {
            let ident: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            (SymbolKind::Struct, ident)
        } else {
            (SymbolKind::VerbatimBlock, trimmed.to_string())
        };

        let signature = extract_signature("go", kind, decl_slice);
        spans.push(SymbolSpan {
            name,
            kind,
            signature,
            start_byte,
            end_byte,
            start_line: start_line + 1,
            end_line: end_line + 1,
            body_hash: hash_bytes(raw_slice.trim().as_bytes()),
            referenced_symbols: vec![],
        });
        idx = end_line + 1;
    }
    Ok(spans)
}

/// Counts braces while ignoring `{` and `}` inside `//` comments, `/* */` comments,
/// `"..."` string literals, and `'...'` char literals.
pub fn is_brace_balanced_lexically(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut i = 0usize;
    let mut depth = 0i64;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if b == b'"' || b == b'\'' || b == b'`' {
            let quote = b;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && quote != b'`' {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else {
            if b == b'{' {
                depth += 1;
            } else if b == b'}' {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            i += 1;
        }
    }
    depth == 0
}

fn find_brace_end_line_lexically(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut saw_open = false;
    for (idx, line) in lines.iter().enumerate().skip(start) {
        let bytes = line.as_bytes();
        let mut k = 0usize;
        while k < bytes.len() {
            if bytes[k] == b'/' && k + 1 < bytes.len() && bytes[k + 1] == b'/' {
                break;
            }
            if bytes[k] == b'"' || bytes[k] == b'\'' || bytes[k] == b'`' {
                let q = bytes[k];
                k += 1;
                while k < bytes.len() {
                    if bytes[k] == b'\\' && q != b'`' {
                        k += 2;
                        continue;
                    }
                    if bytes[k] == q {
                        k += 1;
                        break;
                    }
                    k += 1;
                }
                continue;
            }
            if bytes[k] == b'{' {
                depth += 1;
                saw_open = true;
            } else if bytes[k] == b'}' {
                depth -= 1;
            }
            k += 1;
        }
        if saw_open && depth <= 0 {
            return idx;
        }
        if !saw_open && depth <= 0 {
            return idx;
        }
    }
    lines.len().saturating_sub(1)
}

fn extract_signature(lang: &str, kind: SymbolKind, decl_slice: &str) -> String {
    if matches!(kind, SymbolKind::Import | SymbolKind::VerbatimBlock | SymbolKind::Module) {
        return decl_slice.trim().trim_end_matches(';').to_string();
    }
    if lang == "python" {
        if let Some((head, _)) = decl_slice.split_once(':') {
            return head.split_whitespace().collect::<Vec<_>>().join(" ");
        }
    }
    if let Some((head, _)) = decl_slice.split_once('{') {
        return head.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    decl_slice
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Computes the structured `FileSemanticDelta` between `old_source` and `new_source`.
pub fn compute_file_semantic_delta(
    file_path: &str,
    old_blob_oid: ObjectId,
    new_blob_oid: ObjectId,
    old_source: &str,
    new_source: &str,
) -> Option<FileSemanticDelta> {
    let lang = detect_language(file_path)?;
    let old_spans = parse_cst_spans(file_path, old_source).ok()?;
    let new_spans = parse_cst_spans(file_path, new_source).ok()?;

    let old_map: BTreeMap<(SymbolKind, String), &SymbolSpan> = old_spans
        .iter()
        .map(|s| ((s.kind, s.name.clone()), s))
        .collect();
    let new_map: BTreeMap<(SymbolKind, String), &SymbolSpan> = new_spans
        .iter()
        .map(|s| ((s.kind, s.name.clone()), s))
        .collect();

    let mut deltas = Vec::new();
    let mut imports = Vec::new();

    for (key, new_s) in &new_map {
        if new_s.kind == SymbolKind::Import {
            imports.push(new_s.name.clone());
            continue;
        }
        if new_s.kind == SymbolKind::VerbatimBlock {
            continue;
        }
        match old_map.get(key) {
            None => {
                deltas.push(SymbolDelta {
                    symbol_name: new_s.name.clone(),
                    kind: new_s.kind,
                    op: ChangeOperation::Inserted {
                        signature: new_s.signature.clone(),
                    },
                    referenced_symbols: new_s.referenced_symbols.clone(),
                });
            }
            Some(old_s) => {
                if old_s.body_hash != new_s.body_hash {
                    deltas.push(SymbolDelta {
                        symbol_name: new_s.name.clone(),
                        kind: new_s.kind,
                        op: ChangeOperation::Modified {
                            old_signature: old_s.signature.clone(),
                            new_signature: new_s.signature.clone(),
                            signature_changed: old_s.signature != new_s.signature,
                        },
                        referenced_symbols: new_s.referenced_symbols.clone(),
                    });
                }
            }
        }
    }

    for (key, old_s) in &old_map {
        if matches!(old_s.kind, SymbolKind::Import | SymbolKind::VerbatimBlock) {
            continue;
        }
        if !new_map.contains_key(key) {
            deltas.push(SymbolDelta {
                symbol_name: old_s.name.clone(),
                kind: old_s.kind,
                op: ChangeOperation::Deleted {
                    old_signature: old_s.signature.clone(),
                },
                referenced_symbols: vec![],
            });
        }
    }

    Some(FileSemanticDelta {
        file_path: file_path.to_string(),
        language: lang.to_string(),
        old_blob_oid,
        new_blob_oid,
        symbol_deltas: deltas,
        imports,
    })
}

/// Verbatim CST Byte-Span Commutative 3-Way Merger powered by `tree-sitter`.
///
/// Key properties:
/// 1. Never pretty-prints ASTs: splices exact byte slices from the original source buffers.
/// 2. Preserves leading doc-comments, attributes (`#[...]`), inner attributes (`#![...]`),
///    `mod` items, `static` items, and `macro_rules!` blocks without dropping any top-level code.
/// 3. Keys declarations by `(SymbolKind, canonical_name)` so `struct Foo` and multiple
///    `impl` blocks (`impl Foo` vs `impl Display for Foo`) never collide.
pub fn cst_merge_3way(
    file_path: &str,
    branch_a_name: &str,
    branch_b_name: &str,
    base_src: &str,
    branch_a_src: &str,
    branch_b_src: &str,
) -> CstMergeOutcome {
    if branch_a_src == branch_b_src {
        return CstMergeOutcome::Clean {
            merged_source: branch_a_src.to_string(),
            merged_symbols: vec![],
        };
    }
    if branch_a_src == base_src {
        return CstMergeOutcome::Clean {
            merged_source: branch_b_src.to_string(),
            merged_symbols: vec![],
        };
    }
    if branch_b_src == base_src {
        return CstMergeOutcome::Clean {
            merged_source: branch_a_src.to_string(),
            merged_symbols: vec![],
        };
    }

    let (Ok(base_spans), Ok(a_spans), Ok(b_spans)) = (
        parse_cst_spans(file_path, base_src),
        parse_cst_spans(file_path, branch_a_src),
        parse_cst_spans(file_path, branch_b_src),
    ) else {
        return CstMergeOutcome::Conflict {
            conflicts: vec![SemanticConflict {
                file_path: file_path.to_string(),
                symbol_name: "<file>".to_string(),
                branch_a: branch_a_name.to_string(),
                branch_b: branch_b_name.to_string(),
                kind: SemanticConflictKind::UnstructuredOverlapFallback,
                description: "File could not be decomposed into error-free CST spans (syntax error or unsupported format)".to_string(),
            }],
            algebraic_conflict_preview: format_algebraic_conflict(
                file_path,
                branch_a_name,
                branch_b_name,
                branch_a_src,
                branch_b_src,
            ),
        };
    };

    if base_spans.is_empty() && a_spans.is_empty() && b_spans.is_empty() {
        return CstMergeOutcome::Conflict {
            conflicts: vec![SemanticConflict {
                file_path: file_path.to_string(),
                symbol_name: "<unstructured>".to_string(),
                branch_a: branch_a_name.to_string(),
                branch_b: branch_b_name.to_string(),
                kind: SemanticConflictKind::UnstructuredOverlapFallback,
                description: "No CST declarations found in file".to_string(),
            }],
            algebraic_conflict_preview: format_algebraic_conflict(
                file_path,
                branch_a_name,
                branch_b_name,
                branch_a_src,
                branch_b_src,
            ),
        };
    }

    type DeclKey = (SymbolKind, String);

    let base_map: BTreeMap<DeclKey, &SymbolSpan> = base_spans
        .iter()
        .filter(|s| s.kind != SymbolKind::Import)
        .map(|s| ((s.kind, s.name.clone()), s))
        .collect();
    let a_map: BTreeMap<DeclKey, &SymbolSpan> = a_spans
        .iter()
        .filter(|s| s.kind != SymbolKind::Import)
        .map(|s| ((s.kind, s.name.clone()), s))
        .collect();
    let b_map: BTreeMap<DeclKey, &SymbolSpan> = b_spans
        .iter()
        .filter(|s| s.kind != SymbolKind::Import)
        .map(|s| ((s.kind, s.name.clone()), s))
        .collect();

    let mut conflicts = Vec::new();
    let mut merged_symbols = Vec::new();

    // 1. Collect & deduplicate imports in deterministic order
    let mut import_slices: Vec<String> = Vec::new();
    let mut seen_imports: HashSet<String> = HashSet::new();
    let base_deleted_imports: HashSet<String> = base_spans
        .iter()
        .filter(|s| s.kind == SymbolKind::Import)
        .filter(|s| {
            !a_spans.iter().any(|x| x.kind == SymbolKind::Import && x.name == s.name)
                || !b_spans.iter().any(|x| x.kind == SymbolKind::Import && x.name == s.name)
        })
        .map(|s| s.name.clone())
        .collect();

    for (src, spans) in [
        (base_src, &base_spans),
        (branch_a_src, &a_spans),
        (branch_b_src, &b_spans),
    ] {
        for sp in spans.iter().filter(|s| s.kind == SymbolKind::Import) {
            if !base_deleted_imports.contains(&sp.name) && seen_imports.insert(sp.name.clone()) {
                let raw = src[sp.start_byte..sp.end_byte].trim_end().to_string();
                import_slices.push(raw);
            }
        }
    }

    // 2. Determine declaration order: base order -> new in A -> new in B
    let mut ordered_keys: Vec<DeclKey> = Vec::new();
    let mut seen_keys: HashSet<DeclKey> = HashSet::new();
    for sp in base_spans
        .iter()
        .chain(a_spans.iter())
        .chain(b_spans.iter())
        .filter(|s| s.kind != SymbolKind::Import)
    {
        let key = (sp.kind, sp.name.clone());
        if seen_keys.insert(key.clone()) {
            ordered_keys.push(key);
        }
    }

    let mut header_verbatim_blocks = Vec::new();
    let mut merged_blocks = Vec::new();

    for key in &ordered_keys {
        let name = &key.1;
        let base_sp = base_map.get(key).copied();
        let a_sp = a_map.get(key).copied();
        let b_sp = b_map.get(key).copied();

        let target_vec = if key.0 == SymbolKind::VerbatimBlock && name.starts_with("#![") {
            &mut header_verbatim_blocks
        } else {
            &mut merged_blocks
        };

        match (base_sp, a_sp, b_sp) {
            (Some(base), Some(a), Some(b)) => {
                let a_changed = a.body_hash != base.body_hash;
                let b_changed = b.body_hash != base.body_hash;
                if !a_changed && !b_changed {
                    target_vec.push(base_src[base.start_byte..base.end_byte].trim_end().to_string());
                } else if a_changed && !b_changed {
                    merged_symbols.push(name.clone());
                    target_vec.push(branch_a_src[a.start_byte..a.end_byte].trim_end().to_string());
                } else if !a_changed && b_changed {
                    merged_symbols.push(name.clone());
                    target_vec.push(branch_b_src[b.start_byte..b.end_byte].trim_end().to_string());
                } else if a.body_hash == b.body_hash {
                    merged_symbols.push(name.clone());
                    target_vec.push(branch_a_src[a.start_byte..a.end_byte].trim_end().to_string());
                } else {
                    conflicts.push(SemanticConflict {
                        file_path: file_path.to_string(),
                        symbol_name: name.clone(),
                        branch_a: branch_a_name.to_string(),
                        branch_b: branch_b_name.to_string(),
                        kind: SemanticConflictKind::SameSymbolModified,
                        description: format!(
                            "Both '{branch_a_name}' and '{branch_b_name}' modified AST symbol '{name}' in '{file_path}'"
                        ),
                    });
                }
            }
            (None, Some(a), None) => {
                merged_symbols.push(name.clone());
                target_vec.push(branch_a_src[a.start_byte..a.end_byte].trim_end().to_string());
            }
            (None, None, Some(b)) => {
                merged_symbols.push(name.clone());
                target_vec.push(branch_b_src[b.start_byte..b.end_byte].trim_end().to_string());
            }
            (None, Some(a), Some(b)) => {
                if a.body_hash == b.body_hash {
                    merged_symbols.push(name.clone());
                    target_vec.push(branch_a_src[a.start_byte..a.end_byte].trim_end().to_string());
                } else {
                    conflicts.push(SemanticConflict {
                        file_path: file_path.to_string(),
                        symbol_name: name.clone(),
                        branch_a: branch_a_name.to_string(),
                        branch_b: branch_b_name.to_string(),
                        kind: SemanticConflictKind::SameSymbolModified,
                        description: format!(
                            "Both '{branch_a_name}' and '{branch_b_name}' inserted different definitions of symbol '{name}'"
                        ),
                    });
                }
            }
            (Some(base), Some(a), None) => {
                if a.body_hash != base.body_hash {
                    conflicts.push(SemanticConflict {
                        file_path: file_path.to_string(),
                        symbol_name: name.clone(),
                        branch_a: branch_a_name.to_string(),
                        branch_b: branch_b_name.to_string(),
                        kind: SemanticConflictKind::SameSymbolModified,
                        description: format!(
                            "'{branch_a_name}' modified symbol '{name}' while '{branch_b_name}' deleted it"
                        ),
                    });
                } else {
                    merged_symbols.push(name.clone());
                }
            }
            (Some(base), None, Some(b)) => {
                if b.body_hash != base.body_hash {
                    conflicts.push(SemanticConflict {
                        file_path: file_path.to_string(),
                        symbol_name: name.clone(),
                        branch_a: branch_a_name.to_string(),
                        branch_b: branch_b_name.to_string(),
                        kind: SemanticConflictKind::SameSymbolModified,
                        description: format!(
                            "'{branch_a_name}' deleted symbol '{name}' while '{branch_b_name}' modified it"
                        ),
                    });
                } else {
                    merged_symbols.push(name.clone());
                }
            }
            (Some(_), None, None) | (None, None, None) => {}
        }
    }

    if !conflicts.is_empty() {
        return CstMergeOutcome::Conflict {
            conflicts,
            algebraic_conflict_preview: format_algebraic_conflict(
                file_path,
                branch_a_name,
                branch_b_name,
                branch_a_src,
                branch_b_src,
            ),
        };
    }

    let mut out = String::new();
    if !header_verbatim_blocks.is_empty() {
        out.push_str(&header_verbatim_blocks.join("\n"));
        out.push_str("\n\n");
    }
    if !import_slices.is_empty() {
        out.push_str(&import_slices.join("\n"));
        out.push_str("\n\n");
    }
    out.push_str(&merged_blocks.join("\n\n"));
    out.push('\n');

    CstMergeOutcome::Clean {
        merged_source: out,
        merged_symbols,
    }
}

fn format_algebraic_conflict(
    file_path: &str,
    branch_a: &str,
    branch_b: &str,
    a_src: &str,
    b_src: &str,
) -> String {
    format!(
        "<<<<<<< [Mesh Algebraic Conflict in {file_path}]\n+++++++ {branch_a}\n{a_src}=======\n+++++++ {branch_b}\n{b_src}>>>>>>>\n"
    )
}

/// Cross-File & Intra-File Semantic Conflict Radar.
pub fn detect_semantic_conflicts(
    branch_a_name: &str,
    branch_a_deltas: &[FileSemanticDelta],
    branch_b_name: &str,
    branch_b_deltas: &[FileSemanticDelta],
) -> Vec<SemanticConflict> {
    let mut conflicts = Vec::new();

    for fa in branch_a_deltas {
        for fb in branch_b_deltas {
            if fa.file_path == fb.file_path {
                for sa in &fa.symbol_deltas {
                    for sb in &fb.symbol_deltas {
                        if sa.kind == sb.kind && sa.symbol_name == sb.symbol_name {
                            conflicts.push(SemanticConflict {
                                file_path: fa.file_path.clone(),
                                symbol_name: sa.symbol_name.clone(),
                                branch_a: branch_a_name.to_string(),
                                branch_b: branch_b_name.to_string(),
                                kind: SemanticConflictKind::SameSymbolModified,
                                description: format!(
                                    "Both '{branch_a_name}' and '{branch_b_name}' mutate symbol '{}' in '{}'",
                                    sa.symbol_name, fa.file_path
                                ),
                            });
                        }
                    }
                }
            }
        }
    }

    check_directional_signature_conflicts(
        branch_a_name,
        branch_a_deltas,
        branch_b_name,
        branch_b_deltas,
        &mut conflicts,
    );
    check_directional_signature_conflicts(
        branch_b_name,
        branch_b_deltas,
        branch_a_name,
        branch_a_deltas,
        &mut conflicts,
    );

    conflicts
}

fn check_directional_signature_conflicts(
    def_branch: &str,
    def_deltas: &[FileSemanticDelta],
    caller_branch: &str,
    caller_deltas: &[FileSemanticDelta],
    out: &mut Vec<SemanticConflict>,
) {
    for def_file in def_deltas {
        let module_stem = extract_module_stem(&def_file.file_path);
        for sym in &def_file.symbol_deltas {
            let sig_broken_desc = match &sym.op {
                ChangeOperation::Modified {
                    old_signature,
                    new_signature,
                    signature_changed: true,
                } => Some(format!(
                    "changed signature from `{old_signature}` to `{new_signature}`"
                )),
                ChangeOperation::Deleted { old_signature } => {
                    Some(format!("deleted `{old_signature}`"))
                }
                _ => None,
            };

            let Some(break_desc) = sig_broken_desc else {
                continue;
            };

            for caller_file in caller_deltas {
                let same_file = caller_file.file_path == def_file.file_path;
                let imports_module_or_symbol = same_file
                    || caller_file.imports.iter().any(|imp| {
                        imp.contains(&sym.symbol_name)
                            || (!module_stem.is_empty() && imp.contains(&module_stem))
                    });

                if !imports_module_or_symbol {
                    continue;
                }

                for caller_sym in &caller_file.symbol_deltas {
                    if caller_sym
                        .referenced_symbols
                        .iter()
                        .any(|r| r == &sym.symbol_name)
                    {
                        out.push(SemanticConflict {
                            file_path: format!("{} -> {}", def_file.file_path, caller_file.file_path),
                            symbol_name: sym.symbol_name.clone(),
                            branch_a: def_branch.to_string(),
                            branch_b: caller_branch.to_string(),
                            kind: SemanticConflictKind::SignatureCallMismatch,
                            description: format!(
                                "Semantic Call-Graph Conflict: '{def_branch}' {} in '{}', while '{caller_branch}' calls `{}` inside `{}` in '{}'",
                                break_desc,
                                def_file.file_path,
                                sym.symbol_name,
                                caller_sym.symbol_name,
                                caller_file.file_path
                            ),
                        });
                    }
                }
            }
        }
    }
}

fn extract_module_stem(file_path: &str) -> String {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    file_name.split('.').next().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cst_commutative_merge_preserves_mods_attributes_and_string_braces() {
        let base = "\
#![allow(dead_code)]

use std::collections::HashMap;

pub mod telemetry;

pub struct User {
    pub id: u64,
}

impl User {
    pub fn id(&self) -> u64 {
        self.id
    }
}

/// Existing function with unbalanced brace in string literal
pub fn health_check() -> &'static str {
    let _brace_in_str = \"{\";
    \"ok\"
}
";

        let branch_a = "\
#![allow(dead_code)]

use std::collections::HashMap;
use serde::Serialize;

pub mod telemetry;

pub struct User {
    pub id: u64,
}

impl User {
    pub fn id(&self) -> u64 {
        self.id
    }
}

/// Existing function with unbalanced brace in string literal
pub fn health_check() -> &'static str {
    let _brace_in_str = \"{\";
    \"ok\"
}

/// Added by Agent A
pub fn create_user(name: &str) -> String {
    format!(\"user:{name}\")
}
";

        let branch_b = "\
#![allow(dead_code)]

use std::collections::HashMap;
use sha2::Sha256;

pub mod telemetry;

pub struct User {
    pub id: u64,
}

impl User {
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl std::fmt::Display for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, \"User({{{{}}}})\", self.id)
    }
}

/// Existing function with unbalanced brace in string literal
pub fn health_check() -> &'static str {
    let _brace_in_str = \"{\";
    \"ok\"
}

/// Added by Agent B
pub fn charge_card(amount_cents: u64) -> bool {
    amount_cents > 0
}
";

        let outcome = cst_merge_3way(
            "src/api.rs",
            "refs/heads/agent-a",
            "refs/heads/agent-b",
            base,
            branch_a,
            branch_b,
        );

        match outcome {
            CstMergeOutcome::Clean {
                merged_source,
                merged_symbols,
            } => {
                assert!(merged_source.contains("#![allow(dead_code)]"));
                assert!(merged_source.contains("pub mod telemetry;"));
                assert!(merged_source.contains("pub struct User"));
                assert!(merged_source.contains("impl User"));
                assert!(merged_source.contains("impl std::fmt::Display for User"));
                assert!(merged_source.contains("use serde::Serialize;"));
                assert!(merged_source.contains("use sha2::Sha256;"));
                assert!(merged_source.contains("/// Added by Agent A\npub fn create_user"));
                assert!(merged_source.contains("/// Added by Agent B\npub fn charge_card"));
                assert!(merged_symbols.contains(&"create_user".to_string()));
                assert!(merged_symbols.contains(&"charge_card".to_string()));
            }
            other => panic!("expected clean commutative CST merge, got {other:?}"),
        }
    }

    #[test]
    fn test_cross_file_signature_call_conflict_radar() {
        let base_billing = "\
pub fn charge(amount: u64) -> bool {
    amount > 0
}
";
        let new_billing = "\
pub fn charge(amount: u64, currency: &str) -> bool {
    amount > 0 && !currency.is_empty()
}
";

        let base_checkout = "\
use crate::billing::charge;

pub fn status() -> bool {
    true
}
";
        let new_checkout = "\
use crate::billing::charge;

pub fn status() -> bool {
    true
}

pub fn run_checkout() -> bool {
    charge(100)
}
";

        let delta_a = compute_file_semantic_delta(
            "src/billing.rs",
            ObjectId::ZERO,
            ObjectId::ZERO,
            base_billing,
            new_billing,
        )
        .unwrap();
        let delta_b = compute_file_semantic_delta(
            "src/checkout.rs",
            ObjectId::ZERO,
            ObjectId::ZERO,
            base_checkout,
            new_checkout,
        )
        .unwrap();

        let conflicts = detect_semantic_conflicts(
            "refs/heads/agent-billing",
            &[delta_a],
            "refs/heads/agent-checkout",
            &[delta_b],
        );

        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0].kind,
            SemanticConflictKind::SignatureCallMismatch
        );
        assert_eq!(conflicts[0].symbol_name, "charge");
    }
}
