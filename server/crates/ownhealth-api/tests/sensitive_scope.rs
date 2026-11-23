use std::collections::HashSet;
use std::path::Path;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

fn rust_files(dir: &Path, check: &mut impl FnMut(&Path)) {
    for entry in dir.read_dir().expect("read source directory") {
        let entry = entry.expect("source directory entry");
        let path = entry.path();
        if entry.file_type().expect("source file type").is_dir() {
            rust_files(&path, check);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            check(&path);
        }
    }
}

/// Every caller of these readers builds the scopes with `Permission::read_scopes_for_sensitive`.
const FILTERED_SCOPES: [&str; 1] = ["ownhealth-rules/src/drugs.rs"];

fn punct(token: &TokenTree, ch: char) -> bool {
    matches!(token, TokenTree::Punct(p) if p.as_char() == ch)
}

fn scope_value(tokens: &[TokenTree], aliases: &HashSet<String>) -> bool {
    match tokens {
        [TokenTree::Ident(name)] => aliases.contains(&name.to_string()),
        [TokenTree::Literal(literal)] => {
            let literal = literal.to_string();
            let scope = "\"read:sensitive\"";
            literal == scope
                || literal.strip_prefix('r').is_some_and(|raw| {
                    let fence = raw.bytes().take_while(|&b| b == b'#').count();
                    raw[fence..].strip_suffix(&raw[..fence]) == Some(scope)
                })
        }
        [TokenTree::Group(group)] if group.delimiter() == Delimiter::Parenthesis => {
            scope_value(&group.stream().into_iter().collect::<Vec<_>>(), aliases)
        }
        [first, rest @ ..] if punct(first, '&') => scope_value(rest, aliases),
        _ => false,
    }
}

/// Track simple `let` and `const` aliases in each token group, including typed bindings.
fn update_alias(tokens: &[TokenTree], aliases: &mut HashSet<String>) {
    let [TokenTree::Ident(kind), rest @ ..] = tokens else {
        return;
    };
    let rest = match kind.to_string().as_str() {
        "let" | "const" => rest,
        _ => return,
    };
    let rest = match rest {
        [TokenTree::Ident(mutable), rest @ ..] if mutable == "mut" => rest,
        _ => rest,
    };
    let [TokenTree::Ident(name), rest @ ..] = rest else {
        return;
    };
    let declaration = rest.split(|token| punct(token, ';')).next().unwrap_or_default();
    let value = declaration.splitn(2, |token| punct(token, '=')).nth(1);
    let name = name.to_string();
    if value.is_some_and(|value| scope_value(value, aliases)) {
        aliases.insert(name);
    } else {
        aliases.remove(&name);
    }
}

/// Rust tokenization discards comments and keeps each string as a single literal. Only a scope
/// literal or an alias in a membership call or an `any` equality predicate counts.
fn direct_check(stream: TokenStream, mut aliases: HashSet<String>, in_any: bool) -> bool {
    let tokens: Vec<_> = stream.into_iter().collect();
    for (i, token) in tokens.iter().enumerate() {
        update_alias(&tokens[i..], &mut aliases);
        if in_any
            && let [left, eq, next, right, ..] = &tokens[i..]
            && punct(eq, '=')
            && punct(next, '=')
            && (scope_value(std::slice::from_ref(left), &aliases) || scope_value(std::slice::from_ref(right), &aliases))
        {
            return true;
        }
        let TokenTree::Group(group) = token else {
            continue;
        };
        let method = match &tokens[..i] {
            [.., dot, TokenTree::Ident(name)] if punct(dot, '.') && group.delimiter() == Delimiter::Parenthesis => {
                name.to_string()
            }
            _ => String::new(),
        };
        if matches!(method.as_str(), "has" | "contains") {
            let args: Vec<_> = group.stream().into_iter().collect();
            let first = args.split(|token| punct(token, ',')).next().unwrap_or_default();
            if scope_value(first, &aliases) {
                return true;
            }
        }
        if direct_check(group.stream(), aliases.clone(), in_any || method == "any") {
            return true;
        }
    }
    false
}

fn has_direct_scope_check(source: &str) -> bool {
    direct_check(source.parse().expect("tokenize Rust source"), HashSet::new(), false)
}

#[test]
fn sensitive_facts_must_use_category_aware_permissions() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("server crates");
    let access = crates.join("ownhealth-core/src/access.rs");
    let mut violations = Vec::new();
    for krate in crates.read_dir().expect("server crates") {
        let krate = krate.expect("crate path").path();
        if !krate.is_dir() {
            continue;
        }
        rust_files(&krate, &mut |path| {
            let relative = path.strip_prefix(crates).expect("crate source path");
            if path == access || FILTERED_SCOPES.iter().any(|f| relative == Path::new(f)) {
                return;
            }
            let source = std::fs::read_to_string(path).expect("read Rust source");
            if has_direct_scope_check(&source) {
                violations.push(relative.to_path_buf());
            }
        });
    }
    assert!(
        violations.is_empty(),
        "Use may_read_sensitive(category_scope): {violations:?}"
    );
}

#[test]
fn comments_inside_a_sensitive_scope_call_are_ignored() {
    assert!(has_direct_scope_check(
        r#"scopes.contains("read:sensitive" /* explanation */)"#
    ));
}

#[test]
fn documented_sensitive_scope_calls_are_allowed() {
    assert!(!has_direct_scope_check(
        r#"
        // scopes.contains("read:sensitive")
        /* scopes.has("read:sensitive") */
    "#
    ));
    assert!(!has_direct_scope_check(
        r##"let example = r#"scopes.contains("read:sensitive")"#;"##
    ));
}

#[test]
fn sensitive_scope_checks_through_a_binding_are_caught() {
    assert!(has_direct_scope_check(
        r#"let scope = "read:sensitive"; scopes.contains(scope)"#
    ));
}

#[test]
fn sensitive_scope_checks_through_an_iterator_are_caught() {
    assert!(has_direct_scope_check(
        r#"scopes.iter().any(|s| s == "read:sensitive")"#
    ));
}

#[test]
fn the_scan_preserves_bindings_and_literal_boundaries() {
    assert!(has_direct_scope_check(
        r##"
        const SENSITIVE: &str = r#"read:sensitive"#;
        scopes.has(&SENSITIVE,)
    "##
    ));
    assert!(!has_direct_scope_check(
        r#"
        let scope = "read:sensitive";
        { let scope = "read:labs"; scopes.contains(scope); }
        /// Example: scopes.has("read:sensitive")
        fn ordinary() {}
    "#
    ));
}
