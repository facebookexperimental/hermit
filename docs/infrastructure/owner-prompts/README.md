# Owner prompts about validation and landing infrastructure

This folder preserves the owner's requirements that govern Hermit's validation,
landing, CI, and worktree infrastructure.  It exists so a maintainer can check a
design against what the owner asked for instead of inferring intent from the
current implementation or from later agent-written policy.

The source for the 2026-08-11 prompts is ORC session
`31071829-387c-439f-ac27-7adbb2c56d35`, table `content_blocks`, role `user`.
Each excerpt records its ORC turn and UTC timestamp.  The excerpts preserve the
requirements while omitting unrelated chat context; punctuation, capitalization,
profanity, and obvious speech-to-text errors are normalized for a product
document.  Use the session row above when exact original wording is required.
The task directive supplied directly to the infrastructure-overhaul agent is in
[`2026-08-11-to-12.md`](2026-08-11-to-12.md#single-threaded-overhaul).

Use ordinary repository search:

```sh
rg -n -i 'local validate|ledger|landing|worktree|warnings|help' \
  docs/infrastructure/owner-prompts
```

[`INTENT.md`](INTENT.md) is the derived brief.  It links every conclusion back
to the prompt excerpts here.  Newer explicit owner decisions supersede older
ones; implementation behavior and agent prose do not supersede either.
