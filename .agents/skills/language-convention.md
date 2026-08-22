# Skill: Language Convention (English-Only)

## When to Use

Before writing or reviewing anything persisted to the repo: code, comments, docs, commit messages, PR/issue text, metadata files, script output, CI annotations. Also when replying in chat — to know where the boundary is.

The rule itself lives in [`AGENTS.md`](../../AGENTS.md) → *Hard rules* #9. This skill adds the operational details.

## Rule

**All persisted artifacts MUST be in English. No Spanish anywhere in the repository.**

Scope: code comments · docstrings · commit messages · PR/issue titles and bodies · docs (`README.md`, `AGENTS.md`, `.agents/skills/*.md`) · extension metadata (`extension.toml`, `Cargo.toml`, `package.json`, `tree-sitter.json`) · script output (`println!`/`eprintln!`/`::error::`) · CI log strings.

**Exception:** chat replies may be Spanish if the user writes Spanish. Never let chat language leak into persisted artifacts.

## Commit Messages

Conventional Commits, imperative mood:

```
<type>(<scope>): <short summary in English>

- bullet 1 in English
- bullet 2 in English
```

Types: `feat`, `fix`, `docs`, `chore`, `refactor`, `test`, `ci`, `build`.

Self-check before committing:

```bash
# Heuristic: common Spanish words + accents in staged content
git diff --cached | grep -i -E "ñ|á|é|í|ó|ú|solventa|añade|corrige|fallo" && echo "Spanish detected — rewrite"
```

If a Spanish commit already slipped in and is unpushed: `git commit --amend -m "<english message>"`. If pushed: leave history alone, ensure subsequent commits are English.

## Anti-Patterns

- `fix: solventa los pipelines` — Spanish commit.
- Mixed: `fix(ci): resolve failures — solventa fallos` — still fails; entire message must be English.
- Translating only the title, leaving the body in Spanish.
- `// Comentario en español` in code — comments count too.

False positives on the accent heuristic (e.g., test fixtures with non-English data) are acceptable — judge context; the rule applies to authored prose.
