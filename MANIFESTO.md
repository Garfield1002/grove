# The Grove Manifesto

## A durable workspace for parallel software development

Modern software work is no longer one repository, one branch, one terminal,
and one uninterrupted train of thought.

Developers move between features, investigations, reviews, production issues,
experiments, and maintenance. Some work is performed directly by a human.
Some is delegated to coding agents. Much of it remains active while attention
moves elsewhere.

The constraint has moved. When work is delegated as easily as it is started,
producing it is no longer the slow part. The scarce resource is human
attention spread across many parallel lines of work, not the work itself.

Failure at that scale looks like not noticing rather than working slowly:
something finished and was never reviewed, something has been waiting for an
answer, something failed quietly and still looks alive. None of it is visible
from inside any single session.

The tools underneath this work are already good. Git owns source history and
worktrees. tmux owns persistent terminal sessions. Terminal emulators present
them. Coding agents participate in them.

What is missing is a reliable place from which a human can understand and
control all of those concurrent lines of work.

Grove will be that place.

## The objective

Grove will become the local control center for parallel software development.
Every active line of work will have a durable workspace, terminal session,
agent context, status, and safe lifecycle. Moving between them will be
instantaneous.

Opening Grove should immediately answer:

- What work exists?
- What is running?
- What is blocked or failing?
- What needs human attention?
- What is ready to review or finish?
- How do I return to the exact context?

A developer should be able to begin a task, leave it running for days, switch
to other work, restart Grove or the machine, and return without reconstructing
the workspace manually.

Our long-term measure of success is demanding:

> A developer can manage 10–30 concurrent lines of work, understand their
> state immediately, resume any one in under five seconds, and safely close
> completed work without manually managing worktrees, tmux sessions, or agent
> processes.

## The line of work

The central object in Grove is not merely a repository, branch, worktree,
terminal, or agent. It is a **line of work**.

A line of work may include:

- a repository and Git worktree,
- a branch, upstream, and integration state,
- persistent terminal windows and running commands,
- one or more coding-agent conversations,
- a task, issue, incident, or review reference,
- test, lint, build, and validation results,
- pull-request and review state,
- a lifecycle such as active, waiting, blocked, failed, ready, merged, or
  archived.

These parts remain owned by the tools that created them. Grove observes,
relates, and controls them without pretending to replace them.

Git and tmux are sources of truth. Grove's state is an index. Losing Grove's
index must not lose work, and stale metadata must never cause destructive
action.

## Four product promises

### Durable workspaces

Work survives changes in attention and failures in presentation.

Closing the GUI does not end a session. Restarting Grove reconstructs reality
from Git and tmux. Restarting the machine offers a clear and safe path back to
the work that can be resumed. Grove never relies on one fragile process or one
opaque database to remember what matters.

### Awareness across parallel work

Grove makes many simultaneous efforts understandable at a glance.

It reports what is active, idle, blocked, failed, awaiting input, ready for
review, or complete using explicit and trustworthy evidence. It does not
invent semantic certainty from process names or terminal text.

Attention is a contract: when Grove says something needs the user, it can
explain why.

### Instant context switching

Returning to work should take one deliberate action.

Grove selects the correct project, worktree, tmux session, window, and agent
conversation. It restores context without moving or focusing external windows
through compositor-specific tricks.

The developer spends attention on the work, not on reconstructing where the
work was.

### Safe completion

Finishing work is as important as starting it.

Grove helps inspect, validate, review, integrate, archive, and eventually
remove completed work. Removing a project from Grove, closing a tmux session,
removing a worktree, and deleting a branch remain distinct operations with
distinct confirmation.

No cleanup shortcut is worth lost work.

## Bulletproof by construction

Grove coordinates repositories, processes, persistent sessions, and agent
work. A defect can lose time, misrepresent important state, interrupt running
work, or destroy data. Correctness is therefore a product feature and a
precondition for every other promise in this manifesto.

Our objective is not merely "high quality." Grove should be effectively
bug-free in normal and adversarial use.

We will pursue that objective with uncompromising engineering standards:

- **Zero known defects.** A known correctness defect blocks a release. Bugs
  are reproduced with a regression test before or alongside the fix.
- **100% test coverage.** All reachable production behavior is covered,
  including lines, branches, error paths, parsers, state transitions,
  reconciliation cases, and destructive-operation guards. Coverage may not be
  increased by deleting meaningful assertions or declaring production code
  exempt.
- **Tests at the correct boundary.** Pure logic receives exhaustive unit
  tests. Git behavior is tested against temporary real repositories. tmux
  behavior is tested against throwaway private sockets. Service behavior is
  tested through its actual protocol. Critical user workflows receive
  end-to-end tests.
- **Adversarial inputs are ordinary inputs.** Malformed subprocess output,
  corrupt or newer state, vanished paths, stale sockets, interrupted writes,
  duplicate messages, delayed events, disconnected clients, unusual Unicode,
  and hostile branch or path strings are expected and tested.
- **Super-hard linting.** Every crate, target, feature combination, test,
  example, and platform-relevant configuration builds without warnings.
  Clippy's strict and pedantic correctness lints are enabled deliberately.
  Suppressions require a local explanation and review.
- **No panic as control flow.** Production code does not use `unwrap`,
  `expect`, unchecked indexing, or intentional panic for recoverable
  conditions. Failures retain their source and reach a surface where the user
  can act on them.
- **No unverified concurrency assumptions.** Ownership, ordering,
  idempotency, reconnect behavior, queue bounds, and shutdown behavior are
  explicit and tested. The GUI may cache state for presentation, but one
  component owns every durable mutation.
- **No destructive ambiguity.** Every destructive action resolves an exact
  target, gathers current risk information, requires the appropriate
  confirmation, and has a test proving that adjacent resources are untouched.
- **No silent degradation.** Missing dependencies and unsupported
  environments are reported clearly. Integration tests skip only when their
  external binary is genuinely absent, and say so.
- **No quality debt hidden behind velocity.** New functionality and its tests
  land together. A feature that cannot yet be made reliable is unfinished,
  not "good enough for now."

Coverage is a floor, not evidence by itself. A test suite that executes every
line but fails to challenge the design does not meet this standard. Reviews
must also examine invariants, failure modes, ownership, and whether the code is
small enough to reason about.

Simplicity is part of correctness. Grove should have one owner for durable
state, one implementation of each operation, and the fewest runtime components
that satisfy the product. Duplicate paths are defects waiting to disagree.

## Principles

### Preserve reality; reconstruct indexes

Git repositories, worktrees, branches, tmux sessions, and agent conversations
must never be deleted because Grove's index forgot them.

Reconciliation marks missing, stopped, unavailable, or orphaned resources. It
does not silently clean them up.

### Prefer explicit signals

Grove does not read terminal contents. It does not claim that an agent needs
attention because of a guessed process name. Agents and integrations report
semantic state through bounded, documented interfaces.

When certainty is unavailable, Grove reports what it actually knows.

### Keep control local

Grove is a local desktop tool. Its daemon exists to preserve state, observe
local tools, and serve local clients reliably. It is not a remote execution
platform or a general workflow server.

Automation surfaces should express Grove's domain operations rather than
expose arbitrary commands.

### Integrate; do not replace

Grove will not become an IDE, terminal emulator, Git implementation, tmux
replacement, coding agent, issue tracker, or CI service.

It provides the durable connective tissue between those tools.

### Humans authorize consequences

Agents may start work, report status, run configured checks, and prepare
changes. Destructive cleanup and irreversible lifecycle transitions remain
visible and deliberate.

### Earn every abstraction

The implementation should be small enough to understand and test completely.
New layers, protocols, caches, fallbacks, and background workers must remove
more complexity than they introduce.

When two components can disagree, either establish one authoritative owner or
remove one of them.

## What Grove will not become

- It will not render terminals.
- It will not replace Git or hide Git's actual state.
- It will not parse terminal scrollback to infer meaning.
- It will not require a Grove-specific repository layout.
- It will not depend on compositor-specific window manipulation.
- It will not become a cloud account or hosted control plane.
- It will not expose arbitrary remote shell execution.
- It will not make autonomous destructive decisions.
- It will not trade correctness for a larger feature list.

## The standard for progress

A change advances Grove when it makes parallel work more durable,
understandable, resumable, or safe while preserving the ability to reason
completely about the system.

Every substantial feature should answer:

1. Which part of a line of work does this represent or control?
2. What is the source of truth?
3. Who owns the state transition?
4. How does it recover after interruption or restart?
5. How can it fail?
6. What proves that failure is safe?
7. Is every reachable behavior tested?
8. Did this remove or introduce another way for components to disagree?

The destination is not a dashboard full of integrations. It is confidence:

> Open Grove. See the truth. Resume anything. Lose nothing.
