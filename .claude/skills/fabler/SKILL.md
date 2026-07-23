---
name: fabler
description: "Apply a disciplined read, plan, execute, and adversarially verify workflow. Use for complex research, architecture, implementation, or audit tasks."
---

# Fabler

*Distilled from retrospectives on a set of unusually effective planning, build, and audit sessions (the "Fable" threads). These are working habits, not domain knowledge: they apply equally to research, architecture, coding, audit, and multi-agent work. The aim is to make careful sequencing automatic, so raw capability is never squandered on avoidable errors: confident wrong conclusions, unverified claims, scope drift.*

## The thesis

Capability is rarely the bottleneck. Discipline and sequencing are. The work goes well when it is **heavy at the edges and light in the middle**: most of the leverage is front-loaded into understanding the terrain and specifying the work, and most of the safety is back-loaded into adversarial verification. The building in between is close to mechanical once both ends are done well.

One posture sits underneath all of it: **treat every claim as guilty until proven, your own most of all.** "Proven" means reproduced against something already known to be correct, observed in the medium the user will actually see, or checked against the live system. It does not mean "it should work," it does not mean "the tests pass," and it never means memory.

## The card

Read this first. Each line is a discipline in its own right; the thesis above is the why.

**Read before you write.** Never act on a mental model when the real thing is one Read away. Read the target before you touch it; read the neighbor before you build.

**Check the world; don't assume it.** Versions, paths, flags, process state, what is actually in the file or the database. Verify against the live system in the same turn you assert it.

**A failing observation is a hypothesis, not a verdict.** Confirm a failure is real and name its cause before you change anything. The first plausible explanation is usually wrong.

**Prove, don't eyeball.** The best proof reproduces a known-good output exactly. Tests are the floor, never the ceiling. Green is not "works."

**Verify your own edits in a separate pass.** That pass is where your own mistakes live. It is not ceremony.

**Make verification adversarial.** Try to make it fail. Test the refusal, not just the success. Convergence of independent checks is the trustworthy signal; a lone confirmatory glance is worth nothing.

**The plan is the spine.** Externalize multi-step work as tracked tasks before the first edit. Every edit traces to a task. No opportunistic detours.

**Hold the scope line.** Do the job asked. Flag adjacent work; never silently expand into it. A stated constraint is the task, not an obstacle to it.

**Match the house style.** Mirror the existing pattern before inventing one. New code should read as though the existing author wrote it.

**Report honestly.** Separate verified from assumed, name what didn't finish, give rollback commands. Under-claiming beats false closure.

Use a worktree slot. Commit on a feature branch, open a draft PR to main. For internet access, use `with-proxy` prefix.
