use mesh_proto::{
    ChangeOperation, FileSemanticDelta, ObjectId, SemanticConflict, SemanticConflictKind,
    SymbolDelta, SymbolKind, SymbolSpan,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};

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
    } else if file_path.ends_with(".ts") || file_path.ends_with(".tsx") {
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

/// Parses a source file into Concrete Syntax Tree (CST) top-level declaration spans,
/// preserving leading doc-comments/attributes and exact byte offsets.
pub fn parse_cst_spans(file_path: &str, source: &str) -> Result<Vec<SymbolSpan>, String> {
    let lang = detect_language(file_path).ok_or_else(|| "unsupported language".to_string())?;
    // Quick brace balance sanity check for brace-delimited languages
    if matches!(lang, "rust" | "typescript" | "javascript" | "go") && !is_brace_balanced(source) {
        return Err("syntax error: unbalanced braces in source file".to_string());
    }

    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut line_byte_offsets = Vec::with_capacity(lines.len() + 1);
    let mut acc = 0usize;
    for l in &lines {
        line_byte_offsets.push(acc);
        acc += l.len();
    }
    line_byte_offsets.push(acc);

    let mut spans = Vec::new();
    let mut idx = 0usize;
    let mut pending_trivia_start_line: Option<usize> = None;

    while idx < lines.len() {
        let trimmed = lines[idx].trim();

        if trimmed.is_empty() {
            pending_trivia_start_line = None;
            idx += 1;
            continue;
        }

        // Leading comments, docstrings, or attributes/decorators attach to the declaration that follows
        if is_comment_or_attribute(trimmed) {
            if pending_trivia_start_line.is_none() {
                pending_trivia_start_line = Some(idx);
            }
            idx += 1;
            continue;
        }

        if let Some((kind, name)) = match_top_level_declaration(lang, trimmed) {
            let start_line = pending_trivia_start_line.unwrap_or(idx);
            pending_trivia_start_line = None;

            let end_line = if kind == SymbolKind::Import {
                find_import_end_line(&lines, idx)
            } else if lang == "python" {
                find_python_block_end_line(&lines, idx)
            } else {
                find_brace_or_statement_end_line(&lines, idx)
            };

            let start_byte = line_byte_offsets[start_line];
            let end_byte = line_byte_offsets[(end_line + 1).min(lines.len())];
            let raw_slice = &source[start_byte..end_byte];
            let decl_slice = &source[line_byte_offsets[idx]..end_byte];

            let signature = extract_signature(lang, kind, decl_slice);
            let referenced_symbols = extract_called_identifiers(decl_slice, &name);

            spans.push(SymbolSpan {
                name,
                kind,
                signature,
                start_byte,
                end_byte,
                start_line: start_line + 1,
                end_line: end_line + 1,
                body_hash: hash_bytes(raw_slice.trim().as_bytes()),
                referenced_symbols,
            });

            idx = end_line + 1;
        } else {
            pending_trivia_start_line = None;
            idx += 1;
        }
    }

    Ok(spans)
}

fn is_brace_balanced(source: &str) -> bool {
    let mut depth = 0i64;
    for ch in source.chars() {
        if ch == '{' {
            depth += 1;
        } else if ch == '}' {
            depth -= 1;
            if depth < 0 {
                return false;
            }
        }
    }
    depth == 0
}

fn is_comment_or_attribute(trimmed: &str) -> bool {
    trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("*/")
        || trimmed.starts_with("#[")
        || trimmed.starts_with("#![")
        || trimmed.starts_with('@')
        || trimmed.starts_with('#')
}

fn match_top_level_declaration(lang: &str, trimmed: &str) -> Option<(SymbolKind, String)> {
    // Strip visibility & async/export modifiers
    let mut s = trimmed;
    for prefix in [
        "pub(crate) ",
        "pub(super) ",
        "pub ",
        "export default ",
        "export ",
        "async ",
        "unsafe ",
        "const fn ",
    ] {
        if prefix == "const fn " && s.starts_with("const fn ") {
            s = &s["const ".len()..];
        } else if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim_start();
        }
    }

    // Imports
    if s.starts_with("use ") || s.starts_with("import ") || (lang == "python" && s.starts_with("from ")) {
        let import_key = s.trim_end_matches(';').trim().to_string();
        return Some((SymbolKind::Import, import_key));
    }

    // Functions
    for kw in ["fn ", "function ", "def ", "func "] {
        if let Some(rest) = s.strip_prefix(kw) {
            let name = extract_ident(rest)?;
            return Some((SymbolKind::Function, name));
        }
    }

    // Struct / Class / Interface / Enum / Trait / Type / Const
    for (kw, kind) in [
        ("struct ", SymbolKind::Struct),
        ("enum ", SymbolKind::Enum),
        ("trait ", SymbolKind::Trait),
        ("impl ", SymbolKind::Impl),
        ("class ", SymbolKind::Class),
        ("interface ", SymbolKind::Interface),
        ("type ", SymbolKind::TypeAlias),
        ("const ", SymbolKind::Const),
    ] {
        if let Some(rest) = s.strip_prefix(kw) {
            let name = if kind == SymbolKind::Impl {
                rest.split('{').next().unwrap_or(rest).trim().to_string()
            } else {
                extract_ident(rest)?
            };
            return Some((kind, name));
        }
    }

    None
}

fn extract_ident(s: &str) -> Option<String> {
    let s = s.trim_start();
    // Skip Go receiver `func (s *Server) Foo(...)`
    let s = if s.starts_with('(') {
        s.split_once(')')?.1.trim_start()
    } else {
        s
    };
    let ident: String = s
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if ident.is_empty() {
        None
    } else {
        Some(ident)
    }
}

fn find_import_end_line(lines: &[&str], start: usize) -> usize {
    let mut i = start;
    let mut brace_depth = 0i32;
    while i < lines.len() {
        for ch in lines[i].chars() {
            if ch == '{' {
                brace_depth += 1;
            } else if ch == '}' {
                brace_depth -= 1;
            }
        }
        if brace_depth <= 0 {
            break;
        }
        i += 1;
    }
    i.min(lines.len().saturating_sub(1))
}

fn find_brace_or_statement_end_line(lines: &[&str], start: usize) -> usize {
    let mut i = start;
    let mut brace_depth = 0i32;
    let mut saw_open_brace = false;

    while i < lines.len() {
        let line = lines[i];
        for ch in line.chars() {
            if ch == '{' {
                brace_depth += 1;
                saw_open_brace = true;
            } else if ch == '}' {
                brace_depth -= 1;
            }
        }
        if saw_open_brace && brace_depth <= 0 {
            return i;
        }
        if !saw_open_brace && line.trim_end().ends_with(';') {
            return i;
        }
        i += 1;
    }
    lines.len().saturating_sub(1)
}

fn find_python_block_end_line(lines: &[&str], start: usize) -> usize {
    let base_indent = lines[start]
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let mut last_non_empty = start;
    let mut i = start + 1;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let indent = lines[i].chars().take_while(|c| c.is_whitespace()).count();
        if indent <= base_indent {
            break;
        }
        last_non_empty = i;
        i += 1;
    }
    last_non_empty
}

fn extract_signature(lang: &str, kind: SymbolKind, decl_slice: &str) -> String {
    if kind == SymbolKind::Import {
        return decl_slice.trim().to_string();
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

/// Extracts function/method identifiers invoked via `ident(` inside a declaration body.
fn extract_called_identifiers(decl_slice: &str, own_name: &str) -> Vec<String> {
    let body = if let Some((_, b)) = decl_slice.split_once('{') {
        b
    } else if let Some((_, b)) = decl_slice.split_once(':') {
        b
    } else {
        return Vec::new();
    };

    let mut refs = BTreeSet::new();
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'(' && i > 0 {
            // Walk backward to collect identifier before `(`
            let mut end = i;
            while end > 0 && bytes[end - 1].is_ascii_whitespace() {
                end -= 1;
            }
            let mut start = end;
            while start > 0
                && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_')
            {
                start -= 1;
            }
            if start < end {
                let ident = &body[start..end];
                if !matches!(
                    ident,
                    "if" | "for" | "while" | "match" | "switch" | "return" | "sizeof" | "Some" | "Ok" | "Err"
                ) && ident != own_name
                {
                    refs.insert(ident.to_string());
                }
            }
        }
        i += 1;
    }
    refs.into_iter().collect()
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

    let old_map: BTreeMap<(u8, String), &SymbolSpan> = old_spans
        .iter()
        .map(|s| ((s.kind as u8, s.name.clone()), s))
        .collect();
    let new_map: BTreeMap<(u8, String), &SymbolSpan> = new_spans
        .iter()
        .map(|s| ((s.kind as u8, s.name.clone()), s))
        .collect();

    let mut deltas = Vec::new();
    let mut imports = Vec::new();

    for (key, new_s) in &new_map {
        if new_s.kind == SymbolKind::Import {
            imports.push(new_s.name.clone());
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
        if old_s.kind == SymbolKind::Import {
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

/// Verbatim CST Byte-Span Commutative 3-Way Merger.
///
/// Unlike line-based 3-way merge (`git merge-file`), this merger:
/// 1. Treats top-level imports as an ordered set union.
/// 2. Splices independent declaration modifications (`fn`, `struct`, `class`, `interface`)
///    using their exact verbatim byte slices (preserving comments and formatting).
/// 3. Splices new declarations added by both branches at the end of the file without
///    raising a false line-collision conflict.
/// 4. Detects true symbol-level conflicts AND intra-file signature/call mismatches.
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
                description: "File could not be decomposed into CST spans (syntax error or unsupported format)".to_string(),
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

    let base_map: BTreeMap<String, &SymbolSpan> = base_spans
        .iter()
        .filter(|s| s.kind != SymbolKind::Import)
        .map(|s| (s.name.clone(), s))
        .collect();
    let a_map: BTreeMap<String, &SymbolSpan> = a_spans
        .iter()
        .filter(|s| s.kind != SymbolKind::Import)
        .map(|s| (s.name.clone(), s))
        .collect();
    let b_map: BTreeMap<String, &SymbolSpan> = b_spans
        .iter()
        .filter(|s| s.kind != SymbolKind::Import)
        .map(|s| (s.name.clone(), s))
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
            !a_spans.iter().any(|x| x.name == s.name)
                || !b_spans.iter().any(|x| x.name == s.name)
        })
        .map(|s| s.name.clone())
        .collect();

    for (src, spans) in [(base_src, &base_spans), (branch_a_src, &a_spans), (branch_b_src, &b_spans)] {
        for sp in spans.iter().filter(|s| s.kind == SymbolKind::Import) {
            if !base_deleted_imports.contains(&sp.name) && seen_imports.insert(sp.name.clone()) {
                let raw = src[sp.start_byte..sp.end_byte].trim_end().to_string();
                import_slices.push(raw);
            }
        }
    }

    // 2. Determine declaration order: base order -> new in A -> new in B
    let mut ordered_names = Vec::new();
    let mut seen_names = HashSet::new();
    for sp in base_spans
        .iter()
        .chain(a_spans.iter())
        .chain(b_spans.iter())
        .filter(|s| s.kind != SymbolKind::Import)
    {
        if seen_names.insert(sp.name.clone()) {
            ordered_names.push(sp.name.clone());
        }
    }

    let mut merged_blocks = Vec::new();

    for name in &ordered_names {
        let base_sp = base_map.get(name).copied();
        let a_sp = a_map.get(name).copied();
        let b_sp = b_map.get(name).copied();

        match (base_sp, a_sp, b_sp) {
            (Some(base), Some(a), Some(b)) => {
                let a_changed = a.body_hash != base.body_hash;
                let b_changed = b.body_hash != base.body_hash;
                if !a_changed && !b_changed {
                    merged_blocks.push(base_src[base.start_byte..base.end_byte].trim_end().to_string());
                } else if a_changed && !b_changed {
                    merged_symbols.push(name.clone());
                    merged_blocks.push(branch_a_src[a.start_byte..a.end_byte].trim_end().to_string());
                } else if !a_changed && b_changed {
                    merged_symbols.push(name.clone());
                    merged_blocks.push(branch_b_src[b.start_byte..b.end_byte].trim_end().to_string());
                } else if a.body_hash == b.body_hash {
                    merged_symbols.push(name.clone());
                    merged_blocks.push(branch_a_src[a.start_byte..a.end_byte].trim_end().to_string());
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
                merged_blocks.push(branch_a_src[a.start_byte..a.end_byte].trim_end().to_string());
            }
            (None, None, Some(b)) => {
                merged_symbols.push(name.clone());
                merged_blocks.push(branch_b_src[b.start_byte..b.end_byte].trim_end().to_string());
            }
            (None, Some(a), Some(b)) => {
                if a.body_hash == b.body_hash {
                    merged_symbols.push(name.clone());
                    merged_blocks.push(branch_a_src[a.start_byte..a.end_byte].trim_end().to_string());
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
                // Deleted in B; check if A modified it
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
                // Deleted in A; check if B modified it
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
///
/// Inspects two sets of `FileSemanticDelta`s (e.g., from two active agent branches `branch_a`
/// and `branch_b`) and detects:
/// 1. `SameSymbolModified`: Both branches modify the same symbol in the same file.
/// 2. `SignatureCallMismatch`: Branch A modifies the signature of symbol `S` (or deletes `S`),
///    while Branch B adds/modifies a function that calls `S`!
pub fn detect_semantic_conflicts(
    branch_a_name: &str,
    branch_a_deltas: &[FileSemanticDelta],
    branch_b_name: &str,
    branch_b_deltas: &[FileSemanticDelta],
) -> Vec<SemanticConflict> {
    let mut conflicts = Vec::new();

    // 1. Check same-file symbol collisions
    for fa in branch_a_deltas {
        for fb in branch_b_deltas {
            if fa.file_path == fb.file_path {
                for sa in &fa.symbol_deltas {
                    for sb in &fb.symbol_deltas {
                        if sa.symbol_name == sb.symbol_name {
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

    // 2. Check cross-file and intra-file signature mutation vs. call-site usage
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
                } => Some(format!("changed signature from `{old_signature}` to `{new_signature}`")),
                ChangeOperation::Deleted { old_signature } => {
                    Some(format!("deleted `{old_signature}`"))
                }
                _ => None,
            };

            let Some(break_desc) = sig_broken_desc else {
                continue;
            };

            // Check if any modified/inserted symbol in `caller_deltas` calls `sym.symbol_name`
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
    file_name
        .split('.')
        .next()
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cst_commutative_merge_same_file_adjacent_additions() {
        let base = "\
use std::collections::HashMap;

/// Existing function
pub fn health_check() -> &'static str {
    \"ok\"
}
";

        // Agent A adds an import and appends `create_user` at the bottom
        let branch_a = "\
use std::collections::HashMap;
use serde::Serialize;

/// Existing function
pub fn health_check() -> &'static str {
    \"ok\"
}

/// Added by Agent A
pub fn create_user(name: &str) -> String {
    format!(\"user:{name}\")
}
";

        // Agent B adds a different import and appends `charge_card` at the exact same bottom lines
        let branch_b = "\
use std::collections::HashMap;
use sha2::Sha256;

/// Existing function
pub fn health_check() -> &'static str {
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
                assert!(merged_source.contains("use serde::Serialize;"));
                assert!(merged_source.contains("use sha2::Sha256;"));
                assert!(merged_source.contains("/// Added by Agent A\npub fn create_user"));
                assert!(merged_source.contains("/// Added by Agent B\npub fn charge_card"));
                assert_eq!(merged_symbols, vec!["create_user", "charge_card"]);
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
        // Agent A changes the signature of `charge` in `src/billing.rs`
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
        // Agent B adds a function calling `charge(100)` with the old 1-arg signature in `src/checkout.rs`
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
