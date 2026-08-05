# Codex skill entrypoints

Stock Codex discovers the product skills in this directory. Every entry is a
whole-package symlink back to `.claude/skills/<name>/`; `.llms/skills` links to
the same canonical package root. Claude, Codex, and `.llms` consumers therefore
read one `SKILL.md` plus the same bundled resources.

Run `scripts/check-skill-discovery.rs` after changing product skills. Parent
coordinator roles do not belong in this product repository.
