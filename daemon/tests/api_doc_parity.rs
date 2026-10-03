use std::collections::{BTreeMap, BTreeSet};

const API_RS: &str = include_str!("../src/api.rs");
const DOCS: &str = include_str!("../../docs/API.md");

const MERGED_ROUTE_SOURCES: &[(&str, &str)] = &[
    (
        "crate::supervisor::responder_api",
        include_str!("../src/supervisor/responder_api.rs"),
    ),
    (
        "crate::release_triage::http",
        include_str!("../src/release_triage/http.rs"),
    ),
    (
        "crate::upstream_update",
        include_str!("../src/upstream_update.rs"),
    ),
    ("crate::judge::http", include_str!("../src/judge/http.rs")),
    (
        "crate::deleted_bots",
        include_str!("../src/deleted_bots.rs"),
    ),
];

#[test]
fn api_route_methods_match_documented_inventory() {
    let api_router = function_body(API_RS, "pub fn router(");
    let merged_modules = merged_route_modules(api_router);
    let known_modules: BTreeSet<_> = MERGED_ROUTE_SOURCES
        .iter()
        .map(|(name, _)| name.to_string())
        .collect();
    assert_eq!(
        merged_modules, known_modules,
        "update MERGED_ROUTE_SOURCES when the main router merges a route module"
    );

    let root_router = api_router
        .find("Router::new()\n        .nest(\"/api\", api)")
        .expect("main router must keep the /api nesting boundary");
    let mut actual = BTreeSet::new();
    actual.extend(collect_routes(&api_router[..root_router], "/api"));
    actual.insert(("ANY".to_owned(), "/api/*".to_owned()));
    actual.extend(collect_routes(&api_router[root_router..], ""));
    actual.insert(("GET".to_owned(), "/*".to_owned()));
    actual.insert(("HEAD".to_owned(), "/*".to_owned()));
    for (_, source) in MERGED_ROUTE_SOURCES {
        actual.extend(collect_routes(
            function_body(source, "pub fn routes("),
            "/api",
        ));
    }

    let documented = documented_routes(DOCS);
    let missing: Vec<_> = actual
        .difference(&documented.keys().cloned().collect())
        .cloned()
        .collect();
    let stale: Vec<_> = documented
        .keys()
        .filter(|route| !actual.contains(*route))
        .cloned()
        .collect();
    assert!(missing.is_empty() && stale.is_empty(), "API.md route inventory differs from router; undocumented={missing:?}; not_registered={stale:?}");
    assert!(
        documented.values().all(|policy| !policy.trim().is_empty()),
        "every documented route needs a permission description"
    );
    for route in user_only_routes(API_RS) {
        let mut expected = vec![route.clone()];
        if route.0 == "GET" {
            expected.push(("HEAD".to_owned(), route.1.clone()));
        }
        for expected_route in expected {
            let policy = documented.get(&expected_route).unwrap_or_else(|| {
                panic!(
                    "User-only route missing from API.md: {} {}",
                    expected_route.0, expected_route.1
                )
            });
            assert!(
                policy.starts_with("User-only"),
                "permission mismatch for {} {}: source enforces User-only, docs say {policy}",
                expected_route.0,
                expected_route.1
            );
        }
    }
    for route in strict_user_only_routes(API_RS) {
        let mut expected = vec![route.clone()];
        if route.0 == "GET" {
            expected.push(("HEAD".to_owned(), route.1.clone()));
        }
        for expected_route in expected {
            let policy = documented.get(&expected_route).unwrap_or_else(|| {
                panic!(
                    "strict User-only route missing from API.md: {} {}",
                    expected_route.0, expected_route.1
                )
            });
            assert!(
                policy.starts_with("User-only") && policy.contains("AGM role 均"),
                "strict User-only policy must explicitly deny AGM roles for {} {}: {policy}",
                expected_route.0,
                expected_route.1
            );
        }
    }
}

fn strict_user_only_routes(source: &str) -> BTreeSet<(String, String)> {
    const START: &str = "const BOT_STRICT_USER_ONLY_ROUTES: &[(&str, &str)] = &[";
    let start = source
        .find(START)
        .expect("api.rs must keep the strict User-only route policy at the auth boundary");
    let tail = &source[start + START.len()..];
    let end = tail
        .find("];\n")
        .expect("strict User-only route policy must be closed");
    let mut routes = BTreeSet::new();
    for line in tail[..end].lines() {
        let values: Vec<_> = line.split('"').skip(1).step_by(2).collect();
        if values.len() == 2 {
            assert!(
                routes.insert((values[0].to_owned(), values[1].to_owned())),
                "duplicate strict User-only route: {line}"
            );
        }
    }
    routes
}

fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
    let start = source
        .find(signature)
        .unwrap_or_else(|| panic!("missing router function {signature}"));
    let open = source[start..]
        .find('{')
        .expect("router function has a body")
        + start;
    let masked = mask_rust(&source[open..]);
    let mut depth = 0usize;
    for (offset, ch) in masked.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unterminated function body {signature}");
}

fn collect_routes(source: &str, prefix: &str) -> BTreeSet<(String, String)> {
    let masked = mask_rust(source);
    let mut routes = BTreeSet::new();
    let mut cursor = 0usize;
    while let Some(relative) = masked[cursor..].find(".route(") {
        let start = cursor + relative + ".route".len();
        let open = start;
        let close = matching_delimiter(&masked, open, '(', ')')
            .expect("route call has closing parenthesis");
        let raw = &source[open + 1..close];
        let path = first_string_literal(raw).expect("route path must be a string literal");
        let method_offset = raw.find(',').expect("route call has a second argument") + 1;
        let methods = route_methods(&masked[open + 1 + method_offset..close]);
        assert!(
            !methods.is_empty(),
            "route {path} uses an unsupported method declaration: {raw}"
        );
        for method in methods {
            assert!(
                routes.insert((method.to_owned(), format!("{prefix}{path}"))),
                "duplicate route {method} {prefix}{path}"
            );
            if method == "GET" {
                assert!(
                    routes.insert(("HEAD".to_owned(), format!("{prefix}{path}"))),
                    "duplicate route HEAD {prefix}{path}"
                );
            }
        }
        cursor = close + 1;
    }
    routes
}

fn matching_delimiter(source: &str, open: usize, left: char, right: char) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            c if c == left => depth += 1,
            c if c == right => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn first_string_literal(source: &str) -> Option<String> {
    let source = source.trim_start();
    let rest = source.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

fn route_methods(source: &str) -> BTreeSet<&'static str> {
    const METHODS: &[(&str, &str)] = &[
        ("get", "GET"),
        ("post", "POST"),
        ("put", "PUT"),
        ("patch", "PATCH"),
        ("delete", "DELETE"),
    ];
    let mut found = BTreeSet::new();
    let bytes = source.as_bytes();
    for (method, verb) in METHODS {
        let mut cursor = 0usize;
        while let Some(relative) = source[cursor..].find(method) {
            let start = cursor + relative;
            let end = start + method.len();
            let before_ok = start == 0 || !is_ident(bytes[start - 1]);
            let after_ok = end == bytes.len() || !is_ident(bytes[end]);
            let next = source[end..].trim_start();
            if before_ok && after_ok && next.starts_with('(') {
                found.insert(*verb);
            }
            cursor = end;
        }
    }
    found
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn merged_route_modules(source: &str) -> BTreeSet<String> {
    let masked = mask_rust(source);
    let mut result = BTreeSet::new();
    let mut cursor = 0usize;
    while let Some(relative) = masked[cursor..].find("::routes") {
        let end = cursor + relative;
        let mut start = end;
        while start > 0 {
            let byte = masked.as_bytes()[start - 1];
            if is_ident(byte) || byte == b':' {
                start -= 1;
            } else {
                break;
            }
        }
        let module = masked[start..end].trim_end_matches(':');
        if module.starts_with("crate::") {
            result.insert(module.to_owned());
        }
        cursor = end + "::routes".len();
    }
    result
}

fn documented_routes(docs: &str) -> BTreeMap<(String, String), String> {
    const START: &str = "<!-- api-route-inventory:start -->";
    const END: &str = "<!-- api-route-inventory:end -->";
    let start = docs
        .find(START)
        .expect("API.md must contain the complete route inventory table");
    let tail = &docs[start + START.len()..];
    let end = tail
        .find(END)
        .expect("API.md route inventory table must have an end marker");
    let table = &tail[..end];
    let mut routes = BTreeMap::new();
    for line in table
        .lines()
        .filter(|line| line.trim_start().starts_with('|'))
    {
        let cells: Vec<_> = line.split('|').map(str::trim).collect();
        if cells.len() < 5 || cells[1] == "方法" || cells[1].chars().all(|c| c == '-' || c == ':')
        {
            continue;
        }
        let method = cells[1].trim_matches('`').to_owned();
        let path = cells[2].trim_matches('`').to_owned();
        assert!(
            matches!(
                method.as_str(),
                "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "ANY"
            ),
            "unknown documented method: {method}"
        );
        assert!(!cells[3].is_empty(), "empty permission for {method} {path}");
        assert!(
            routes
                .insert((method, path.clone()), cells[3].to_owned())
                .is_none(),
            "duplicate documented route {path}"
        );
    }
    routes
}

fn user_only_routes(source: &str) -> BTreeSet<(String, String)> {
    const START: &str = "const BOT_USER_ONLY_ROUTES: &[(&str, &str)] = &[";
    let start = source
        .find(START)
        .expect("api.rs must keep the User-only route policy at the auth boundary");
    let tail = &source[start + START.len()..];
    let end = tail
        .find("];\n")
        .expect("User-only route policy must be closed");
    let mut routes = BTreeSet::new();
    for line in tail[..end].lines() {
        let values: Vec<_> = line.split('"').skip(1).step_by(2).collect();
        if values.len() == 2 {
            assert!(
                routes.insert((values[0].to_owned(), values[1].to_owned())),
                "duplicate User-only route: {line}"
            );
        }
    }
    routes
}

fn mask_rust(source: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        String,
        LineComment,
        BlockComment(usize),
    }
    let mut state = State::Code;
    let mut chars = source.chars().peekable();
    let mut out = String::with_capacity(source.len());
    while let Some(ch) = chars.next() {
        match state {
            State::Code => match ch {
                '"' => {
                    out.push(' ');
                    state = State::String;
                }
                '/' if chars.peek() == Some(&'/') => {
                    chars.next();
                    out.push_str("  ");
                    state = State::LineComment;
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    out.push_str("  ");
                    state = State::BlockComment(1);
                }
                _ => out.push(ch),
            },
            State::String => match ch {
                '\\' => {
                    out.push(' ');
                    if let Some(escaped) = chars.next() {
                        push_spaces(&mut out, escaped);
                    }
                }
                '"' => {
                    out.push(' ');
                    state = State::Code;
                }
                '\n' => out.push('\n'),
                _ => push_spaces(&mut out, ch),
            },
            State::LineComment => {
                if ch == '\n' {
                    out.push('\n');
                    state = State::Code;
                } else {
                    push_spaces(&mut out, ch);
                }
            }
            State::BlockComment(depth) => match ch {
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    out.push_str("  ");
                    state = State::BlockComment(depth + 1);
                }
                '*' if chars.peek() == Some(&'/') => {
                    chars.next();
                    out.push_str("  ");
                    if depth == 1 {
                        state = State::Code;
                    } else {
                        state = State::BlockComment(depth - 1);
                    }
                }
                '\n' => out.push('\n'),
                _ => push_spaces(&mut out, ch),
            },
        }
    }
    out
}

fn push_spaces(out: &mut String, ch: char) {
    for _ in 0..ch.len_utf8() {
        out.push(' ');
    }
}
