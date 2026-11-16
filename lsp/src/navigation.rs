// ── Variable navigation ────────────────────────────────────────────────────
//
// Resolve each occurrence once against declarations visible at that point.
// The index uses offsets in joined logical lines. Callers map spans to the
// original document and negotiate position encoding at the protocol boundary.

use std::collections::{HashMap, HashSet};

use crate::caps::{MAX_REFERENCES, MAX_VARIABLE_OCCURRENCES};
use crate::diagnostics::LogicalLine;
use crate::parser::{MAX_BRACE_DEPTH, QuoteAdvance, QuoteState, SpanToken};

/// Which command introduced a binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeclKind {
    Local,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HitKind {
    Declaration(DeclKind),
    Usage,
}

/// A name span, excluding the dollar sigil, quotes, and initializer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VariableHit {
    pub name: String,
    pub kind: HitKind,
    pub logical_line: usize,
    pub start: usize,
    pub end: usize,
    /// Index of the declaration in document order.
    binding: Option<usize>,
    uncertain: bool,
}

pub(crate) fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True when `text` is a statement separator that may precede a `:local` /
/// `:global` command token on the same logical line: an isolated brace or
/// semicolon (`{`, `}`, `;`), or any token ending in `;` (a `;`-separated
/// tail like `:put 1;`). `/` and `..` are NOT separators — a `/`-prefixed
/// token opens a menu path, so `/ :local x` must never declare.
fn is_leading_separator(text: &str) -> bool {
    matches!(text, "{" | "}" | ";") || text.ends_with(';')
}

/// Extract the declaration introduced by a `:local` / `:global` command line.
///
/// Returns `(kind, identifier, ident_start, ident_end)` where the offsets
/// locate the bare identifier for the document symbol label.
///
/// The command token is the FIRST `:local` / `:global` token whose
/// predecessors are all statement separators (see [`is_leading_separator`]
/// — covering `{ :local x }` block openers and `;`-separated tails like
/// `:put 1; :local y`), or the token at index 0. A `:local` buried after a
/// real command (`:put $x :local y`) is NOT a declaration. Returns `None`
/// when no such command token exists or it carries no bare identifier
/// token (`:global` alone, quoted names).
pub(crate) fn declared_variable(tokens: &[SpanToken]) -> Option<(DeclKind, String, usize, usize)> {
    let cmd_idx = tokens
        .iter()
        .position(|t| t.text == ":local" || t.text == ":global")?;
    // Enforce the separator rule for non-zero positions: all tokens before
    // the command must be separators.
    if cmd_idx > 0
        && !tokens[..cmd_idx]
            .iter()
            .all(|t| is_leading_separator(&t.text))
    {
        // Fallback: accept when the IMMEDIATELY preceding token terminates
        // a statement (`;`-separated tail) even if earlier tokens are real
        // commands — `:put 1; :local y` still declares.
        let prev = &tokens[cmd_idx - 1].text;
        if !(prev == "{" || prev == "}" || prev == ";" || prev.ends_with(';')) {
            return None;
        }
        // Earlier tokens may be real commands here; the `;` boundary is
        // what matters.
    }
    let kind = match tokens[cmd_idx].text.as_str() {
        ":local" => DeclKind::Local,
        ":global" => DeclKind::Global,
        _ => return None,
    };
    // The identifier is the token immediately following the command token…
    let var = tokens.get(cmd_idx + 1)?;
    // …restricted to its bare-identifier prefix: `:local x=1` tokenizes as
    // one "x=1" token and the declaration owns only `x`.
    let end = var.text.bytes().take_while(|&b| is_ident_char(b)).count();
    if end == 0 {
        return None;
    }
    Some((
        kind,
        var.text[..end].to_string(),
        var.start,
        var.start + end,
    ))
}

/// A bare name or an unescaped quoted name. Escaped names need runtime
/// decoding, so the index records their presence and disables rename.
fn name_span(text: &str, start: usize, quoted: bool) -> Option<(usize, usize)> {
    let tail = text.get(start..)?;
    if quoted && let Some(inner) = tail.strip_prefix('"') {
        let first_special = inner.find(['"', '\\'])?;
        return (first_special > 0 && inner.as_bytes()[first_special] == b'"')
            .then_some((start + 1, start + 1 + first_special));
    }
    let end = tail.bytes().take_while(|&b| is_ident_char(b)).count();
    if tail.as_bytes().get(end).is_some_and(|b| !b.is_ascii()) {
        return None;
    }
    (end != 0).then_some((start, start + end))
}

#[derive(Default)]
struct Statement {
    started: bool,
    loop_bindings: [Option<usize>; 2],
    initializer: Option<usize>,
    shadowed_initializer: Option<usize>,
    where_clause: bool,
}

#[derive(Default)]
struct Scope {
    closer: u8,
    previous: Vec<(String, Option<usize>)>,
    statement: Statement,
    enclosing_initializer: Option<(usize, usize)>,
    dynamic: bool,
}

#[derive(Debug, Default)]
pub(crate) struct VariableIndex {
    hits: Vec<VariableHit>,
    unsafe_to_rename: bool,
}

impl std::ops::Deref for VariableIndex {
    type Target = [VariableHit];

    fn deref(&self) -> &[VariableHit] {
        &self.hits
    }
}

#[derive(Default)]
struct IndexBuilder {
    index: VariableIndex,
    scopes: Vec<Scope>,
    locals: HashMap<String, usize>,
    globals: HashMap<String, usize>,
    uncertain_bindings: HashSet<usize>,
}

impl IndexBuilder {
    fn scope(&mut self) -> &mut Scope {
        self.scopes.last_mut().expect("root scope")
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.locals
            .get(name)
            .or_else(|| self.globals.get(name))
            .copied()
    }

    fn activate_local(&mut self, binding: usize) {
        let name = self.index[binding].name.clone();
        let previous = self.locals.insert(name.clone(), binding);
        self.scope().previous.push((name, previous));
    }

    fn declaration(
        &mut self,
        kind: DeclKind,
        name: &str,
        line: usize,
        span: (usize, usize),
    ) -> usize {
        let id = self.index.len();
        let dynamic = self.scope().dynamic;
        let binding = match kind {
            DeclKind::Local => id,
            DeclKind::Global => *self.globals.entry(name.to_string()).or_insert(id),
        };
        self.index.hits.push(VariableHit {
            name: name.to_string(),
            kind: HitKind::Declaration(kind),
            logical_line: line,
            start: span.0,
            end: span.1,
            binding: Some(binding),
            uncertain: dynamic,
        });
        id
    }

    fn usage(&mut self, name: &str, line: usize, span: (usize, usize)) {
        let binding = self.lookup(name);
        let scope = self.scope();
        let shadowed = scope
            .statement
            .shadowed_initializer
            .filter(|_| scope.statement.initializer == binding)
            .or_else(|| {
                scope
                    .enclosing_initializer
                    .filter(|(decl, _)| Some(*decl) == binding)
                    .map(|(_, previous)| previous)
            });
        let uncertain = scope.dynamic || scope.statement.where_clause || shadowed.is_some();
        if uncertain && let Some(binding) = binding {
            self.uncertain_bindings.insert(binding);
        }
        if let Some(shadowed) = shadowed {
            self.uncertain_bindings.insert(shadowed);
        }
        self.index.hits.push(VariableHit {
            name: name.to_string(),
            kind: HitKind::Usage,
            logical_line: line,
            start: span.0,
            end: span.1,
            binding,
            uncertain,
        });
    }

    fn open_scope(&mut self, closer: u8, prefix: &str) {
        let parent = self.scope();
        let enclosing_initializer = parent
            .statement
            .initializer
            .zip(parent.statement.shadowed_initializer)
            .or(parent.enclosing_initializer);
        let is_body = closer == b'}' && prefix.ends_with("do=");
        let loop_bindings = if is_body {
            parent.statement.loop_bindings
        } else {
            [None; 2]
        };
        let dynamic = parent.dynamic
            || parent.statement.where_clause
            || (is_body && parent.statement.initializer.is_some())
            || (closer == b'}'
                && [
                    "source=",
                    "on-event=",
                    "lease-script=",
                    "up-script=",
                    "down-script=",
                ]
                .iter()
                .any(|key| prefix.ends_with(key)));
        self.scopes.push(Scope {
            closer,
            enclosing_initializer,
            dynamic,
            ..Scope::default()
        });
        for binding in loop_bindings.into_iter().flatten() {
            self.activate_local(binding);
        }
    }

    fn close_scope(&mut self, closer: u8) {
        if self.scope().closer != closer {
            self.index.unsafe_to_rename = true;
            return;
        }
        let scope = self.scopes.pop().expect("matching scope");
        for (name, previous) in scope.previous.into_iter().rev() {
            if let Some(binding) = previous {
                self.locals.insert(name, binding);
            } else {
                self.locals.remove(&name);
            }
        }
    }

    fn command_target(&mut self, text: &str, line: usize, i: usize) -> Option<usize> {
        let bytes = text.as_bytes();
        let command = text[i..].split_ascii_whitespace().next()?;
        if ![":local", ":global", ":set", ":for", ":foreach", ":onerror"]
            .iter()
            .any(|name| command.eq_ignore_ascii_case(name))
        {
            return None;
        }
        let mut start = i + command.len();
        while bytes.get(start).is_some_and(u8::is_ascii_whitespace) {
            start += 1;
        }
        let Some(span) = name_span(text, start, true) else {
            self.index.unsafe_to_rename |= bytes.get(start) == Some(&b'"');
            return None;
        };
        let quoted = bytes[start] == b'"';
        let mut end = span.1 + usize::from(quoted);
        let second = if command.eq_ignore_ascii_case(":foreach") && bytes.get(end) == Some(&b',') {
            let start = end + 1;
            let second = name_span(text, start, true)?;
            end = second.1 + usize::from(bytes[start] == b'"');
            Some(second)
        } else {
            None
        };
        if bytes
            .get(end)
            .is_some_and(|b| !b.is_ascii_whitespace() && !b"=;}]".contains(b))
        {
            return None;
        }
        let name = &text[span.0..span.1];
        if command.eq_ignore_ascii_case(":set") {
            self.usage(name, line, span);
        } else if command.eq_ignore_ascii_case(":global") {
            let id = self.declaration(DeclKind::Global, name, line, span);
            self.scope().statement.initializer = Some(id);
            if self.locals.contains_key(name) {
                self.activate_local(self.index[id].binding.expect("global binding"));
            }
        } else {
            let previous = self.lookup(name);
            let id = self.declaration(DeclKind::Local, name, line, span);
            if command.eq_ignore_ascii_case(":local") {
                self.activate_local(id);
                self.scope().statement.initializer = Some(id);
                self.scope().statement.shadowed_initializer = previous;
            } else {
                let second = second.map(|span| {
                    self.declaration(DeclKind::Local, &text[span.0..span.1], line, span)
                });
                self.scope().statement.loop_bindings = [Some(id), second];
            }
        }
        Some(end)
    }

    fn scan_line(&mut self, text: &str, line: usize, quote: &mut QuoteState) -> bool {
        let bytes = text.as_bytes();
        self.scope().statement = Statement::default();
        quote.reset_line();
        let mut i = 0;
        while i < bytes.len() {
            if self.index.len() >= MAX_VARIABLE_OCCURRENCES || self.scopes.len() > MAX_BRACE_DEPTH {
                return false;
            }
            let b = bytes[i];
            match quote.advance_byte(b) {
                QuoteAdvance::CommentStart => break,
                QuoteAdvance::Other(b'$') if quote.allows_interpolation() => {
                    if bytes.get(i + 1) == Some(&b'$') {
                        while bytes.get(i) == Some(&b'$') {
                            i += 1;
                        }
                        self.scope().statement.started = true;
                        continue;
                    }
                    if let Some(span) = name_span(text, i + 1, !quote.is_in_quote()) {
                        self.usage(&text[span.0..span.1], line, span);
                        let quoted = bytes[i + 1] == b'"';
                        i = span.1 + usize::from(quoted);
                        self.scope().statement.started = true;
                        continue;
                    }
                    self.index.unsafe_to_rename |=
                        !quote.is_in_quote() && bytes.get(i + 1) == Some(&b'"');
                }
                QuoteAdvance::Other(_) if !quote.is_in_quote() => match b {
                    b'{' | b'[' => {
                        self.scope().statement.started = true;
                        let closer = if b == b'{' { b'}' } else { b']' };
                        self.open_scope(closer, text[..i].trim_end());
                    }
                    b'}' | b']' => self.close_scope(b),
                    b';' => self.scope().statement = Statement::default(),
                    b if b.is_ascii_whitespace() => {}
                    b':' if !self.scope().statement.started => {
                        self.scope().statement.started = true;
                        if let Some(end) = self.command_target(text, line, i) {
                            i = end;
                            continue;
                        }
                    }
                    _ => {
                        let scope = self.scope();
                        scope.statement.started = true;
                        if bytes
                            .get(i..i + 5)
                            .is_some_and(|word| word.eq_ignore_ascii_case(b"where"))
                            && i > 0
                            && bytes[i - 1].is_ascii_whitespace()
                            && bytes.get(i + 5).is_some_and(u8::is_ascii_whitespace)
                        {
                            scope.statement.where_clause = true;
                        }
                    }
                },
                QuoteAdvance::SingleOpen | QuoteAdvance::DoubleOpen => {
                    self.scope().statement.started = true;
                }
                _ => {}
            }
            i += 1;
        }
        true
    }
}

/// Index lexical bindings and their reads and writes in document order.
/// Unsupported or incomplete constructs leave ambiguous uses unresolved.
pub(crate) fn build_variable_index(logicals: &[LogicalLine]) -> VariableIndex {
    let mut builder = IndexBuilder {
        scopes: vec![Scope::default()],
        ..IndexBuilder::default()
    };
    let mut quote = QuoteState::new();
    for (line, ll) in logicals.iter().enumerate() {
        if !builder.scan_line(ll.text(), line, &mut quote)
            || builder.index.len() > MAX_VARIABLE_OCCURRENCES
        {
            return VariableIndex::default();
        }
    }
    builder.index.unsafe_to_rename |= builder.scopes.len() != 1 || quote.is_in_quote();
    for id in builder.uncertain_bindings {
        builder.index.hits[id].uncertain = true;
    }
    builder.index
}

impl VariableIndex {
    pub(crate) fn hit_at(&self, line: usize, offset: usize) -> Option<&VariableHit> {
        let start = self.hits.partition_point(|hit| hit.logical_line < line);
        self.hits[start..]
            .iter()
            .take_while(|hit| hit.logical_line == line && hit.start <= offset)
            .find(|hit| offset <= hit.end)
    }

    pub(crate) fn definition(&self, hit: &VariableHit) -> Option<&VariableHit> {
        if hit.uncertain && hit.kind == HitKind::Usage {
            return None;
        }
        self.hits.get(hit.binding?)
    }

    pub(crate) fn references(
        &self,
        hit: &VariableHit,
        include_declaration: bool,
    ) -> impl Iterator<Item = &VariableHit> {
        let binding = self.definition(hit).and_then(|decl| decl.binding);
        self.hits
            .iter()
            .filter(move |candidate| {
                binding.is_some()
                    && candidate.binding == binding
                    && (include_declaration || candidate.kind == HitKind::Usage)
                    && !(candidate.uncertain && candidate.kind == HitKind::Usage)
            })
            .take(MAX_REFERENCES)
    }

    pub(crate) fn rename_hits(
        &self,
        hit: &VariableHit,
        replacement: Option<&str>,
    ) -> Option<Vec<&VariableHit>> {
        let declaration = self.definition(hit)?;
        if self.unsafe_to_rename
            || declaration.kind != HitKind::Declaration(DeclKind::Local)
            || declaration.uncertain
            || replacement.is_some_and(|name| self.hits.iter().any(|other| other.name == name))
        {
            return None;
        }
        // Discard the whole edit if it exceeds the cap. A partial rename
        // would leave the script with mismatched names.
        let hits: Vec<_> = self
            .hits
            .iter()
            .filter(|candidate| candidate.binding == declaration.binding)
            .take(MAX_REFERENCES + 1)
            .collect();
        (hits.len() <= MAX_REFERENCES).then_some(hits)
    }
}
